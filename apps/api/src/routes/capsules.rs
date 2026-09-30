//! Capsules — a hull with a mass and volume budget, and the cargo booked into
//! it.
//!
//! "Capsule" is the space case; the same thing models a container, a truck or
//! a pallet. What matters is that it has two finite budgets and a departure.
//!
//! The rule the whole module exists to enforce: **a booking that does not fit
//! is refused, by mass or by volume, whichever binds first.** Cargo that fits
//! by mass and not by volume is the ordinary case, not the exotic one — which
//! is why a single "capacity" number would have been useless.
//!
//! This is also how capsule *sharing* works. Several parties book into one
//! hull; each booking snapshots its own mass and volume and its share of the
//! total, so the cost split is reproducible afterwards even if the capsule's
//! stated capacity is later corrected.

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

fn dec(v: f64) -> Decimal {
    Decimal::try_from(v).unwrap_or_default()
}

pub fn capsule_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_capsules).post(create_capsule))
        .route("/:id", get(get_capsule))
        .route("/:id/manifest", get(get_manifest).post(book))
        .route("/:id/manifest/:order_id", axum::routing::delete(unbook))
        .route("/:id/seal", post(seal))
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Serialize, sqlx::FromRow)]
pub struct Capsule {
    pub id: Uuid,
    pub name: String,
    pub operator_id: Option<Uuid>,
    pub operator_outpost: Option<Uuid>,
    #[serde(with = "rust_decimal::serde::float")]
    pub mass_capacity_kg: Decimal,
    #[serde(with = "rust_decimal::serde::float")]
    pub volume_capacity_m3: Decimal,
    pub origin_body_id: Option<i32>,
    pub destination_body_id: Option<i32>,
    pub origin_address: Option<String>,
    pub destination_address: Option<String>,
    pub departs_at: Option<DateTime<Utc>>,
    pub arrives_at: Option<DateTime<Utc>>,
    pub status: String,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewCapsule {
    pub name: String,
    pub mass_capacity_kg: f64,
    pub volume_capacity_m3: f64,
    pub origin_body_id: Option<i32>,
    pub destination_body_id: Option<i32>,
    pub origin_address: Option<String>,
    pub destination_address: Option<String>,
    pub departs_at: Option<DateTime<Utc>>,
    pub arrives_at: Option<DateTime<Utc>>,
    pub operator_outpost: Option<Uuid>,
    pub notes: Option<String>,
}

#[derive(Deserialize)]
pub struct BookRequest {
    pub order_id: Uuid,
    /// Override the order's own figures — a shipper who has actually packed
    /// the crate knows better than the estimate on the order.
    pub mass_kg: Option<f64>,
    pub volume_m3: Option<f64>,
    pub price: Option<f64>,
    pub notes: Option<String>,
}

/// What a capsule has left. Computed, never stored: a cached remaining-capacity
/// column is a race waiting to happen, and the sums are trivial.
struct Load {
    mass: Decimal,
    volume: Decimal,
    bookings: i64,
}

async fn current_load(state: &AppState, capsule_id: Uuid) -> Result<Load, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT COALESCE(SUM(mass_kg),0)   AS mass,
               COALESCE(SUM(volume_m3),0) AS volume,
               COUNT(*)                   AS bookings
        FROM capsule_manifest WHERE capsule_id = $1
        "#,
    )
    .bind(capsule_id)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("manifest load query failed"))?;

    Ok(Load {
        mass: row.get("mass"),
        volume: row.get("volume"),
        bookings: row.get("bookings"),
    })
}

// ---------------------------------------------------------------------------
// Capsules
// ---------------------------------------------------------------------------

async fn create_capsule(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Json(body): Json<NewCapsule>,
) -> Result<(StatusCode, Json<Capsule>), ApiError> {
    if body.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a capsule needs a name".into()));
    }
    if body.mass_capacity_kg <= 0.0 || body.volume_capacity_m3 <= 0.0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "both mass and volume capacity must be greater than zero".into(),
        ));
    }
    let operator = Uuid::parse_str(&claims.sub).ok();

    let capsule = sqlx::query_as::<_, Capsule>(
        r#"
        INSERT INTO capsules
          (name, operator_id, operator_outpost, mass_capacity_kg, volume_capacity_m3,
           origin_body_id, destination_body_id, origin_address, destination_address,
           departs_at, arrives_at, notes)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
        RETURNING *
        "#,
    )
    .bind(body.name.trim())
    .bind(operator)
    .bind(body.operator_outpost)
    .bind(dec(body.mass_capacity_kg))
    .bind(dec(body.volume_capacity_m3))
    .bind(body.origin_body_id)
    .bind(body.destination_body_id)
    .bind(&body.origin_address)
    .bind(&body.destination_address)
    .bind(body.departs_at)
    .bind(body.arrives_at)
    .bind(&body.notes)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("could not create the capsule"))?;

    Ok((StatusCode::CREATED, Json(capsule)))
}

#[derive(Deserialize)]
pub struct CapsuleFilter {
    pub status: Option<String>,
    pub destination_body_id: Option<i32>,
}

async fn list_capsules(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(f): Query<CapsuleFilter>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT c.*,
               COALESCE(m.mass,0)   AS booked_mass,
               COALESCE(m.volume,0) AS booked_volume,
               COALESCE(m.n,0)      AS bookings
        FROM capsules c
        LEFT JOIN (
            SELECT capsule_id,
                   SUM(mass_kg)   AS mass,
                   SUM(volume_m3) AS volume,
                   COUNT(*)       AS n
            FROM capsule_manifest GROUP BY capsule_id
        ) m ON m.capsule_id = c.id
        WHERE ($1::text IS NULL OR c.status = $1)
          AND ($2::int  IS NULL OR c.destination_body_id = $2)
        ORDER BY c.departs_at NULLS LAST, c.created_at DESC
        "#,
    )
    .bind(&f.status)
    .bind(f.destination_body_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("capsule list failed"))?;

    Ok(Json(rows.iter().map(summarise).collect()))
}

/// One capsule with both budgets and what is left of each.
fn summarise(r: &sqlx::postgres::PgRow) -> Value {
    let mass_cap: Decimal = r.get("mass_capacity_kg");
    let vol_cap: Decimal = r.get("volume_capacity_m3");
    let mass: Decimal = r.get("booked_mass");
    let vol: Decimal = r.get("booked_volume");

    let pct = |used: Decimal, cap: Decimal| -> String {
        if cap.is_zero() { "0".into() }
        else { ((used / cap) * Decimal::from(100)).round_dp(1).to_string() }
    };

    json!({
        "id": r.get::<Uuid, _>("id"),
        "name": r.get::<String, _>("name"),
        "status": r.get::<String, _>("status"),
        "departsAt": r.get::<Option<DateTime<Utc>>, _>("departs_at"),
        "originBodyId": r.get::<Option<i32>, _>("origin_body_id"),
        "destinationBodyId": r.get::<Option<i32>, _>("destination_body_id"),
        "destinationAddress": r.get::<Option<String>, _>("destination_address"),
        "bookings": r.get::<i64, _>("bookings"),
        "mass": {
            "capacityKg": mass_cap.to_string(),
            "bookedKg": mass.to_string(),
            "remainingKg": (mass_cap - mass).to_string(),
            "percentUsed": pct(mass, mass_cap),
        },
        "volume": {
            "capacityM3": vol_cap.to_string(),
            "bookedM3": vol.to_string(),
            "remainingM3": (vol_cap - vol).to_string(),
            "percentUsed": pct(vol, vol_cap),
        },
        // Which budget will stop the next booking. Operators plan against the
        // binding one, and it is volume more often than people expect.
        "bindingConstraint": if mass_cap.is_zero() || vol_cap.is_zero() { "unknown" }
            else if (mass / mass_cap) >= (vol / vol_cap) { "mass" } else { "volume" },
    })
}

async fn get_capsule(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT c.*,
               COALESCE(m.mass,0) AS booked_mass, COALESCE(m.volume,0) AS booked_volume,
               COALESCE(m.n,0) AS bookings
        FROM capsules c
        LEFT JOIN (SELECT capsule_id, SUM(mass_kg) mass, SUM(volume_m3) volume, COUNT(*) n
                   FROM capsule_manifest GROUP BY capsule_id) m ON m.capsule_id = c.id
        WHERE c.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("capsule lookup failed"))?
    .ok_or((StatusCode::NOT_FOUND, "no such capsule".to_string()))?;

    Ok(Json(summarise(&row)))
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// POST /api/capsules/:id/manifest — book an order into a capsule.
///
/// Refuses if it does not fit, naming the budget that failed and by how much.
/// "Does not fit" is the answer an operator can act on; a bare 400 is not.
async fn book(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Path(capsule_id): Path<Uuid>,
    Json(body): Json<BookRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let capsule = sqlx::query_as::<_, Capsule>("SELECT * FROM capsules WHERE id = $1")
        .bind(capsule_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("capsule lookup failed"))?
        .ok_or((StatusCode::NOT_FOUND, "no such capsule".to_string()))?;

    if capsule.status != "open" {
        return Err((
            StatusCode::CONFLICT,
            format!("this capsule is {} and is no longer accepting cargo", capsule.status),
        ));
    }

    // What the order says it weighs, unless the shipper supplied better
    // figures from the packed crate.
    let order = sqlx::query("SELECT mass_kg, volume_m3, description FROM orders WHERE id = $1")
        .bind(body.order_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("order lookup failed"))?
        .ok_or((StatusCode::NOT_FOUND, "no such order".to_string()))?;

    let order_mass: Option<Decimal> = order.get("mass_kg");
    let order_volume: Option<Decimal> = order.get("volume_m3");

    let mass = body.mass_kg.map(dec).or(order_mass);
    let volume = body.volume_m3.map(dec).or(order_volume);

    // Unknown is not zero. Booking cargo of unknown mass would let a capsule
    // silently overrun its budget, which is the one thing this table exists to
    // prevent.
    let (Some(mass), Some(volume)) = (mass, volume) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "this order has no mass and/or volume recorded — give mass_kg and volume_m3, \
             or set them on the order. Unknown cargo cannot be budgeted."
                .into(),
        ));
    };
    if mass < Decimal::ZERO || volume < Decimal::ZERO {
        return Err((StatusCode::BAD_REQUEST, "mass and volume cannot be negative".into()));
    }

    let load = current_load(&state, capsule_id).await?;
    let mass_after = load.mass + mass;
    let volume_after = load.volume + volume;

    let over_mass = mass_after > capsule.mass_capacity_kg;
    let over_volume = volume_after > capsule.volume_capacity_m3;

    if over_mass || over_volume {
        let mut why = Vec::new();
        if over_mass {
            why.push(format!(
                "over mass by {} kg ({} booked + {} requested vs {} capacity)",
                (mass_after - capsule.mass_capacity_kg),
                load.mass, mass, capsule.mass_capacity_kg
            ));
        }
        if over_volume {
            why.push(format!(
                "over volume by {} m³ ({} booked + {} requested vs {} capacity)",
                (volume_after - capsule.volume_capacity_m3),
                load.volume, volume, capsule.volume_capacity_m3
            ));
        }
        tracing::info!(
            capsule = %capsule.name, order = %body.order_id,
            "booking refused: {}", why.join("; ")
        );
        return Err((StatusCode::CONFLICT, format!("does not fit — {}", why.join("; "))));
    }

    let shipper = Uuid::parse_str(&claims.sub).ok();
    // Share of the hull by mass, recorded now so a cost split stays
    // reproducible if capacity is corrected later.
    let share = if capsule.mass_capacity_kg.is_zero() {
        Decimal::ZERO
    } else {
        (mass / capsule.mass_capacity_kg).round_dp(6)
    };

    sqlx::query(
        r#"
        INSERT INTO capsule_manifest
          (capsule_id, order_id, shipper_id, mass_kg, volume_m3, price, share_of_mass, notes)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
        "#,
    )
    .bind(capsule_id)
    .bind(body.order_id)
    .bind(shipper)
    .bind(mass)
    .bind(volume)
    .bind(body.price.map(dec))
    .bind(share)
    .bind(&body.notes)
    .execute(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (StatusCode::CONFLICT, "that order is already on this capsule".into());
            }
        }
        tracing::error!(error = %e, "manifest insert failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not book the cargo".into())
    })?;

    tracing::info!(
        capsule = %capsule.name, order = %body.order_id,
        mass = %mass, volume = %volume, "cargo booked"
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "capsuleId": capsule_id,
            "orderId": body.order_id,
            "massKg": mass.to_string(),
            "volumeM3": volume.to_string(),
            "shareOfMass": share.to_string(),
            "remainingMassKg": (capsule.mass_capacity_kg - mass_after).to_string(),
            "remainingVolumeM3": (capsule.volume_capacity_m3 - volume_after).to_string(),
            "bookingsOnCapsule": load.bookings + 1,
        })),
    ))
}

/// GET /api/capsules/:id/manifest — who is riding, and what each party owes.
async fn get_manifest(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(capsule_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT m.id, m.order_id, m.shipper_id, m.mass_kg, m.volume_m3, m.price,
               m.share_of_mass, m.booked_at, m.notes,
               o.description, o.delivery_address,
               u.username AS shipper_name
        FROM capsule_manifest m
        JOIN orders o ON o.id = m.order_id
        LEFT JOIN users u ON u.id = m.shipper_id
        WHERE m.capsule_id = $1
        ORDER BY m.booked_at
        "#,
    )
    .bind(capsule_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("manifest lookup failed"))?;

    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "manifestId": r.get::<Uuid, _>("id"),
                "orderId": r.get::<Uuid, _>("order_id"),
                "description": r.get::<String, _>("description"),
                "deliveryAddress": r.get::<Option<String>, _>("delivery_address"),
                "shipper": r.get::<Option<String>, _>("shipper_name"),
                "massKg": r.get::<Decimal, _>("mass_kg").to_string(),
                "volumeM3": r.get::<Decimal, _>("volume_m3").to_string(),
                "price": r.get::<Option<Decimal>, _>("price").map(|p| p.to_string()),
                "shareOfMass": r.get::<Option<Decimal>, _>("share_of_mass").map(|s| s.to_string()),
                "bookedAt": r.get::<DateTime<Utc>, _>("booked_at"),
            })
        })
        .collect();

    let load = current_load(&state, capsule_id).await?;
    Ok(Json(json!({
        "capsuleId": capsule_id,
        "bookings": items.len(),
        "totalMassKg": load.mass.to_string(),
        "totalVolumeM3": load.volume.to_string(),
        "manifest": items,
    })))
}

async fn unbook(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path((capsule_id, order_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM capsules WHERE id = $1")
        .bind(capsule_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("capsule lookup failed"))?;

    match status.as_deref() {
        None => return Err((StatusCode::NOT_FOUND, "no such capsule".into())),
        Some("open") => {}
        // Once sealed the manifest is what flew. Removing a row would rewrite
        // history and break the settlement split.
        Some(other) => {
            return Err((
                StatusCode::CONFLICT,
                format!("this capsule is {other}; its manifest is closed"),
            ))
        }
    }

    let done = sqlx::query("DELETE FROM capsule_manifest WHERE capsule_id = $1 AND order_id = $2")
        .bind(capsule_id)
        .bind(order_id)
        .execute(&state.db)
        .await
        .map_err(server_err("unbook failed"))?;

    if done.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "that order is not on this capsule".into()));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/capsules/:id/seal — close the manifest.
async fn seal(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let load = current_load(&state, id).await?;
    if load.bookings == 0 {
        return Err((StatusCode::BAD_REQUEST, "nothing is booked on this capsule".into()));
    }

    let updated = sqlx::query(
        "UPDATE capsules SET status='sealed', updated_at=NOW() WHERE id=$1 AND status='open' RETURNING name",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("seal failed"))?;

    let Some(row) = updated else {
        return Err((StatusCode::CONFLICT, "capsule is missing or not open".into()));
    };

    tracing::info!(capsule = %row.get::<String, _>("name"), bookings = load.bookings, "capsule sealed");
    Ok(Json(json!({
        "capsuleId": id,
        "status": "sealed",
        "bookings": load.bookings,
        "totalMassKg": load.mass.to_string(),
        "totalVolumeM3": load.volume.to_string(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// Decimal literal without pulling in another crate.
    fn d(v: &str) -> Decimal {
        Decimal::from_str(v).unwrap()
    }

    /// The fit check as `book` applies it.
    fn fits(cap_m: Decimal, cap_v: Decimal, booked_m: Decimal, booked_v: Decimal,
            m: Decimal, v: Decimal) -> bool {
        booked_m + m <= cap_m && booked_v + v <= cap_v
    }

    #[test]
    fn volume_can_bind_before_mass() {
        // The ordinary case, not the exotic one: light bulky cargo. A single
        // "capacity" number would have accepted this.
        assert!(!fits(d("1000"), d("2"), d("0"), d("0"), d("50"), d("3")));
        assert!(fits(d("1000"), d("5"), d("0"), d("0"), d("50"), d("3")));
    }

    #[test]
    fn mass_can_bind_before_volume() {
        assert!(!fits(d("100"), d("50"), d("0"), d("0"), d("150"), d("1")));
    }

    #[test]
    fn a_booking_that_exactly_fills_the_hull_is_allowed() {
        assert!(fits(d("100"), d("10"), d("60"), d("6"), d("40"), d("4")));
        // One gram over is not.
        assert!(!fits(d("100"), d("10"), d("60"), d("6"), d("40.001"), d("4")));
    }

    #[test]
    fn share_of_mass_is_the_fraction_of_the_hull_consumed() {
        // Recorded at booking so a split stays reproducible if the capsule's
        // stated capacity is corrected afterwards.
        let share = (d("250") / d("1000")).round_dp(6);
        assert_eq!(share, d("0.25"));
    }

    #[test]
    fn shares_of_a_shared_capsule_sum_to_the_fraction_used() {
        let cap = d("1000");
        let parties = [d("250"), d("300"), d("150")];
        let total: Decimal = parties.iter().sum();
        let shares: Decimal = parties.iter().map(|m| (m / cap).round_dp(6)).sum();
        assert_eq!(total, d("700"));
        assert_eq!(shares, d("0.7"));
    }
}
