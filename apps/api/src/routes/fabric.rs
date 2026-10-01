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
    Router::new()
        .route("/status", get(status))
        .route("/outposts", get(outposts))
}

/// GET /api/fabric/outposts — who is in this fabric, and how to reach them.
///
/// Exists because the console could report *counts* of online nodes but never
/// said which, where, or how to get to one. An operator who needs the farm's
/// console had no way to find it short of reading the database.
///
/// Each entry carries a `ui` URL derived from the peer's registered API
/// endpoint. Following it lands on that outpost's own sign-in: outposts are
/// sovereign and hold their own user tables, so there is no shared session —
/// the response says so rather than letting a dead-end link imply otherwise.
/// See Documentation/OrgBoundaries.md for how single sign-on would work.
async fn outposts(
    State(state): State<AppState>,
    _user: crate::routes::auth_middleware::AuthenticatedUser,
) -> Json<serde_json::Value> {
    use sqlx::Row;

    let rows = sqlx::query(
        r#"
        SELECT node_id::text AS node_id, name, api_endpoint, last_seen,
               COALESCE(revoked, false) AS revoked, body_id, location, ui_url
        FROM node_registry
        ORDER BY name
        "#,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let now = chrono::Utc::now();
    let mut online = 0;
    let mut peers: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let endpoint: String = r.get("api_endpoint");
            let last_seen: Option<chrono::DateTime<chrono::Utc>> = r.try_get("last_seen").ok();
            let age = last_seen.map(|t| (now - t).num_seconds().max(0));
            let revoked: bool = r.get("revoked");

            // Same four states the console paints with: fresh, late, dark, or
            // never heard from at all.
            let status = if revoked {
                "revoked"
            } else {
                match age {
                    Some(a) if a < 120 => { online += 1; "online" }
                    Some(a) if a < 900 => "lagging",
                    Some(_) => "dark",
                    None => "unknown",
                }
            };

            serde_json::json!({
                "nodeId": r.get::<String, _>("node_id"),
                "name": r.get::<Option<String>, _>("name"),
                "apiEndpoint": endpoint,
                // The advertised browser URL when the outpost published one.
                // Falling back to the fabric endpoint is a guess that is often
                // wrong — a container hostname resolves for peers and not for
                // an operator — so the guess is marked as such.
                "ui": r.try_get::<Option<String>, _>("ui_url").ok().flatten()
                        .map(|u| format!("{}/ui/console.html", u.trim_end_matches('/')))
                        .unwrap_or_else(|| endpoint.strip_suffix("/api").unwrap_or(&endpoint).to_string() + "/ui/console.html"),
                "uiAdvertised": r.try_get::<Option<String>, _>("ui_url").ok().flatten().is_some(),
                "lastSeen": last_seen,
                "ageSeconds": age,
                "status": status,
                "bodyId": r.try_get::<Option<i32>, _>("body_id").ok().flatten(),
                "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
            })
        })
        .collect();

    // This outpost first — it is the one you are signed in to.
    peers.insert(0, serde_json::json!({
        "nodeId": state.identity.node_id,
        "name": std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into()),
        "apiEndpoint": std::env::var("API_ENDPOINT").unwrap_or_default(),
        "ui": serde_json::Value::Null,
        "status": "self",
        "ageSeconds": 0,
        "bodyId": std::env::var("BODY_ID").ok().and_then(|b| b.parse::<i32>().ok()),
        "location": std::env::var("OUTPOST_REGION").ok(),
    }));

    Json(serde_json::json!({
        "self": state.identity.node_id,
        "selfName": std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into()),
        "total": peers.len(),
        "online": online + 1,
        "peers": peers,
        // Stated plainly so the UI does not imply a session that does not exist.
        "note": "each outpost holds its own users; following a link means signing in there",
    }))
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
