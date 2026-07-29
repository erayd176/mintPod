use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
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

const LISTEN_ADDRESS: &str = "127.0.0.1:8080";
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone)]
struct ProxyState {
    upstream: String,
    token: Arc<str>,
    client: reqwest::Client,
    last_request_epoch_ms: Arc<AtomicU64>,
}

pub struct LocalProxy {
    token: String,
    last_request_epoch_ms: Arc<AtomicU64>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<(), std::io::Error>>>,
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("local proxy could not bind to {LISTEN_ADDRESS}: {0}")]
    Bind(#[source] std::io::Error),
    #[error("could not generate a local proxy token: {0}")]
    Random(String),
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl LocalProxy {
    pub async fn start(upstream: &str) -> Result<Self, ProxyError> {
        let listener = TcpListener::bind(LISTEN_ADDRESS)
            .await
            .map_err(ProxyError::Bind)?;
        let token = generate_token()?;
        let last_request_epoch_ms = Arc::new(AtomicU64::new(now_epoch_ms()));
        let state = ProxyState {
            upstream: upstream.trim_end_matches('/').to_owned(),
            token: Arc::from(token.as_str()),
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("valid local proxy HTTP client"),
            last_request_epoch_ms: Arc::clone(&last_request_epoch_ms),
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
            token,
            last_request_epoch_ms,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn last_request_epoch_ms(&self) -> u64 {
        self.last_request_epoch_ms.load(Ordering::Relaxed)
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for LocalProxy {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn forward(State(state): State<ProxyState>, request: Request<Body>) -> Response<Body> {
    if !has_valid_token(request.headers(), &state.token) {
        return json_error(StatusCode::UNAUTHORIZED, "invalid local API key");
    }

    state
        .last_request_epoch_ms
        .store(now_epoch_ms(), Ordering::Relaxed);
    let (parts, body) = request.into_parts();
    let target = target_url(&state.upstream, parts.uri.path_and_query());
    let request_body = match to_bytes(body, MAX_REQUEST_BYTES).await {
        Ok(body) => body,
        Err(_) => return json_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let mut outbound = state.client.request(parts.method, target);
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
    let stream = upstream
        .bytes_stream()
        .map(|chunk| chunk.map_err(|error| std::io::Error::other(error.to_string())));
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

fn generate_token() -> Result<String, ProxyError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| ProxyError::Random(error.to_string()))?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
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
    fn token_has_256_bits_of_hex_entropy() {
        let token = generate_token().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn authorization_requires_the_bearer_scheme_and_exact_token() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer correct".parse().unwrap());

        assert!(has_valid_token(&headers, "correct"));
        assert!(!has_valid_token(&headers, "wrong"));
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
