//! HTTP health probe endpoints for orchestrators.
//!
//! Provides liveness and readiness checks suitable for Kubernetes, systemd,
//! or any container runtime that queries HTTP endpoints:
//!
//! - **GET /health/live** → returns 200 when the process is alive.
//! - **GET /health/ready** → returns 200 when all pipelines are running
//!   and no shutdown has been requested; returns 503 otherwise.
//!
//! The server uses axum (already a dependency) and binds to the address
//! configured in `[health].address`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Serialize;

use crate::lifecycle::CourierState;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    pipelines: Vec<PipelineHealth>,
}

#[derive(Serialize)]
struct PipelineHealth {
    name: String,
    state: String,
}

async fn liveness() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn readiness(State(state): State<Arc<CourierState>>) -> impl IntoResponse {
    let pipelines = state
        .pipeline_states()
        .into_iter()
        .map(|(name, s)| PipelineHealth {
            name,
            state: s.to_string(),
        })
        .collect();

    let response = HealthResponse {
        status: if state.is_ready() {
            "ready"
        } else {
            "not_ready"
        },
        pipelines,
    };

    if state.is_ready() {
        (StatusCode::OK, Json(response))
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, Json(response))
    }
}

pub(crate) fn health_router(state: Arc<CourierState>) -> Router {
    Router::new()
        .route("/health/live", get(liveness))
        .route("/health/ready", get(readiness))
        .with_state(state)
}

pub(crate) async fn serve(
    addr: SocketAddr,
    state: Arc<CourierState>,
) -> Result<(), std::io::Error> {
    let app = health_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("health server listening on {addr}");
    axum::serve(listener, app).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::PipelineState;
    use axum::body::Body;
    use axum::http;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn body_json(body: Body) -> serde_json::Value {
        let bytes = body.collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn test_state_with_pipelines(names: Vec<&str>) -> Arc<CourierState> {
        Arc::new(CourierState::new(
            names.into_iter().map(String::from).collect(),
        ))
    }

    #[tokio::test]
    async fn liveness_returns_200() {
        let state = test_state_with_pipelines(vec![]);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_returns_503_when_starting() {
        let state = test_state_with_pipelines(vec!["p1"]);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readiness_returns_200_when_running() {
        let state = test_state_with_pipelines(vec!["p1", "p2"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.pipeline_status(1).set_state(PipelineState::Running);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_returns_503_when_pipeline_failed() {
        let state = test_state_with_pipelines(vec!["p1"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.pipeline_status(0).mark_failed();
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readiness_returns_200_with_no_pipelines() {
        let state = test_state_with_pipelines(vec![]);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_returns_503_after_shutdown_requested() {
        let state = test_state_with_pipelines(vec!["p1"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let state = test_state_with_pipelines(vec!["p1"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.mark_shutdown_requested();
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readiness_returns_503_when_stopped() {
        let state = test_state_with_pipelines(vec!["p1"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.pipeline_status(0).set_state(PipelineState::Stopped);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readiness_returns_503_when_draining() {
        let state = test_state_with_pipelines(vec!["p1"]);
        state.pipeline_status(0).set_state(PipelineState::Draining);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readiness_body_shows_failed_state_for_failed_pipeline() {
        let state = test_state_with_pipelines(vec!["p1"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.pipeline_status(0).mark_failed();
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = body_json(response.into_body()).await;
        assert_eq!(body["status"], "not_ready");
        assert_eq!(body["pipelines"][0]["name"], "p1");
        assert_eq!(body["pipelines"][0]["state"], "failed");
    }

    #[tokio::test]
    async fn readiness_body_shows_running_when_all_healthy() {
        let state = test_state_with_pipelines(vec!["p1", "p2"]);
        state.pipeline_status(0).set_state(PipelineState::Running);
        state.pipeline_status(1).set_state(PipelineState::Running);
        let app = health_router(state);

        let response = app
            .oneshot(
                http::Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response.into_body()).await;
        assert_eq!(body["status"], "ready");
        assert_eq!(body["pipelines"][0]["state"], "running");
        assert_eq!(body["pipelines"][1]["state"], "running");
    }
}
