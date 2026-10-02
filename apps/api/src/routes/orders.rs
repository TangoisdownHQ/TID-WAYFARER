//! 🛒 SupplyLink — orders, bids, fulfillments.
//!
//! Marketplace flow:
//!   1. Requester POSTs /orders to broadcast a need.
//!   2. Anyone with `submit a bid` permission POSTs /orders/:id/bids.
//!   3. Requester POSTs /orders/:id/bids/:bid_id/accept → creates a fulfillment.
//!   4. Shipper marks fulfillment shipped → delivered.
//!   5. Either party records the settlement tx → status=settled.
//!
//! All endpoints require AuthenticatedUser. Endpoints that mutate an order or
//! bid check ownership explicitly.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::services::settlement;
use crate::services::org_scope;
use crate::routes::auth_middleware::{AuthenticatedUser, Caller};
use crate::AppState;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Order {
    pub id:                 Uuid,
    pub requester_id:       Uuid,
    pub requester_outpost:  Option<Uuid>,
    pub description:        String,
    pub item_kind:          String,
    pub target_part_number: Option<String>,
    pub target_meta:        Option<serde_json::Value>,
    /// NUMERIC in Postgres. Decoding it as f64 fails at runtime — sqlx has no
    /// NUMERIC -> f64 decode — which made every query returning an Order 500.
    #[serde(with = "rust_decimal::serde::float")]
    pub quantity:           Decimal,
    pub unit:               String,
    pub needed_by:          Option<DateTime<Utc>>,
    pub delivery_body_id:   Option<i32>,
    pub delivery_lat:       Option<f64>,
    pub delivery_lon:       Option<f64>,
    pub delivery_alt:       Option<f64>,
    pub delivery_address:   Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub max_price:          Option<Decimal>,
    pub status:             String,
    pub accepted_bid_id:    Option<Uuid>,
    /// What this consignment weighs and displaces. Nullable: unknown is not
    /// zero, and a capsule refuses to budget cargo it cannot measure.
    #[serde(with = "rust_decimal::serde::float_option")]
    pub mass_kg:            Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub volume_m3:          Option<Decimal>,
    pub created_at:         DateTime<Utc>,
    pub updated_at:         DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewOrder {
    pub description:        String,
    pub item_kind:          String,
    pub target_part_number: Option<String>,
    pub target_meta:        Option<serde_json::Value>,
    pub quantity:           Option<f64>,
    pub unit:               Option<String>,
    pub needed_by:          Option<DateTime<Utc>>,
    pub delivery_body_id:   Option<i32>,
    pub delivery_lat:       Option<f64>,
    pub delivery_lon:       Option<f64>,
    pub delivery_alt:       Option<f64>,
    pub delivery_address:   Option<String>,
    pub max_price:          Option<f64>,
    pub requester_outpost:  Option<Uuid>,
    pub mass_kg:            Option<f64>,
    pub volume_m3:          Option<f64>,
}

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Bid {
    pub id:             Uuid,
    pub order_id:       Uuid,
    pub bidder_id:      Uuid,
    pub bidder_outpost: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::float")]
    pub price:          Decimal,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub transit_days:   Option<Decimal>,
    pub notes:          Option<String>,
    pub status:         String,
    pub created_at:     DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewBid {
    pub price:          f64,
    pub transit_days:   Option<f64>,
    pub notes:          Option<String>,
    pub bidder_outpost: Option<Uuid>,
}

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Fulfillment {
    pub id:            Uuid,
    pub order_id:      Uuid,
    pub bid_id:        Uuid,
    pub shipper_id:    Uuid,
    pub asset_id:      Option<Uuid>,
    pub package_id:    Option<Uuid>,
    pub status:        String,
    pub shipped_at:    Option<DateTime<Utc>>,
    pub delivered_at:  Option<DateTime<Utc>>,
    pub settled_at:    Option<DateTime<Utc>>,
    pub settlement_tx: Option<String>,
    /// pending | verified | rejected | unverifiable | skipped. Distinct from
    /// `status`: a delivery can be complete while the payment is not proven.
    pub settlement_status: Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub settlement_amount: Option<Decimal>,
    pub settlement_error:  Option<String>,
    pub notes:         Option<String>,
    pub created_at:    DateTime<Utc>,
    pub updated_at:    DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct OrderFilter {
    pub status:           Option<String>,
    pub mine:             Option<bool>,   // only orders I requested
    pub delivery_body_id: Option<i32>,
    pub part_number:      Option<String>,
}

#[derive(Serialize)]
pub struct OrderWithBids {
    pub order: Order,
    pub bids:  Vec<Bid>,
}

#[derive(Deserialize)]
pub struct SettleRequest {
    pub settlement_tx: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_uid(s: &str) -> Result<Uuid, StatusCode> {
    Uuid::parse_str(s).map_err(|_| StatusCode::UNAUTHORIZED)
}

async fn fetch_order(pool: &PgPool, id: Uuid) -> Result<Option<Order>, StatusCode> {
    sqlx::query_as::<_, Order>("SELECT * FROM orders WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn fetch_bid(pool: &PgPool, id: Uuid) -> Result<Option<Bid>, StatusCode> {
    sqlx::query_as::<_, Bid>("SELECT * FROM bids WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn fetch_fulfillment(pool: &PgPool, id: Uuid) -> Result<Option<Fulfillment>, StatusCode> {
    sqlx::query_as::<_, Fulfillment>("SELECT * FROM fulfillments WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn order_routes() -> Router<AppState> {
    Router::new()
        .route("/",                              get(list_orders).post(create_order))
        .route("/:id",                           get(get_order).put(update_order).delete(cancel_order))
        .route("/:id/bids",                      get(list_bids).post(submit_bid))
        .route("/:id/bids/:bid_id/accept",       post(accept_bid))
}

pub fn fulfillment_routes() -> Router<AppState> {
    Router::new()
        .route("/by-order/:order_id", get(get_fulfillment_by_order))
        .route("/:id",          get(get_fulfillment))
        .route("/:id/ship",     post(mark_shipped))
        .route("/:id/deliver",  post(mark_delivered))
        .route("/:id/settle",   post(mark_settled))
}

/// The delivery for an order, if a bid has been accepted.
pub async fn get_fulfillment_by_order(
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Fulfillment>, StatusCode> {
    sqlx::query_as::<_, Fulfillment>("SELECT * FROM fulfillments WHERE order_id = $1")
        .bind(order_id)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

// ───────── Orders ─────────

pub async fn list_orders(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Query(f): Query<OrderFilter>,
) -> Result<Json<Vec<Order>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let orders = sqlx::query_as::<_, Order>(
        r#"
        SELECT * FROM orders
        WHERE ($1::text IS NULL OR status              = $1)
          AND ($2::int  IS NULL OR delivery_body_id    = $2)
          AND ($3::text IS NULL OR target_part_number  = $3)
          AND (NOT COALESCE($4, false) OR requester_id = $5)
        ORDER BY
          CASE status WHEN 'posted' THEN 0 WHEN 'bid' THEN 1 ELSE 2 END,
          needed_by NULLS LAST,
          created_at DESC
        "#,
    )
    .bind(f.status)
    .bind(f.delivery_body_id)
    .bind(f.part_number)
    .bind(f.mine)
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(orders))
}

pub async fn create_order(
    AuthenticatedUser(user): AuthenticatedUser,
    Caller(principal): Caller,
    State(state): State<AppState>,
    Json(payload): Json<NewOrder>,
) -> Result<Json<Order>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    // Stamped at creation. A row with no org is invisible to every caller,
    // including the person who just made it, so this must not be skipped.
    let org = org_scope::caller_org(&state, &principal).await;

    let order = sqlx::query_as::<_, Order>(
        r#"
        INSERT INTO orders (
            requester_id, requester_outpost, description, item_kind,
            target_part_number, target_meta, quantity, unit, needed_by,
            delivery_body_id, delivery_lat, delivery_lon, delivery_alt,
            delivery_address, max_price, mass_kg, volume_m3, org_id
        ) VALUES (
            $1, $2, $3, $4,
            $5, COALESCE($6, '{}'::jsonb), COALESCE($7, 1), COALESCE($8, 'each'), $9,
            $10, $11, $12, $13,
            $14, $15, $16, $17, $18
        )
        RETURNING *
        "#,
    )
    .bind(user_id)
    .bind(payload.requester_outpost)
    .bind(&payload.description)
    .bind(&payload.item_kind)
    .bind(&payload.target_part_number)
    .bind(&payload.target_meta)
    .bind(payload.quantity)
    .bind(&payload.unit)
    .bind(payload.needed_by)
    .bind(payload.delivery_body_id)
    .bind(payload.delivery_lat)
    .bind(payload.delivery_lon)
    .bind(payload.delivery_alt)
    .bind(&payload.delivery_address)
    .bind(payload.max_price)
    .bind(payload.mass_kg.and_then(|v| rust_decimal::Decimal::try_from(v).ok()))
    .bind(payload.volume_m3.and_then(|v| rust_decimal::Decimal::try_from(v).ok()))
    .bind(org)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(order))
}

pub async fn get_order(
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<OrderWithBids>, StatusCode> {
    let order = fetch_order(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;

    let bids = sqlx::query_as::<_, Bid>(
        "SELECT * FROM bids WHERE order_id = $1 ORDER BY price ASC, created_at ASC",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Ok(Json(OrderWithBids { order, bids }))
}

pub async fn update_order(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewOrder>,
) -> Result<Json<Order>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    let existing = fetch_order(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if existing.requester_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    if existing.status != "posted" {
        return Err(StatusCode::CONFLICT);   // can't edit after bids accepted
    }

    let order = sqlx::query_as::<_, Order>(
        r#"
        UPDATE orders SET
            description        = $2,
            item_kind          = $3,
            target_part_number = $4,
            target_meta        = COALESCE($5, target_meta),
            quantity           = COALESCE($6, quantity),
            unit               = COALESCE($7, unit),
            needed_by          = $8,
            delivery_body_id   = $9,
            delivery_lat       = $10,
            delivery_lon       = $11,
            delivery_alt       = $12,
            delivery_address   = $13,
            max_price          = $14,
            updated_at         = NOW()
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(&payload.description)
    .bind(&payload.item_kind)
    .bind(&payload.target_part_number)
    .bind(&payload.target_meta)
    .bind(payload.quantity)
    .bind(&payload.unit)
    .bind(payload.needed_by)
    .bind(payload.delivery_body_id)
    .bind(payload.delivery_lat)
    .bind(payload.delivery_lon)
    .bind(payload.delivery_alt)
    .bind(&payload.delivery_address)
    .bind(payload.max_price)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(order))
}

pub async fn cancel_order(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    let existing = fetch_order(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if existing.requester_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    if existing.status == "settled" {
        return Err(StatusCode::CONFLICT);
    }

    sqlx::query("UPDATE orders SET status = 'cancelled', updated_at = NOW() WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json("order cancelled"))
}

// ───────── Bids ─────────

pub async fn list_bids(
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Bid>>, StatusCode> {
    let bids = sqlx::query_as::<_, Bid>(
        "SELECT * FROM bids WHERE order_id = $1 ORDER BY price ASC, created_at ASC",
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(bids))
}

pub async fn submit_bid(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewBid>,
) -> Result<Json<Bid>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    let order = fetch_order(&state.db, order_id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if order.status != "posted" && order.status != "bid" {
        return Err(StatusCode::CONFLICT);   // bidding closed
    }
    if order.requester_id == user_id {
        return Err(StatusCode::FORBIDDEN);  // can't bid on your own order
    }

    let bid = sqlx::query_as::<_, Bid>(
        r#"
        INSERT INTO bids (order_id, bidder_id, bidder_outpost, price, transit_days, notes)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING *
        "#,
    )
    .bind(order_id)
    .bind(user_id)
    .bind(payload.bidder_outpost)
    .bind(payload.price)
    .bind(payload.transit_days)
    .bind(&payload.notes)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // bump order status posted → bid
    let _ = sqlx::query(
        "UPDATE orders SET status = 'bid', updated_at = NOW() WHERE id = $1 AND status = 'posted'",
    )
    .bind(order_id)
    .execute(&state.db)
    .await;

    Ok(Json(bid))
}

pub async fn accept_bid(
    AuthenticatedUser(user): AuthenticatedUser,
    Path((order_id, bid_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
) -> Result<Json<Fulfillment>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let order = fetch_order(&state.db, order_id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if order.requester_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    if order.status != "posted" && order.status != "bid" {
        return Err(StatusCode::CONFLICT);
    }

    let bid = fetch_bid(&state.db, bid_id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if bid.order_id != order_id {
        return Err(StatusCode::NOT_FOUND);
    }

    // Transaction so the three writes are atomic.
    let mut tx = state.db.begin().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    sqlx::query("UPDATE bids SET status = 'accepted' WHERE id = $1")
        .bind(bid_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    sqlx::query("UPDATE bids SET status = 'rejected' WHERE order_id = $1 AND id <> $2 AND status = 'submitted'")
        .bind(order_id)
        .bind(bid_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    sqlx::query(
        "UPDATE orders SET status = 'accepted', accepted_bid_id = $2, updated_at = NOW() WHERE id = $1",
    )
    .bind(order_id)
    .bind(bid_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let fulfillment = sqlx::query_as::<_, Fulfillment>(
        r#"
        INSERT INTO fulfillments (order_id, bid_id, shipper_id)
        VALUES ($1, $2, $3)
        RETURNING *
        "#,
    )
    .bind(order_id)
    .bind(bid_id)
    .bind(bid.bidder_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tx.commit().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(fulfillment))
}

// ───────── Fulfillments ─────────

pub async fn get_fulfillment(
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Fulfillment>, StatusCode> {
    let f = fetch_fulfillment(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(f))
}

pub async fn mark_shipped(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Fulfillment>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    let f = fetch_fulfillment(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if f.shipper_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    if f.status != "preparing" {
        return Err(StatusCode::CONFLICT);
    }

    let mut tx = state.db.begin().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let updated = sqlx::query_as::<_, Fulfillment>(
        "UPDATE fulfillments SET status='in_transit', shipped_at=NOW(), updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    sqlx::query("UPDATE orders SET status='shipped', updated_at=NOW() WHERE id=$1")
        .bind(f.order_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tx.commit().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(updated))
}

pub async fn mark_delivered(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Fulfillment>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    let f = fetch_fulfillment(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;

    // Either the requester confirming receipt OR the shipper marking delivered.
    let order = fetch_order(&state.db, f.order_id).await?.ok_or(StatusCode::NOT_FOUND)?;
    if order.requester_id != user_id && f.shipper_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    if f.status != "in_transit" {
        return Err(StatusCode::CONFLICT);
    }

    let mut tx = state.db.begin().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let updated = sqlx::query_as::<_, Fulfillment>(
        "UPDATE fulfillments SET status='delivered', delivered_at=NOW(), updated_at=NOW() WHERE id=$1 RETURNING *",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    sqlx::query("UPDATE orders SET status='delivered', updated_at=NOW() WHERE id=$1")
        .bind(f.order_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tx.commit().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(updated))
}

pub async fn mark_settled(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<SettleRequest>,
) -> Result<Json<Fulfillment>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    let f = fetch_fulfillment(&state.db, id).await?.ok_or(StatusCode::NOT_FOUND)?;
    let order = fetch_order(&state.db, f.order_id).await?.ok_or(StatusCode::NOT_FOUND)?;

    // Either party can record settlement once tx is on-chain.
    if order.requester_id != user_id && f.shipper_id != user_id {
        return Err(StatusCode::FORBIDDEN);
    }
    if f.status != "delivered" {
        return Err(StatusCode::CONFLICT);
    }
    // Shape check first — it needs no network, so a disconnected outpost can
    // still reject nonsense. This is what stops "abc" settling an order.
    if !settlement::is_plausible_solana_signature(&payload.settlement_tx) {
        tracing::warn!(
            fulfillment_id = %id,
            "settlement rejected: not a well-formed transaction signature"
        );
        return Err(StatusCode::BAD_REQUEST);
    }

    // What the chain will be checked against: the accepted bid's price, paid
    // to the shipper's wallet.
    let expectation = sqlx::query(
        r#"
        SELECT b.price, u.wallet_address
        FROM fulfillments f
        JOIN bids b  ON b.id = f.bid_id
        JOIN users u ON u.id = f.shipper_id
        WHERE f.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let (amount, payee): (Option<rust_decimal::Decimal>, Option<String>) = match expectation {
        Some(row) => (row.get("price"), row.get("wallet_address")),
        None => (None, None),
    };

    let disabled = matches!(settlement::verify_mode(), settlement::VerifyMode::Disabled);

    // Refuse a settlement that could never be confirmed. Verification needs a
    // payee to have been paid and a price to compare against; without them the
    // verifier can only report the transaction *exists*, which is not the same
    // as someone having been paid. Better to refuse here — where the caller
    // can still fix it by setting a payout wallet — than to accept the record
    // and file it as unverifiable later.
    //
    // Skipped when verification is off: that deployment has already opted out,
    // and the record is stamped 'skipped' so it never masquerades as verified.
    if !disabled && (payee.is_none() || amount.is_none()) {
        tracing::warn!(
            fulfillment_id = %id,
            has_payee = payee.is_some(),
            has_amount = amount.is_some(),
            "settlement refused: nothing to verify the payment against"
        );
        return Err(StatusCode::PRECONDITION_FAILED);
    }

    // Recorded, not asserted. The verifier daemon promotes this to 'settled'
    // once the chain confirms it; until then the money has not demonstrably
    // moved, so neither status claims it has. Under SETTLEMENT_VERIFY=off the
    // settlement completes immediately but is stamped 'skipped', never
    // 'verified' — the record stays honest about what was actually checked.
    let (fulfillment_status, settlement_status) = if disabled {
        ("settled", "skipped")
    } else {
        ("settling", "pending")
    };

    let mut tx = state.db.begin().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let updated = sqlx::query_as::<_, Fulfillment>(
        r#"
        UPDATE fulfillments
        SET status=$3, settlement_tx=$2, settlement_status=$4,
            settlement_amount=$5, settlement_payee=$6, settlement_chain='solana',
            settlement_attempts=0, settlement_next_try_at=NOW(),
            settled_at = CASE WHEN $3 = 'settled' THEN NOW() ELSE settled_at END,
            updated_at=NOW()
        WHERE id=$1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(payload.settlement_tx.trim())
    .bind(fulfillment_status)
    .bind(settlement_status)
    .bind(amount)
    .bind(&payee)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        // The unique index on settlement_tx turns a replayed payment — one tx
        // used to settle several fulfillments — into a clean 409.
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return StatusCode::CONFLICT;
            }
        }
        tracing::error!(error = %e, "could not record settlement");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    sqlx::query("UPDATE orders SET status=$2, updated_at=NOW() WHERE id=$1")
        .bind(f.order_id)
        .bind(if disabled { "settled" } else { "settling" })
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tx.commit().await.map_err(|e| {
            tracing::error!(error = %e, "supplylink query failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    tracing::info!(fulfillment_id = %id, settlement_status, "settlement recorded");
    Ok(Json(updated))
}
