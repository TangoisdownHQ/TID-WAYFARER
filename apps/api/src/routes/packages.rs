use axum::{
    routing::{get, post, put, delete},
    Router, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use uuid::Uuid;
use chrono::NaiveDateTime;

use crate::{
    AppState,
    routes::auth_middleware::{AuthenticatedUser, AdminUser},
};
use core_db::models::packages::Package;
use core_db::logic::packages_logic;

/// Payload for creating a package (NFT fields optional)
#[derive(Deserialize, Debug)]
pub struct NewPackage {
    pub inventory_item_id: Option<Uuid>,
    pub description: Option<String>,
    pub status: String,
    pub location: Option<String>,
    pub eta: Option<NaiveDateTime>,
    pub nft_token: Option<String>,
    pub nft_image_url: Option<String>,
}

/// Payload for updating a package (NFT fields optional)
#[derive(Deserialize, Debug)]
pub struct UpdatePackage {
    pub status: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub eta: Option<NaiveDateTime>,
    pub nft_token: Option<String>,
    pub nft_image_url: Option<String>,
}

/// Mount all package routes
pub fn package_routes() -> Router<AppState> {
    Router::new()
        // Current user’s packages
        .route("/", get(get_packages).post(create_package))
        // Admin only: see everything
        .route("/all", get(get_all_packages))
        // Specific package operations
        .route("/:id", get(get_package).put(update_package).delete(delete_package))
        // Mark a package as delivered
        .route("/:id/deliver", put(mark_package_delivered))
}

/// List only the authenticated user’s packages
pub async fn get_packages(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Package>>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    packages_logic::get_all_packages(&state.db)
        .await
        .map(|all| {
            let user_packages: Vec<Package> =
                all.into_iter().filter(|p| p.owner_id == user_id).collect();
            Json(user_packages)
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// 🚨 List ALL packages (admin only)
pub async fn get_all_packages(
    AdminUser(_admin): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Package>>, StatusCode> {
    packages_logic::get_all_packages(&state.db)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Get a single package by ID (only if owner, unless admin)
pub async fn get_package(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Package>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    match packages_logic::get_package_by_id(&state.db, id).await {
        Ok(Some(pkg)) if pkg.owner_id == user_id => Ok(Json(pkg)),
        Ok(Some(_)) => Err(StatusCode::FORBIDDEN),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Create a new package
pub async fn create_package(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewPackage>,
) -> Result<Json<Package>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    packages_logic::add_package(
        &state.db,
        user_id,
        payload.inventory_item_id,
        &payload.status,
        payload.description.as_deref(),
        payload.location.as_deref(),
        payload.nft_token.as_deref(),
        payload.nft_image_url.as_deref(),
        payload.eta,
    )
    .await
    .map(Json)
    .map_err(|_| StatusCode::BAD_REQUEST)
}

/// Update a package (only if owner)
pub async fn update_package(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<UpdatePackage>,
) -> Result<Json<Package>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    match packages_logic::get_package_by_id(&state.db, id).await {
        Ok(Some(pkg)) if pkg.owner_id == user_id => {
            match packages_logic::update_package(
                &state.db,
                id,
                payload.status.as_deref(),
                payload.description.as_deref(),
                payload.location.as_deref(),
                payload.eta,
                payload.nft_token.as_deref(),
                payload.nft_image_url.as_deref(),
            )
            .await
            {
                Ok(Some(pkg)) => Ok(Json(pkg)),
                Ok(None) => Err(StatusCode::NOT_FOUND),
                Err(_) => Err(StatusCode::BAD_REQUEST),
            }
        }
        Ok(Some(_)) => Err(StatusCode::FORBIDDEN),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Delete a package (admin only 🚨)
pub async fn delete_package(
    AdminUser(_admin): AdminUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    match packages_logic::delete_package(&state.db, id).await {
        Ok(true) => Ok(Json("Package deleted")),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Mark a package as delivered (owner only ✅)
pub async fn mark_package_delivered(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Package>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let package = sqlx::query_as!(
        Package,
        r#"
        UPDATE packages
        SET status = 'delivered',
            completed_at = NOW()
        WHERE id = $1 AND owner_id = $2
        RETURNING id, owner_id, inventory_item_id, description, status,
                  location, eta, created_at, completed_at,
                  nft_token, nft_image_url
        "#,
        id,
        user_id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    package.map(Json).ok_or(StatusCode::NOT_FOUND)
}
