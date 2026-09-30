//! Console summary — everything the operator console needs in one request.
//!
//! The old version counted `commands` (the pull-flow table) for "queued
//! commands", so it reported zero while the autonomy engine was filling
//! `command_queue`. It also had no notion of the states that actually need a
//! human: a rejected settlement, an undecryptable DTN payload, a revoked node,
//! an outpost sitting in lockdown.
//!
//! Every count is paired with how current it is where that matters. In a
//! fabric where disconnection is normal, "0 queued" from a node last seen four
//! hours ago means something very different from "0 queued" right now, and the
//! console cannot tell that story if the API only sends the number.

use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::services::actuators;
use crate::AppState;

/// Seconds before a node is "stale" rather than online. Matches fabric.rs.
const ONLINE_SECS: i64 = 60;
const STALE_SECS: i64 = 300;

#[derive(Serialize)]
pub struct ConsoleSummary {
    outpost: Value,
    fabric: Value,
    autonomy: Value,
    comms: Value,
    logistics: Value,
    /// Things a human has to decide about. Empty is the good case.
    attention: Vec<Value>,
    updated_at: String,
}

pub fn dashboard_routes() -> Router<AppState> {
    Router::new()
        .route("/summary", get(summary))
        .route("/events", get(events_feed))
}

/// Count helper. A missing table (unmigrated DB) yields 0 rather than failing
/// the whole summary — the console must still render on a partial deploy.
async fn count(state: &AppState, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .fetch_one(&state.db)
        .await
        .unwrap_or(0)
}

pub async fn summary(State(state): State<AppState>) -> Json<ConsoleSummary> {
    // ---- Local outpost posture ----
    let lockdown = actuators::flag_is_set(&state, actuators::STATE_LOCKDOWN).await;
    let isolated = actuators::flag_is_set(&state, actuators::STATE_NETWORK_ISOLATED).await;

    // ---- Fabric ----
    let nodes_online = count(&state, &format!(
        "SELECT COUNT(*)::int8 FROM node_registry WHERE last_seen > NOW() - INTERVAL '{ONLINE_SECS} seconds'")).await;
    let nodes_stale = count(&state, &format!(
        "SELECT COUNT(*)::int8 FROM node_registry WHERE last_seen <= NOW() - INTERVAL '{ONLINE_SECS} seconds' AND last_seen > NOW() - INTERVAL '{STALE_SECS} seconds'")).await;
    let nodes_offline = count(&state, &format!(
        "SELECT COUNT(*)::int8 FROM node_registry WHERE last_seen <= NOW() - INTERVAL '{STALE_SECS} seconds'")).await;
    let nodes_total = count(&state, "SELECT COUNT(*)::int8 FROM node_registry").await;
    let nodes_revoked = count(&state, "SELECT COUNT(*)::int8 FROM node_registry WHERE revoked = true").await;

    // How long since we heard from *anyone*. This is the number that tells an
    // operator whether the rest of the panel can be trusted.
    let last_contact_secs: Option<i64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (NOW() - MAX(last_seen)))::int8 FROM node_registry",
    )
    .fetch_one(&state.db)
    .await
    .ok()
    .flatten();

    // ---- Autonomy (command_queue — the push engine the rules engine feeds) ----
    let queued = count(&state, "SELECT COUNT(*)::int8 FROM command_queue WHERE status='queued'").await;
    let unacked = count(&state, "SELECT COUNT(*)::int8 FROM command_queue WHERE status='sent' AND acked_at IS NULL").await;
    let failed_cmds = count(&state, "SELECT COUNT(*)::int8 FROM command_queue WHERE status='failed'").await;
    let rules_24h = count(&state, "SELECT COUNT(*)::int8 FROM ops_events WHERE created_at > NOW() - INTERVAL '24 hours' AND kind NOT IN ('telemetry_ok','command_received')").await;
    let telemetry_backlog = count(&state, "SELECT COUNT(*)::int8 FROM fleet_telemetry WHERE processed=false").await;

    // ---- Comms ----
    let dtn_outbox = count(&state, "SELECT COUNT(*)::int8 FROM dtn_outbox").await;
    let dtn_unverified = count(&state, "SELECT COUNT(*)::int8 FROM dtn_inbox WHERE verified=false").await;
    let dtn_plaintext = count(&state, "SELECT COUNT(*)::int8 FROM dtn_outbox WHERE encrypted=false").await;

    // ---- Logistics ----
    let open_orders = count(&state, "SELECT COUNT(*)::int8 FROM orders WHERE status IN ('posted','bid')").await;
    let in_flight = count(&state, "SELECT COUNT(*)::int8 FROM fulfillments WHERE status IN ('preparing','in_transit')").await;
    let settling = count(&state, "SELECT COUNT(*)::int8 FROM fulfillments WHERE settlement_status='pending'").await;
    let rejected = count(&state, "SELECT COUNT(*)::int8 FROM fulfillments WHERE settlement_status='rejected'").await;
    let unverifiable = count(&state, "SELECT COUNT(*)::int8 FROM fulfillments WHERE settlement_status='unverifiable'").await;

    // ---- What needs a human ----
    // Only genuinely actionable states. A long list here is the alarm; an
    // empty list means the fabric is running itself, which is the norm.
    let mut attention: Vec<Value> = Vec::new();

    if lockdown {
        attention.push(json!({ "kind": "lockdown", "severity": "critical",
            "detail": "This outpost is in LOCKDOWN and refusing changes. Send UNLOCK to clear it." }));
    }
    if isolated {
        attention.push(json!({ "kind": "network_isolated", "severity": "critical",
            "detail": "Network isolation is active on this outpost." }));
    }
    if rejected > 0 {
        attention.push(json!({ "kind": "settlement_rejected", "severity": "critical", "count": rejected,
            "detail": "A settlement was confirmed on-chain and did not match the agreed amount or payee. This is a dispute." }));
    }
    if nodes_revoked > 0 {
        attention.push(json!({ "kind": "node_revoked", "severity": "warn", "count": nodes_revoked,
            "detail": "Revoked outposts cannot authenticate. Rotate a key to restore one." }));
    }
    if failed_cmds > 0 {
        attention.push(json!({ "kind": "command_failed", "severity": "warn", "count": failed_cmds,
            "detail": "Commands the fabric gave up delivering or that an outpost could not run." }));
    }
    if unverifiable > 0 {
        attention.push(json!({ "kind": "settlement_unverifiable", "severity": "warn", "count": unverifiable,
            "detail": "Settlements recorded but never confirmed — no chain access from this outpost." }));
    }
    if dtn_unverified > 0 {
        attention.push(json!({ "kind": "dtn_unverified", "severity": "warn", "count": dtn_unverified,
            "detail": "Received messages whose sender signature did not verify." }));
    }
    if dtn_plaintext > 0 {
        attention.push(json!({ "kind": "dtn_plaintext", "severity": "warn", "count": dtn_plaintext,
            "detail": "Queued messages going out unencrypted — the peer has published no ML-KEM key." }));
    }
    if nodes_offline > 0 {
        attention.push(json!({ "kind": "nodes_offline", "severity": "info", "count": nodes_offline,
            "detail": "Outposts out of contact for more than five minutes." }));
    }

    Json(ConsoleSummary {
        outpost: json!({
            "name": std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into()),
            "role": std::env::var("OUTPOST_ROLE").unwrap_or_else(|_| "outpost".into()),
            "nodeId": state.identity.node_id,
            "lockdown": lockdown,
            "networkIsolated": isolated,
        }),
        fabric: json!({
            "online": nodes_online, "stale": nodes_stale, "offline": nodes_offline,
            "total": nodes_total, "revoked": nodes_revoked,
            "lastContactSecs": last_contact_secs,
        }),
        autonomy: json!({
            "queued": queued, "awaitingAck": unacked, "failed": failed_cmds,
            "firings24h": rules_24h, "telemetryBacklog": telemetry_backlog,
        }),
        comms: json!({
            "dtnOutbox": dtn_outbox, "dtnUnverified": dtn_unverified,
            "dtnPlaintext": dtn_plaintext,
        }),
        logistics: json!({
            "openOrders": open_orders, "inFlight": in_flight,
            "settling": settling, "rejected": rejected, "unverifiable": unverifiable,
        }),
        attention,
        updated_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Recent ops events for the console feed. Excludes the per-row telemetry_ok
/// noise so what remains is what actually happened.
pub async fn events_feed(State(state): State<AppState>) -> Json<Vec<Value>> {
    Json(recent_events(&state, 40).await)
}

async fn recent_events(state: &AppState, limit: i64) -> Vec<Value> {
    let rows = sqlx::query(
        r#"
        SELECT kind, severity, details, node_id, trace_id, created_at
        FROM ops_events
        WHERE kind <> 'telemetry_ok'
        ORDER BY id DESC LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    rows.iter()
        .map(|r| {
            json!({
                "kind": r.get::<String, _>("kind"),
                "severity": r.get::<String, _>("severity"),
                "nodeId": r.get::<Option<String>, _>("node_id"),
                "traceId": r.get::<Option<uuid::Uuid>, _>("trace_id"),
                "details": r.get::<Option<Value>, _>("details"),
                "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect()
}
