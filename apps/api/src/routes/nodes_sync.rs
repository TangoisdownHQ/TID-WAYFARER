use axum::{extract::State, Json, routing::post, Router};
use serde::{Deserialize, Serialize};
use crate::AppState;
use sqlx::Row;
use uuid::Uuid;
use chrono::{DateTime, Utc};

/// Represents a node entry for synchronization
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub node_id: String,       // received as String for JSON interoperability
    pub public_key: String,
    pub last_seen: String,
}

/// Register endpoint router
pub fn node_sync_routes() -> Router<AppState> {
    Router::new().route("/sync", post(sync_nodes))
}

/// Synchronize node registries between Outposts
pub async fn sync_nodes(
    State(state): State<AppState>,
    Json(remote_nodes): Json<Vec<NodeInfo>>,
) -> Json<Vec<NodeInfo>> {
    let pool = &state.db;

    // 1️⃣ Insert or update nodes from the remote peer
    for node in &remote_nodes {
        let parsed_id = match Uuid::parse_str(&node.node_id) {
            Ok(uuid) => uuid,
            Err(_) => continue, // skip invalid UUIDs
        };

        sqlx::query!(
            r#"
            INSERT INTO node_registry (node_id, public_key, last_seen)
            VALUES ($1, $2, NOW())
            ON CONFLICT (node_id) DO UPDATE SET last_seen = NOW()
            "#,
            parsed_id,
            node.public_key
        )
        .execute(pool)
        .await
        .ok();
    }

    // 2️⃣ Return all known nodes from the local registry
    let local_nodes = sqlx::query("SELECT node_id, public_key, last_seen FROM node_registry")
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    let response: Vec<NodeInfo> = local_nodes
        .into_iter()
        .map(|r| {
            let node_id: Uuid = r.get("node_id");
            let public_key: String = r.get("public_key");
            let last_seen: DateTime<Utc> = r.get("last_seen");

            NodeInfo {
                node_id: node_id.to_string(),
                public_key,
                last_seen: last_seen.to_rfc3339(),
            }
        })
        .collect();

    tracing::info!("🔁 Node sync completed: exchanged {} nodes", response.len());

    Json(response)
}

