use axum::{
    routing::{get, post, put, delete},
    Router, Json,
    extract::{Path, State, Query},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use std::collections::HashMap;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::{
    AppState,
    routes::auth_middleware::{AuthenticatedUser, AdminUser},
    routes::movements::{apply_stock_change, StockChange},
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

/// Create an inventory item and record its opening balance as a genesis
/// 'receipt' movement, in one transaction.
///
/// The row is inserted at zero and the opening applied through
/// [`apply_stock_change`], so a single rule holds — the level only ever moves
/// through the ledger — and `inventory.quantity` is exactly the sum of its
/// movements, opening balance included. That exact equality is what a
/// stock-counter CRDT folds; an opening set outside the ledger would be an
/// unaccounted starting offset on every item.
async fn insert_item_with_opening(
    db: &PgPool,
    owner: Uuid,
    actor: Option<Uuid>,
    it: &NewInventory,
) -> Result<Inventory, String> {
    if it.quantity < 0 {
        return Err("cannot create an item with negative stock".into());
    }
    let mut tx = db.begin().await.map_err(|e| e.to_string())?;

    // Mirror of queries::create_inventory, seeded at zero: the opening is
    // applied below as a movement rather than set here.
    let id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO inventory
          (id, owner_id, name, description, quantity, location,
           token_id, token_image_url, category, unit, threshold, created_at)
        VALUES
          (gen_random_uuid(), $1, $2, $3, 0, NULL,
           $4, $5, COALESCE($6,'general'), COALESCE($7,'units'), COALESCE($8,0), NOW())
        RETURNING id
        "#,
    )
    .bind(owner)
    .bind(&it.name)
    .bind(it.description.as_deref())
    .bind(it.token_id.as_deref())
    .bind(it.token_image_url.as_deref())
    .bind(it.category.as_deref())
    .bind(it.unit.as_deref())
    .bind(it.threshold)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    if it.quantity > 0 {
        apply_stock_change(
            &mut tx,
            id,
            StockChange::Delta(Decimal::from(it.quantity)),
            "receipt",
            None,
            None,
            None,
            actor,
            Some("opening balance"),
            None,
        )
        .await
        .map_err(|(_, m)| m)?;
    }

    tx.commit().await.map_err(|e| e.to_string())?;

    inventory_logic::get_inventory_by_id(db, id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "created item could not be read back".to_string())
}

/// Create a new inventory item
pub async fn create_inventory(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewInventory>,
) -> Result<Json<Inventory>, StatusCode> {
    let owner = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;
    insert_item_with_opening(&state.db, owner, Some(owner), &payload)
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
            let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            // The quantity moves only through the ledger, so the edit leaves a
            // movement behind just like a stock change does. Everything else is
            // metadata and is written directly.
            apply_stock_change(
                &mut tx,
                id,
                StockChange::SetLevel(Decimal::from(payload.quantity)),
                "adjustment",
                None,
                None,
                None,
                Some(user_id),
                Some("item edit"),
                None,
            )
            .await
            .map_err(|(code, _)| code)?;
            sqlx::query(
                "UPDATE inventory SET name=$2, description=$3, \
                 category=COALESCE($4, category), unit=COALESCE($5, unit), \
                 threshold=COALESCE($6, threshold) WHERE id=$1",
            )
            .bind(id)
            .bind(&payload.name)
            .bind(payload.description.as_deref())
            .bind(payload.category.as_deref())
            .bind(payload.unit.as_deref())
            .bind(payload.threshold)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let updated = inventory_logic::get_inventory_by_id(&state.db, id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;
            Ok(Json(updated))
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
            let mut tx = state.db.begin().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            // Setting a level is a ledger event like any other: the difference
            // from the current level is recorded as an 'adjustment' movement, so
            // the level never changes without an event behind it.
            apply_stock_change(
                &mut tx,
                id,
                StockChange::SetLevel(Decimal::from(payload.new_quantity)),
                "adjustment",
                None,
                None,
                None,
                Some(user_id),
                Some("manual quantity set"),
                None,
            )
            .await
            .map_err(|(code, _)| code)?;
            tx.commit().await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            let updated = inventory_logic::get_inventory_by_id(&state.db, id)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
                .ok_or(StatusCode::NOT_FOUND)?;
            Ok(Json(updated))
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
        match insert_item_with_opening(&state.db, user_id, Some(user_id), &item).await {
            Ok(_) => inserted += 1,
            Err(err) => {
                errors.insert(item.name.clone(), err);
            }
        }
    }

    Ok(Json(BulkImportResponse { inserted, errors }))
}

