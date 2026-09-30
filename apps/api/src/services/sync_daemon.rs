use tokio::time::{sleep, Duration};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};
use uuid::Uuid;
use crate::AppState;

/// Structure for exchanging node data
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeInfo {
    pub node_id: String,
    pub public_key: String,
    pub last_seen: String,
}

/// Background daemon for peer-to-peer node synchronization
pub async fn start_sync_daemon(state: AppState) {
    tokio::spawn(async move {
        let client = Client::new();
        let pool = state.db;
        let identity = state.identity;
        // Peers guard /api/nodes-sync with this shared secret (X-Node-Token).
        let node_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();
        if node_token.is_empty() {
            tracing::warn!("⚠️ NODE_SHARED_SECRET not set — peers will reject our sync requests");
        }

        loop {
            tracing::debug!("sync daemon heartbeat");

            // 1️⃣ Fetch known peers
            let peers = match sqlx::query!("SELECT url FROM peer_nodes")
                .fetch_all(&pool)
                .await
            {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!("⚠️ Failed to fetch peers: {}", e);
                    sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };

            if peers.is_empty() {
                tracing::debug!("no peers registered; add via POST /api/peers");
                sleep(Duration::from_secs(60)).await;
                continue;
            }

            // 2️⃣ Prepare local node info payload
            let local_node = NodeInfo {
                node_id: identity.node_id.clone(),
                public_key: identity.public_key.clone(),
                last_seen: Utc::now().to_rfc3339(),
            };

            let payload = vec![local_node.clone()];

            // 3️⃣ Iterate over peers and sync
            for peer in peers {
                let peer_url = peer.url;
                // Matches main.rs: node_sync_routes() nested at /api/nodes-sync
                let sync_url = format!("{}/api/nodes-sync/sync", peer_url);

                tracing::debug!(peer = %sync_url, "syncing with peer");

                let request = crate::services::fabric_auth::signed_post_json(
                    &client,
                    &identity,
                    &sync_url,
                    &serde_json::json!(payload),
                    &node_token,
                );

                match request.send().await {
                    Ok(resp) => {
                        if resp.status().is_success() {
                            if let Ok(remote_nodes) = resp.json::<Vec<NodeInfo>>().await {
                                // 4️⃣ Merge returned nodes into local registry
                                for node in &remote_nodes {
                                    if node.node_id != identity.node_id {
                                        if let (Ok(parsed_id), Ok(parsed_last_seen)) = (
                                            Uuid::parse_str(&node.node_id),
                                            DateTime::parse_from_rfc3339(&node.last_seen),
                                        ) {
                                            let utc_last_seen = parsed_last_seen.with_timezone(&Utc);

                                            if let Err(e) = sqlx::query!(
                                                r#"
                                                INSERT INTO node_registry (node_id, public_key, last_seen)
                                                VALUES ($1, $2, $3)
                                                ON CONFLICT (node_id)
                                                DO UPDATE SET last_seen = EXCLUDED.last_seen
                                                "#,
                                                parsed_id,
                                                node.public_key,
                                                utc_last_seen
                                            )
                                            .execute(&pool)
                                            .await
                                            {
                                                tracing::warn!("⚠️ Failed to insert node {}: {}", node.node_id, e);
                                            }
                                        }
                                    }
                                }

                                tracing::info!(
                                    "✅ Synced {} remote nodes from {}",
                                    remote_nodes.len(),
                                    peer_url
                                );
                            } else {
                                tracing::warn!("⚠️ Failed to parse response from {}", peer_url);
                            }
                        } else {
                            tracing::warn!(
                                "⚠️ Peer {} responded with status {}",
                                peer_url,
                                resp.status()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("⚠️ Failed to reach peer {}: {}", peer_url, e);
                    }
                }
            }

            // 5️⃣ Wait before next sync cycle
            sleep(Duration::from_secs(60)).await;
        }
    });
}

