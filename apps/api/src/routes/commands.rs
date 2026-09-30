//! Command receiver — the executing end of core's push-based command engine.
//!
//! There used to be a second, parallel command system here: `/enqueue`,
//! `/pull` and `/ack` over a `commands` table. Nothing executed from it. The
//! command engine, the actuators, the dashboard, the fabric status endpoint,
//! the metrics and the rules engine all use `command_queue`, so `/enqueue`
//! wrote a row that would never run and answered 200 — a silent black hole for
//! any operator who called it.
//!
//! It was also unauthorised in a way that mattered: `/pull` took the target
//! node from a query string and marked those commands `sent`, so any
//! authenticated caller could drain and blackhole another outpost's queue,
//! and `/ack` took the node from the body and wrote receipts in its name.
//! Deleting the dead path removed three confused-deputy holes outright.
//!
//! What remains is `/execute`: core POSTs a command here, the actuator runs,
//! and the response body *is* the ack — no callback and no second
//! authenticated hop.

use axum::{
    extract::State,
    http::StatusCode,
    routing::post,
    Json, Router,
};
use uuid::Uuid;

use crate::routes::auth_middleware::{Caller, Principal};
use crate::services::actuators;
use crate::AppState;

pub fn command_routes() -> Router<AppState> {
    Router::new().route("/execute", post(execute))
}

/// POST /api/commands/execute
///
/// Authority, not merely membership. A command actuates physical state —
/// LOCKDOWN refuses every mutating request on this outpost, ISOLATE_NETWORK
/// cuts it off — so being *some* authenticated party is not enough. Previously
/// any role-`user` JWT could lock down an outpost, and worse, could UNLOCK one
/// that autonomy had locked in response to physical tamper: the defence was
/// undone by any credential the tampering party might hold.
///
/// Accepted from a peer node that signed its request (core pushing a command),
/// or an admin user (manual intervention). Everyone else gets 403.
///
/// This is still transport-level authority: it proves who *delivered* the
/// command, not who *issued* it. A command that has travelled through a relay
/// or sat in a queue for days cannot be validated this way — that needs the
/// command envelope itself to be signed by its issuer, which is the same shape
/// as the vouchers in `Documentation/OfflineSettlement.md`.
pub async fn execute(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Json(command): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let authorised = matches!(principal, Principal::Node(_)) || principal.is_admin();
    if !authorised {
        tracing::warn!(
            caller = %principal.describe(),
            command_type = command.get("type").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "refused command: caller may not actuate this outpost"
        );
        return Err((
            StatusCode::FORBIDDEN,
            "commands may only be pushed by a fabric node or an admin".to_string(),
        ));
    }

    // The pushing core folds its trace_id into the command envelope, so the
    // executing outpost records the same id — one query then spans both sides
    // of the push, from the telemetry row that caused it to the execution.
    let trace_id = command
        .get("trace_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());
    let command_id = command.get("command_id").and_then(|v| v.as_i64());
    let command_type = command
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let outcome = actuators::dispatch(&state, &command, trace_id).await;

    // Severity follows the outcome so a failed actuator is visible in the
    // ops feed rather than filed as routine.
    let severity = match outcome {
        actuators::Outcome::Executed(_) => "info",
        actuators::Outcome::Unsupported(_) => "warn",
        actuators::Outcome::Failed(_) => "error",
    };

    sqlx::query(
        r#"
        INSERT INTO ops_events (node_id, kind, severity, details, trace_id)
        VALUES ($1, 'command_received', $2, $3, $4)
        "#,
    )
    .bind(&state.identity.node_id)
    .bind(severity)
    .bind(serde_json::json!({
        "command": command,
        "ack_status": outcome.status(),
        "detail": outcome.detail(),
        // Who actuated this outpost, recorded alongside what happened.
        "issued_by": principal.describe(),
    }))
    .bind(trace_id)
    .execute(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "could not record command execution");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not record execution".to_string())
    })?;

    // Render Option fields as bare values: `?opt` would emit Some(..)/None,
    // which is noise in a log query.
    tracing::info!(
        trace_id = trace_id.map(|t| t.to_string()).unwrap_or_default(),
        command_id = command_id.unwrap_or(-1),
        command_type,
        ack_status = outcome.status(),
        detail = outcome.detail(),
        issued_by = %principal.describe(),
        "command executed"
    );

    Ok(Json(serde_json::json!({
        "status": "received",
        "commandId": command_id,
        "ackStatus": outcome.status(),
        "detail": outcome.detail(),
    })))
}
