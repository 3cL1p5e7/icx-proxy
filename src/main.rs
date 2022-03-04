use crate::config::dns_canister_config::DnsCanisterConfig;
use clap::{crate_authors, crate_version, AppSettings, Parser};
use flate2::read::{DeflateDecoder, GzDecoder};
use hyper::{
    body,
    body::Bytes,
    http::uri::Parts,
    service::{make_service_fn, service_fn},
    Body, Client, Request, Response, Server, StatusCode, Uri,
};
use ic_agent::{
    agent::http_transport::ReqwestHttpReplicaV2Transport,
    export::Principal,
    ic_types::{hash_tree::LookupResult, HashTree},
    lookup_value, Agent, AgentError, Certificate,
};
use ic_utils::{
    call::AsyncCall,
    call::SyncCall,
    interfaces::http_request::{
        HeaderField, HttpRequestCanister, HttpResponse, StreamingCallbackHttpResponse,
        StreamingStrategy,
    },
};
use lazy_regex::regex_captures;
use sha2::{Digest, Sha256};
use slog::Drain;
use std::io::prelude::Read;
use std::{
    convert::Infallible,
    error::Error,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

mod config;
mod logging;

// Limit the total number of calls to an HTTP Request loop to 1000 for now.
static MAX_HTTP_REQUEST_STREAM_CALLBACK_CALL_COUNT: i32 = 1000;

// The maximum length of a body we should log as tracing.
static MAX_LOG_BODY_SIZE: usize = 100;

// The limit of a buffer we should decompress ~10mb.
static MAX_BYTES_SIZE_TO_DECOMPRESS: u64 = 10_000_000;

#[derive(Parser)]
#[clap(
    version = crate_version!(),
    author = crate_authors!(),
    global_setting = AppSettings::PropagateVersion,
)]
pub(crate) struct Opts {
    /// Verbose level. By default, INFO will be used. Add a single `-v` to upgrade to
    /// DEBUG, and another `-v` to upgrade to TRACE.
    #[clap(long, short('v'), parse(from_occurrences))]
    verbose: u64,

    /// Quiet level. The opposite of verbose. A single `-q` will drop the logging to
    /// WARN only, then another one to ERR, and finally another one for FATAL. Another
    /// `-q` will silence ALL logs.
    #[clap(long, short('q'), parse(from_occurrences))]
    quiet: u64,

    /// Mode to use the logging. "stderr" will output logs in STDERR, "file" will output
    /// logs in a file, and "tee" will do both.
    #[clap(long("log"), default_value("stderr"), possible_values(&["stderr", "tee", "file"]))]
    logmode: String,

    /// File to output the log to, when using logmode=tee or logmode=file.
    #[clap(long)]
    logfile: Option<PathBuf>,

    /// The address to bind to.
    #[clap(long, default_value = "127.0.0.1:3000")]
    address: SocketAddr,

    /// A replica to use as backend. Locally, this should be a local instance or the
    /// boundary node. Multiple replicas can be passed and they'll be used round-robin.
    #[clap(long, default_value = "http://localhost:8000/")]
    replica: Vec<String>,

    /// An address to forward any requests from /_/
    #[clap(long)]
    proxy: Option<String>,

    /// Whether or not this is run in a debug context (e.g. errors returned in responses
    /// should show full stack and error details).
    #[clap(long)]
    debug: bool,

    /// Whether or not to fetch the root key from the replica back end. Do not use this when
    /// talking to the Internet Computer blockchain mainnet as it is unsecure.
    #[clap(long)]
    fetch_root_key: bool,

    /// A map of domain names to canister IDs.
    /// Format: domain.name:canister-id
    #[clap(long)]
    dns_alias: Vec<String>,

    /// A list of domain name suffixes.  If found, the next (to the left) subdomain
    /// is used as the Principal, if it parses as a Principal.
    #[clap(long, default_value = "localhost")]
    dns_suffix: Vec<String>,
}

fn resolve_canister_id_from_hostname(
    hostname: &str,
    dns_canister_config: &DnsCanisterConfig,
) -> Option<Principal> {
    let url = Uri::from_str(hostname).ok()?;

    let split_hostname = url.host()?.split('.').collect::<Vec<&str>>();
    let split_hostname = split_hostname.as_slice();

    if let Some(principal) =
        dns_canister_config.resolve_canister_id_from_split_hostname(split_hostname)
    {
        return Some(principal);
    }
    // Check if it's localhost or ic0.
    match split_hostname {
        [.., maybe_canister_id, "localhost"] => Principal::from_text(maybe_canister_id).ok(),
        [maybe_canister_id, ..] => Principal::from_text(maybe_canister_id).ok(),
        _ => None,
    }
}

fn resolve_canister_id_from_uri(url: &hyper::Uri) -> Option<Principal> {
    let (_, canister_id) = url::form_urlencoded::parse(url.query()?.as_bytes())
        .find(|(name, _)| name == "canisterId")?;
    Principal::from_text(canister_id.as_ref()).ok()
}

/// Try to resolve a canister ID from an HTTP Request. If it cannot be resolved,
/// [None] will be returned.
fn resolve_canister_id(
    request: &Request<Body>,
    dns_canister_config: &DnsCanisterConfig,
) -> Option<Principal> {
    // Look for subdomains if there's a host header.
    if let Some(host_header) = request.headers().get("Host") {
        if let Ok(host) = host_header.to_str() {
            if let Some(canister_id) = resolve_canister_id_from_hostname(host, dns_canister_config)
            {
                return Some(canister_id);
            }
        }
    }

    // Look into the URI.
    if let Some(canister_id) = resolve_canister_id_from_uri(request.uri()) {
        return Some(canister_id);
    }

    // Look into the request by header.
    if let Some(referer_header) = request.headers().get("referer") {
        if let Ok(referer) = referer_header.to_str() {
            if let Ok(referer_uri) = hyper::Uri::from_str(referer) {
                if let Some(canister_id) = resolve_canister_id_from_uri(&referer_uri) {
                    return Some(canister_id);
                }
            }
        }
    }

    None
}

fn decode_hash_tree(
    name: &str,
    value: Option<String>,
    logger: &slog::Logger,
) -> Result<Vec<u8>, ()> {
    match value {
        Some(tree) => base64::decode(tree).map_err(|e| {
            slog::warn!(logger, "Unable to decode {} from base64: {}", name, e);
        }),
        _ => Err(()),
    }
}

struct HeadersData {
    certificate: Option<Result<Vec<u8>, ()>>,
    tree: Option<Result<Vec<u8>, ()>>,
    encoding: Option<String>,
}

fn extract_headers_data(headers: &[HeaderField], logger: &slog::Logger) -> HeadersData {
    let mut headers_data = HeadersData {
        certificate: None,
        tree: None,
        encoding: None,
    };

    for HeaderField(name, value) in headers {
        if name.eq_ignore_ascii_case("IC-CERTIFICATE") {
            for field in value.split(',') {
                if let Some((_, name, b64_value)) = regex_captures!("^(.*)=:(.*):$", field.trim()) {
                    slog::trace!(logger, ">> certificate {}: {}", name, b64_value);
                    let bytes = decode_hash_tree(name, Some(b64_value.to_string()), logger);
                    if name == "certificate" {
                        headers_data.certificate = Some(match (headers_data.certificate, bytes) {
                            (None, bytes) => bytes,
                            (Some(Ok(certificate)), Ok(bytes)) => {
                                slog::warn!(logger, "duplicate certificate field: {:?}", bytes);
                                Ok(certificate)
                            }
                            (Some(Ok(certificate)), Err(_)) => {
                                slog::warn!(
                                    logger,
                                    "duplicate certificate field (failed to decode)"
                                );
                                Ok(certificate)
                            }
                            (Some(Err(_)), bytes) => {
                                slog::warn!(
                                    logger,
                                    "duplicate certificate field (failed to decode)"
                                );
                                bytes
                            }
                        });
                    } else if name == "tree" {
                        headers_data.tree = Some(match (headers_data.tree, bytes) {
                            (None, bytes) => bytes,
                            (Some(Ok(tree)), Ok(bytes)) => {
                                slog::warn!(logger, "duplicate tree field: {:?}", bytes);
                                Ok(tree)
                            }
                            (Some(Ok(tree)), Err(_)) => {
                                slog::warn!(logger, "duplicate tree field (failed to decode)");
                                Ok(tree)
                            }
                            (Some(Err(_)), bytes) => {
                                slog::warn!(logger, "duplicate tree field (failed to decode)");
                                bytes
                            }
                        });
                    }
                }
            }
        } else if name.eq_ignore_ascii_case("CONTENT-ENCODING") {
            let enc = value.trim().to_string();
            headers_data.encoding = Some(enc);
        }
    }

    headers_data
}

async fn forward_request(
    request: Request<Body>,
    agent: Arc<Agent>,
    dns_canister_config: &DnsCanisterConfig,
    logger: slog::Logger,
) -> Result<Response<Body>, Box<dyn Error>> {
    let canister_id = match resolve_canister_id(&request, dns_canister_config) {
        None => {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body("Could not find a canister id to forward to.".into())
                .unwrap())
        }
        Some(x) => x,
    };

    slog::trace!(
        logger,
        "<< {} {} {:?}",
        request.method(),
        request.uri(),
        &request.version()
    );

    let method = request.method().to_string();
    let uri = request.uri().clone();
    let headers = request
        .headers()
        .into_iter()
        .filter_map(|(name, value)| {
            Some(HeaderField(
                name.to_string(),
                value.to_str().ok()?.to_string(),
            ))
        })
        .inspect(|HeaderField(name, value)| {
            slog::trace!(logger, "<< {}: {}", name, value);
        })
        .collect::<Vec<_>>();

    let entire_body = body::to_bytes(request.into_body()).await?.to_vec();

    slog::trace!(logger, "<<");
    if logger.is_trace_enabled() {
        let body = String::from_utf8_lossy(
            &entire_body[0..usize::min(entire_body.len(), MAX_LOG_BODY_SIZE)],
        );
        slog::trace!(
            logger,
            "<< \"{}\"{}",
            &body.escape_default(),
            if body.len() > MAX_LOG_BODY_SIZE {
                format!("... {} bytes total", body.len())
            } else {
                String::new()
            }
        );
    }

    let canister = HttpRequestCanister::create(agent.as_ref(), canister_id);
    let query_result = canister
        .http_request(
            method.clone(),
            uri.to_string(),
            headers.clone(),
            &entire_body,
        )
        .call()
        .await;

    fn handle_result(
        result: Result<(HttpResponse,), AgentError>,
    ) -> Result<HttpResponse, Result<Response<Body>, Box<dyn Error>>> {
        // If the result is a Replica error, returns the 500 code and message. There is no information
        // leak here because a user could use `dfx` to get the same reply.
        match result {
            Ok((http_response,)) => Ok(http_response),
            Err(AgentError::ReplicaError {
                reject_code,
                reject_message,
            }) => Err(Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(format!(r#"Replica Error ({}): "{}""#, reject_code, reject_message).into())
                .unwrap())),
            Err(e) => Err(Err(e.into())),
        }
    }

    let http_response = match handle_result(query_result) {
        Ok(http_response) => http_response,
        Err(response_or_error) => return response_or_error,
    };

    let http_response = if http_response.upgrade == Some(true) {
        let waiter = garcon::Delay::builder()
            .throttle(std::time::Duration::from_millis(500))
            .timeout(std::time::Duration::from_secs(15))
            .build();
        let update_result = canister
            .http_request_update(method, uri.to_string(), headers, &entire_body)
            .call_and_wait(waiter)
            .await;
        let http_response = match handle_result(update_result) {
            Ok(http_response) => http_response,
            Err(response_or_error) => return response_or_error,
        };
        http_response
    } else {
        http_response
    };

    let mut builder = Response::builder().status(StatusCode::from_u16(http_response.status_code)?);
    for HeaderField(name, value) in &http_response.headers {
        builder = builder.header(name, value);
    }

    let headers_data = extract_headers_data(&http_response.headers, &logger);
    let body = if logger.is_trace_enabled() {
        Some(http_response.body.clone())
    } else {
        None
    };
    let is_streaming = http_response.streaming_strategy.is_some();
    let response = if is_streaming {
        let streaming_strategy = http_response.streaming_strategy.unwrap();
        let (mut sender, body) = body::Body::channel();
        let agent = agent.as_ref().clone();
        sender.send_data(Bytes::from(http_response.body)).await?;

        match streaming_strategy {
            StreamingStrategy::Callback(callback) => {
                let streaming_canister_id_id = callback.callback.principal;
                let method_name = callback.callback.method;
                let mut callback_token = callback.token;
                let logger = logger.clone();
                tokio::spawn(async move {
                    let canister = HttpRequestCanister::create(&agent, streaming_canister_id_id);
                    // We have not yet called http_request_stream_callback.
                    let mut count = 0;
                    loop {
                        count += 1;
                        if count > MAX_HTTP_REQUEST_STREAM_CALLBACK_CALL_COUNT {
                            sender.abort();
                            break;
                        }

                        match canister
                            .http_request_stream_callback(&method_name, callback_token)
                            .call()
                            .await
                        {
                            Ok((StreamingCallbackHttpResponse { body, token },)) => {
                                if sender.send_data(Bytes::from(body)).await.is_err() {
                                    sender.abort();
                                    break;
                                }
                                if let Some(next_token) = token {
                                    callback_token = next_token;
                                } else {
                                    break;
                                }
                            }
                            Err(e) => {
                                slog::debug!(logger, "Error happened during streaming: {}", e);
                                sender.abort();
                                break;
                            }
                        }
                    }
                });
            }
        }

        builder.body(body)?
    } else {
        let body_valid = validate(
            &headers_data,
            &canister_id,
            &agent,
            &uri,
            &http_response.body,
            logger.clone(),
        );
        if body_valid.is_err() {
            return Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(body_valid.unwrap_err().into())
                .unwrap());
        }
        builder.body(http_response.body.into())?
    };

    if logger.is_trace_enabled() {
        slog::trace!(
            logger,
            ">> {:?} {} {}",
            &response.version(),
            response.status().as_u16(),
            response.status().to_string()
        );

        for (name, value) in response.headers() {
            let value = String::from_utf8_lossy(value.as_bytes());
            slog::trace!(logger, ">> {}: {}", name, value);
        }

        let body = body.unwrap_or_else(|| b"... streaming ...".to_vec());

        slog::trace!(logger, ">>");
        slog::trace!(
            logger,
            ">> \"{}\"{}",
            String::from_utf8_lossy(&body[..usize::min(MAX_LOG_BODY_SIZE, body.len())])
                .escape_default(),
            if is_streaming {
                "... streaming".to_string()
            } else if body.len() > MAX_LOG_BODY_SIZE {
                format!("... {} bytes total", body.len())
            } else {
                String::new()
            }
        );
    }

    Ok(response)
}

fn validate(
    headers_data: &HeadersData,
    canister_id: &Principal,
    agent: &Agent,
    uri: &Uri,
    response_body: &[u8],
    logger: slog::Logger,
) -> Result<(), String> {
    let body_sha = decode_body(response_body, headers_data.encoding.clone());
    let body_valid = match (headers_data.certificate.clone(), headers_data.tree.clone()) {
        (Some(Ok(certificate)), Some(Ok(tree))) => match validate_body(
            Certificates { certificate, tree },
            canister_id,
            agent,
            uri,
            &body_sha,
            logger.clone(),
        ) {
            Ok(valid) => {
                if valid {
                    Ok(())
                } else {
                    Err("Body does not pass verification".to_string())
                }
            }
            Err(e) => Err(format!("Certificate validation failed: {}", e)),
        },
        (Some(_), _) | (_, Some(_)) => Err("Body does not pass verification".to_string()),
        // Canisters don't have to provide certified variables
        (None, None) => Ok(()),
    };

    if body_valid.is_err() && !cfg!(feature = "skip_body_verification") {
        return body_valid;
    }

    Ok(())
}

fn decode_body(body: &[u8], encoding: Option<String>) -> [u8; 32] {
    let mut sha256 = Sha256::new();
    match encoding {
        Some(enc) => match enc.as_str() {
            "gzip" => {
                let decoded: &mut Vec<u8> = &mut vec![];
                let decoder = GzDecoder::new(body);
                decoder
                    .take(MAX_BYTES_SIZE_TO_DECOMPRESS)
                    .read_to_end(decoded)
                    .unwrap();
                sha256.update(decoded);
            }
            "deflate" => {
                let decoded: &mut Vec<u8> = &mut vec![];
                let decoder = DeflateDecoder::new(body);
                decoder
                    .take(MAX_BYTES_SIZE_TO_DECOMPRESS)
                    .read_to_end(decoded)
                    .unwrap();
                sha256.update(decoded);
            }
            _ => sha256.update(body),
        },
        _ => sha256.update(body),
    };
    sha256.finalize().into()
}

struct Certificates {
    certificate: Vec<u8>,
    tree: Vec<u8>,
}

fn validate_body(
    certificates: Certificates,
    canister_id: &Principal,
    agent: &Agent,
    uri: &Uri,
    body_sha: &[u8; 32],
    logger: slog::Logger,
) -> anyhow::Result<bool> {
    let cert: Certificate =
        serde_cbor::from_slice(&certificates.certificate).map_err(AgentError::InvalidCborData)?;
    let tree: HashTree =
        serde_cbor::from_slice(&certificates.tree).map_err(AgentError::InvalidCborData)?;

    if let Err(e) = agent.verify(&cert) {
        slog::trace!(logger, ">> certificate failed verification: {}", e);
        return Ok(false);
    }

    let certified_data_path = vec![
        "canister".into(),
        canister_id.into(),
        "certified_data".into(),
    ];
    let witness = match lookup_value(&cert, certified_data_path) {
        Ok(witness) => witness,
        Err(e) => {
            slog::trace!(
                logger,
                ">> Could not find certified data for this canister in the certificate: {}",
                e
            );
            return Ok(false);
        }
    };
    let digest = tree.digest();

    if witness != digest {
        slog::trace!(
            logger,
            ">> witness ({}) did not match digest ({})",
            hex::encode(witness),
            hex::encode(digest)
        );

        return Ok(false);
    }

    let path = ["http_assets".into(), uri.path().into()];
    let tree_sha = match tree.lookup_path(&path) {
        LookupResult::Found(v) => v,
        _ => match tree.lookup_path(&["http_assets".into(), "/index.html".into()]) {
            LookupResult::Found(v) => v,
            _ => {
                slog::trace!(
                    logger,
                    ">> Invalid Tree in the header. Does not contain path {:?}",
                    path
                );
                return Ok(false);
            }
        },
    };

    Ok(body_sha == tree_sha)
}

fn is_hop_header(name: &str) -> bool {
    name.to_ascii_lowercase() == "connection"
        || name.to_ascii_lowercase() == "keep-alive"
        || name.to_ascii_lowercase() == "proxy-authenticate"
        || name.to_ascii_lowercase() == "proxy-authorization"
        || name.to_ascii_lowercase() == "te"
        || name.to_ascii_lowercase() == "trailers"
        || name.to_ascii_lowercase() == "transfer-encoding"
        || name.to_ascii_lowercase() == "upgrade"
}

/// Returns a clone of the headers without the [hop-by-hop headers].
///
/// [hop-by-hop headers]: http://www.w3.org/Protocols/rfc2616/rfc2616-sec13.html
fn remove_hop_headers(
    headers: &hyper::header::HeaderMap<hyper::header::HeaderValue>,
) -> hyper::header::HeaderMap<hyper::header::HeaderValue> {
    let mut result = hyper::HeaderMap::new();
    for (k, v) in headers.iter() {
        if !is_hop_header(k.as_str()) {
            result.insert(k.clone(), v.clone());
        }
    }
    result
}

fn forward_uri<B>(forward_url: &str, req: &Request<B>) -> Result<Uri, Box<dyn Error>> {
    let uri = Uri::from_str(forward_url)?;
    let mut parts = Parts::from(uri);
    parts.path_and_query = req.uri().path_and_query().cloned();

    Ok(Uri::from_parts(parts)?)
}

fn create_proxied_request<B>(
    client_ip: &IpAddr,
    forward_url: &str,
    mut request: Request<B>,
) -> Result<Request<B>, Box<dyn Error>> {
    *request.headers_mut() = remove_hop_headers(request.headers());
    *request.uri_mut() = forward_uri(forward_url, &request)?;

    let x_forwarded_for_header_name = "x-forwarded-for";

    // Add forwarding information in the headers
    match request.headers_mut().entry(x_forwarded_for_header_name) {
        hyper::header::Entry::Vacant(entry) => {
            entry.insert(client_ip.to_string().parse()?);
        }

        hyper::header::Entry::Occupied(mut entry) => {
            let addr = format!("{}, {}", entry.get().to_str()?, client_ip);
            entry.insert(addr.parse()?);
        }
    }

    Ok(request)
}

async fn forward_api(
    ip_addr: &IpAddr,
    request: Request<Body>,
    replica_url: &str,
) -> Result<Response<Body>, Box<dyn Error>> {
    let proxied_request = create_proxied_request(ip_addr, replica_url, request)?;

    let client = Client::builder().build(hyper_tls::HttpsConnector::new());
    let response = client.request(proxied_request).await?;
    Ok(response)
}

fn not_found() -> Result<Response<Body>, Box<dyn Error>> {
    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body("Not found".into())?)
}

fn unable_to_fetch_root_key() -> Result<Response<Body>, Box<dyn Error>> {
    Ok(Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body("Unable to fetch root key".into())?)
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    ip_addr: IpAddr,
    request: Request<Body>,
    replica_url: String,
    proxy_url: Option<String>,
    dns_canister_config: Arc<DnsCanisterConfig>,
    logger: slog::Logger,
    fetch_root_key: bool,
    debug: bool,
) -> Result<Response<Body>, Infallible> {
    let request_uri_path = request.uri().path();
    let result = if request_uri_path.starts_with("/api/") {
        slog::debug!(
            logger,
            "URI Request to path '{}' being forwarded to Replica",
            &request.uri().path()
        );
        forward_api(&ip_addr, request, &replica_url).await
    } else if request_uri_path.starts_with("/_/") && !request_uri_path.starts_with("/_/raw") {
        if let Some(proxy_url) = proxy_url {
            slog::debug!(
                logger,
                "URI Request to path '{}' being forwarded to proxy",
                &request.uri().path(),
            );
            forward_api(&ip_addr, request, &proxy_url).await
        } else {
            slog::warn!(
                logger,
                "Unable to proxy {} because no --proxy is configured",
                &request.uri().path()
            );
            not_found()
        }
    } else {
        let agent = Arc::new(
            ic_agent::Agent::builder()
                .with_transport(ReqwestHttpReplicaV2Transport::create(replica_url).unwrap())
                .build()
                .expect("Could not create agent..."),
        );
        if fetch_root_key && agent.fetch_root_key().await.is_err() {
            unable_to_fetch_root_key()
        } else {
            forward_request(request, agent, dns_canister_config.as_ref(), logger.clone()).await
        }
    };

    match result {
        Err(err) => {
            slog::warn!(logger, "Internal Error during request:\n{:#?}", err);

            Ok(Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(if debug {
                    format!("Internal Error: {:?}", err).into()
                } else {
                    "Internal Server Error".into()
                })
                .unwrap())
        }
        Ok(x) => Ok::<_, Infallible>(x),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let opts: Opts = Opts::parse();

    let logger = logging::setup_logging(&opts);

    // Prepare a list of agents for each backend replicas.
    let replicas = Mutex::new(opts.replica.clone());

    let dns_canister_config = Arc::new(DnsCanisterConfig::new(&opts.dns_alias, &opts.dns_suffix)?);

    let counter = AtomicUsize::new(0);
    let debug = opts.debug;
    let proxy_url = opts.proxy.clone();
    let fetch_root_key = opts.fetch_root_key;

    let service = make_service_fn(|socket: &hyper::server::conn::AddrStream| {
        let ip_addr = socket.remote_addr();
        let ip_addr = ip_addr.ip();
        let dns_canister_config = dns_canister_config.clone();
        let logger = logger.clone();

        // Select an agent.
        let replica_url_array = replicas.lock().unwrap();
        let count = counter.fetch_add(1, Ordering::SeqCst);
        let replica_url = replica_url_array
            .get(count % replica_url_array.len())
            .unwrap_or_else(|| unreachable!());
        let replica_url = replica_url.clone();
        slog::debug!(logger, "Replica URL: {}", replica_url);

        let proxy_url = proxy_url.clone();

        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                let logger = logger.clone();
                let dns_canister_config = dns_canister_config.clone();
                handle_request(
                    ip_addr,
                    req,
                    replica_url.clone(),
                    proxy_url.clone(),
                    dns_canister_config,
                    logger,
                    fetch_root_key,
                    debug,
                )
            }))
        }
    });

    slog::info!(
        logger,
        "Starting server. Listening on http://{}/",
        opts.address
    );

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(10)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = Server::bind(&opts.address).serve(service);
        server.await?;
        Ok(())
    })
}
