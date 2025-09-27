use std::{sync::OnceLock, time::Duration};

use axum::{
    Json, Router,
    body::Bytes,
    extract::MatchedPath,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};

use serde::Deserialize;
use tokio::fs;
use tower_http::{classify::ServerErrorsFailureClass, trace::TraceLayer};
use tracing::{Span, info, info_span};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

static SIGNAL_PATH: OnceLock<String> = OnceLock::new();

fn signal_path() -> &'static str {
    SIGNAL_PATH.get_or_init(|| std::env::var("SIGNAL_PATH").unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionType {
    Buy,
    Sell,
    Close,
}

impl std::str::FromStr for ActionType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "buy" => Ok(ActionType::Buy),
            "sell" => Ok(ActionType::Sell),
            "close" => Ok(ActionType::Close),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for ActionType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ActionType::Buy => "buy",
            ActionType::Sell => "sell",
            ActionType::Close => "close",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug, Deserialize)]
pub struct WebhookRequest {
    symbol: String,
    action: ActionType,
    strategy_id: String,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_option_i32")]
    lot: i32,
}

fn deserialize_option_i32<'de, D>(deserializer: D) -> Result<i32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<i32>::deserialize(deserializer)?;
    Ok(opt.unwrap_or(0))
}

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                format!(
                    "{}=debug,tower_http=debug,axum::rejection=trace",
                    env!("CARGO_CRATE_NAME")
                )
                .into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let app = app();

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000")
        .await
        .unwrap();
    println!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

fn app() -> Router {
    Router::new()
        .route("/webhook", post(webhook_handler))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<_>| {
                    let matched_path = request
                        .extensions()
                        .get::<MatchedPath>()
                        .map(MatchedPath::as_str);
                    info_span!(
                        "http_request",
                        method = ?request.method(),
                        matched_path,
                        some_other_field = tracing::field::Empty,
                    )
                })
                .on_request(|_request: &Request<_>, _span: &Span| {
                    // You can use `_span.record("some_other_field", value)` in one of these
                    // closures to attach a value to the initially empty field in the info_span
                    // created above.
                })
                .on_response(|_response: &Response, _latency: Duration, _span: &Span| {
                    // ...
                })
                .on_body_chunk(|_chunk: &Bytes, _latency: Duration, _span: &Span| {
                    // ...
                })
                .on_eos(
                    |_trailers: Option<&HeaderMap>, _stream_duration: Duration, _span: &Span| {
                        // ...
                    },
                )
                .on_failure(
                    |_error: ServerErrorsFailureClass, _latency: Duration, _span: &Span| {
                        // ...
                    },
                ),
        )
}

async fn webhook_handler(Json(payload): Json<WebhookRequest>) -> Result<(), AppError> {
    webhook(payload).await?;
    Ok(())
}

async fn webhook(payload: WebhookRequest) -> Result<(), anyhow::Error> {
    info!("Received payload: {:#?}", payload);

    let line = match payload.action {
        ActionType::Buy | ActionType::Sell => {
            if payload.lot <= 0 {
                anyhow::bail!("Invalid or missing lot size")
            }
            format!(
                "{},{},{},{}",
                payload.symbol, payload.action, payload.lot, payload.strategy_id
            )
        }
        ActionType::Close => format!(
            "{},{},,{}",
            payload.symbol, payload.action, payload.strategy_id
        ),
    };

    write_file(signal_path(), line).await?;

    Ok(())
}

async fn write_file(path: &str, contents: String) -> Result<(), anyhow::Error> {
    fs::write(path, contents).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use axum::{body::Body, http::Request, http::StatusCode, response::Response};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tempfile::TempDir;
    use tower::ServiceExt;

    #[tokio::test]
    async fn webhook_flow_sequential() {
        let temp_dir = TempDir::new().unwrap();
        let signal_file = temp_dir.path().join("signal.txt");
        let signal_str = signal_file.to_str().unwrap().to_owned();

        unsafe {
            std::env::set_var("SIGNAL_PATH", &signal_str);
        }

        assert_eq!(signal_path(), signal_str);

        write_file(signal_path(), "Hello World!".to_string())
            .await
            .unwrap();
        assert!(Path::new(signal_path()).exists());
        tokio::fs::remove_file(signal_path()).await.unwrap();

        // Scenario: close action without lot.
        let response = send_payload(json!({
            "symbol": "Dc",
            "action": "close",
            "strategy_id": "2n-T1"
        }))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = response_body(response).await;
        let bytes = tokio::fs::read(signal_path()).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "Dc,close,,2n-T1");
        println!("{}", html);
        tokio::fs::remove_file(signal_path()).await.unwrap();

        // Scenario: close action with a lot value.
        let response = send_payload(json!({
            "symbol": "Dc",
            "action": "close",
            "lot": 10,
            "strategy_id": "2n-T1"
        }))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = response_body(response).await;
        let bytes = tokio::fs::read(signal_path()).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "Dc,close,,2n-T1");
        println!("{}", html);
        tokio::fs::remove_file(signal_path()).await.unwrap();

        // Scenario: buy action with lot value.
        let response = send_payload(json!({
            "symbol": "Dc",
            "action": "buy",
            "lot": 10,
            "strategy_id": "2n-T1"
        }))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let html = response_body(response).await;
        let bytes = tokio::fs::read(signal_path()).await.unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "Dc,buy,10,2n-T1");
        println!("{}", html);
    }

    async fn send_payload(payload: serde_json::Value) -> Response {
        app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn response_body(response: Response) -> String {
        let body = response.into_body();
        let body_bytes = body.collect().await.unwrap().to_bytes();
        String::from_utf8(body_bytes.to_vec()).unwrap()
    }
}
