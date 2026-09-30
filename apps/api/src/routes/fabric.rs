//! Fabric status — one aggregated view of outpost + queue health, so an
//! operator can see the whole mesh at a glance. Backs the /ui console.
//! All counts are resilient: a missing table (unmigrated DB) reads as 0
//! rather than failing the whole response.

use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::AppState;

/// A node is "online" if seen within this window; "stale" up to the second.
const ONLINE_SECS: i64 = 120;
const STALE_SECS: i64 = 900;

pub fn fabric_routes() -> Router<AppState> {
    Router::new().route("/status", get(status))
}

async fn count(state: &AppState, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    let total_nodes = count(&state, "SELECT COUNT(*) FROM node_registry").await;
    let online_nodes = count(
        &state,
        &format!("SELECT COUNT(*) FROM node_registry WHERE last_seen > NOW() - INTERVAL '{ONLINE_SECS} seconds'"),
    )
    .await;
    let stale_nodes = count(
        &state,
        &format!("SELECT COUNT(*) FROM node_registry WHERE last_seen <= NOW() - INTERVAL '{ONLINE_SECS} seconds' AND last_seen > NOW() - INTERVAL '{STALE_SECS} seconds'"),
    )
    .await;

    let cmd_queued = count(&state, "SELECT COUNT(*) FROM command_queue WHERE status='queued'").await;
    let cmd_sent = count(&state, "SELECT COUNT(*) FROM command_queue WHERE status='sent'").await;
    let cmd_failed = count(&state, "SELECT COUNT(*) FROM command_queue WHERE status='failed'").await;

    let dtn_pending = count(&state, "SELECT COUNT(*) FROM dtn_outbox").await;
    let dtn_backoff = count(&state, "SELECT COUNT(*) FROM dtn_outbox WHERE attempts > 0").await;

    let inbox_total = count(&state, "SELECT COUNT(*) FROM dtn_inbox").await;
    let inbox_unverified = count(&state, "SELECT COUNT(*) FROM dtn_inbox WHERE verified = false").await;

    let rules_enabled = count(&state, "SELECT COUNT(*) FROM autonomy_rules WHERE enabled = true").await;
    let events_last_hour = count(
        &state,
        "SELECT COUNT(*) FROM ops_events WHERE kind <> 'telemetry_ok' AND created_at > NOW() - INTERVAL '1 hour'",
    )
    .await;

    let assets_total = count(&state, "SELECT COUNT(*) FROM fleet_assets").await;
    let telemetry_last_hour = count(
        &state,
        "SELECT COUNT(*) FROM fleet_telemetry WHERE timestamp > NOW() - INTERVAL '1 hour'",
    )
    .await;

    Json(json!({
        "outpost": {
            "name": std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into()),
            "role": std::env::var("OUTPOST_ROLE").unwrap_or_else(|_| "outpost".into()),
            "nodeId": state.identity.node_id,
        },
        "nodes": {
            "total": total_nodes,
            "online": online_nodes,
            "stale": stale_nodes,
            "offline": (total_nodes - online_nodes - stale_nodes).max(0),
        },
        "commandQueue": { "queued": cmd_queued, "sent": cmd_sent, "failed": cmd_failed },
        "dtnOutbox": { "pending": dtn_pending, "retrying": dtn_backoff },
        "dtnInbox": { "total": inbox_total, "unverified": inbox_unverified },
        "rules": { "enabled": rules_enabled, "eventsLastHour": events_last_hour },
        "fleet": { "assets": assets_total, "telemetryLastHour": telemetry_last_hour },
    }))
}
