use std::{
    sync::{
        Arc, RwLock,
        atomic::{AtomicI64, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{
        HeaderMap, HeaderName, Request, Response, StatusCode,
        header::{AUTHORIZATION, CONNECTION, HOST},
    },
    response::IntoResponse,
    routing::any,
};
use futures_util::StreamExt;
use serde::Serialize;
use thiserror::Error;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

pub const LOCAL_GATEWAY_URL: &str = "http://127.0.0.1:11435";
const LISTEN_ADDRESS: &str = "127.0.0.1:11435";
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct Upstream {
    url: String,
    token: Arc<str>,
}

#[derive(Clone)]
struct GatewayState {
    upstream: Arc<RwLock<Option<Upstream>>>,
    local_token: Arc<str>,
    client: reqwest::Client,
    activity: Arc<Activity>,
}

/// Inference activity used by the idle timeout.
///
/// A single agent generation can stream for longer than the idle timeout, so
/// requests are counted while they are in flight instead of only being stamped
/// when they arrive.
struct Activity {
    last_epoch_ms: AtomicU64,
    in_flight: AtomicI64,
}

/// Keeps its request counted as in flight until the response body is finished
/// or dropped.
struct ActivityGuard {
    activity: Arc<Activity>,
}

pub struct LocalGateway {
    local_token: String,
    upstream: Arc<RwLock<Option<Upstream>>>,
    activity: Arc<Activity>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("local gateway could not bind to {LISTEN_ADDRESS}: {0}")]
    Bind(#[source] std::io::Error),
    #[error("local gateway routing lock is poisoned")]
    RoutingLock,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl Activity {
    fn new() -> Self {
        Self {
            last_epoch_ms: AtomicU64::new(now_epoch_ms()),
            in_flight: AtomicI64::new(0),
        }
    }

    fn touch(&self) {
        self.last_epoch_ms.store(now_epoch_ms(), Ordering::Relaxed);
    }

    fn begin(self: &Arc<Self>) -> ActivityGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        self.touch();
        ActivityGuard {
            activity: Arc::clone(self),
        }
    }

    fn last_epoch_ms(&self) -> u64 {
        if self.in_flight.load(Ordering::Acquire) > 0 {
            now_epoch_ms()
        } else {
            self.last_epoch_ms.load(Ordering::Relaxed)
        }
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.activity.touch();
        self.activity.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LocalGateway {
    pub async fn start(local_token: String) -> Result<Self, GatewayError> {
        let listener = TcpListener::bind(LISTEN_ADDRESS)
            .await
            .map_err(GatewayError::Bind)?;
        let upstream = Arc::new(RwLock::new(None));
        let activity = Arc::new(Activity::new());
        let state = GatewayState {
            upstream: Arc::clone(&upstream),
            local_token: Arc::from(local_token.as_str()),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("valid local gateway HTTP client"),
            activity: Arc::clone(&activity),
        };
        let router = Router::new()
            .route("/{*path}", any(forward))
            .fallback(forward)
            .with_state(state);
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = receiver.await;
                })
                .await
        });

        Ok(Self {
            local_token,
            upstream,
            activity,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub fn token(&self) -> &str {
        &self.local_token
    }

    pub fn connect(&self, upstream_url: &str, upstream_token: &str) -> Result<(), GatewayError> {
        *self
            .upstream
            .write()
            .map_err(|_| GatewayError::RoutingLock)? = Some(Upstream {
            url: upstream_url.trim_end_matches('/').to_owned(),
            token: Arc::from(upstream_token),
        });
        self.activity.touch();
        Ok(())
    }

    pub fn disconnect(&self) -> Result<(), GatewayError> {
        *self
            .upstream
            .write()
            .map_err(|_| GatewayError::RoutingLock)? = None;
        Ok(())
    }

    pub fn last_inference_epoch_ms(&self) -> u64 {
        self.activity.last_epoch_ms()
    }

    pub fn is_connected(&self) -> Result<bool, GatewayError> {
        self.upstream
            .read()
            .map(|upstream| upstream.is_some())
            .map_err(|_| GatewayError::RoutingLock)
    }
}

impl Drop for LocalGateway {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn forward(State(state): State<GatewayState>, request: Request<Body>) -> Response<Body> {
    if !has_valid_token(request.headers(), &state.local_token) {
        return json_error(StatusCode::UNAUTHORIZED, "invalid local API key");
    }
    let upstream = match state.upstream.read() {
        Ok(route) => route.clone(),
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "gateway state unavailable",
            );
        }
    };
    let Some(upstream) = upstream else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no model session is running",
        );
    };

    let activity = is_inference_path(request.uri().path()).then(|| state.activity.begin());
    let (parts, body) = request.into_parts();
    let target = target_url(&upstream.url, parts.uri.path_and_query());
    let request_body = match to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let mut outbound = state
        .client
        .request(parts.method, target)
        .bearer_auth(upstream.token.as_ref());
    for (name, value) in &parts.headers {
        if should_forward_request_header(name) {
            outbound = outbound.header(name, value);
        }
    }

    let upstream = match outbound.body(request_body).send().await {
        Ok(response) => response,
        Err(_) => return json_error(StatusCode::BAD_GATEWAY, "model endpoint unavailable"),
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let stream = upstream.bytes_stream().map(move |chunk| {
        if let Some(activity) = activity.as_ref() {
            activity.activity.touch();
        }
        chunk.map_err(|error| std::io::Error::other(error.to_string()))
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    copy_response_headers(&headers, response.headers_mut());
    response
}

fn has_valid_token(headers: &HeaderMap, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected)
}

fn is_inference_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/chat/completions" | "/v1/completions" | "/api/chat" | "/api/generate"
    )
}

fn target_url(upstream: &str, path_and_query: Option<&axum::http::uri::PathAndQuery>) -> String {
    match path_and_query {
        Some(path) => format!("{upstream}{path}"),
        None => upstream.to_owned(),
    }
}

fn should_forward_request_header(name: &HeaderName) -> bool {
    name != HOST
        && name != AUTHORIZATION
        && name != CONNECTION
        && name.as_str() != "proxy-authorization"
        && name.as_str() != "proxy-connection"
        && name.as_str() != "transfer-encoding"
        && name.as_str() != "upgrade"
}

fn copy_response_headers(source: &HeaderMap, destination: &mut HeaderMap) {
    for (name, value) in source {
        if name != CONNECTION
            && name.as_str() != "keep-alive"
            && name.as_str() != "proxy-authenticate"
            && name.as_str() != "proxy-connection"
            && name.as_str() != "te"
            && name.as_str() != "trailers"
            && name.as_str() != "transfer-encoding"
            && name.as_str() != "upgrade"
        {
            destination.append(name, value.clone());
        }
    }
}

fn json_error(status: StatusCode, message: &'static str) -> Response<Body> {
    (status, axum::Json(ErrorBody { error: message })).into_response()
}

fn now_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_requires_the_bearer_scheme_and_exact_token() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer correct".parse().unwrap());

        assert!(has_valid_token(&headers, "correct"));
        assert!(!has_valid_token(&headers, "wrong"));
    }

    #[test]
    fn only_generation_requests_count_as_activity() {
        assert!(is_inference_path("/v1/chat/completions"));
        assert!(is_inference_path("/api/generate"));
        assert!(!is_inference_path("/v1/models"));
        assert!(!is_inference_path("/api/tags"));
    }

    #[test]
    fn a_streaming_request_stays_active_however_long_it_runs() {
        let activity = Arc::new(Activity::new());
        activity.last_epoch_ms.store(0, Ordering::Relaxed);
        let guard = activity.begin();
        activity.last_epoch_ms.store(0, Ordering::Relaxed);

        assert!(
            activity.last_epoch_ms() > 0,
            "an in-flight generation must never look idle"
        );

        drop(guard);
        assert_eq!(activity.in_flight.load(Ordering::Acquire), 0);
        assert!(
            activity.last_epoch_ms() > 0,
            "finishing a generation restarts the idle countdown"
        );
    }

    #[test]
    fn concurrent_requests_release_activity_only_when_all_finish() {
        let activity = Arc::new(Activity::new());
        let first = activity.begin();
        let second = activity.begin();

        drop(first);
        assert_eq!(activity.in_flight.load(Ordering::Acquire), 1);

        drop(second);
        assert_eq!(activity.in_flight.load(Ordering::Acquire), 0);
    }

    #[test]
    fn path_and_query_are_preserved() {
        let path = "/v1/chat/completions?stream=true".parse().unwrap();
        assert_eq!(
            target_url("https://pod.example", Some(&path)),
            "https://pod.example/v1/chat/completions?stream=true"
        );
    }

    #[test]
    fn strips_secrets_and_hop_by_hop_headers() {
        assert!(!should_forward_request_header(&AUTHORIZATION));
        assert!(!should_forward_request_header(&CONNECTION));
        assert!(!should_forward_request_header(&HOST));
        assert!(should_forward_request_header(
            &axum::http::header::CONTENT_TYPE
        ));
    }
}
