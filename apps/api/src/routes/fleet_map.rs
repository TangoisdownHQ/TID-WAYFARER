use axum::{Router, routing::get, Json, extract::State};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::AppState;

// Nested under /api/map/fleet in main.rs.
pub fn fleet_map_routes() -> Router<AppState> {
    Router::new()
        .route("/geojson", get(geojson_latest))
        .route("/tracks", get(tracks_geojson))
}

/// Per-asset movement history as a GeoJSON FeatureCollection of LineStrings
/// (the "fleet transcripts" trail). Query params: `hours` (default 24, max
/// 168) time window, `limit` (default 500, max 5000) points per asset.
async fn tracks_geojson(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let hours: i64 = q.get("hours").and_then(|v| v.parse().ok()).unwrap_or(24).clamp(1, 168);
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(500).clamp(2, 5000);

    let rows = sqlx::query(
        r#"
        SELECT ft.asset_id, fa.name, fa.asset_type,
               ft.lon, ft.lat, ft.timestamp
        FROM fleet_telemetry ft
        JOIN fleet_assets fa ON fa.id = ft.asset_id
        WHERE ft.lat IS NOT NULL AND ft.lon IS NOT NULL
          AND ft.timestamp > NOW() - make_interval(hours => $1)
        ORDER BY ft.asset_id, ft.timestamp ASC
        "#,
    )
    .bind(hours as i32)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Group ordered points by asset into LineString coordinate arrays.
    use std::collections::HashMap as Map;
    let mut order: Vec<Uuid> = Vec::new();
    let mut coords: Map<Uuid, Vec<[f64; 2]>> = Map::new();
    let mut meta: Map<Uuid, (Option<String>, Option<String>)> = Map::new();

    for r in &rows {
        let Ok(asset_id) = r.try_get::<Uuid, _>("asset_id") else { continue };
        let lon: f64 = r.try_get("lon").unwrap_or(0.0);
        let lat: f64 = r.try_get("lat").unwrap_or(0.0);
        let entry = coords.entry(asset_id).or_insert_with(|| { order.push(asset_id); Vec::new() });
        if (entry.len() as i64) < limit {
            entry.push([lon, lat]);
        }
        meta.entry(asset_id).or_insert_with(|| (
            r.try_get::<String, _>("name").ok(),
            r.try_get::<String, _>("asset_type").ok(),
        ));
    }

    let features: Vec<serde_json::Value> = order
        .iter()
        .filter(|id| coords.get(*id).map(|c| c.len() >= 2).unwrap_or(false))
        .map(|id| {
            let (name, asset_type) = meta.get(id).cloned().unwrap_or((None, None));
            json!({
                "type": "Feature",
                "geometry": { "type": "LineString", "coordinates": coords[id] },
                "properties": {
                    "asset_id": id.to_string(),
                    "name": name,
                    "asset_type": asset_type,
                    "points": coords[id].len(),
                }
            })
        })
        .collect();

    Json(json!({ "type": "FeatureCollection", "features": features }))
}

// Returns a GeoJSON FeatureCollection of latest point per asset
async fn geojson_latest(State(state): State<AppState>) -> Json<serde_json::Value> {
    let rows = sqlx::query(
        r#"
        WITH last_fix AS (
          SELECT
            ft.asset_id, ft.node_id, ft.timestamp,
            ft.lat, ft.lon, ft.alt, ft.speed, ft.heading,
            ft.battery, ft.signal_db, ft.anomaly_score, ft.malware_flag,
            fa.name, fa.asset_type,
            ROW_NUMBER() OVER (PARTITION BY ft.asset_id ORDER BY ft.timestamp DESC) AS rn
          FROM fleet_telemetry ft
          JOIN fleet_assets fa ON fa.id = ft.asset_id
          WHERE ft.lat IS NOT NULL AND ft.lon IS NOT NULL
        )
        SELECT * FROM last_fix WHERE rn = 1;
        "#
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let features: Vec<serde_json::Value> = rows.iter().map(|r| {
        let lat: f64 = r.try_get("lat").unwrap_or(0.0);
        let lon: f64 = r.try_get("lon").unwrap_or(0.0);

        json!({
          "type": "Feature",
          "geometry": { "type": "Point", "coordinates": [lon, lat] },
          "properties": {
            "asset_id": r.try_get::<uuid::Uuid, _>("asset_id").ok().map(|x| x.to_string()),
            "name": r.try_get::<String, _>("name").ok(),
            "asset_type": r.try_get::<String, _>("asset_type").ok(),
            "node_id": r.try_get::<String, _>("node_id").ok(),
            "timestamp": r.try_get::<chrono::NaiveDateTime, _>("timestamp").ok().map(|t| t.to_string()),
            "alt": r.try_get::<f64, _>("alt").ok(),
            "speed": r.try_get::<f64, _>("speed").ok(),
            "heading": r.try_get::<f64, _>("heading").ok(),
            "battery": r.try_get::<f64, _>("battery").ok(),
            "signal_db": r.try_get::<f64, _>("signal_db").ok(),
            "anomaly_score": r.try_get::<f64, _>("anomaly_score").ok(),
            "malware_flag": r.try_get::<bool, _>("malware_flag").ok()
          }
        })
    }).collect();

    Json(json!({
        "type": "FeatureCollection",
        "features": features
    }))
}

