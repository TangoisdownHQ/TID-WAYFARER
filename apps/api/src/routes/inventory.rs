use axum::{
    routing::{get, post, put, delete},
    Router, Json,
    extract::{Path, State, Query},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use std::collections::HashMap;

use crate::{
    AppState,
    routes::auth_middleware::{AuthenticatedUser, AdminUser},
};
use core_db::models::inventory::Inventory;
use core_db::logic::inventory_logic;

#[derive(Deserialize)]
pub struct NewInventory {
    pub name: String,
    pub description: Option<String>,
    pub quantity: i32,
    pub category: Option<String>,
    pub unit: Option<String>,
    pub threshold: Option<i32>,
    pub token_id: Option<String>,        // ✅ NFT metadata (optional)
    pub token_image_url: Option<String>, // ✅ NFT metadata (optional)
}

#[derive(Deserialize)]
pub struct UpdateQuantityRequest {
    pub new_quantity: i32,
}

/// ✅ Search query params
#[derive(Deserialize)]
pub struct InventorySearchQuery {
    pub name: Option<String>,
    pub category: Option<String>,
}

/// ✅ Bulk import request
#[derive(Deserialize)]
pub struct BulkImportRequest {
    pub items: Vec<NewInventory>,
}

/// ✅ Response for bulk import
#[derive(Serialize)]
pub struct BulkImportResponse {
    pub inserted: usize,
    pub errors: HashMap<String, String>, // failed items (key=name, value=error)
}

/// Mount all inventory routes
pub fn inventory_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_inventory_all).post(create_inventory))
        .route("/:id", get(get_inventory).put(update_inventory).delete(delete_inventory))
        .route("/:id/quantity", put(update_inventory_quantity_handler))
        .route("/low-stock", get(get_low_stock))
        .route("/search", get(search_inventory))
        .route("/bulk-import", post(bulk_import_inventory))
}

/// List all inventory items (scoped to authenticated user)
pub async fn get_inventory_all(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>
) -> Result<Json<Vec<Inventory>>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    inventory_logic::get_all_inventory(&state.db)
        .await
        .map(|all| {
            let user_inventory: Vec<Inventory> =
                all.into_iter().filter(|i| i.owner_id == user_id).collect();
            Json(user_inventory)
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// Get single inventory item by ID (only if owner)
pub async fn get_inventory(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Inventory>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    match inventory_logic::get_inventory_by_id(&state.db, id).await {
        Ok(Some(item)) if item.owner_id == user_id => Ok(Json(item)),
        Ok(Some(_)) => Err(StatusCode::FORBIDDEN),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Create a new inventory item
pub async fn create_inventory(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewInventory>,
) -> Result<Json<Inventory>, StatusCode> {
    inventory_logic::add_inventory_item(
        &state.db,
        Uuid::parse_str(&user.sub).unwrap_or(Uuid::new_v4()),
        &payload.name,
        payload.description.as_deref(),
        payload.quantity,
        None, // location (optional, future expansion)
        payload.token_id.as_deref(),
        payload.token_image_url.as_deref(),
        payload.category.as_deref(),
        payload.unit.as_deref(),
        payload.threshold,
    )
    .await
    .map(Json)
    .map_err(|_| StatusCode::BAD_REQUEST)
}

/// Update an existing inventory item (only if owner)
pub async fn update_inventory(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewInventory>,
) -> Result<Json<Inventory>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    match inventory_logic::get_inventory_by_id(&state.db, id).await {
        Ok(Some(item)) if item.owner_id == user_id => {
            inventory_logic::update_inventory_item(
                &state.db,
                id,
                &payload.name,
                payload.description.as_deref(),
                payload.quantity,
                payload.category.as_deref(),
                payload.unit.as_deref(),
                payload.threshold,
            )
            .await
            .map(Json)
            .map_err(|_| StatusCode::BAD_REQUEST)
        }
        Ok(Some(_)) => Err(StatusCode::FORBIDDEN),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Update inventory quantity only
pub async fn update_inventory_quantity_handler(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<UpdateQuantityRequest>,
) -> Result<Json<Inventory>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    match inventory_logic::get_inventory_by_id(&state.db, id).await {
        Ok(Some(item)) if item.owner_id == user_id => {
            inventory_logic::update_inventory_quantity_with_rules(
                &state.db,
                id,
                payload.new_quantity,
            )
            .await
            .map(Json)
            .map_err(|_| StatusCode::BAD_REQUEST)
        }
        Ok(Some(_)) => Err(StatusCode::FORBIDDEN),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Delete an inventory item (admin only 🚨)
pub async fn delete_inventory(
    AdminUser(_admin): AdminUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    match inventory_logic::delete_inventory_item(&state.db, id).await {
        Ok(true) => Ok(Json("Inventory item deleted")),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

/// Get low-stock items (owner only)
pub async fn get_low_stock(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Inventory>>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    inventory_logic::get_low_stock_items(&state.db)
        .await
        .map(|all| {
            let user_low_stock: Vec<Inventory> =
                all.into_iter().filter(|i| i.owner_id == user_id).collect();
            Json(user_low_stock)
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// 🔍 Search inventory (scoped to owner)
pub async fn search_inventory(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Query(params): Query<InventorySearchQuery>,
) -> Result<Json<Vec<Inventory>>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    inventory_logic::search_inventory(&state.db, params.name, params.category)
        .await
        .map(|all| {
            let user_results: Vec<Inventory> =
                all.into_iter().filter(|i| i.owner_id == user_id).collect();
            Json(user_results)
        })
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// 📥 Bulk import inventory (scoped to owner)
pub async fn bulk_import_inventory(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(request): Json<BulkImportRequest>,
) -> Result<Json<BulkImportResponse>, StatusCode> {
    let mut inserted = 0;
    let mut errors = HashMap::new();
    let user_id = Uuid::parse_str(&user.sub).unwrap_or(Uuid::new_v4());

    for item in request.items {
        match inventory_logic::add_inventory_item(
            &state.db,
            user_id,
            &item.name,
            item.description.as_deref(),
            item.quantity,
            None,
            item.token_id.as_deref(),
            item.token_image_url.as_deref(),
            item.category.as_deref(),
            item.unit.as_deref(),
            item.threshold,
        )
        .await
        {
            Ok(_) => inserted += 1,
            Err(err) => {
                errors.insert(item.name.clone(), format!("{:?}", err));
            }
        }
    }

    Ok(Json(BulkImportResponse { inserted, errors }))
}

