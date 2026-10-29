//! HTTP daemon: chat gateway + metrics/config API.
//!
//!   GET  /health               liveness (no auth)
//!   GET  /metrics              Prometheus text (auth)
//!   GET  /v1/snapshot          dashboard data (auth)
//!   GET  /v1/requests/{id}     one logged request (auth)
//!   POST /v1/chat              send a chat request (auth)
//!   GET  /v1/config            read config (auth)
//!   PUT  /v1/config            validate + replace config (auth)

use crate::config::Config;
use crate::engine::{ChatRequest, Engine};
use crate::store;
use crate::stream::{self, StreamEvent};
use anyhow::{Result, bail};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, sse::{Event, KeepAlive, Sse}},
    routing::{get, post},
};
use serde_json::json;

#[derive(Clone)]
struct App {
    engine: Engine,
    token: Option<String>,
}

struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}
fn bad(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, format!("{e:#}"))
}
fn internal(e: anyhow::Error) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

pub async fn serve(engine: Engine, bind_override: Option<String>) -> Result<()> {
    let cfg = engine.config();
    let bind = bind_override.unwrap_or_else(|| cfg.server.bind.clone());
    let addr: std::net::SocketAddr = bind.parse().map_err(|_| anyhow::anyhow!("invalid bind address '{bind}'"))?;
    let token = if cfg.server.token_env.is_empty() { None } else { crate::secrets::get(&cfg.server.token_env) };
    if token.is_none() && !addr.ip().is_loopback() {
        bail!(
            "refusing to listen on {addr} without authentication: set the env var {} (server.token_env) to a secret token",
            if cfg.server.token_env.is_empty() { "AIWS_TOKEN" } else { &cfg.server.token_env }
        );
    }

    let app = App { engine, token };
    let protected = Router::new()
        .route("/metrics", get(metrics))
        .route("/v1/snapshot", get(snapshot))
        .route("/v1/requests/{id}", get(request_by_id))
        .route("/v1/chat", post(chat))
        .route("/v1/approvals/{id}", post(approve))
        .route("/v1/config", get(get_config).put(put_config))
        .route_layer(middleware::from_fn_with_state(app.clone(), auth));
    let router = Router::new().route("/health", get(|| async { "ok" })).merge(protected).with_state(app);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("aiws daemon listening on http://{addr} (auth: {})", if cfg.server.token_env.is_empty() { "off" } else { "bearer token" });
    axum::serve(listener, router).with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; }).await?;
    Ok(())
}

async fn auth(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Some(expected) = &app.token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|got| constant_eq(got.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
        if !ok {
            return ApiError(StatusCode::UNAUTHORIZED, "missing or invalid bearer token".into()).into_response();
        }
    }
    next.run(req).await
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn snapshot(State(app): State<App>) -> Result<Json<store::Snapshot>, ApiError> {
    app.engine.snapshot().map(Json).map_err(internal)
}

async fn request_by_id(State(app): State<App>, Path(id): Path<i64>) -> Result<Json<store::RequestRow>, ApiError> {
    match app.engine.store.get(id).map_err(internal)? {
        Some(r) => Ok(Json(r)),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such request".into())),
    }
}

async fn chat(State(app): State<App>, Json(mut req): Json<ChatRequest>) -> Result<Response, ApiError> {
    req.source.get_or_insert_with(|| "api".into());
    if !req.stream {
        return app.engine.chat(req).await.map(|r| Json(r).into_response()).map_err(bad);
    }
    // Streaming: the request runs inside `drive`, which stops (cancelling the
    // upstream call) if the client disconnects.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();
    let engine = app.engine.clone();
    tokio::spawn(async move {
        let sink = tx.clone();
        stream::drive(engine.chat_stream(req, sink), tx).await;
    });
    let events = futures::stream::unfold(rx, |mut rx| async move {
        let ev = rx.recv().await?;
        let data = serde_json::to_string(&ev).unwrap_or_default();
        Some((Ok::<Event, std::convert::Infallible>(Event::default().data(data)), rx))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()).into_response())
}

#[derive(serde::Deserialize)]
struct ApprovalBody {
    choice: String,
}

async fn approve(State(app): State<App>, Path(id): Path<String>, Json(b): Json<ApprovalBody>) -> Json<serde_json::Value> {
    Json(json!({ "resolved": app.engine.resolve_approval(&id, &b.choice) }))
}

async fn get_config(State(app): State<App>) -> Json<Config> {
    Json(app.engine.config())
}

async fn put_config(State(app): State<App>, Json(cfg): Json<Config>) -> Result<Json<Config>, ApiError> {
    app.engine.save_config(&cfg).map_err(bad)?;
    Ok(Json(cfg))
}

async fn metrics(State(app): State<App>) -> Result<Response, ApiError> {
    let s = app.engine.snapshot().map_err(internal)?;
    let mut out = String::new();
    let mut line = |help: &str, name: &str, kind: &str, val: String| {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {val}\n"));
    };
    line("Total requests logged", "aiws_requests_total", "counter", s.all_time.requests.to_string());
    line("Total failed requests", "aiws_errors_total", "counter", s.all_time.errors.to_string());
    line("Input tokens", "aiws_input_tokens_total", "counter", s.all_time.input_tokens.to_string());
    line("Output tokens", "aiws_output_tokens_total", "counter", s.all_time.output_tokens.to_string());
    line("Pay-per-token spend today (USD)", "aiws_api_cost_usd_today", "gauge", s.today.api_cost_usd.to_string());
    line("Pay-per-token spend this month (USD)", "aiws_api_cost_usd_month", "gauge", s.month.api_cost_usd.to_string());
    line("Average successful latency today (ms)", "aiws_avg_latency_ms_today", "gauge", s.today.avg_latency_ms.to_string());
    Ok(([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response())
}
