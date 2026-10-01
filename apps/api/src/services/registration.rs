//! Outpost self-registration: non-core outposts announce themselves to their
//! core HQ so the command engine (and DTN default routing) can reach them.
//!
//! Reads CORE_API_URL (accepts a bare host, an /api base, or the full
//! /api/nodes/register URL) and API_ENDPOINT (this node's own API base as
//! reachable by the core). Re-registers every heartbeat, which doubles as a
//! last_seen keepalive. Fails soft: an unreachable core never stops the node.

use base64::Engine as _;
use chrono::Utc;
use reqwest::Client;
use serde_json::json;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use crate::routes::nodes::registration_message;
use crate::services::identity::sign_message;
use crate::AppState;

const HEARTBEAT_SECS: u64 = 60;

/// Normalize CORE_API_URL into the register endpoint.
fn register_url(core_api_url: &str) -> String {
    let base = core_api_url.trim_end_matches('/');
    if base.ends_with("/nodes/register") {
        base.to_string()
    } else if base.ends_with("/api") {
        format!("{base}/nodes/register")
    } else {
        format!("{base}/api/nodes/register")
    }
}

pub async fn run_registration(state: AppState) {
    let Ok(core_api_url) = std::env::var("CORE_API_URL") else {
        tracing::info!("ℹ️ CORE_API_URL not set — skipping self-registration (standalone/core mode)");
        return;
    };
    if core_api_url.is_empty() {
        tracing::info!("ℹ️ CORE_API_URL empty — skipping self-registration (standalone/core mode)");
        return;
    }

    let url = register_url(&core_api_url);
    let api_endpoint = std::env::var("API_ENDPOINT").unwrap_or_default();
    if api_endpoint.is_empty() {
        tracing::warn!("⚠️ API_ENDPOINT not set — registering without a reachable endpoint; core cannot deliver commands to this node");
    }
    let name = std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into());
    let node_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();

    let kem_public_key = base64::engine::general_purpose::STANDARD
        .encode(pqcrypto_traits::kem::PublicKey::as_bytes(&state.commsec.pk));

    let http = Client::new();
    tracing::info!("📡 Self-registration active → {url}");

    loop {
        // Fresh timestamp + signature per heartbeat (registrations expire).
        let ts = Utc::now().timestamp();
        let message = registration_message(&state.identity.node_id, &name, &api_endpoint, ts);
        let payload = json!({
            "node_id": state.identity.node_id,
            "public_key": state.identity.public_key,
            "name": name,
            "api_endpoint": api_endpoint,
            "ts": ts,
            "signature": sign_message(&state.identity, message.as_bytes()),
            // Ed25519 says who we are; ML-KEM says how to seal traffic for us.
            // One handshake carries both.
            "kem_public_key": kem_public_key,
            // Where a browser can reach this outpost's console, which is not
            // necessarily where peers reach its API.
            "ui_url": std::env::var("UI_PUBLIC_URL").ok(),
        });

        let mut request = http.post(&url).json(&payload);
        if !node_token.is_empty() {
            request = request.header("X-Node-Token", &node_token);
        }

        match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(body) if body["status"] == "ok" => {
                        tracing::info!("📡 Registered with core ({name})");
                        store_core_identity(&state, &body, &core_api_url).await;
                    }
                    Ok(body) => {
                        tracing::warn!("⚠️ Core refused registration: {}", body["status"]);
                    }
                    Err(e) => tracing::warn!(error = %e, "bad registration response from core"),
                }
            }
            Ok(resp) => {
                tracing::warn!("⚠️ Core registration rejected: {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("⚠️ Core unreachable for registration: {e}");
            }
        }

        sleep(Duration::from_secs(HEARTBEAT_SECS)).await;
    }
}

/// The mutual half of the exchange: remember who core is so core-signed
/// traffic (DTN envelopes, future commands) can be verified locally.
async fn store_core_identity(state: &AppState, body: &serde_json::Value, core_api_url: &str) {
    let (Some(core_id), Some(core_pk)) = (
        body["core_node_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
        body["core_public_key"].as_str(),
    ) else {
        return; // older core without identity in response
    };
    let core_name = body["core_name"].as_str().unwrap_or("core");
    let core_kem = body["core_kem_public_key"].as_str();

    // Core's API base: normalize to the register URL, then strip the suffix —
    // handles bare-host CORE_API_URL values too.
    let core_endpoint = register_url(core_api_url)
        .trim_end_matches("/nodes/register")
        .to_string();

    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO node_registry (node_id, name, public_key, api_endpoint, kem_public_key, last_seen)
        VALUES ($1, $2, $3, $4, $5, NOW())
        ON CONFLICT (node_id) DO UPDATE SET
            name = EXCLUDED.name, public_key = EXCLUDED.public_key,
            api_endpoint = EXCLUDED.api_endpoint,
            kem_public_key = COALESCE(EXCLUDED.kem_public_key, node_registry.kem_public_key),
            last_seen = NOW()
        "#,
    )
    .bind(core_id)
    .bind(core_name)
    .bind(core_pk)
    .bind(&core_endpoint)
    .bind(core_kem)
    .execute(&state.db)
    .await
    {
        tracing::warn!("⚠️ Failed to store core identity: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::register_url;

    #[test]
    fn normalizes_core_api_url_shapes() {
        assert_eq!(
            register_url("http://core:3000"),
            "http://core:3000/api/nodes/register"
        );
        assert_eq!(
            register_url("http://core:3000/api"),
            "http://core:3000/api/nodes/register"
        );
        assert_eq!(
            register_url("http://core:3000/api/nodes/register"),
            "http://core:3000/api/nodes/register"
        );
        assert_eq!(
            register_url("http://core:3000/api/"),
            "http://core:3000/api/nodes/register"
        );
    }
}
