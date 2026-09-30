//! Peer fabric management — the outposts this node dials during sync cycles.
//! The sync daemon reads peer_nodes every cycle, so changes here take effect
//! within one heartbeat. Reads need any authenticated principal; mutations
//! require an admin JWT.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::AdminUser;
use crate::AppState;

pub fn peer_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_peers).post(add_peer))
        .route("/:id", axum::routing::delete(remove_peer))
}

async fn list_peers(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = sqlx::query(
        r#"
        SELECT id, node_id, url, trust_level, last_seen
        FROM peer_nodes
        ORDER BY last_seen DESC NULLS LAST
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let peers: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "nodeId": r.get::<Option<Uuid>, _>("node_id"),
                "url": r.get::<String, _>("url"),
                "trustLevel": r.get::<Option<String>, _>("trust_level"),
                "lastSeen": r.get::<Option<chrono::NaiveDateTime>, _>("last_seen")
                    .map(|t| t.to_string()),
            })
        })
        .collect();

    Ok(Json(json!(peers)))
}

#[derive(Deserialize)]
struct AddPeer {
    url: String,
    trust_level: Option<String>,
}

async fn add_peer(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<AddPeer>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, String)> {
    let url = body.url.trim_end_matches('/').to_string();
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err((
            StatusCode::BAD_REQUEST,
            "url must start with http:// or https://".to_string(),
        ));
    }

    let row = sqlx::query(
        r#"
        INSERT INTO peer_nodes (url, trust_level)
        VALUES ($1, $2)
        ON CONFLICT (url) DO UPDATE SET trust_level = EXCLUDED.trust_level
        RETURNING id, url, trust_level
        "#,
    )
    .bind(&url)
    .bind(body.trust_level.as_deref().unwrap_or("trusted"))
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("insert failed: {e}")))?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.get::<Uuid, _>("id"),
            "url": row.get::<String, _>("url"),
            "trustLevel": row.get::<Option<String>, _>("trust_level"),
        })),
    ))
}

async fn remove_peer(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, (StatusCode, String)> {
    let result = sqlx::query("DELETE FROM peer_nodes WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "peer not found".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}
