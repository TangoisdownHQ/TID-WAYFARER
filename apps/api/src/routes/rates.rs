//! Rate cards, quotes, and bid comparison.
//!
//! A bid carried one number, and one number cannot answer the question an
//! operator is actually asking. The cheapest bid is routinely the wrong one:
//! it is slower, or it comes from a carrier that damaged the last three
//! consignments, or its price looks low until you account for what the
//! consignment weighs.
//!
//! The comparison endpoint deliberately **does not return a winner**. It
//! returns the three axes — cost, speed, demonstrated reliability — names
//! which bid leads each, and leaves the trade to the person placing the order.
//! A composite score would hide its own weighting, and the right weighting
//! depends on whether this is a spare bolt or the oxygen supply.
//!
//! Reliability is computed from what actually happened: delivered
//! fulfillments, lateness against the order's deadline, and condition
//! exceptions recorded on custody receipts. It always carries its sample size,
//! and says plainly when the sample is too small to mean anything — three
//! shipments is not a track record, and presenting it as one would be worse
//! than showing nothing.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

fn dec(v: f64) -> Option<Decimal> {
    Decimal::try_from(v).ok()
}

/// Below this many completed shipments, a reliability figure is noise. Stated
/// as a constant because the threshold is a judgement and should be visible.
pub const MIN_SAMPLE_FOR_RELIABILITY: i64 = 5;

pub fn rate_routes() -> Router<AppState> {
    Router::new()
        .route("/cards", get(list_cards).post(create_card))
        .route("/quote", post(quote))
        .route("/compare/:order_id", get(compare_bids))
}

// ---------------------------------------------------------------------------
// Rate cards
// ---------------------------------------------------------------------------

#[derive(Serialize, sqlx::FromRow)]
pub struct RateCard {
    pub id: Uuid,
    pub carrier_id: Option<Uuid>,
    pub carrier_outpost: Option<Uuid>,
    pub name: String,
    pub origin_body_id: Option<i32>,
    pub destination_body_id: Option<i32>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub price_per_kg: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub price_per_m3: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub minimum_charge: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub handling_fee: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub transit_days: Option<Decimal>,
    pub currency: String,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub active: bool,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewRateCard {
    pub name: String,
    pub origin_body_id: Option<i32>,
    pub destination_body_id: Option<i32>,
    pub price_per_kg: Option<f64>,
    pub price_per_m3: Option<f64>,
    pub minimum_charge: Option<f64>,
    pub handling_fee: Option<f64>,
    pub transit_days: Option<f64>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_to: Option<DateTime<Utc>>,
    pub notes: Option<String>,
}

async fn create_card(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Json(b): Json<NewRateCard>,
) -> Result<(StatusCode, Json<RateCard>), ApiError> {
    if b.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a rate card needs a name".into()));
    }
    if b.price_per_kg.is_none() && b.price_per_m3.is_none() && b.minimum_charge.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "give at least one of price_per_kg, price_per_m3 or minimum_charge — \
             a card that prices nothing cannot quote anything"
                .into(),
        ));
    }
    let carrier = Uuid::parse_str(&claims.sub).ok();

    let card = sqlx::query_as::<_, RateCard>(
        r#"
        INSERT INTO rate_cards
          (carrier_id, name, origin_body_id, destination_body_id, price_per_kg,
           price_per_m3, minimum_charge, handling_fee, transit_days, valid_from, valid_to, notes)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
        RETURNING *
        "#,
    )
    .bind(carrier)
    .bind(b.name.trim())
    .bind(b.origin_body_id)
    .bind(b.destination_body_id)
    .bind(b.price_per_kg.and_then(dec))
    .bind(b.price_per_m3.and_then(dec))
    .bind(b.minimum_charge.and_then(dec))
    .bind(b.handling_fee.and_then(dec))
    .bind(b.transit_days.and_then(dec))
    .bind(b.valid_from)
    .bind(b.valid_to)
    .bind(&b.notes)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("could not create the rate card"))?;

    Ok((StatusCode::CREATED, Json(card)))
}

#[derive(Deserialize)]
pub struct CardFilter {
    pub destination_body_id: Option<i32>,
    pub carrier_id: Option<Uuid>,
}

async fn list_cards(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(f): Query<CardFilter>,
) -> Result<Json<Vec<RateCard>>, ApiError> {
    sqlx::query_as::<_, RateCard>(
        r#"
        SELECT * FROM rate_cards
        WHERE active
          AND ($1::int  IS NULL OR destination_body_id IS NULL OR destination_body_id = $1)
          AND ($2::uuid IS NULL OR carrier_id = $2)
          AND (valid_from IS NULL OR valid_from <= NOW())
          AND (valid_to   IS NULL OR valid_to   >= NOW())
        ORDER BY name
        "#,
    )
    .bind(f.destination_body_id)
    .bind(f.carrier_id)
    .fetch_all(&state.db)
    .await
    .map(Json)
    .map_err(server_err("rate card list failed"))
}

// ---------------------------------------------------------------------------
// Quoting
// ---------------------------------------------------------------------------

/// What a consignment costs on a card.
///
/// Charges the **greater** of the mass and volume figures rather than their
/// sum. That is how freight actually prices: a hull sells both, and the one
/// you exhaust first is the one you pay for. Summing would double-charge, and
/// taking only mass would let bulky-but-light cargo ride almost free.
pub fn price_consignment(
    mass_kg: Option<Decimal>,
    volume_m3: Option<Decimal>,
    price_per_kg: Option<Decimal>,
    price_per_m3: Option<Decimal>,
    minimum_charge: Option<Decimal>,
    handling_fee: Option<Decimal>,
) -> (Decimal, Decimal, Decimal, &'static str) {
    let mass_charge = match (mass_kg, price_per_kg) {
        (Some(m), Some(r)) => m * r,
        _ => Decimal::ZERO,
    };
    let volume_charge = match (volume_m3, price_per_m3) {
        (Some(v), Some(r)) => v * r,
        _ => Decimal::ZERO,
    };

    let (dimensional, basis) = if volume_charge > mass_charge {
        (volume_charge, "volume")
    } else {
        (mass_charge, "mass")
    };

    let handling = handling_fee.unwrap_or(Decimal::ZERO);
    let subtotal = dimensional + handling;
    let minimum = minimum_charge.unwrap_or(Decimal::ZERO);

    let total = if subtotal < minimum { minimum } else { subtotal };
    let basis = if total == minimum && subtotal < minimum { "minimum" } else { basis };

    (total, mass_charge, volume_charge, basis)
}

#[derive(Deserialize)]
pub struct QuoteRequest {
    pub mass_kg: Option<f64>,
    pub volume_m3: Option<f64>,
    pub destination_body_id: Option<i32>,
    /// Quote one specific card instead of every applicable one.
    pub rate_card_id: Option<Uuid>,
}

/// POST /api/rates/quote — what would this cost, before anyone bids.
async fn quote(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(q): Json<QuoteRequest>,
) -> Result<Json<Value>, ApiError> {
    let mass = q.mass_kg.and_then(dec);
    let volume = q.volume_m3.and_then(dec);
    if mass.is_none() && volume.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "give a mass and/or a volume — there is nothing to price otherwise".into(),
        ));
    }

    let cards = sqlx::query_as::<_, RateCard>(
        r#"
        SELECT * FROM rate_cards
        WHERE active
          AND ($1::uuid IS NULL OR id = $1)
          AND ($2::int  IS NULL OR destination_body_id IS NULL OR destination_body_id = $2)
          AND (valid_from IS NULL OR valid_from <= NOW())
          AND (valid_to   IS NULL OR valid_to   >= NOW())
        "#,
    )
    .bind(q.rate_card_id)
    .bind(q.destination_body_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("quote lookup failed"))?;

    let mut quotes: Vec<Value> = cards
        .iter()
        .map(|c| {
            let (total, mass_charge, volume_charge, basis) = price_consignment(
                mass, volume, c.price_per_kg, c.price_per_m3, c.minimum_charge, c.handling_fee,
            );
            json!({
                "rateCardId": c.id,
                "name": c.name,
                "total": total.round_dp(2).to_string(),
                "currency": c.currency,
                "chargedOn": basis,
                "massCharge": mass_charge.round_dp(2).to_string(),
                "volumeCharge": volume_charge.round_dp(2).to_string(),
                "handlingFee": c.handling_fee.unwrap_or(Decimal::ZERO).to_string(),
                "transitDays": c.transit_days.map(|d| d.to_string()),
                "costPerKg": mass.filter(|m| !m.is_zero())
                    .map(|m| (total / m).round_dp(4).to_string()),
            })
        })
        .collect();

    quotes.sort_by(|a, b| {
        let pa: f64 = a["total"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        let pb: f64 = b["total"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
        pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Json(json!({
        "massKg": mass.map(|m| m.to_string()),
        "volumeM3": volume.map(|v| v.to_string()),
        "quotes": quotes,
    })))
}

// ---------------------------------------------------------------------------
// Bid comparison
// ---------------------------------------------------------------------------

/// A carrier's demonstrated performance, with the sample it rests on.
struct Reliability {
    completed: i64,
    on_time: i64,
    condition_exceptions: i64,
}

impl Reliability {
    fn to_json(&self) -> Value {
        let enough = self.completed >= MIN_SAMPLE_FOR_RELIABILITY;
        let pct = |n: i64| {
            if self.completed == 0 { None }
            else { Some(((n as f64 / self.completed as f64) * 1000.0).round() / 10.0) }
        };
        json!({
            "completedShipments": self.completed,
            "onTimeRate": if enough { json!(pct(self.on_time)) } else { Value::Null },
            "conditionExceptionRate": if enough { json!(pct(self.condition_exceptions)) } else { Value::Null },
            // Said out loud rather than implied by a null. Three shipments is
            // not a track record, and a percentage computed from it reads as
            // one.
            "sampleSufficient": enough,
            "note": if enough { Value::Null } else {
                json!(format!("only {} completed shipment(s) — too few to rate", self.completed))
            },
        })
    }
}

async fn reliability_for(state: &AppState, bidder: Uuid) -> Result<Reliability, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT
          COUNT(*) FILTER (WHERE f.status IN ('delivered','settled'))                     AS completed,
          COUNT(*) FILTER (WHERE f.status IN ('delivered','settled')
                             AND (o.needed_by IS NULL OR f.delivered_at <= o.needed_by))  AS on_time,
          COUNT(*) FILTER (WHERE EXISTS (
                SELECT 1 FROM custody_receipts cr
                WHERE cr.fulfillment_id = f.id AND cr.condition <> 'ok'))                 AS exceptions
        FROM fulfillments f
        JOIN orders o ON o.id = f.order_id
        WHERE f.shipper_id = $1
        "#,
    )
    .bind(bidder)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("reliability query failed"))?;

    Ok(Reliability {
        completed: row.get("completed"),
        on_time: row.get("on_time"),
        condition_exceptions: row.get("exceptions"),
    })
}

/// GET /api/rates/compare/:order_id
///
/// Every bid on an order, on the three axes that matter, with no winner
/// declared. `leaders` names which bid heads each axis; choosing between them
/// is the operator's call and depends on whether this is a spare bolt or the
/// oxygen supply.
async fn compare_bids(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let order = sqlx::query("SELECT description, mass_kg, volume_m3, needed_by, max_price FROM orders WHERE id = $1")
        .bind(order_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("order lookup failed"))?
        .ok_or((StatusCode::NOT_FOUND, "no such order".to_string()))?;

    let mass: Option<Decimal> = order.get("mass_kg");
    let max_price: Option<Decimal> = order.get("max_price");

    let rows = sqlx::query(
        r#"
        SELECT b.id, b.bidder_id, b.price, b.transit_days, b.notes, b.status,
               b.mass_charge, b.volume_charge, b.handling_fee, b.surcharges,
               u.username AS bidder_name
        FROM bids b
        LEFT JOIN users u ON u.id = b.bidder_id
        WHERE b.order_id = $1 AND b.status IN ('submitted','accepted')
        ORDER BY b.price
        "#,
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("bid lookup failed"))?;

    let mut bids = Vec::new();
    for r in &rows {
        let bidder: Uuid = r.get("bidder_id");
        let price: Decimal = r.get("price");
        let transit: Option<Decimal> = r.get("transit_days");
        let rel = reliability_for(&state, bidder).await?;

        bids.push(json!({
            "bidId": r.get::<Uuid, _>("id"),
            "bidder": r.get::<Option<String>, _>("bidder_name"),
            "bidderId": bidder,
            "price": price.to_string(),
            "transitDays": transit.map(|d| d.to_string()),
            // The number that makes bids comparable when consignments differ.
            "costPerKg": mass.filter(|m| !m.is_zero())
                .map(|m| (price / m).round_dp(4).to_string()),
            "withinCeiling": max_price.map(|c| price <= c),
            "breakdown": {
                "massCharge": r.get::<Option<Decimal>, _>("mass_charge").map(|v| v.to_string()),
                "volumeCharge": r.get::<Option<Decimal>, _>("volume_charge").map(|v| v.to_string()),
                "handlingFee": r.get::<Option<Decimal>, _>("handling_fee").map(|v| v.to_string()),
                "surcharges": r.get::<Value, _>("surcharges"),
            },
            "reliability": rel.to_json(),
            "notes": r.get::<Option<String>, _>("notes"),
            "status": r.get::<String, _>("status"),
        }));
    }

    // Which bid leads each axis. Not a recommendation — three separate facts.
    let lead = |key: &str, lower_is_better: bool| -> Value {
        bids.iter()
            .filter(|b| !b[key].is_null())
            .min_by(|a, b| {
                let pa: f64 = a[key].as_str().and_then(|s| s.parse().ok()).unwrap_or(f64::MAX);
                let pb: f64 = b[key].as_str().and_then(|s| s.parse().ok()).unwrap_or(f64::MAX);
                if lower_is_better { pa.partial_cmp(&pb).unwrap() } else { pb.partial_cmp(&pa).unwrap() }
            })
            .map(|b| json!({ "bidId": b["bidId"], "bidder": b["bidder"], "value": b[key] }))
            .unwrap_or(Value::Null)
    };

    // Most reliable only among carriers with a real sample; otherwise null,
    // rather than crowning whoever happens to have one clean delivery.
    let most_reliable = bids
        .iter()
        .filter(|b| b["reliability"]["sampleSufficient"] == json!(true))
        .max_by(|a, b| {
            let ra = a["reliability"]["onTimeRate"].as_f64().unwrap_or(0.0);
            let rb = b["reliability"]["onTimeRate"].as_f64().unwrap_or(0.0);
            ra.partial_cmp(&rb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|b| json!({
            "bidId": b["bidId"], "bidder": b["bidder"],
            "onTimeRate": b["reliability"]["onTimeRate"],
        }))
        .unwrap_or(Value::Null);

    Ok(Json(json!({
        "orderId": order_id,
        "description": order.get::<String, _>("description"),
        "massKg": mass.map(|m| m.to_string()),
        "priceCeiling": max_price.map(|c| c.to_string()),
        "bidCount": bids.len(),
        // Three facts, not a verdict. The weighting between them belongs to
        // whoever is placing the order.
        "leaders": {
            "cheapest": lead("price", true),
            "fastest": lead("transitDays", true),
            "mostReliable": most_reliable,
        },
        "bids": bids,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(v: &str) -> Decimal {
        Decimal::from_str(v).unwrap()
    }

    #[test]
    fn the_greater_of_mass_and_volume_is_charged_not_the_sum() {
        // A hull sells both; you pay for whichever you exhaust first. Summing
        // double-charges the same shipment.
        let (total, m, v, basis) =
            price_consignment(Some(d("100")), Some(d("2")), Some(d("10")), Some(d("50")), None, None);
        assert_eq!(m, d("1000"));
        assert_eq!(v, d("100"));
        assert_eq!(total, d("1000"));
        assert_eq!(basis, "mass");
    }

    #[test]
    fn bulky_light_cargo_is_charged_on_volume() {
        // The case that makes mass-only pricing wrong: 40 kg of insulation
        // panels filling six cubic metres would otherwise ride almost free.
        let (total, _, _, basis) =
            price_consignment(Some(d("40")), Some(d("6")), Some(d("10")), Some(d("200")), None, None);
        assert_eq!(total, d("1200"));
        assert_eq!(basis, "volume");
    }

    #[test]
    fn a_minimum_charge_floors_a_tiny_consignment() {
        let (total, _, _, basis) =
            price_consignment(Some(d("0.5")), None, Some(d("10")), None, Some(d("250")), None);
        assert_eq!(total, d("250"));
        assert_eq!(basis, "minimum");
    }

    #[test]
    fn handling_is_added_before_the_minimum_is_applied() {
        // 5 kg at 10 = 50, plus 30 handling = 80, still under a 250 minimum.
        let (total, _, _, _) =
            price_consignment(Some(d("5")), None, Some(d("10")), None, Some(d("250")), Some(d("30")));
        assert_eq!(total, d("250"));
        // Above the minimum, handling rides on top of the dimensional charge.
        let (total2, _, _, basis) =
            price_consignment(Some(d("100")), None, Some(d("10")), None, Some(d("250")), Some(d("30")));
        assert_eq!(total2, d("1030"));
        assert_eq!(basis, "mass");
    }

    #[test]
    fn a_dimension_the_card_does_not_price_contributes_nothing() {
        // A mass-only card quoting bulky cargo must not silently treat the
        // missing volume rate as zero-cost *and* charge for it.
        let (total, _, v, _) =
            price_consignment(Some(d("10")), Some(d("99")), Some(d("5")), None, None, None);
        assert_eq!(v, Decimal::ZERO);
        assert_eq!(total, d("50"));
    }

    #[test]
    fn reliability_is_withheld_until_the_sample_justifies_it() {
        let thin = Reliability { completed: 3, on_time: 3, condition_exceptions: 0 };
        let j = thin.to_json();
        assert_eq!(j["sampleSufficient"], json!(false));
        // A perfect 100% from three shipments must not be published as a rate.
        assert!(j["onTimeRate"].is_null());
        assert!(j["note"].as_str().unwrap().contains("too few"));

        let solid = Reliability { completed: 10, on_time: 9, condition_exceptions: 1 };
        let j = solid.to_json();
        assert_eq!(j["sampleSufficient"], json!(true));
        assert_eq!(j["onTimeRate"], json!(90.0));
        assert_eq!(j["conditionExceptionRate"], json!(10.0));
    }
}
