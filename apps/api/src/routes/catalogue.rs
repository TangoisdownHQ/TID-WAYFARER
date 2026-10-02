//! The resource catalogue: registering what a thing *is*.
//!
//! `inventory` previously described a quantity with a name, which is enough
//! for bulk consumables and useless for a part. A drone motor, a server PSU,
//! an EV cell module and an aircraft actuator are each identified by a
//! manufacturer part number, and without one two rows called "pump" could be
//! entirely different pumps with no way to tell.
//!
//! The existing create path ran through a ten-argument helper that did not
//! even accept a location. Rather than add four more positional arguments to
//! it, registration lives here with the full record in one place.
//!
//! Scoped by organisation throughout: a catalogue is commercially sensitive —
//! which parts you stock and from whom is competitive information — and
//! `inventory` is deliberately not a grantable cross-org scope.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AuthenticatedUser, Caller};
use crate::services::org_scope;
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

fn dec(v: Option<f64>) -> Option<Decimal> {
    v.and_then(|x| Decimal::try_from(x).ok())
}

pub fn catalogue_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/:id", get(detail).put(update))
}

#[derive(Serialize)]
pub struct CatalogueEntry {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub part_number: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub revision: Option<String>,
    pub barcode: Option<String>,
    pub category: String,
    pub unit: String,
    pub quantity: i32,
    pub threshold: i32,
    pub location: Option<String>,
    pub specification: Value,
    pub datasheet_url: Option<String>,
    pub serialised: bool,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub shelf_life_days: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub unit_mass_kg: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub unit_volume_m3: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub lead_time_days: Option<Decimal>,
    pub lot_count: i64,
    pub created_at: DateTime<Utc>,
}

fn entry(r: &sqlx::postgres::PgRow) -> CatalogueEntry {
    CatalogueEntry {
        id: r.get("id"),
        name: r.get("name"),
        description: r.get("description"),
        part_number: r.get("part_number"),
        manufacturer: r.get("manufacturer"),
        model: r.get("model"),
        revision: r.get("revision"),
        barcode: r.get("barcode"),
        category: r.get("category"),
        unit: r.get("unit"),
        quantity: r.get("quantity"),
        threshold: r.get("threshold"),
        location: r.get("location"),
        specification: r.get("specification"),
        datasheet_url: r.get("datasheet_url"),
        serialised: r.get("serialised"),
        shelf_life_days: r.get("shelf_life_days"),
        unit_mass_kg: r.get("unit_mass_kg"),
        unit_volume_m3: r.get("unit_volume_m3"),
        lead_time_days: r.get("lead_time_days"),
        lot_count: r.try_get("lot_count").unwrap_or(0),
        created_at: r.get::<chrono::NaiveDateTime, _>("created_at").and_utc(),
    }
}

const SELECT: &str = r#"
    SELECT i.*, COALESCE(l.n, 0) AS lot_count
    FROM inventory i
    LEFT JOIN (SELECT inventory_id, COUNT(*) AS n FROM inventory_lots GROUP BY inventory_id) l
           ON l.inventory_id = i.id
"#;

#[derive(Deserialize)]
pub struct ListFilter {
    pub category: Option<String>,
    pub manufacturer: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Query(f): Query<ListFilter>,
) -> Result<Json<Vec<CatalogueEntry>>, ApiError> {
    let org = org_scope::caller_org(&state, &principal).await;
    let rows = sqlx::query(&format!(
        "{SELECT} WHERE i.org_id IS NOT DISTINCT FROM $1
           AND ($2::text IS NULL OR i.category = $2)
           AND ($3::text IS NULL OR i.manufacturer = $3)
         ORDER BY i.name"
    ))
    .bind(org)
    .bind(&f.category)
    .bind(&f.manufacturer)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("catalogue list failed"))?;

    Ok(Json(rows.iter().map(entry).collect()))
}

async fn detail(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_scope::caller_org(&state, &principal).await;
    let row = sqlx::query(&format!(
        "{SELECT} WHERE i.id = $1 AND i.org_id IS NOT DISTINCT FROM $2"
    ))
    .bind(id)
    .bind(org)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("catalogue lookup failed"))?
    .ok_or((StatusCode::NOT_FOUND, "no such resource".to_string()))?;

    let e = entry(&row);

    // A serialised part with no serials recorded is a catalogue entry nobody
    // has actually tracked yet. Worth saying, because the whole point of
    // marking it serialised was to make each unit identifiable.
    let untracked = e.serialised && e.lot_count == 0 && e.quantity > 0;

    Ok(Json(json!({
        "resource": e,
        "untrackedSerialised": untracked,
        "note": if untracked {
            json!("marked as serialised but no serials are recorded — the units on hand are not individually identifiable")
        } else { Value::Null },
    })))
}

#[derive(Deserialize)]
pub struct NewEntry {
    pub name: String,
    pub description: Option<String>,
    pub part_number: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub revision: Option<String>,
    pub barcode: Option<String>,
    pub category: Option<String>,
    pub unit: Option<String>,
    pub quantity: Option<i32>,
    pub threshold: Option<i32>,
    pub location: Option<String>,
    pub specification: Option<Value>,
    pub datasheet_url: Option<String>,
    pub serialised: Option<bool>,
    pub shelf_life_days: Option<f64>,
    pub unit_mass_kg: Option<f64>,
    pub unit_volume_m3: Option<f64>,
    pub lead_time_days: Option<f64>,
}

/// Trim to None rather than storing an empty string: a blank part number that
/// is `""` rather than NULL defeats the unique index, so two blank entries
/// would collide while two genuinely unnumbered ones should not.
fn clean(v: &Option<String>) -> Option<String> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

async fn create(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Caller(principal): Caller,
    Json(b): Json<NewEntry>,
) -> Result<(StatusCode, Json<CatalogueEntry>), ApiError> {
    if b.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a resource needs a name".into()));
    }
    let org = org_scope::caller_org(&state, &principal).await;
    if org.is_none() {
        // Without an org the row would be invisible to its own creator.
        return Err((
            StatusCode::FORBIDDEN,
            "you are not a member of an organisation, so there is nowhere to file this".into(),
        ));
    }
    let owner = Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "token subject is not a user id".to_string()))?;

    let spec = b.specification.clone().unwrap_or_else(|| json!({}));
    if !spec.is_object() {
        return Err((StatusCode::BAD_REQUEST, "specification must be an object".into()));
    }

    let row = sqlx::query(
        r#"
        INSERT INTO inventory
          (id, owner_id, org_id, name, description, part_number, manufacturer, model, revision,
           barcode, category, unit, quantity, threshold, location, specification,
           datasheet_url, serialised, shelf_life_days, unit_mass_kg, unit_volume_m3, lead_time_days)
        VALUES
          (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8,
           $9, COALESCE($10,'general'), COALESCE($11,'each'), COALESCE($12,0), COALESCE($13,0),
           $14, $15, $16, COALESCE($17,false), $18, $19, $20, $21)
        RETURNING *, 0::int8 AS lot_count
        "#,
    )
    .bind(owner).bind(org).bind(b.name.trim()).bind(clean(&b.description))
    .bind(clean(&b.part_number)).bind(clean(&b.manufacturer)).bind(clean(&b.model))
    .bind(clean(&b.revision)).bind(clean(&b.barcode))
    .bind(clean(&b.category)).bind(clean(&b.unit))
    .bind(b.quantity).bind(b.threshold).bind(clean(&b.location))
    .bind(&spec).bind(clean(&b.datasheet_url)).bind(b.serialised)
    .bind(dec(b.shelf_life_days)).bind(dec(b.unit_mass_kg))
    .bind(dec(b.unit_volume_m3)).bind(dec(b.lead_time_days))
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (
                    StatusCode::CONFLICT,
                    "that part number is already in your catalogue — two records of one part \
                     defeat every count and every recall that depends on it".into(),
                );
            }
        }
        tracing::error!(error = %e, "catalogue insert failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not register the resource".into())
    })?;

    tracing::info!(name = %b.name, part = ?clean(&b.part_number), "resource registered");
    Ok((StatusCode::CREATED, Json(entry(&row))))
}

async fn update(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<NewEntry>,
) -> Result<Json<CatalogueEntry>, ApiError> {
    let org = org_scope::caller_org(&state, &principal).await;

    // COALESCE throughout, so a partial payload edits only what it names
    // rather than blanking every field the form did not send.
    let row = sqlx::query(
        r#"
        UPDATE inventory SET
          name            = COALESCE($3, name),
          description     = COALESCE($4, description),
          part_number     = COALESCE($5, part_number),
          manufacturer    = COALESCE($6, manufacturer),
          model           = COALESCE($7, model),
          revision        = COALESCE($8, revision),
          barcode         = COALESCE($9, barcode),
          category        = COALESCE($10, category),
          unit            = COALESCE($11, unit),
          threshold       = COALESCE($12, threshold),
          location        = COALESCE($13, location),
          specification   = COALESCE($14, specification),
          datasheet_url   = COALESCE($15, datasheet_url),
          serialised      = COALESCE($16, serialised),
          shelf_life_days = COALESCE($17, shelf_life_days),
          unit_mass_kg    = COALESCE($18, unit_mass_kg),
          unit_volume_m3  = COALESCE($19, unit_volume_m3),
          lead_time_days  = COALESCE($20, lead_time_days)
        WHERE id = $1 AND org_id IS NOT DISTINCT FROM $2
        RETURNING *, (SELECT COUNT(*) FROM inventory_lots WHERE inventory_id = $1) AS lot_count
        "#,
    )
    .bind(id).bind(org)
    .bind(clean(&Some(b.name.clone()))).bind(clean(&b.description))
    .bind(clean(&b.part_number)).bind(clean(&b.manufacturer)).bind(clean(&b.model))
    .bind(clean(&b.revision)).bind(clean(&b.barcode))
    .bind(clean(&b.category)).bind(clean(&b.unit))
    .bind(b.threshold).bind(clean(&b.location))
    .bind(&b.specification).bind(clean(&b.datasheet_url)).bind(b.serialised)
    .bind(dec(b.shelf_life_days)).bind(dec(b.unit_mass_kg))
    .bind(dec(b.unit_volume_m3)).bind(dec(b.lead_time_days))
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (StatusCode::CONFLICT, "that part number is already in your catalogue".into());
            }
        }
        tracing::error!(error = %e, "catalogue update failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not update the resource".into())
    })?
    // Not found and not-yours are the same answer: telling a caller a resource
    // exists in another org is itself a disclosure.
    .ok_or((StatusCode::NOT_FOUND, "no such resource".to_string()))?;

    Ok(Json(entry(&row)))
}
