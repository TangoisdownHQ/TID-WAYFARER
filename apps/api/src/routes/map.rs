use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::AppState;

#[derive(Deserialize)]
pub struct MapUpdateRequest {
    pub node_id: Uuid,
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

/// Insert if missing, update if exists
pub async fn update_location(
    State(state): State<AppState>,
    Json(req): Json<MapUpdateRequest>,
) -> Json<serde_json::Value> {
    tracing::info!(
        "📍 /api/map/update: node_id={}, lat={}, lon={}",
        req.node_id, req.lat, req.lon
    );

    // Just refresh last_seen; node is assumed to already exist in node_registry
    let _ = sqlx::query!(
        r#"
        UPDATE node_registry
        SET last_seen = NOW()
        WHERE node_id = $1
        "#,
        req.node_id
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
        req.node_id,
        req.lat,
        req.lon
    )
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => Json(json!({ "status": "ok" })),
        Err(e) => Json(json!({ "status": "error", "message": e.to_string() })),
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

