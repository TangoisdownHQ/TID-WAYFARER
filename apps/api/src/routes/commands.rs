use axum::{
    routing::{post, get},
    Json, Router,
    extract::{State, Query},
    http::StatusCode,
};
use serde::{Serialize, Deserialize};
use uuid::Uuid;
use chrono::Utc;
use std::collections::HashMap;

use crate::services::actuators;
use crate::AppState;

#[derive(Deserialize)]
pub struct EnqueueCommand {
    pub target_node: Uuid,
    pub asset_id: Option<Uuid>,
    pub cmd_type: String,
    pub payload: serde_json::Value,
}

#[derive(Serialize)]
pub struct CommandQueued { 
    pub id: Uuid, 
    pub created_at: String 
}

#[derive(Deserialize)]
pub struct AckCommand {
    pub command_id: Uuid,
    pub node_id: Uuid,
    pub status: String, // acked | failed
    pub info: Option<serde_json::Value>
}

#[derive(Serialize)]
pub struct AckSaved { 
    pub id: Uuid, 
    pub created_at: String 
}

pub fn command_routes() -> Router<AppState> {
    Router::new()
        .route("/enqueue", post(enqueue))
        .route("/pull", get(pull_for_node))
        .route("/ack", post(ack))
        .route("/execute", post(execute))
}

// ✅ POST /api/commands/execute — receiving side of the core's push-based
// command engine (it POSTs to {api_endpoint}/commands/execute). Dispatches to
// the actuator registry and reports the outcome in the response body, which
// is what core records as the ack: the request/response pair *is* the ack
// channel, so no callback (and no second authenticated hop) is needed.
pub async fn execute(
    State(state): State<AppState>,
    Json(command): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
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
    }))
    .bind(trace_id)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Render Option fields as bare values: `?opt` would emit Some(..)/None,
    // which is noise in a log query.
    tracing::info!(
        trace_id = trace_id.map(|t| t.to_string()).unwrap_or_default(),
        command_id = command_id.unwrap_or(-1),
        command_type,
        ack_status = outcome.status(),
        detail = outcome.detail(),
        "command executed"
    );

    Ok(Json(serde_json::json!({
        "status": "received",
        "commandId": command_id,
        "ackStatus": outcome.status(),
        "detail": outcome.detail(),
    })))
}

// ✅ POST /api/commands/enqueue
pub async fn enqueue(
    State(state): State<AppState>, 
    Json(input): Json<EnqueueCommand>
) -> Result<Json<CommandQueued>, StatusCode> {

    let id = Uuid::new_v4();

    sqlx::query!(
        r#"
        INSERT INTO commands (id, target_node, asset_id, cmd_type, payload)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        id, input.target_node, input.asset_id, input.cmd_type, input.payload
    )
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(CommandQueued { id, created_at: Utc::now().to_rfc3339() }))
}

// ✅ GET /api/commands/pull?node_id=<uuid>
pub async fn pull_for_node(
    State(state): State<AppState>, 
    Query(q): Query<HashMap<String, String>>
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {

    let node_id = q
        .get("node_id")
        .and_then(|v| Uuid::parse_str(v).ok())
        .ok_or(StatusCode::BAD_REQUEST)?;

    let rows = sqlx::query!(
        r#"
        SELECT id::text, asset_id::text as "asset_id?", cmd_type, payload, created_at
        FROM commands
        WHERE target_node = $1 AND status = 'queued'
        ORDER BY created_at ASC
        LIMIT 20
        "#,
        node_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // ✅ fixed: handle Option<String> for id correctly
    let ids: Vec<Uuid> = rows.iter()
        .filter_map(|r| r.id.as_ref().and_then(|id| Uuid::parse_str(id).ok()))
        .collect();

    if !ids.is_empty() {
        let _ = sqlx::query!(
            r#"UPDATE commands SET status = 'sent' WHERE id = ANY($1)"#,
            &ids[..]
        )
        .execute(&state.db)
        .await;
    }

    // ✅ fixed: handle Option<JsonValue> properly
    let payload = rows.into_iter().map(|r| {
        serde_json::json!({
            "id": r.id,
            "asset_id": r.asset_id,
            "cmd_type": r.cmd_type,
            "payload": if r.payload.is_null() { serde_json::json!({}) } else { r.payload.clone() },
            "created_at": r.created_at.to_rfc3339()
        })
    }).collect();

    Ok(Json(payload))
}

// ✅ POST /api/commands/ack
pub async fn ack(
    State(state): State<AppState>, 
    Json(a): Json<AckCommand>
) -> Result<Json<AckSaved>, StatusCode> {

    let id = Uuid::new_v4();

    sqlx::query!(
        r#"UPDATE commands SET status = $2 WHERE id = $1"#,
        a.command_id,
        a.status
    )
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    sqlx::query!(
        r#"
        INSERT INTO command_receipts (id, command_id, node_id, status, info)
        VALUES ($1, $2, $3, $4, $5)
        "#,
        id, a.command_id, a.node_id, a.status, a.info.unwrap_or_else(|| serde_json::json!({}))
    )
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(AckSaved { id, created_at: Utc::now().to_rfc3339() }))
}

