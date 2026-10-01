//! Stock movements and demand forecasting.
//!
//! Recording a movement keeps `inventory.quantity` in step automatically, so
//! there is one way to change stock rather than two that can disagree. The
//! level stays as a cached figure because every read wants it, but it is now
//! derived from events instead of being the only record.
//!
//! The forecast maths lives in [`crate::services::forecast`] — pure, so the
//! judgements it makes are testable. What this module adds is the fleet-wide
//! sweep: `GET /api/movements/forecast` ranks every item by how close it is to
//! running out *relative to how long resupply takes*, which is a different
//! and more useful ordering than "what is below its reorder threshold".
//!
//! A fixed threshold is correct for exactly one burn rate. An item consuming
//! nothing sits below its threshold for a year and never matters; an item
//! burning fast can be comfortably above one and still miss the next window.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
use crate::services::forecast::{self, Consumption};
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

const REASONS: [&str; 5] = ["issue", "receipt", "adjustment", "transfer", "spoilage"];

pub fn movement_routes() -> Router<AppState> {
    Router::new()
        .route("/", post(record_movement))
        .route("/forecast", get(fleet_forecast))
        .route("/forecast/:inventory_id", get(item_forecast))
        .route("/item/:inventory_id", get(list_for_item))
}

#[derive(Deserialize)]
pub struct NewMovement {
    pub inventory_id: Uuid,
    pub lot_id: Option<Uuid>,
    /// Signed: negative leaves, positive arrives.
    pub delta: f64,
    pub reason: Option<String>,
    pub order_id: Option<Uuid>,
    pub fulfillment_id: Option<Uuid>,
    pub note: Option<String>,
    pub occurred_at: Option<DateTime<Utc>>,
}

/// POST /api/movements — record a stock change.
///
/// The movement and the resulting level are written in one transaction. If
/// they could drift apart, the ledger would stop explaining the level and the
/// whole point of keeping it would be lost.
async fn record_movement(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Json(b): Json<NewMovement>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if b.delta == 0.0 {
        return Err((StatusCode::BAD_REQUEST, "a movement of zero is not a movement".into()));
    }
    let reason = b.reason.as_deref().unwrap_or("issue").to_string();
    if !REASONS.contains(&reason.as_str()) {
        return Err((StatusCode::BAD_REQUEST, format!("reason must be one of {}", REASONS.join(", "))));
    }
    let delta = Decimal::try_from(b.delta)
        .map_err(|_| (StatusCode::BAD_REQUEST, "delta is not a usable number".to_string()))?;
    let actor = Uuid::parse_str(&claims.sub).ok();

    let mut tx = state.db.begin().await.map_err(server_err("could not start transaction"))?;

    let current: Option<i32> = sqlx::query_scalar("SELECT quantity FROM inventory WHERE id = $1 FOR UPDATE")
        .bind(b.inventory_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(server_err("inventory lookup failed"))?;

    let Some(current) = current else {
        return Err((StatusCode::NOT_FOUND, "no such inventory item".into()));
    };

    let new_level = Decimal::from(current) + delta;
    if new_level < Decimal::ZERO {
        return Err((
            StatusCode::CONFLICT,
            format!("that would take stock to {new_level}; there are only {current} on hand"),
        ));
    }

    sqlx::query(
        r#"
        INSERT INTO inventory_movements
          (inventory_id, lot_id, delta, reason, order_id, fulfillment_id, actor_id, note, occurred_at)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,COALESCE($9, NOW()))
        "#,
    )
    .bind(b.inventory_id).bind(b.lot_id).bind(delta).bind(&reason)
    .bind(b.order_id).bind(b.fulfillment_id).bind(actor).bind(&b.note).bind(b.occurred_at)
    .execute(&mut *tx)
    .await
    .map_err(server_err("could not record the movement"))?;

    sqlx::query("UPDATE inventory SET quantity = $2 WHERE id = $1")
        .bind(b.inventory_id)
        .bind(new_level.round().to_string().parse::<i32>().unwrap_or(current))
        .execute(&mut *tx)
        .await
        .map_err(server_err("could not update the level"))?;

    tx.commit().await.map_err(server_err("could not commit"))?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "inventoryId": b.inventory_id,
            "delta": delta.to_string(),
            "reason": reason,
            "quantityBefore": current,
            "quantityAfter": new_level.to_string(),
        })),
    ))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    pub days: Option<i64>,
}

async fn list_for_item(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(inventory_id): Path<Uuid>,
    Query(q): Query<HistoryQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = q.days.unwrap_or(90).clamp(1, 3650);
    let rows = sqlx::query(
        r#"
        SELECT m.*, u.username AS actor_name
        FROM inventory_movements m
        LEFT JOIN users u ON u.id = m.actor_id
        WHERE m.inventory_id = $1 AND m.occurred_at >= NOW() - make_interval(days => $2)
        ORDER BY m.occurred_at DESC
        "#,
    )
    .bind(inventory_id)
    .bind(days as i32)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("movement history failed"))?;

    let items: Vec<Value> = rows.iter().map(|r| {
        json!({
            "id": r.get::<Uuid, _>("id"),
            "delta": r.get::<Decimal, _>("delta").to_string(),
            "reason": r.get::<String, _>("reason"),
            "actor": r.get::<Option<String>, _>("actor_name"),
            "note": r.get::<Option<String>, _>("note"),
            "occurredAt": r.get::<DateTime<Utc>, _>("occurred_at"),
        })
    }).collect();

    Ok(Json(json!({ "inventoryId": inventory_id, "windowDays": days, "movements": items })))
}

// ---------------------------------------------------------------------------
// Forecasting
// ---------------------------------------------------------------------------

/// Everything the forecast needs for one item.
struct ItemState {
    id: Uuid,
    name: String,
    location: Option<String>,
    unit: String,
    on_hand: f64,
    threshold: f64,
    lead_time_days: Option<f64>,
}

async fn load_item(state: &AppState, id: Uuid) -> Result<Option<ItemState>, ApiError> {
    let row = sqlx::query(
        "SELECT id, name, location, unit, quantity, threshold, lead_time_days FROM inventory WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("inventory lookup failed"))?;

    Ok(row.map(|r| ItemState {
        id: r.get("id"),
        name: r.get("name"),
        location: r.get("location"),
        unit: r.get("unit"),
        on_hand: r.get::<i32, _>("quantity") as f64,
        threshold: r.get::<i32, _>("threshold") as f64,
        lead_time_days: r.get::<Option<Decimal>, _>("lead_time_days").and_then(|d| d.to_string().parse().ok()),
    }))
}

async fn consumption_for(state: &AppState, id: Uuid, days: i64) -> Result<Vec<Consumption>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT delta, occurred_at FROM inventory_movements
        WHERE inventory_id = $1 AND delta < 0
          AND occurred_at >= NOW() - make_interval(days => $2)
        ORDER BY occurred_at
        "#,
    )
    .bind(id)
    .bind(days as i32)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("consumption lookup failed"))?;

    Ok(rows
        .iter()
        .map(|r| Consumption {
            at: r.get("occurred_at"),
            // Stored signed; a burn rate works in magnitudes.
            amount: r.get::<Decimal, _>("delta").to_string().parse::<f64>().unwrap_or(0.0).abs(),
        })
        .collect())
}

fn forecast_json(item: &ItemState, f: &forecast::Forecast) -> Value {
    json!({
        "inventoryId": item.id,
        "name": item.name,
        "location": item.location,
        "unit": item.unit,
        "onHand": item.on_hand,
        "threshold": item.threshold,
        "leadTimeDays": item.lead_time_days,
        "history": {
            "events": f.events,
            "windowDays": (f.window_days * 10.0).round() / 10.0,
            "totalConsumed": f.total_consumed,
        },
        "burnPerDay": f.burn_per_day.map(|b| (b * 1000.0).round() / 1000.0),
        "daysOfCover": f.days_of_cover.map(|d| (d * 10.0).round() / 10.0),
        // A range, never a date: the mean hides its own spread, and a precise
        // stockout day from lumpy consumption is a false promise.
        "stockout": {
            "earliest": f.stockout_earliest,
            "latest": f.stockout_latest,
        },
        "confidence": f.confidence.as_str(),
        "variability": f.variability.map(|v| (v * 100.0).round() / 100.0),
        // The comparison a reorder threshold cannot express: will what is here
        // outlast the time it takes to replace it?
        "pastReorderPoint": f.past_reorder_point,
        "orderBy": f.order_by,
        "recommendedQuantity": f.recommended_quantity.map(|q| q.ceil()),
        "note": match f.confidence {
            forecast::Confidence::Insufficient => json!(format!(
                "{} consumption event(s) over {:.0} day(s) — too little history to forecast",
                f.events, f.window_days)),
            forecast::Confidence::Erratic => json!(
                "consumption is lumpy; treat the stockout window as wide, not as a date"),
            forecast::Confidence::Good => Value::Null,
        },
    })
}

#[derive(Deserialize)]
pub struct ForecastQuery {
    /// How far back to look for consumption. Defaults to 90 days.
    pub days: Option<i64>,
}

/// GET /api/movements/forecast/:inventory_id
async fn item_forecast(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(inventory_id): Path<Uuid>,
    Query(q): Query<ForecastQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = q.days.unwrap_or(90).clamp(7, 3650);
    let item = load_item(&state, inventory_id)
        .await?
        .ok_or((StatusCode::NOT_FOUND, "no such inventory item".to_string()))?;
    let consumption = consumption_for(&state, inventory_id, days).await?;
    let f = forecast::forecast(&consumption, item.on_hand, item.lead_time_days, Utc::now());
    Ok(Json(forecast_json(&item, &f)))
}

/// GET /api/movements/forecast
///
/// Every item, ranked by urgency rather than by stock level. An item already
/// unable to outlast its own resupply comes first; after that, fewest days of
/// cover. Items with too little history sort last and say so — they are
/// unknown, not fine, and the distinction is the same one the console's fourth
/// colour exists for.
async fn fleet_forecast(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(q): Query<ForecastQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = q.days.unwrap_or(90).clamp(7, 3650);

    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM inventory ORDER BY name")
        .fetch_all(&state.db)
        .await
        .map_err(server_err("inventory list failed"))?;

    let mut out = Vec::new();
    let mut at_risk = 0usize;
    let mut unknown = 0usize;

    for id in ids {
        let Some(item) = load_item(&state, id).await? else { continue };
        let consumption = consumption_for(&state, id, days).await?;
        let f = forecast::forecast(&consumption, item.on_hand, item.lead_time_days, Utc::now());
        if f.past_reorder_point {
            at_risk += 1;
        }
        if matches!(f.confidence, forecast::Confidence::Insufficient) {
            unknown += 1;
        }
        out.push((f.past_reorder_point, f.days_of_cover, forecast_json(&item, &f)));
    }

    out.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| match (a.1, b.1) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            // No estimate sorts last: unknown is not urgent, but it is also
            // not safe, so it must not displace something that is measurably
            // about to run out.
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
    });

    Ok(Json(json!({
        "windowDays": days,
        "items": out.len(),
        "cannotOutlastResupply": at_risk,
        "withoutEnoughHistory": unknown,
        "forecasts": out.into_iter().map(|(_, _, j)| j).collect::<Vec<_>>(),
    })))
}
