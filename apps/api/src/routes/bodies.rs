//! 🌌 Bodies catalog — read-only registry of celestial bodies
//! (Sun, planets, moons, asteroids, host stars, exoplanets) and the
//! named surface features (landing sites, craters, mares) attached to them.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::AppState;

#[derive(Serialize)]
pub struct Body {
    pub body_id: i32,
    pub name: String,
    pub parent_id: Option<i32>,
    pub body_class: String,
    pub frame: Option<String>,
    pub radius_km: Option<f64>,
    pub gravity_ms2: Option<f64>,
    pub rotation_hr: Option<f64>,
    pub ephemeris_src: Option<String>,
    pub distance_pc: Option<f64>,
    pub meta: serde_json::Value,
}

#[derive(Serialize)]
pub struct SurfaceFeature {
    pub id: String,
    pub body_id: i32,
    pub name: String,
    pub feature_type: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt: Option<f64>,
    pub source: Option<String>,
    pub meta: serde_json::Value,
}

#[derive(Deserialize)]
pub struct BodyListQuery {
    /// Filter by body_class — e.g. ?class=moon or ?class=exoplanet
    pub class: Option<String>,
    /// Filter by parent — e.g. ?parent_id=399 lists Earth's moons
    pub parent_id: Option<i32>,
}

#[derive(Deserialize)]
pub struct FeatureListQuery {
    /// Filter by feature_type — e.g. ?type=landing_site
    pub r#type: Option<String>,
}

pub fn body_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_bodies))
        .route("/:id", get(get_body))
        .route("/:id/features", get(list_features))
}

/// `GET /api/bodies` — list all bodies, optionally filtered by class or parent.
pub async fn list_bodies(
    State(state): State<AppState>,
    Query(params): Query<BodyListQuery>,
) -> Result<Json<Vec<Body>>, StatusCode> {
    let rows = sqlx::query!(
        r#"
        SELECT body_id, name, parent_id, body_class, frame,
               radius_km, gravity_ms2, rotation_hr, ephemeris_src,
               distance_pc, meta
        FROM bodies
        WHERE ($1::text IS NULL OR body_class = $1)
          AND ($2::int  IS NULL OR parent_id  = $2)
        ORDER BY
          CASE body_class
            WHEN 'star'         THEN 0
            WHEN 'planet'       THEN 1
            WHEN 'dwarf_planet' THEN 2
            WHEN 'moon'         THEN 3
            WHEN 'asteroid'     THEN 4
            WHEN 'exoplanet'    THEN 5
            ELSE 9
          END,
          body_id
        "#,
        params.class,
        params.parent_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let bodies = rows
        .into_iter()
        .map(|r| Body {
            body_id: r.body_id,
            name: r.name,
            parent_id: r.parent_id,
            body_class: r.body_class,
            frame: r.frame,
            radius_km: r.radius_km,
            gravity_ms2: r.gravity_ms2,
            rotation_hr: r.rotation_hr,
            ephemeris_src: r.ephemeris_src,
            distance_pc: r.distance_pc,
            meta: r.meta.unwrap_or_else(|| serde_json::json!({})),
        })
        .collect();

    Ok(Json(bodies))
}

/// `GET /api/bodies/:id` — fetch one body by NAIF / synthetic id.
pub async fn get_body(
    State(state): State<AppState>,
    Path(id): Path<i32>,
) -> Result<Json<Body>, StatusCode> {
    let r = sqlx::query!(
        r#"
        SELECT body_id, name, parent_id, body_class, frame,
               radius_km, gravity_ms2, rotation_hr, ephemeris_src,
               distance_pc, meta
        FROM bodies
        WHERE body_id = $1
        "#,
        id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(Body {
        body_id: r.body_id,
        name: r.name,
        parent_id: r.parent_id,
        body_class: r.body_class,
        frame: r.frame,
        radius_km: r.radius_km,
        gravity_ms2: r.gravity_ms2,
        rotation_hr: r.rotation_hr,
        ephemeris_src: r.ephemeris_src,
        distance_pc: r.distance_pc,
        meta: r.meta.unwrap_or_else(|| serde_json::json!({})),
    }))
}

/// `GET /api/bodies/:id/features` — surface features for a body
/// (landing sites, craters, mares, named outposts).
pub async fn list_features(
    State(state): State<AppState>,
    Path(id): Path<i32>,
    Query(params): Query<FeatureListQuery>,
) -> Result<Json<Vec<SurfaceFeature>>, StatusCode> {
    let rows = sqlx::query!(
        r#"
        SELECT id::text AS "id!", body_id, name, feature_type,
               lat, lon, alt, source, meta
        FROM surface_features
        WHERE body_id = $1
          AND ($2::text IS NULL OR feature_type = $2)
        ORDER BY feature_type NULLS LAST, name
        "#,
        id,
        params.r#type
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let features = rows
        .into_iter()
        .map(|r| SurfaceFeature {
            id: r.id,
            body_id: r.body_id,
            name: r.name,
            feature_type: r.feature_type,
            lat: r.lat,
            lon: r.lon,
            alt: r.alt,
            source: r.source,
            meta: r.meta.unwrap_or_else(|| serde_json::json!({})),
        })
        .collect();

    Ok(Json(features))
}
