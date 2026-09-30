use axum::{
    routing::get,
    Router, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::{NaiveDateTime, DateTime, Utc};

use crate::{
    AppState,
    routes::auth_middleware::{AuthenticatedUser, AdminUser},
};

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Asset {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub location: String,
    pub status: String,
    pub nft_token: Option<String>,
    pub nft_image_url: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: Option<NaiveDateTime>,
    // 🆕 from 20260612_01_bodies_and_asset_logistics.sql
    pub meta: Option<serde_json::Value>,
    pub part_number: Option<String>,
    pub serial_number: Option<String>,
    pub lot_number: Option<String>,
    pub manufacturer: Option<String>,
    pub supplier: Option<String>,
    pub body_id: Option<i32>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt: Option<f64>,
    pub mass_kg: Option<f64>,
    pub volume_m3: Option<f64>,
    pub hazmat_class: Option<String>,
    pub condition: Option<String>,
    pub lifecycle: Option<String>,
    pub custody_node: Option<Uuid>,
    pub expires_at: Option<DateTime<Utc>>,
    pub retired_at: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Assignment {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub package_id: Option<Uuid>,
    pub inventory_id: Option<Uuid>,
    pub schedule_start: Option<NaiveDateTime>,
    pub schedule_end: Option<NaiveDateTime>,
    pub status: String,
    pub created_at: NaiveDateTime,
}

#[derive(Serialize)]
pub struct AssetWithAssignments {
    pub asset: Asset,
    pub assignments: Vec<Assignment>,
}

#[derive(Deserialize)]
pub struct NewAsset {
    pub name: String,
    pub description: Option<String>,
    pub location: String,
    pub status: Option<String>, // defaults to "in_transit"
    pub nft_token: Option<String>,
    pub nft_image_url: Option<String>,
    // 🆕 logistics fields
    pub meta: Option<serde_json::Value>,
    pub part_number: Option<String>,
    pub serial_number: Option<String>,
    pub lot_number: Option<String>,
    pub manufacturer: Option<String>,
    pub supplier: Option<String>,
    pub body_id: Option<i32>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt: Option<f64>,
    pub mass_kg: Option<f64>,
    pub volume_m3: Option<f64>,
    pub hazmat_class: Option<String>,
    pub condition: Option<String>,
    pub lifecycle: Option<String>,
}

/// Query-string filters for `GET /assets`.
#[derive(Deserialize)]
pub struct AssetFilter {
    pub body_id:       Option<i32>,
    pub lifecycle:     Option<String>,
    pub part_number:   Option<String>,
    pub serial_number: Option<String>,
    pub lot_number:    Option<String>,
    pub condition:     Option<String>,
}

/// Mount all asset routes
pub fn asset_routes() -> Router<AppState> {
    Router::new()
        .route("/assets", get(get_assets).post(create_asset))
        .route("/assets/:id", get(get_asset).put(update_asset).delete(delete_asset))
        .route("/assets/all", get(get_all_assets))                  // 🚨 admin-only
        .route("/assets/by-body/:body_id", get(get_assets_by_body)) // 🆕 filter helper
}

/// List assets owned by the authenticated user, optionally filtered.
pub async fn get_assets(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Query(filter): Query<AssetFilter>,
) -> Result<Json<Vec<Asset>>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let assets = sqlx::query_as::<_, Asset>(
        r#"
        SELECT * FROM assets
        WHERE owner_id = $1
          AND ($2::int  IS NULL OR body_id       = $2)
          AND ($3::text IS NULL OR lifecycle     = $3)
          AND ($4::text IS NULL OR part_number   = $4)
          AND ($5::text IS NULL OR serial_number = $5)
          AND ($6::text IS NULL OR lot_number    = $6)
          AND ($7::text IS NULL OR condition     = $7)
        ORDER BY created_at DESC
        "#,
    )
    .bind(user_id)
    .bind(filter.body_id)
    .bind(filter.lifecycle)
    .bind(filter.part_number)
    .bind(filter.serial_number)
    .bind(filter.lot_number)
    .bind(filter.condition)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assets))
}

/// 🚨 List ALL assets (admin only)
pub async fn get_all_assets(
    AdminUser(_admin): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Asset>>, StatusCode> {
    let assets = sqlx::query_as::<_, Asset>("SELECT * FROM assets ORDER BY created_at DESC")
        .fetch_all(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assets))
}

/// 🆕 List every asset on a given body (admin — for ops/map views).
pub async fn get_assets_by_body(
    AdminUser(_admin): AdminUser,
    Path(body_id): Path<i32>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Asset>>, StatusCode> {
    let assets = sqlx::query_as::<_, Asset>(
        "SELECT * FROM assets WHERE body_id = $1 ORDER BY name",
    )
    .bind(body_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assets))
}

/// Get a single asset and its assignments (owner-only, unless admin)
pub async fn get_asset(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<AssetWithAssignments>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let asset = sqlx::query_as::<_, Asset>("SELECT * FROM assets WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(asset) = asset else {
        return Err(StatusCode::NOT_FOUND);
    };

    if asset.owner_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }

    let assignments = sqlx::query_as::<_, Assignment>(
        "SELECT * FROM assignments WHERE asset_id = $1 ORDER BY created_at DESC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Ok(Json(AssetWithAssignments { asset, assignments }))
}

/// Create a new asset (owner = current user)
pub async fn create_asset(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewAsset>,
) -> Result<Json<Asset>, StatusCode> {
    let owner_id = Uuid::parse_str(&user.sub).unwrap_or_else(|_| Uuid::new_v4());
    let id = Uuid::new_v4();

    let asset = sqlx::query_as::<_, Asset>(
        r#"
        INSERT INTO assets (
            id, owner_id, name, description, location,
            status, nft_token, nft_image_url, created_at,
            meta, part_number, serial_number, lot_number, manufacturer, supplier,
            body_id, lat, lon, alt, mass_kg, volume_m3,
            hazmat_class, condition, lifecycle
        )
        VALUES (
            $1, $2, $3, $4, $5,
            COALESCE($6, 'in_transit'), $7, $8, NOW(),
            COALESCE($9, '{}'::jsonb), $10, $11, $12, $13, $14,
            $15, $16, $17, $18, $19, $20,
            $21, $22, $23
        )
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(owner_id)
    .bind(&payload.name)
    .bind(&payload.description)
    .bind(&payload.location)
    .bind(&payload.status)
    .bind(&payload.nft_token)
    .bind(&payload.nft_image_url)
    .bind(&payload.meta)
    .bind(&payload.part_number)
    .bind(&payload.serial_number)
    .bind(&payload.lot_number)
    .bind(&payload.manufacturer)
    .bind(&payload.supplier)
    .bind(payload.body_id)
    .bind(payload.lat)
    .bind(payload.lon)
    .bind(payload.alt)
    .bind(payload.mass_kg)
    .bind(payload.volume_m3)
    .bind(&payload.hazmat_class)
    .bind(&payload.condition)
    .bind(&payload.lifecycle)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(asset))
}

/// Update an existing asset (only if owner). All new fields are patch-style:
/// pass NULL/omit to leave the existing value untouched.
pub async fn update_asset(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewAsset>,
) -> Result<Json<Asset>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let asset = sqlx::query_as::<_, Asset>(
        r#"
        UPDATE assets SET
            name          = $2,
            description   = $3,
            location      = $4,
            status        = COALESCE($5, status),
            nft_token     = $6,
            nft_image_url = $7,
            meta          = COALESCE($8, meta),
            part_number   = COALESCE($9,  part_number),
            serial_number = COALESCE($10, serial_number),
            lot_number    = COALESCE($11, lot_number),
            manufacturer  = COALESCE($12, manufacturer),
            supplier      = COALESCE($13, supplier),
            body_id       = COALESCE($14, body_id),
            lat           = COALESCE($15, lat),
            lon           = COALESCE($16, lon),
            alt           = COALESCE($17, alt),
            mass_kg       = COALESCE($18, mass_kg),
            volume_m3     = COALESCE($19, volume_m3),
            hazmat_class  = COALESCE($20, hazmat_class),
            condition     = COALESCE($21, condition),
            lifecycle     = COALESCE($22, lifecycle),
            updated_at    = NOW()
        WHERE id = $1 AND owner_id = $23
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(&payload.name)
    .bind(&payload.description)
    .bind(&payload.location)
    .bind(&payload.status)
    .bind(&payload.nft_token)
    .bind(&payload.nft_image_url)
    .bind(&payload.meta)
    .bind(&payload.part_number)
    .bind(&payload.serial_number)
    .bind(&payload.lot_number)
    .bind(&payload.manufacturer)
    .bind(&payload.supplier)
    .bind(payload.body_id)
    .bind(payload.lat)
    .bind(payload.lon)
    .bind(payload.alt)
    .bind(payload.mass_kg)
    .bind(payload.volume_m3)
    .bind(&payload.hazmat_class)
    .bind(&payload.condition)
    .bind(&payload.lifecycle)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match asset {
        Some(a) => Ok(Json(a)),
        None    => Err(StatusCode::NOT_FOUND),
    }
}

/// Delete an asset (admin only 🚨)
pub async fn delete_asset(
    AdminUser(_admin): AdminUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let rows_affected = sqlx::query("DELETE FROM assets WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .rows_affected();

    if rows_affected == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("Asset deleted"))
    }
}
