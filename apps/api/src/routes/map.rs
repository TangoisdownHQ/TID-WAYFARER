use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::routes::auth_middleware::{Caller, Principal};
use crate::AppState;

#[derive(Deserialize)]
pub struct MapUpdateRequest {
    /// Which node's position this is. Ignored for a signing node — it may only
    /// report its own — and required for an admin reporting on one's behalf.
    pub node_id: Option<Uuid>,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Serialize)]
pub struct MapNode {
    pub node_id: Uuid,
    pub name: String,
    pub api_endpoint: String,
    pub lat: f64,
    pub lon: f64,
    pub updated_at: String,
}

pub fn map_routes() -> Router<AppState> {
    Router::new()
        .route("/nodes", get(list_nodes))
        .route("/update", post(update_location))
}

/// Record where a node is.
///
/// The node is taken from the verified caller, not the payload. This endpoint
/// also refreshes `node_registry.last_seen`, which is what the console, the
/// fabric status and the rollup all read to decide whether an outpost is in
/// contact — so letting any authenticated caller name any node meant being
/// able to make a dark outpost look alive, and to move it on the map. In a
/// system whose central claim is that a reading carries its true age, that is
/// the one thing that must not be forgeable.
pub async fn update_location(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Json(req): Json<MapUpdateRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let node_id = match (&principal, req.node_id) {
        // A signing node reports itself; a node_id in the body is ignored
        // rather than honoured.
        (Principal::Node(id), _) => *id,
        // An admin may place a node that cannot reach us itself.
        (p, Some(id)) if p.is_admin() => id,
        (p, None) if p.is_admin() => {
            return Err((StatusCode::BAD_REQUEST, "node_id is required".to_string()))
        }
        _ => {
            tracing::warn!(
                caller = %principal.describe(),
                "refused map update: caller may not report a node's position"
            );
            return Err((
                StatusCode::FORBIDDEN,
                "only a signing node (for itself) or an admin may report a position".to_string(),
            ));
        }
    };

    tracing::info!(%node_id, lat = req.lat, lon = req.lon, "map position reported");

    // Refresh last_seen; the node is expected to be in node_registry already.
    let _ = sqlx::query!(
        r#"
        UPDATE node_registry
        SET last_seen = NOW()
        WHERE node_id = $1
        "#,
        node_id
    )
    .execute(&state.db)
    .await;

    // Upsert into node_map
    let result = sqlx::query!(
        r#"
        INSERT INTO node_map (node_id, lat, lon, updated_at)
        VALUES ($1, $2, $3, NOW())
        ON CONFLICT (node_id)
        DO UPDATE SET 
            lat        = EXCLUDED.lat,
            lon        = EXCLUDED.lon,
            updated_at = NOW()
        "#,
        node_id,
        req.lat,
        req.lon
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Ok(Json(json!({ "status": "ok", "nodeId": node_id }))),
        Err(e) => {
            tracing::error!(%node_id, error = %e, "map upsert failed");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "could not record position".to_string()))
        }
    }
}

pub async fn list_nodes(State(state): State<AppState>) -> Json<Vec<MapNode>> {
    let rows = sqlx::query!(
        r#"
        SELECT 
            r.node_id,
            r.name,
            r.api_endpoint,
            m.lat,
            m.lon,
            m.updated_at
        FROM node_registry r
        INNER JOIN node_map m ON m.node_id = r.node_id
        ORDER BY m.updated_at DESC
        "#
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Json(
        rows
            .into_iter()
            .map(|r| MapNode {
                node_id: r.node_id,
                name: r.name,
                api_endpoint: r.api_endpoint,
                lat: r.lat,
                lon: r.lon,
                updated_at: r.updated_at.to_string(),
            })
            .collect(),
    )
}

