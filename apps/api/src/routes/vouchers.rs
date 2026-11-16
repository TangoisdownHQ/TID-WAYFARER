//! 🎟️ Offline settlement — Conditional Transfer Vouchers.
//!
//! Endpoints over the `services::voucher` primitive: commit a CTV (close a
//! trade locally, settle later), present/list vouchers, and discharge one once
//! its delivery condition is met. IOU mode only in this build — `escrow_ref`
//! is always NULL; on-chain escrow backing is a later step.
//!
//! The payer is always taken from the *verified* principal (the authenticated
//! user on this outpost, signing with this outpost's node identity), never from
//! the request body: a voucher endpoint that trusted a body-supplied payer
//! would let any fabric member sign trades as anyone else.
//!
//! Delivery discharge reuses `services::custody` — the order's signed receipt
//! chain is the evidence, not a button click.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
use crate::services::voucher::{self, Condition};
use crate::services::{custody, settlement};
use crate::AppState;

/// Default settlement horizon when the caller names none. A real deployment
/// sets this per route (minutes for terrestrial, weeks for Mars); 30 days is a
/// safe generic default that still expires.
const DEFAULT_HORIZON_DAYS: i64 = 30;

#[derive(Serialize, sqlx::FromRow)]
pub struct Voucher {
    pub id: Uuid,
    pub order_id: Uuid,
    pub fulfillment_id: Option<Uuid>,
    pub payer_node_id: Uuid,
    pub payer_user_id: Uuid,
    pub payee_wallet: String,
    #[serde(with = "rust_decimal::serde::float")]
    pub amount: Decimal,
    pub mint: String,
    pub escrow_ref: Option<String>,
    pub conditions: sqlx::types::Json<Vec<Condition>>,
    pub nonce: i64,
    pub anchor_by: DateTime<Utc>,
    pub payer_sig: String,
    pub state: String,
    pub anchor_tx: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct CommitRequest {
    pub order_id: Uuid,
    /// Which token the promise is denominated in. Defaults to `TIDAT_MINT`, or
    /// the literal "TIDAT" when no mint is configured (an IOU names the unit
    /// even where no mint is deployed).
    pub mint: Option<String>,
    /// The settlement horizon. Defaults to 30 days out.
    pub anchor_by: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub order_id: Uuid,
}

pub fn voucher_routes() -> Router<AppState> {
    Router::new()
        .route("/", post(commit_voucher).get(list_vouchers))
        .route("/:id", get(present_voucher))
        .route("/:id/discharge", post(discharge_voucher))
}

fn parse_uid(s: &str) -> Result<Uuid, StatusCode> {
    Uuid::parse_str(s).map_err(|_| StatusCode::UNAUTHORIZED)
}

fn ise<E: std::fmt::Display>(e: E) -> StatusCode {
    tracing::error!(error = %e, "voucher query failed");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// Commit a voucher: the payer (= the order's requester) promises to pay the
/// shipper. The trade is locally final on return; settlement follows later.
pub async fn commit_voucher(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(req): Json<CommitRequest>,
) -> Result<Json<Voucher>, StatusCode> {
    let payer_user = parse_uid(&user.sub)?;

    // The payer must be the order's requester — the party that owes payment.
    let order = sqlx::query("SELECT requester_id FROM orders WHERE id=$1")
        .bind(req.order_id)
        .fetch_optional(&state.db)
        .await
        .map_err(ise)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if order.get::<Uuid, _>("requester_id") != payer_user {
        return Err(StatusCode::FORBIDDEN);
    }

    // The fulfilment being paid for, with the bid price and the shipper's payee
    // wallet (organisation wallet first, then the shipper's own — the same
    // resolution the on-chain settlement path uses).
    let exp = sqlx::query(
        r#"
        SELECT f.id AS fulfillment_id, b.price,
               u.wallet_address AS user_wallet,
               o.wallet_address AS org_wallet
        FROM fulfillments f
        JOIN bids  b ON b.id = f.bid_id
        JOIN users u ON u.id = f.shipper_id
        LEFT JOIN org_members  m ON m.user_id = f.shipper_id
        LEFT JOIN organisations o ON o.id = m.org_id
        WHERE f.order_id = $1
        ORDER BY m.added_at ASC NULLS LAST
        LIMIT 1
        "#,
    )
    .bind(req.order_id)
    .fetch_optional(&state.db)
    .await
    .map_err(ise)?
    .ok_or(StatusCode::CONFLICT)?; // nothing accepted to pay for yet

    let fulfillment_id: Uuid = exp.get("fulfillment_id");
    let amount: Option<Decimal> = exp.get("price");
    let payee = settlement::resolve_payee(exp.get("org_wallet"), exp.get("user_wallet"));

    let (Some(amount), Some(payee)) = (amount, payee) else {
        // No price to promise, or nobody to pay. Refuse here where the caller
        // can still set a payout wallet, rather than minting a useless voucher.
        return Err(StatusCode::PRECONDITION_FAILED);
    };

    let mint = req
        .mint
        .filter(|m| !m.trim().is_empty())
        .or_else(|| settlement::SettlementRail::Tidat.expected_mint())
        .unwrap_or_else(|| "TIDAT".to_string());
    let anchor_by = req
        .anchor_by
        .unwrap_or_else(|| Utc::now() + Duration::days(DEFAULT_HORIZON_DAYS));

    let payer_node = parse_uid(&state.identity.node_id)?;

    // Next nonce for this payer's IOU series (NULL escrow_ref groups as '').
    let highest: Option<i64> = sqlx::query_scalar(
        "SELECT MAX(nonce) FROM transfer_vouchers \
         WHERE payer_node_id=$1 AND COALESCE(escrow_ref,'')=''",
    )
    .bind(payer_node)
    .fetch_one(&state.db)
    .await
    .map_err(ise)?;
    let nonce = voucher::next_nonce(highest.map(|h| h as u64));

    let id = Uuid::new_v4();
    let conditions = vec![Condition::DeliveryReceipt, Condition::NotAfter(anchor_by.timestamp())];

    // Bind the exact strings that go into the signature so the temporaries
    // outlive the claim.
    let amount_str = amount.to_string();
    let anchor_str = anchor_by.timestamp().to_string();
    let claim = voucher::VoucherClaim {
        id,
        order_id: req.order_id,
        fulfillment_id: Some(fulfillment_id),
        payer_node_id: payer_node,
        payer_user_id: payer_user,
        payee_wallet: &payee,
        amount: &amount_str,
        mint: &mint,
        escrow_ref: None,
        conditions: &conditions,
        nonce,
        anchor_by: &anchor_str,
    };
    let sig = voucher::sign_voucher(&state.identity, &claim);

    let mut tx = state.db.begin().await.map_err(ise)?;
    let v = sqlx::query_as::<_, Voucher>(
        r#"
        INSERT INTO transfer_vouchers
          (id, order_id, fulfillment_id, payer_node_id, payer_user_id, payee_wallet,
           amount, mint, escrow_ref, conditions, nonce, anchor_by, payer_sig, state)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,NULL,$9,$10,$11,$12,'committed')
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(req.order_id)
    .bind(fulfillment_id)
    .bind(payer_node)
    .bind(payer_user)
    .bind(&payee)
    .bind(amount)
    .bind(&mint)
    .bind(sqlx::types::Json(&conditions))
    .bind(nonce as i64)
    .bind(anchor_by)
    .bind(&sig)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        // The unique nonce index turns a reused slot — the offline
        // double-spend signature — into a clean 409.
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return StatusCode::CONFLICT;
            }
        }
        tracing::error!(error = %e, "could not commit voucher");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    sqlx::query("UPDATE fulfillments SET ctv_id=$2, updated_at=NOW() WHERE id=$1")
        .bind(fulfillment_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(ise)?;

    tx.commit().await.map_err(ise)?;
    tracing::info!(voucher_id = %id, order_id = %req.order_id, nonce, "CTV committed (IOU)");
    Ok(Json(v))
}

/// Present one voucher. Restricted to the payer who committed it; the payee
/// holds their own out-of-band copy (delivered over DTN), so this never has to
/// expose a voucher across a commercial boundary.
pub async fn present_voucher(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Voucher>, StatusCode> {
    let uid = parse_uid(&user.sub)?;
    let v = sqlx::query_as::<_, Voucher>("SELECT * FROM transfer_vouchers WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(ise)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if v.payer_user_id != uid {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(v))
}

/// List the vouchers for an order. Restricted to the order's requester.
pub async fn list_vouchers(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<Voucher>>, StatusCode> {
    let uid = parse_uid(&user.sub)?;
    let order = sqlx::query("SELECT requester_id FROM orders WHERE id=$1")
        .bind(q.order_id)
        .fetch_optional(&state.db)
        .await
        .map_err(ise)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if order.get::<Uuid, _>("requester_id") != uid {
        return Err(StatusCode::FORBIDDEN);
    }
    let rows = sqlx::query_as::<_, Voucher>(
        "SELECT * FROM transfer_vouchers WHERE order_id=$1 ORDER BY nonce",
    )
    .bind(q.order_id)
    .fetch_all(&state.db)
    .await
    .map_err(ise)?;
    Ok(Json(rows))
}

/// Discharge a committed voucher: its conditions are met, so the payee may
/// claim. Delivery is judged from the order's signed custody chain
/// (`services::custody`), not asserted by the caller.
pub async fn discharge_voucher(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Voucher>, StatusCode> {
    let uid = parse_uid(&user.sub)?;
    let v = sqlx::query_as::<_, Voucher>("SELECT * FROM transfer_vouchers WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(ise)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Either party to the trade may trigger the check.
    let parties = sqlx::query(
        "SELECT o.requester_id, f.shipper_id \
         FROM orders o \
         LEFT JOIN fulfillments f ON f.order_id = o.id \
         WHERE o.id=$1 LIMIT 1",
    )
    .bind(v.order_id)
    .fetch_optional(&state.db)
    .await
    .map_err(ise)?
    .ok_or(StatusCode::NOT_FOUND)?;
    let requester: Uuid = parties.get("requester_id");
    let shipper: Option<Uuid> = parties.get("shipper_id");
    if uid != requester && Some(uid) != shipper {
        return Err(StatusCode::FORBIDDEN);
    }

    if v.state != "committed" {
        // Only a committed voucher discharges; anything else is already past
        // this point (or contested).
        return Err(StatusCode::CONFLICT);
    }

    // Delivery is discharged by a complete, continuous custody chain that
    // reached a final 'delivery' handoff.
    let legs_rows = sqlx::query(
        "SELECT seq, from_node_id, to_node_id, event FROM custody_receipts WHERE order_id=$1",
    )
    .bind(v.order_id)
    .fetch_all(&state.db)
    .await
    .map_err(ise)?;

    let mut legs: Vec<custody::Leg> = legs_rows
        .iter()
        .map(|r| custody::Leg {
            seq: r.get("seq"),
            from_node_id: r.get("from_node_id"),
            to_node_id: r.get("to_node_id"),
        })
        .collect();
    let has_delivery = legs_rows
        .iter()
        .any(|r| r.get::<String, _>("event") == "delivery");
    let faults = custody::check_chain(&mut legs);
    let delivery_discharged = !legs.is_empty() && faults.is_empty() && has_delivery;

    if !voucher::conditions_met(&v.conditions.0, Utc::now().timestamp(), delivery_discharged) {
        // Conditions not yet met — delivery incomplete, or past the horizon.
        return Err(StatusCode::PRECONDITION_FAILED);
    }

    let updated = sqlx::query_as::<_, Voucher>(
        "UPDATE transfer_vouchers SET state='discharged', updated_at=NOW() \
         WHERE id=$1 AND state='committed' RETURNING *",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(ise)?
    .ok_or(StatusCode::CONFLICT)?;

    tracing::info!(voucher_id = %id, "CTV discharged");
    Ok(Json(updated))
}
