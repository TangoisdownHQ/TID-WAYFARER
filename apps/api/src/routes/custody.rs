//! Chain of custody — who handed what to whom, signed at each leg.
//!
//! `fulfillments` records that a shipment happened. It does not record the
//! handoffs, which is what a customs officer, an insurer or an accident board
//! actually asks for, and what a dispute turns on.
//!
//! Signing rules live in [`crate::services::custody`]; this module is the
//! storage and the chain report. Receipts are signed by the **receiving**
//! party — the one making a claim about the world — and an unsigned receipt is
//! still stored, flagged `verified: false`, because a receipt from a party with
//! no key on file is weaker evidence than a signed one but far better than no
//! record at all. The distinction is reported, never hidden.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AuthenticatedUser, Caller, Principal};
use crate::services::custody::{self, ChainFault, Leg, ReceiptClaim};
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

pub fn custody_routes() -> Router<AppState> {
    Router::new()
        .route("/receipts", post(record_receipt))
        .route("/chain/:order_id", get(chain_for_order))
        .route("/canonical", post(canonical_preview))
}

const EVENTS: [&str; 5] = ["pickup", "transfer", "delivery", "return", "inspection"];
const CONDITIONS: [&str; 5] = ["ok", "damaged", "short", "contaminated", "unknown"];

#[derive(Deserialize)]
pub struct NewReceipt {
    pub order_id: Option<Uuid>,
    pub fulfillment_id: Option<Uuid>,
    pub lot_id: Option<Uuid>,
    pub capsule_id: Option<Uuid>,
    pub seq: i32,
    pub from_node_id: Option<Uuid>,
    /// Who took custody. Defaults to the calling node — a receipt is normally
    /// filed by whoever just received the goods.
    pub to_node_id: Option<Uuid>,
    pub from_label: Option<String>,
    pub to_label: Option<String>,
    pub event: Option<String>,
    pub quantity: Option<f64>,
    pub unit: Option<String>,
    pub item_hash: Option<String>,
    pub condition: Option<String>,
    pub notes: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub body_id: Option<i32>,
    /// Ed25519 over the canonical encoding, by the receiving node.
    pub signature: Option<String>,
}

/// POST /api/custody/receipts
async fn record_receipt(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Json(body): Json<NewReceipt>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.order_id.is_none() && body.fulfillment_id.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "a receipt must name an order or a fulfillment — one anchored to nothing \
             is not evidence of anything"
                .into(),
        ));
    }
    if body.seq < 0 {
        return Err((StatusCode::BAD_REQUEST, "seq cannot be negative".into()));
    }

    let event = body.event.as_deref().unwrap_or("transfer").to_string();
    if !EVENTS.contains(&event.as_str()) {
        return Err((StatusCode::BAD_REQUEST, format!("event must be one of {}", EVENTS.join(", "))));
    }
    let condition = body.condition.as_deref().unwrap_or("ok").to_string();
    if !CONDITIONS.contains(&condition.as_str()) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("condition must be one of {}", CONDITIONS.join(", ")),
        ));
    }

    // Who received. A signing node files as itself; anyone else must say.
    let to_node_id = match (&principal, body.to_node_id) {
        (Principal::Node(id), None) => *id,
        (Principal::Node(id), Some(claimed)) if claimed == *id => *id,
        (Principal::Node(id), Some(claimed)) => {
            // A node filing a receipt naming a *different* receiver is the
            // shape of forging someone else's handoff.
            tracing::warn!(signed_as = %id, claimed = %claimed, "custody receipt rejected: receiver mismatch");
            return Err((
                StatusCode::FORBIDDEN,
                "a node may only file a receipt for custody it took itself".into(),
            ));
        }
        (_, Some(id)) => id,
        (_, None) => {
            return Err((StatusCode::BAD_REQUEST, "to_node_id is required".into()))
        }
    };

    let occurred_at = body.occurred_at.unwrap_or_else(Utc::now);
    // Seconds precision, matching what the signer committed to. A millisecond
    // difference would silently invalidate every signature.
    let occurred_str = occurred_at.to_rfc3339_opts(SecondsFormat::Secs, true);
    let quantity = body.quantity.and_then(|q| Decimal::try_from(q).ok());
    let quantity_str = quantity.map(|q| q.to_string());

    // Verify against the receiving node's registered key, when there is one.
    let mut verified = false;
    if let Some(sig) = body.signature.as_deref() {
        let key: Option<String> =
            sqlx::query_scalar("SELECT public_key FROM node_registry WHERE node_id = $1")
                .bind(to_node_id)
                .fetch_optional(&state.db)
                .await
                .map_err(server_err("key lookup failed"))?;

        if let Some(pk) = key {
            let claim = ReceiptClaim {
                order_id: body.order_id,
                fulfillment_id: body.fulfillment_id,
                seq: body.seq,
                from_node_id: body.from_node_id,
                to_node_id,
                event: &event,
                item_hash: body.item_hash.as_deref(),
                quantity: quantity_str.as_deref(),
                unit: body.unit.as_deref(),
                condition: &condition,
                occurred_at: &occurred_str,
            };
            verified = custody::verify_receipt(&claim, sig, &pk);
            if !verified {
                tracing::warn!(to = %to_node_id, seq = body.seq, "custody receipt signature did not verify");
            }
        }
    }

    let row = sqlx::query(
        r#"
        INSERT INTO custody_receipts
          (order_id, fulfillment_id, lot_id, capsule_id, seq, from_node_id, to_node_id,
           from_label, to_label, event, quantity, unit, item_hash, condition, notes,
           occurred_at, lat, lon, body_id, signature, verified)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
        RETURNING id
        "#,
    )
    .bind(body.order_id).bind(body.fulfillment_id).bind(body.lot_id).bind(body.capsule_id)
    .bind(body.seq).bind(body.from_node_id).bind(to_node_id)
    .bind(&body.from_label).bind(&body.to_label).bind(&event)
    .bind(quantity).bind(&body.unit).bind(&body.item_hash).bind(&condition).bind(&body.notes)
    .bind(occurred_at).bind(body.lat).bind(body.lon).bind(body.body_id)
    .bind(&body.signature).bind(verified)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (
                    StatusCode::CONFLICT,
                    format!("a receipt already exists at position {} for this order", body.seq),
                );
            }
        }
        tracing::error!(error = %e, "custody receipt insert failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not record the receipt".into())
    })?;

    tracing::info!(
        receipt = %row.get::<Uuid, _>("id"), seq = body.seq, %to_node_id,
        event = %event, condition = %condition, verified,
        "custody receipt recorded"
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.get::<Uuid, _>("id"),
            "seq": body.seq,
            "toNodeId": to_node_id,
            "event": event,
            "condition": condition,
            // Reported plainly: an unverified receipt is still a record, and
            // pretending otherwise either discards evidence or overstates it.
            "verified": verified,
            "occurredAt": occurred_str,
        })),
    ))
}

/// GET /api/custody/chain/:order_id — the whole chain, and whether it holds.
async fn chain_for_order(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT r.*, n.name AS to_node_name
        FROM custody_receipts r
        LEFT JOIN node_registry n ON n.node_id = r.to_node_id
        WHERE r.order_id = $1
        ORDER BY r.seq
        "#,
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("custody chain lookup failed"))?;

    let mut legs: Vec<Leg> = rows
        .iter()
        .map(|r| Leg {
            seq: r.get("seq"),
            from_node_id: r.get("from_node_id"),
            to_node_id: r.get("to_node_id"),
        })
        .collect();
    let faults = custody::check_chain(&mut legs);

    let unverified = rows.iter().filter(|r| !r.get::<bool, _>("verified")).count();
    let damaged: Vec<String> = rows
        .iter()
        .filter(|r| r.get::<String, _>("condition") != "ok")
        .map(|r| format!("seq {}: {}", r.get::<i32, _>("seq"), r.get::<String, _>("condition")))
        .collect();

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "seq": r.get::<i32, _>("seq"),
                "event": r.get::<String, _>("event"),
                "fromNodeId": r.get::<Option<Uuid>, _>("from_node_id"),
                "toNodeId": r.get::<Uuid, _>("to_node_id"),
                "toNode": r.get::<Option<String>, _>("to_node_name"),
                "fromLabel": r.get::<Option<String>, _>("from_label"),
                "toLabel": r.get::<Option<String>, _>("to_label"),
                "quantity": r.get::<Option<Decimal>, _>("quantity").map(|q| q.to_string()),
                "unit": r.get::<Option<String>, _>("unit"),
                "condition": r.get::<String, _>("condition"),
                "itemHash": r.get::<Option<String>, _>("item_hash"),
                "occurredAt": r.get::<DateTime<Utc>, _>("occurred_at"),
                "verified": r.get::<bool, _>("verified"),
                "notes": r.get::<Option<String>, _>("notes"),
            })
        })
        .collect();

    Ok(Json(json!({
        "orderId": order_id,
        "legs": items.len(),
        // Two separate claims, deliberately not merged. A chain can be
        // structurally whole and still rest on receipts nobody signed.
        "intact": faults.is_empty(),
        "fullySigned": unverified == 0 && !items.is_empty(),
        "unverifiedLegs": unverified,
        "faults": faults.iter().map(describe_fault).collect::<Vec<_>>(),
        "conditionExceptions": damaged,
        "chain": items,
    })))
}

fn describe_fault(f: &ChainFault) -> Value {
    match f {
        ChainFault::MissingStart(s) => json!({
            "kind": "missing_start",
            "detail": format!("the chain begins at {s}; the first handoff is not recorded"),
        }),
        ChainFault::Gap { after, before } => json!({
            "kind": "gap",
            "detail": format!("no receipt between {after} and {before} — custody is unaccounted for"),
        }),
        ChainFault::Duplicate(s) => json!({
            "kind": "duplicate",
            "detail": format!("two receipts claim position {s}"),
        }),
        ChainFault::Discontinuous { seq, expected_from, actual_from } => json!({
            "kind": "discontinuous",
            "detail": format!(
                "leg {seq} departs from {} but {} was the last party to receive",
                actual_from.map(|u| u.to_string()).unwrap_or_else(|| "nobody".into()),
                expected_from
            ),
        }),
    }
}

/// POST /api/custody/canonical — the exact bytes to sign.
///
/// Exists so a carrier's device never has to reimplement the encoding, which is
/// where signature schemes usually break in practice. Signing happens on the
/// receiver's side; this only says what to sign.
async fn canonical_preview(
    State(_state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<NewReceipt>,
) -> Result<Json<Value>, ApiError> {
    let to = body
        .to_node_id
        .ok_or((StatusCode::BAD_REQUEST, "to_node_id is required".to_string()))?;
    let event = body.event.as_deref().unwrap_or("transfer").to_string();
    let condition = body.condition.as_deref().unwrap_or("ok").to_string();
    let occurred = body
        .occurred_at
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let qty = body
        .quantity
        .and_then(|q| Decimal::try_from(q).ok())
        .map(|q| q.to_string());

    let claim = ReceiptClaim {
        order_id: body.order_id,
        fulfillment_id: body.fulfillment_id,
        seq: body.seq,
        from_node_id: body.from_node_id,
        to_node_id: to,
        event: &event,
        item_hash: body.item_hash.as_deref(),
        quantity: qty.as_deref(),
        unit: body.unit.as_deref(),
        condition: &condition,
        occurred_at: &occurred,
    };

    Ok(Json(json!({
        "canonical": custody::canonical_receipt(&claim),
        "occurredAt": occurred,
        "note": "sign these exact bytes with the receiving node's Ed25519 key",
    })))
}
