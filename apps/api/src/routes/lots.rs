//! Lot, batch and serial tracking.
//!
//! Inventory answers "how much". This answers "which ones" — the question
//! behind every recall, every expiry sweep and every flight-part pedigree.
//!
//! Two endpoints carry the weight:
//!
//!   GET /api/lots/trace?lot_code=…|serial=…   where did that batch go
//!   GET /api/lots/expiring?days=N             what goes off before resupply
//!
//! Both are deliberately answerable without a network round-trip to anyone
//! else: a quarantine decision on a disconnected outpost cannot wait for core.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
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

pub fn lot_routes() -> Router<AppState> {
    Router::new()
        .route("/trace", get(trace))
        .route("/expiring", get(expiring))
        .route("/:id", get(get_lot).patch(update_lot))
        .route("/:id/quarantine", post(quarantine))
}

/// Mounted under /api/inventory so lots hang off the item they belong to.
pub fn inventory_lot_routes() -> Router<AppState> {
    Router::new().route("/:id/lots", get(list_for_item).post(create_lot))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Serialize, sqlx::FromRow)]
pub struct Lot {
    pub id: Uuid,
    pub inventory_id: Uuid,
    pub lot_code: Option<String>,
    pub serial: Option<String>,
    #[serde(with = "rust_decimal::serde::float")]
    pub quantity: Decimal,
    pub manufactured_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub received_at: DateTime<Utc>,
    pub supplier: Option<String>,
    pub pedigree: Value,
    pub status: String,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewLot {
    pub lot_code: Option<String>,
    pub serial: Option<String>,
    pub quantity: Option<f64>,
    pub manufactured_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub supplier: Option<String>,
    pub pedigree: Option<Value>,
    pub notes: Option<String>,
}

#[derive(Deserialize)]
pub struct LotPatch {
    pub status: Option<String>,
    pub quantity: Option<f64>,
    pub expires_at: Option<DateTime<Utc>>,
    pub notes: Option<String>,
    pub pedigree: Option<Value>,
}

const STATUSES: [&str; 6] = [
    "available", "reserved", "in_transit", "consumed", "quarantined", "expired",
];

/// Statuses that still represent stock you could actually use.
pub const ACTIONABLE: [&str; 2] = ["available", "reserved"];

fn check_status(s: &str) -> Result<(), ApiError> {
    if STATUSES.contains(&s) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("status must be one of {}", STATUSES.join(", ")),
        ))
    }
}

// ---------------------------------------------------------------------------
// Lots under an inventory item
// ---------------------------------------------------------------------------

/// GET /api/inventory/:id/lots
///
/// Reports the lot-level total alongside `inventory.quantity`. A mismatch is
/// surfaced, never reconciled: it means the physical count and the record
/// disagree — miscount, shrinkage, an issue nobody recorded — and quietly
/// overwriting one with the other destroys exactly the signal an operator
/// needs.
async fn list_for_item(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(inventory_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let lots = sqlx::query_as::<_, Lot>(
        "SELECT * FROM inventory_lots WHERE inventory_id = $1 ORDER BY expires_at NULLS LAST, received_at",
    )
    .bind(inventory_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("lot lookup failed"))?;

    let recorded: Option<i32> = sqlx::query_scalar("SELECT quantity FROM inventory WHERE id = $1")
        .bind(inventory_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("inventory lookup failed"))?;

    let Some(recorded) = recorded else {
        return Err((StatusCode::NOT_FOUND, "no such inventory item".into()));
    };

    // Only stock that still exists counts toward the on-hand comparison.
    let tracked: Decimal = lots
        .iter()
        .filter(|l| ACTIONABLE.contains(&l.status.as_str()))
        .map(|l| l.quantity)
        .sum();

    let recorded_dec = Decimal::from(recorded);
    Ok(Json(json!({
        "inventoryId": inventory_id,
        "recordedQuantity": recorded,
        "trackedQuantity": tracked.to_string(),
        // Null when nothing is lot-tracked: absence of lots is "untracked",
        // not "zero on hand", and calling that a discrepancy would flag every
        // bulk item in the system.
        "discrepancy": if lots.is_empty() { Value::Null }
                       else { json!((recorded_dec - tracked).to_string()) },
        "lotCount": lots.len(),
        "lots": lots,
    })))
}

/// POST /api/inventory/:id/lots
async fn create_lot(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(inventory_id): Path<Uuid>,
    Json(body): Json<NewLot>,
) -> Result<(StatusCode, Json<Lot>), ApiError> {
    if body.lot_code.is_none() && body.serial.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "a lot needs a lot_code or a serial — without one it is just a quantity".into(),
        ));
    }
    let quantity = body.quantity.unwrap_or(1.0);
    if quantity < 0.0 {
        return Err((StatusCode::BAD_REQUEST, "quantity cannot be negative".into()));
    }
    if body.serial.is_some() && quantity > 1.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "a serial identifies one unit, so quantity cannot exceed 1".into(),
        ));
    }

    let lot = sqlx::query_as::<_, Lot>(
        r#"
        INSERT INTO inventory_lots
          (inventory_id, lot_code, serial, quantity, manufactured_at, expires_at,
           supplier, pedigree, notes)
        VALUES ($1,$2,$3,$4,$5,$6,$7,COALESCE($8,'{}'::jsonb),$9)
        RETURNING *
        "#,
    )
    .bind(inventory_id)
    .bind(&body.lot_code)
    .bind(&body.serial)
    .bind(Decimal::try_from(quantity).unwrap_or_default())
    .bind(body.manufactured_at)
    .bind(body.expires_at)
    .bind(&body.supplier)
    .bind(&body.pedigree)
    .bind(&body.notes)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            match db.code().as_deref() {
                // The same physical unit cannot exist twice.
                Some("23505") => return (StatusCode::CONFLICT, "that serial already exists".into()),
                Some("23503") => return (StatusCode::NOT_FOUND, "no such inventory item".into()),
                _ => {}
            }
        }
        tracing::error!(error = %e, "lot insert failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not record the lot".into())
    })?;

    Ok((StatusCode::CREATED, Json(lot)))
}

// ---------------------------------------------------------------------------
// Single lot
// ---------------------------------------------------------------------------

async fn get_lot(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Lot>, ApiError> {
    sqlx::query_as::<_, Lot>("SELECT * FROM inventory_lots WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("lot lookup failed"))?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "no such lot".into()))
}

async fn update_lot(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(patch): Json<LotPatch>,
) -> Result<Json<Lot>, ApiError> {
    if let Some(s) = &patch.status {
        check_status(s)?;
    }

    sqlx::query_as::<_, Lot>(
        r#"
        UPDATE inventory_lots SET
          status     = COALESCE($2, status),
          quantity   = COALESCE($3, quantity),
          expires_at = COALESCE($4, expires_at),
          notes      = COALESCE($5, notes),
          pedigree   = COALESCE($6, pedigree),
          updated_at = NOW()
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(&patch.status)
    .bind(patch.quantity.and_then(|q| Decimal::try_from(q).ok()))
    .bind(patch.expires_at)
    .bind(&patch.notes)
    .bind(&patch.pedigree)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("lot update failed"))?
    .map(Json)
    .ok_or((StatusCode::NOT_FOUND, "no such lot".into()))
}

#[derive(Deserialize)]
pub struct QuarantineRequest {
    pub reason: String,
}

/// POST /api/lots/:id/quarantine — pull a lot out of usable stock.
///
/// Separate from a status PATCH because it is the action an operator reaches
/// for under time pressure, and because the reason must be recorded. A recall
/// with no reason on the record is not a recall.
async fn quarantine(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<QuarantineRequest>,
) -> Result<Json<Lot>, ApiError> {
    if body.reason.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a quarantine needs a reason".into()));
    }

    let lot = sqlx::query_as::<_, Lot>(
        r#"
        UPDATE inventory_lots SET
          status   = 'quarantined',
          pedigree = pedigree || jsonb_build_object(
                       'quarantine',
                       jsonb_build_object('reason', $2::text, 'by', $3::text, 'at', NOW())),
          updated_at = NOW()
        WHERE id = $1
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(body.reason.trim())
    .bind(&claims.sub)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("quarantine failed"))?
    .ok_or((StatusCode::NOT_FOUND, "no such lot".to_string()))?;

    tracing::warn!(lot_id = %id, by = %claims.sub, reason = %body.reason, "lot quarantined");
    Ok(Json(lot))
}

// ---------------------------------------------------------------------------
// The two queries the feature exists for
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TraceQuery {
    pub lot_code: Option<String>,
    pub serial: Option<String>,
}

/// GET /api/lots/trace?lot_code=…  |  ?serial=…
///
/// The recall question. Given a batch, return every lot from it with the item
/// and location, so an operator can see the blast radius and quarantine it.
async fn trace(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(q): Query<TraceQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.lot_code.is_none() && q.serial.is_none() {
        return Err((StatusCode::BAD_REQUEST, "give a lot_code or a serial".into()));
    }

    let rows = sqlx::query(
        r#"
        SELECT l.id, l.lot_code, l.serial, l.quantity, l.status, l.expires_at,
               l.supplier, l.received_at, l.pedigree,
               i.id AS inventory_id, i.name AS item_name, i.unit, i.location
        FROM inventory_lots l
        JOIN inventory i ON i.id = l.inventory_id
        WHERE ($1::text IS NULL OR l.lot_code = $1)
          AND ($2::text IS NULL OR l.serial   = $2)
        ORDER BY l.received_at DESC
        "#,
    )
    .bind(&q.lot_code)
    .bind(&q.serial)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("trace failed"))?;

    let mut affected = Decimal::ZERO;
    let mut still_out = 0usize;
    let hits: Vec<Value> = rows
        .iter()
        .map(|r| {
            let qty: Decimal = r.get("quantity");
            let status: String = r.get("status");
            affected += qty;
            if ACTIONABLE.contains(&status.as_str()) {
                still_out += 1;
            }
            json!({
                "lotId": r.get::<Uuid, _>("id"),
                "lotCode": r.get::<Option<String>, _>("lot_code"),
                "serial": r.get::<Option<String>, _>("serial"),
                "quantity": qty.to_string(),
                "status": status,
                "expiresAt": r.get::<Option<DateTime<Utc>>, _>("expires_at"),
                "receivedAt": r.get::<DateTime<Utc>, _>("received_at"),
                "supplier": r.get::<Option<String>, _>("supplier"),
                "inventoryId": r.get::<Uuid, _>("inventory_id"),
                "item": r.get::<String, _>("item_name"),
                "unit": r.get::<String, _>("unit"),
                "location": r.get::<Option<String>, _>("location"),
                "pedigree": r.get::<Value, _>("pedigree"),
            })
        })
        .collect();

    Ok(Json(json!({
        "lotCode": q.lot_code,
        "serial": q.serial,
        "matches": hits.len(),
        "totalQuantity": affected.to_string(),
        // The number that matters in a recall: how many are still usable and
        // therefore still need pulling.
        "stillInUsableStock": still_out,
        "lots": hits,
    })))
}

#[derive(Deserialize)]
pub struct ExpiringQuery {
    /// Horizon in days. Defaults to 30.
    pub days: Option<i64>,
}

/// GET /api/lots/expiring?days=N
///
/// Everything going off inside the window, plus anything already past its date
/// but still sitting in usable stock — which is the more urgent half and the
/// reason this is not simply `expires_at < now + N`.
async fn expiring(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(q): Query<ExpiringQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = q.days.unwrap_or(30).clamp(0, 3650);
    let horizon = Utc::now() + Duration::days(days);

    let rows = sqlx::query(
        r#"
        SELECT l.id, l.lot_code, l.serial, l.quantity, l.status, l.expires_at,
               i.name AS item_name, i.unit, i.location
        FROM inventory_lots l
        JOIN inventory i ON i.id = l.inventory_id
        WHERE l.expires_at IS NOT NULL
          AND l.status IN ('available','reserved')
          AND l.expires_at <= $1
        ORDER BY l.expires_at
        "#,
    )
    .bind(horizon)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("expiry sweep failed"))?;

    let now = Utc::now();
    let mut already = 0usize;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let exp: Option<DateTime<Utc>> = r.get("expires_at");
            let lapsed = exp.map(|e| e <= now).unwrap_or(false);
            if lapsed {
                already += 1;
            }
            json!({
                "lotId": r.get::<Uuid, _>("id"),
                "lotCode": r.get::<Option<String>, _>("lot_code"),
                "serial": r.get::<Option<String>, _>("serial"),
                "item": r.get::<String, _>("item_name"),
                "unit": r.get::<String, _>("unit"),
                "location": r.get::<Option<String>, _>("location"),
                "quantity": r.get::<Decimal, _>("quantity").to_string(),
                "status": r.get::<String, _>("status"),
                "expiresAt": exp,
                "daysRemaining": exp.map(|e| (e - now).num_days()),
                "alreadyExpired": lapsed,
            })
        })
        .collect();

    Ok(Json(json!({
        "horizonDays": days,
        "count": items.len(),
        // Split out deliberately: stock that is already past its date but
        // still marked usable is a problem now, not a planning horizon.
        "alreadyExpired": already,
        "lots": items,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_values_are_closed() {
        assert!(check_status("quarantined").is_ok());
        assert!(check_status("available").is_ok());
        let e = check_status("lost").unwrap_err();
        assert_eq!(e.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn only_available_and_reserved_count_as_usable_stock() {
        // Drives both the on-hand comparison and the recall "still out" count.
        // Counting consumed or quarantined stock would understate a shortage
        // and overstate a recall's remaining exposure.
        assert!(ACTIONABLE.contains(&"available"));
        assert!(ACTIONABLE.contains(&"reserved"));
        for gone in ["consumed", "quarantined", "expired", "in_transit"] {
            assert!(!ACTIONABLE.contains(&gone), "{gone} must not count as on hand");
        }
    }
}
