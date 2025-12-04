use std::sync::LazyLock;
use std::{net::SocketAddr, ops::Sub, time::Duration};

use axum::extract::Request;
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Router, extract::State};
use prometheus::TextEncoder;
use reqwest::{Client, StatusCode};
use tokio::net::TcpListener;
use tracing::error;

use crate::monitor::{
    GAUGE_HL_NODE_RESPONDING, GAUGE_HL_NODE_SYSTEM_TIME_MS, GAUGE_HL_NODE_TIME_MS, as_ms_f64,
};

#[derive(Clone)]
struct MonitorServer {
    healthy_drift_threshold: Duration,
    node_url: String,
    client: Client,
}

fn router() -> Router<MonitorServer> {
    Router::new()
        .route("/metrics", get(metrics))
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/info", post(proxy_info))
        .route("/info", get(proxy_info))
}

async fn metrics() -> impl IntoResponse {
    static PROMETHEUS_HEADERS: LazyLock<HeaderMap> = LazyLock::new(|| {
        HeaderMap::from_iter([(CONTENT_TYPE, "text/plain;version=0.0.4".parse().unwrap())])
    });

    let metrics = prometheus::default_registry().gather();

    (
        PROMETHEUS_HEADERS.clone(),
        TextEncoder::new().encode_to_string(&metrics).unwrap(),
    )
}

async fn livez() -> impl IntoResponse {
    if GAUGE_HL_NODE_RESPONDING.get() == 1 {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn readyz(State(state): State<MonitorServer>) -> impl IntoResponse {
    if GAUGE_HL_NODE_RESPONDING.get() == 1
        && GAUGE_HL_NODE_SYSTEM_TIME_MS
            .get()
            .sub(GAUGE_HL_NODE_TIME_MS.get())
            .max(0.0)
            < as_ms_f64(&state.healthy_drift_threshold)
    {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

async fn proxy_info(State(state): State<MonitorServer>, request: Request) -> impl IntoResponse {
    let target_url = format!("{}/info", state.node_url);

    // Extract method, headers, and body from the incoming request
    let method = request.method().clone();
    let headers = request.headers().clone();
    let body = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            error!(?err, "failed to read request body");
            return (StatusCode::BAD_REQUEST, "Failed to read request body").into_response();
        }
    };

    // Build the proxied request
    let mut proxy_request = state.client.request(method, &target_url).body(body);

    // Copy relevant headers (excluding host and connection)
    for (key, value) in headers.iter() {
        let header_name = key.as_str();
        if !matches!(header_name, "host" | "connection" | "content-length") {
            if let Ok(header_value) = value.to_str() {
                proxy_request = proxy_request.header(header_name, header_value);
            }
        }
    }

    // Send the request
    match proxy_request.send().await {
        Ok(response) => {
            let status = response.status();
            let response_headers = response.headers().clone();
            let response_body = match response.bytes().await {
                Ok(bytes) => bytes,
                Err(err) => {
                    error!(?err, "failed to read response body");
                    return (StatusCode::BAD_GATEWAY, "Failed to read response body")
                        .into_response();
                }
            };

            // Build response with status, headers, and body
            let mut response_builder = axum::http::Response::builder().status(status);

            // Copy response headers
            for (key, value) in response_headers.iter() {
                if let Ok(header_value) = value.to_str() {
                    response_builder = response_builder.header(key.as_str(), header_value);
                }
            }

            match response_builder.body(axum::body::Body::from(response_body)) {
                Ok(resp) => resp.into_response(),
                Err(err) => {
                    error!(?err, "failed to build response");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build response",
                    )
                        .into_response()
                }
            }
        }
        Err(err) => {
            error!(?err, target_url = %target_url, "failed to proxy request to node");
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to connect to node: {}", err),
            )
                .into_response()
        }
    }
}

pub async fn run_metrics_server(
    listen_address: SocketAddr,
    healthy_drift_threshold: Duration,
    node_url: Option<String>,
) -> eyre::Result<()> {
    let node_url = node_url.unwrap_or_else(|| "http://127.0.0.1:3001".to_string());
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| eyre::eyre!("failed to create HTTP client: {}", e))?;

    let state = MonitorServer {
        healthy_drift_threshold,
        node_url,
        client,
    };

    let listener = TcpListener::bind(listen_address).await?;
    axum::serve(listener, router().with_state(state).into_make_service()).await?;

    Ok(())
}
