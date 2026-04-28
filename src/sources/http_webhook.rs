use anyhow::{Result, bail};
use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::Value;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;

use crate::config::parse_config;
use crate::envelope::Envelope;
use crate::observability::trace_context::{TRACEPARENT, TRACESTATE};
use crate::observability::{NodeCtx, SourceCtx};
use crate::retry::RetryPolicy;
use crate::sources::Source;

/// Accepts HTTP webhook requests and emits each valid JSON request body as
/// an envelope payload. The request is acknowledged only after the envelope
/// has been sent to the pipeline channel, so normal mpsc backpressure applies
/// to incoming HTTP traffic.
pub struct HttpWebhookSource {
    id: String,
    bind: SocketAddr,
    path: String,
    source_ctx: SourceCtx,
}

impl HttpWebhookSource {
    pub fn new(id: impl Into<String>, bind: SocketAddr, path: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            source_ctx: SourceCtx::new(&id),
            id,
            bind,
            path: path.into(),
        }
    }
}

#[async_trait]
impl Source for HttpWebhookSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn set_node_ctx(&mut self, ctx: NodeCtx) {
        self.source_ctx = SourceCtx::from_node_ctx(ctx);
    }

    async fn run(self: Box<Self>, tx: Sender<Envelope>, cancel: CancellationToken) {
        let state = WebhookState {
            source_id: self.id.clone(),
            source_ctx: self.source_ctx.clone(),
            tx,
            cancel: cancel.clone(),
        };
        let app = Router::new()
            .route(&self.path, post(handle_webhook))
            .fallback(not_found)
            .with_state(state);

        let listener = match TcpListener::bind(self.bind).await {
            Ok(listener) => listener,
            Err(e) => {
                log::error!("[{}] failed to bind {}: {e}", self.id, self.bind);
                return;
            }
        };

        let local_addr = listener.local_addr().unwrap_or(self.bind);
        log::info!(
            "[{}] listening for POST {} on {}",
            self.id,
            self.path,
            local_addr
        );

        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(cancel.cancelled_owned())
            .await
        {
            log::error!("[{}] webhook server failed: {e}", self.id);
        }
    }
}

#[derive(Clone)]
struct WebhookState {
    source_id: String,
    source_ctx: SourceCtx,
    tx: Sender<Envelope>,
    cancel: CancellationToken,
}

async fn handle_webhook(
    State(state): State<WebhookState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload = match serde_json::from_slice::<Value>(&body) {
        Ok(payload) => payload,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid JSON request body: {e}"),
            )
                .into_response();
        }
    };

    let mut env = Envelope::new(&state.source_id, payload);
    capture_headers(&headers, &mut env);

    match state.source_ctx.send(&state.tx, env, &state.cancel).await {
        Ok(()) => (StatusCode::ACCEPTED, "accepted").into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "pipeline is not accepting webhook events",
        )
            .into_response(),
    }
}

async fn not_found() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "webhook path not found")
}

fn capture_headers(headers: &HeaderMap, env: &mut Envelope) {
    for (name, value) in headers {
        let Ok(value) = value.to_str() else {
            continue;
        };
        let name_str = name.as_str();
        if matches!(name_str, TRACEPARENT | TRACESTATE) {
            // Trace-context headers are owned by the observability layer.
            // Store them under the bare key so SourceCtx::send can refresh
            // the span context without leaving a stale `http.header.*` copy.
            env.meta
                .headers
                .insert(name_str.to_string(), value.to_string());
        } else {
            env.meta
                .headers
                .insert(format!("http.header.{}", name_str), value.to_string());
        }
    }
}

#[derive(Debug, Deserialize)]
struct HttpWebhookSourceConfig {
    bind: String,
    path: String,
}

/// Registry factory for [`HttpWebhookSource`]. Registered by
/// `courier::registry::register_builtin` under kind `"http_webhook"`.
///
/// `retry` is rejected at config time: webhook is push-based, so there is
/// nothing to "retry" — the upstream owns the retry decision.
pub fn http_webhook_source_factory(
    id: &str,
    config: Value,
    retry: Option<RetryPolicy>,
) -> Result<Box<dyn Source>> {
    if retry.is_some() {
        bail!(
            "invalid config for component type 'http_webhook': retry has no effect on push-based sources"
        );
    }
    let config: HttpWebhookSourceConfig = parse_config("http_webhook", config)?;
    let bind = config
        .bind
        .parse::<SocketAddr>()
        .map_err(|e| anyhow::anyhow!("invalid http_webhook bind '{}': {e}", config.bind))?;
    if !config.path.starts_with('/') {
        bail!(
            "invalid http_webhook path '{}': path must start with '/'",
            config.path
        );
    }

    Ok(Box::new(HttpWebhookSource::new(id, bind, config.path)))
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdTcpListener};
    use std::time::Duration;

    use reqwest::Client;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn emits_envelope_for_valid_webhook_request() {
        let bind = unused_local_addr();
        let source = HttpWebhookSource::new("webhook", bind, "/events");
        let (tx, mut rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let response = Client::new()
            .post(format!("http://{bind}/events"))
            .header("x-event-id", "evt-1")
            .header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .json(&json!({ "event": "created" }))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let env = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("webhook timed out")
            .expect("source closed before emitting");

        assert_eq!(env.meta.source_id, "webhook");
        assert_eq!(env.payload, json!({ "event": "created" }));
        assert_eq!(
            env.meta.headers.get("http.header.x-event-id"),
            Some(&"evt-1".to_string())
        );
        assert!(env.meta.headers.contains_key(TRACEPARENT));
        // Trace-context headers are not duplicated under http.header.* so
        // that SourceCtx::send can refresh the bare key without leaving a
        // stale copy behind.
        assert!(
            !env.meta.headers.contains_key("http.header.traceparent"),
            "traceparent should not be duplicated under http.header.*"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn rejects_wrong_method_and_invalid_json() {
        let bind = unused_local_addr();
        let source = HttpWebhookSource::new("webhook", bind, "/events");
        let (tx, _rx) = mpsc::channel(8);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = Client::new();
        let wrong_method = client
            .get(format!("http://{bind}/events"))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);

        let invalid_json = client
            .post(format!("http://{bind}/events"))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(invalid_json.status(), StatusCode::BAD_REQUEST);

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), handle).await;
    }

    #[tokio::test]
    async fn response_waits_for_channel_capacity_before_returning_accepted() {
        let bind = unused_local_addr();
        let source = HttpWebhookSource::new("webhook", bind, "/events");
        let (tx, mut rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let source_handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = Client::new();
        let first = client
            .post(format!("http://{bind}/events"))
            .json(&json!({ "n": 1 }))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);

        let second_client = client.clone();
        let second = tokio::spawn(async move {
            second_client
                .post(format!("http://{bind}/events"))
                .json(&json!({ "n": 2 }))
                .send()
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !second.is_finished(),
            "second request returned before downstream channel had capacity"
        );

        let first_env = rx.recv().await.expect("expected first envelope");
        assert_eq!(first_env.payload, json!({ "n": 1 }));

        let second_response = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second request stayed blocked after capacity was freed")
            .expect("second request task failed");
        assert_eq!(second_response.status(), StatusCode::ACCEPTED);

        let second_env = rx.recv().await.expect("expected second envelope");
        assert_eq!(second_env.payload, json!({ "n": 2 }));

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(1), source_handle).await;
    }

    #[tokio::test]
    async fn blocked_request_returns_unavailable_when_cancelled() {
        let bind = unused_local_addr();
        let source = HttpWebhookSource::new("webhook", bind, "/events");
        let (tx, _rx) = mpsc::channel(1);
        let cancel = CancellationToken::new();

        let c = cancel.clone();
        let source_handle = tokio::spawn(async move { Box::new(source).run(tx, c).await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = Client::new();
        let first = client
            .post(format!("http://{bind}/events"))
            .json(&json!({ "n": 1 }))
            .send()
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);

        let second_client = client.clone();
        let second = tokio::spawn(async move {
            second_client
                .post(format!("http://{bind}/events"))
                .json(&json!({ "n": 2 }))
                .send()
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !second.is_finished(),
            "second request returned before cancellation"
        );

        cancel.cancel();
        let second_response = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("second request stayed blocked after cancellation")
            .expect("second request task failed");
        assert_eq!(second_response.status(), StatusCode::SERVICE_UNAVAILABLE);

        let _ = tokio::time::timeout(Duration::from_secs(1), source_handle).await;
    }

    #[test]
    fn factory_rejects_invalid_path() {
        let err = match http_webhook_source_factory(
            "webhook",
            json!({
                "bind": "127.0.0.1:8080",
                "path": "events",
            }),
            None,
        ) {
            Ok(_) => panic!("expected invalid path error"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("path must start with '/'"));
    }

    #[test]
    fn factory_builds_with_required_fields() {
        let source = http_webhook_source_factory(
            "webhook",
            json!({
                "bind": "127.0.0.1:8080",
                "path": "/events",
            }),
            None,
        )
        .unwrap();

        assert_eq!(source.id(), "webhook");
    }

    #[test]
    fn factory_rejects_retry_policy() {
        use crate::retry::{ExhaustedPolicy, RetryPolicy};

        let err = http_webhook_source_factory(
            "webhook",
            json!({ "bind": "127.0.0.1:8080", "path": "/events" }),
            Some(RetryPolicy {
                max_attempts: 3,
                initial_delay_ms: 100,
                backoff_multiplier: 2.0,
                max_delay_ms: 1000,
                on_exhausted: ExhaustedPolicy::Propagate,
            }),
        )
        .err()
        .expect("expected retry rejection");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("retry has no effect on push-based sources"),
            "{msg}"
        );
    }

    fn unused_local_addr() -> SocketAddr {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    }
}
