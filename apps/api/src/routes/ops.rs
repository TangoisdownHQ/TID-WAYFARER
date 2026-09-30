//! Ops event feed — what the autonomy pipeline has been doing. Backs
//! dashboards and lets operators audit rule firings and built-in responses.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

pub fn ops_routes() -> Router<AppState> {
    Router::new()
        .route("/events", get(list_events))
        .route("/state", get(local_state))
        .route("/trace/:trace_id", get(trace_chain))
}

/// What the actuators have set on this outpost — lockdown, network isolation,
/// last diagnostic. A GET so it stays readable while the outpost is locked
/// down (lockdown only refuses mutations).
async fn local_state(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = sqlx::query("SELECT key, value, updated_at, trace_id FROM outpost_state ORDER BY key")
        .fetch_all(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let entries: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "key": r.get::<String, _>("key"),
                "value": r.get::<Value, _>("value"),
                "updatedAt": r.get::<chrono::DateTime<chrono::Utc>, _>("updated_at").to_rfc3339(),
                "traceId": r.get::<Option<Uuid>, _>("trace_id"),
            })
        })
        .collect();

    Ok(Json(json!(entries)))
}

/// The whole causal chain for one trace id: the telemetry that started it,
/// every event it produced, and every command it queued — the question the
/// trace ids exist to answer, answered in one request instead of three.
async fn trace_chain(
    State(state): State<AppState>,
    axum::extract::Path(trace_id): axum::extract::Path<Uuid>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let events = sqlx::query(
        r#"
        SELECT kind, severity, details, created_at, node_id
        FROM ops_events WHERE trace_id = $1 ORDER BY id
        "#,
    )
    .bind(trace_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let commands = sqlx::query(
        r#"
        SELECT id, node_id, command, status, ack_status, ack_detail, created_at, acked_at
        FROM command_queue WHERE trace_id = $1 ORDER BY id
        "#,
    )
    .bind(trace_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "traceId": trace_id,
        "events": events.iter().map(|r| json!({
            "kind": r.get::<String, _>("kind"),
            "severity": r.get::<String, _>("severity"),
            "nodeId": r.get::<Option<String>, _>("node_id"),
            "details": r.get::<Option<Value>, _>("details"),
            "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
        })).collect::<Vec<_>>(),
        "commands": commands.iter().map(|r| json!({
            "id": r.get::<i64, _>("id"),
            "nodeId": r.get::<Option<String>, _>("node_id"),
            "command": r.get::<Value, _>("command"),
            "status": r.get::<String, _>("status"),
            "ackStatus": r.get::<Option<String>, _>("ack_status"),
            "ackDetail": r.get::<Option<String>, _>("ack_detail"),
            "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            "ackedAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("acked_at").map(|t| t.to_rfc3339()),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct EventFilter {
    kind: Option<String>,
    severity: Option<String>,
    asset_id: Option<Uuid>,
    /// Max rows returned (default 100, cap 1000)
    limit: Option<i64>,
}

async fn list_events(
    State(state): State<AppState>,
    Query(f): Query<EventFilter>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let limit = f.limit.unwrap_or(100).clamp(1, 1000);

    let rows = sqlx::query(
        r#"
        SELECT id, asset_id, node_id, kind, severity, details, created_at
        FROM ops_events
        WHERE ($1::text IS NULL OR kind = $1)
          AND ($2::text IS NULL OR severity = $2)
          AND ($3::uuid IS NULL OR asset_id = $3)
        ORDER BY id DESC
        LIMIT $4
        "#,
    )
    .bind(&f.kind)
    .bind(&f.severity)
    .bind(f.asset_id)
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let events: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "assetId": r.get::<Option<Uuid>, _>("asset_id"),
                "nodeId": r.get::<Option<String>, _>("node_id"),
                "kind": r.get::<String, _>("kind"),
                "severity": r.get::<String, _>("severity"),
                "details": r.get::<Option<Value>, _>("details"),
                "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!(events)))
}
