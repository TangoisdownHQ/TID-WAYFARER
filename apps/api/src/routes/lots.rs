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
        .route("/life", get(life_remaining))
        .route("/:id/usage", axum::routing::patch(update_usage))
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

// ---------------------------------------------------------------------------
// Life limits
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct UsageUpdate {
    pub cycles_used: Option<f64>,
    pub hours_used: Option<f64>,
    pub in_service_since: Option<DateTime<Utc>>,
}

/// PATCH /api/lots/:id/usage — record what a unit has actually done.
///
/// Usage accrues against the individual unit, which is the whole reason
/// serialised parts exist: two motors from the same batch retire at different
/// times because one flew twice as much.
async fn update_usage(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(b): Json<UsageUpdate>,
) -> Result<Json<Value>, ApiError> {
    for (name, v) in [("cycles_used", b.cycles_used), ("hours_used", b.hours_used)] {
        if v.map_or(false, |x| x < 0.0) {
            return Err((StatusCode::BAD_REQUEST, format!("{name} cannot be negative")));
        }
    }

    let row = sqlx::query(
        r#"
        UPDATE inventory_lots SET
          cycles_used      = COALESCE($2, cycles_used),
          hours_used       = COALESCE($3, hours_used),
          in_service_since = COALESCE($4, in_service_since),
          updated_at       = NOW()
        WHERE id = $1
        RETURNING cycles_used, hours_used, in_service_since
        "#,
    )
    .bind(id)
    .bind(b.cycles_used.and_then(|v| Decimal::try_from(v).ok()))
    .bind(b.hours_used.and_then(|v| Decimal::try_from(v).ok()))
    .bind(b.in_service_since)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("usage update failed"))?
    .ok_or((StatusCode::NOT_FOUND, "no such lot".to_string()))?;

    Ok(Json(json!({
        "lotId": id,
        "cyclesUsed": row.get::<Decimal,_>("cycles_used").to_string(),
        "hoursUsed": row.get::<Decimal,_>("hours_used").to_string(),
        "inServiceSince": row.get::<Option<DateTime<Utc>>,_>("in_service_since"),
    })))
}

#[derive(Deserialize)]
pub struct LifeQuery {
    /// Report units at or past this fraction of any limit. Default 0.8.
    pub at_pct: Option<f64>,
}

/// GET /api/lots/life?at_pct=0.8
///
/// Which units are approaching retirement, and on which clock.
///
/// Three limits can apply at once — cycles for a battery, hours for a motor,
/// calendar time for a seal that ages on the shelf — and whichever is reached
/// first retires the unit. So the figure reported is the *worst* of the three,
/// named: telling an operator a motor is at 40% of its hours while it is at
/// 98% of its cycles would be true and useless.
async fn life_remaining(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(q): Query<LifeQuery>,
) -> Result<Json<Value>, ApiError> {
    let at = q.at_pct.unwrap_or(0.8).clamp(0.0, 1.0);

    let rows = sqlx::query(
        r#"
        SELECT l.id, l.serial, l.lot_code, l.status, l.cycles_used, l.hours_used,
               l.in_service_since,
               i.name, i.unit, i.location,
               i.life_limit_cycles, i.life_limit_hours, i.life_limit_days
        FROM inventory_lots l
        JOIN inventory i ON i.id = l.inventory_id
        WHERE l.status IN ('available','reserved')
          AND (i.life_limit_cycles IS NOT NULL
            OR i.life_limit_hours  IS NOT NULL
            OR i.life_limit_days   IS NOT NULL)
        "#,
    )
    .fetch_all(&state.db)
    .await
    .map_err(server_err("life sweep failed"))?;

    let now = Utc::now();
    let f = |d: Option<Decimal>| d.and_then(|v| v.to_string().parse::<f64>().ok());

    let mut out: Vec<Value> = Vec::new();
    let mut expired = 0usize;

    for r in &rows {
        let cycles = f(r.get("cycles_used")).unwrap_or(0.0);
        let hours = f(r.get("hours_used")).unwrap_or(0.0);
        let since: Option<DateTime<Utc>> = r.get("in_service_since");
        let days = since.map(|s| (now - s).num_days() as f64).unwrap_or(0.0);

        // (fraction consumed, which clock) for each limit that applies.
        let mut worst: Option<(f64, &'static str, f64, f64)> = None;
        let mut consider = |used: f64, limit: Option<f64>, label: &'static str| {
            if let Some(l) = limit.filter(|l| *l > 0.0) {
                let frac = used / l;
                if worst.map_or(true, |(w, _, _, _)| frac > w) {
                    worst = Some((frac, label, used, l));
                }
            }
        };
        consider(cycles, f(r.get("life_limit_cycles")), "cycles");
        consider(hours, f(r.get("life_limit_hours")), "hours");
        consider(days, f(r.get("life_limit_days")), "days");

        let Some((frac, clock, used, limit)) = worst else { continue };
        if frac < at {
            continue;
        }
        if frac >= 1.0 {
            expired += 1;
        }

        out.push(json!({
            "lotId": r.get::<Uuid,_>("id"),
            "item": r.get::<String,_>("name"),
            "serial": r.get::<Option<String>,_>("serial"),
            "lotCode": r.get::<Option<String>,_>("lot_code"),
            "location": r.get::<Option<String>,_>("location"),
            "status": r.get::<String,_>("status"),
            // The binding clock, not an average across three of them.
            "limitedBy": clock,
            "used": (used * 100.0).round() / 100.0,
            "limit": limit,
            "remaining": ((limit - used).max(0.0) * 100.0).round() / 100.0,
            "percentUsed": (frac * 1000.0).round() / 10.0,
            "pastLimit": frac >= 1.0,
        }));
    }

    out.sort_by(|a, b| b["percentUsed"].as_f64().unwrap_or(0.0)
        .partial_cmp(&a["percentUsed"].as_f64().unwrap_or(0.0))
        .unwrap_or(std::cmp::Ordering::Equal));

    Ok(Json(json!({
        "atPercent": at * 100.0,
        "count": out.len(),
        // Past its limit and still marked usable: a present problem, the same
        // split the expiry sweep makes.
        "pastLimit": expired,
        "units": out,
    })))
}
