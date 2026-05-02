//! Optional HTTP server exposing `/health` and `/api/v1/*` endpoints.
//! Maps to SPEC.md §13.7.

use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::Json,
    routing::{get, post},
};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::types::OrchestratorState;

/// Shared application state for the HTTP server.
#[derive(Clone)]
pub struct HttpState {
    pub orchestrator_state: Arc<RwLock<OrchestratorState>>,
    /// Sender side of a watch channel: POST /api/v1/refresh writes `true`.
    pub refresh_tx: tokio::sync::watch::Sender<bool>,
}

/// Start the HTTP server on the given port.
pub async fn start_http_server(state: HttpState, port: u16) -> Result<(), std::io::Error> {
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/state", get(api_state))
        .route("/api/v1/refresh", post(api_refresh))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .map_err(|e| {
            tracing::warn!(
                port = port,
                error = %e,
                "could not bind HTTP server, continuing without it"
            );
            e
        })?;

    tracing::info!(port = port, "HTTP server listening");
    axum::serve(listener, app)
        .await
        .map_err(std::io::Error::other)?;

    Ok(())
}

// ── Endpoints ───────────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

#[derive(Serialize)]
struct ApiStateResponse {
    generated_at: String,
    counts: CountSummary,
    running: Vec<RunningSummary>,
    retrying: Vec<RetryingSummary>,
    codex_totals: Option<CodexTotalsSummary>,
}

#[derive(Serialize)]
struct CountSummary {
    running: usize,
    retrying: usize,
}

#[derive(Serialize)]
struct RunningSummary {
    issue_id: String,
    issue_identifier: String,
    state: String,
    session_id: Option<String>,
    turn_count: u64,
    last_event: Option<String>,
    last_message: Option<String>,
    started_at: String,
    last_event_at: Option<String>,
    tokens: TokenSummary,
}

#[derive(Serialize)]
struct TokenSummary {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[derive(Serialize)]
struct RetryingSummary {
    issue_id: String,
    issue_identifier: String,
    attempt: u32,
    due_at: String,
    error: Option<String>,
}

#[derive(Serialize)]
struct CodexTotalsSummary {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    seconds_running: f64,
}

async fn api_state(State(state): State<HttpState>) -> Json<ApiStateResponse> {
    let s = state.orchestrator_state.read().await;

    let running: Vec<RunningSummary> = s
        .running
        .iter()
        .map(|(id, entry)| RunningSummary {
            issue_id: id.clone(),
            issue_identifier: entry.identifier.clone(),
            state: entry.issue.state.clone(),
            session_id: entry.session.session_id.clone(),
            turn_count: entry.session.turn_count,
            last_event: entry.session.last_codex_event.clone(),
            last_message: entry.session.last_codex_message.clone(),
            started_at: entry.started_at.to_rfc3339(),
            last_event_at: entry.session.last_codex_timestamp.map(|t| t.to_rfc3339()),
            tokens: TokenSummary {
                input_tokens: entry.session.codex_input_tokens,
                output_tokens: entry.session.codex_output_tokens,
                total_tokens: if entry.session.codex_total_tokens > 0 {
                    entry.session.codex_total_tokens
                } else {
                    entry
                        .session
                        .codex_input_tokens
                        .saturating_add(entry.session.codex_output_tokens)
                },
            },
        })
        .collect();

    let retrying: Vec<RetryingSummary> = s
        .retry_attempts
        .iter()
        .map(|(id, entry)| RetryingSummary {
            issue_id: id.clone(),
            issue_identifier: entry.identifier.clone(),
            attempt: entry.attempt,
            due_at: format!("{}ms", entry.due_at_ms),
            error: entry.error.clone(),
        })
        .collect();

    let codex_totals = Some(CodexTotalsSummary {
        input_tokens: s.codex_totals.input_tokens,
        output_tokens: s.codex_totals.output_tokens,
        total_tokens: s.codex_totals.total_tokens,
        seconds_running: s.codex_totals.seconds_running,
    });

    Json(ApiStateResponse {
        generated_at: chrono::Utc::now().to_rfc3339(),
        counts: CountSummary {
            running: running.len(),
            retrying: retrying.len(),
        },
        running,
        retrying,
        codex_totals,
    })
}

#[derive(Serialize)]
struct RefreshResponse {
    queued: bool,
    requested_at: String,
}

async fn api_refresh(State(state): State<HttpState>) -> (StatusCode, Json<RefreshResponse>) {
    // Send refresh signal. If the channel is closed (receiver dropped),
    // log a warning but still return accepted.
    if state.refresh_tx.send(true).is_err() {
        tracing::warn!("refresh channel closed, orchestrator may have shut down");
    }
    (
        StatusCode::ACCEPTED,
        Json(RefreshResponse {
            queued: true,
            requested_at: chrono::Utc::now().to_rfc3339(),
        }),
    )
}
