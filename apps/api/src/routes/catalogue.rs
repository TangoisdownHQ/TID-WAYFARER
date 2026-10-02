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
        .route("/lookup", get(lookup))
        .route("/:id", get(detail).put(update))
        .route("/:id/suppliers", get(list_suppliers).post(add_supplier))
        .route("/:id/relations", get(list_relations).post(add_relation))
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
    pub hs_code: Option<String>,
    pub country_of_origin: Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub storage_temp_min_c: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub storage_temp_max_c: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub storage_humidity_max_pct: Option<Decimal>,
    pub hazard_class: Option<String>,
    pub un_number: Option<String>,
    pub storage_notes: Option<String>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub life_limit_cycles: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub life_limit_hours: Option<Decimal>,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub life_limit_days: Option<Decimal>,
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
        hs_code: r.get("hs_code"),
        country_of_origin: r.get("country_of_origin"),
        storage_temp_min_c: r.get("storage_temp_min_c"),
        storage_temp_max_c: r.get("storage_temp_max_c"),
        storage_humidity_max_pct: r.get("storage_humidity_max_pct"),
        hazard_class: r.get("hazard_class"),
        un_number: r.get("un_number"),
        storage_notes: r.get("storage_notes"),
        life_limit_cycles: r.get("life_limit_cycles"),
        life_limit_hours: r.get("life_limit_hours"),
        life_limit_days: r.get("life_limit_days"),
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
    pub hs_code: Option<String>,
    pub country_of_origin: Option<String>,
    pub storage_temp_min_c: Option<f64>,
    pub storage_temp_max_c: Option<f64>,
    pub storage_humidity_max_pct: Option<f64>,
    pub hazard_class: Option<String>,
    pub un_number: Option<String>,
    pub storage_notes: Option<String>,
    pub life_limit_cycles: Option<f64>,
    pub life_limit_hours: Option<f64>,
    pub life_limit_days: Option<f64>,
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
           datasheet_url, serialised, shelf_life_days, unit_mass_kg, unit_volume_m3, lead_time_days,
           hs_code, country_of_origin, storage_temp_min_c, storage_temp_max_c,
           storage_humidity_max_pct, hazard_class, un_number, storage_notes,
           life_limit_cycles, life_limit_hours, life_limit_days)
        VALUES
          (gen_random_uuid(), $1, $2, $3, $4, $5, $6, $7, $8,
           $9, COALESCE($10,'general'), COALESCE($11,'each'), COALESCE($12,0), COALESCE($13,0),
           $14, $15, $16, COALESCE($17,false), $18, $19, $20, $21,
           $22, $23, $24, $25, $26, $27, $28, $29, $30, $31, $32)
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
    .bind(clean(&b.hs_code)).bind(clean(&b.country_of_origin))
    .bind(dec(b.storage_temp_min_c)).bind(dec(b.storage_temp_max_c))
    .bind(dec(b.storage_humidity_max_pct))
    .bind(clean(&b.hazard_class)).bind(clean(&b.un_number)).bind(clean(&b.storage_notes))
    .bind(dec(b.life_limit_cycles)).bind(dec(b.life_limit_hours)).bind(dec(b.life_limit_days))
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
          lead_time_days  = COALESCE($20, lead_time_days),
          hs_code                  = COALESCE($21, hs_code),
          country_of_origin        = COALESCE($22, country_of_origin),
          storage_temp_min_c       = COALESCE($23, storage_temp_min_c),
          storage_temp_max_c       = COALESCE($24, storage_temp_max_c),
          storage_humidity_max_pct = COALESCE($25, storage_humidity_max_pct),
          hazard_class             = COALESCE($26, hazard_class),
          un_number                = COALESCE($27, un_number),
          storage_notes            = COALESCE($28, storage_notes),
          life_limit_cycles        = COALESCE($29, life_limit_cycles),
          life_limit_hours         = COALESCE($30, life_limit_hours),
          life_limit_days          = COALESCE($31, life_limit_days)
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
    .bind(clean(&b.hs_code)).bind(clean(&b.country_of_origin))
    .bind(dec(b.storage_temp_min_c)).bind(dec(b.storage_temp_max_c))
    .bind(dec(b.storage_humidity_max_pct))
    .bind(clean(&b.hazard_class)).bind(clean(&b.un_number)).bind(clean(&b.storage_notes))
    .bind(dec(b.life_limit_cycles)).bind(dec(b.life_limit_hours)).bind(dec(b.life_limit_days))
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

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LookupQuery {
    pub code: String,
}

/// GET /api/catalogue/lookup?code=…
///
/// One scan, any kind of label. A person at a shelf points a scanner at
/// whatever barcode is in front of them — a GTIN on the packaging, the
/// manufacturer's part number, a lot sticker, an individual serial — and
/// should not have to tell the system which of those they just scanned.
/// Making them choose a mode first is how scanning becomes slower than typing.
///
/// Resolution is ordered by how specific the match is: a serial names one
/// physical unit, a lot code names a batch, a part number or barcode names a
/// kind of thing. The most specific wins, because that is the one carrying
/// information the others cannot.
async fn lookup(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Query(q): Query<LookupQuery>,
) -> Result<Json<Value>, ApiError> {
    let code = q.code.trim();
    if code.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "nothing was scanned".into()));
    }
    let org = org_scope::caller_org(&state, &principal).await;

    // 1. A serial — one specific unit, with its own history.
    let unit = sqlx::query(
        r#"
        SELECT l.id AS lot_id, l.serial, l.lot_code, l.status, l.expires_at, l.supplier,
               l.pedigree, l.quantity, i.id AS inventory_id, i.name, i.part_number, i.unit, i.location
        FROM inventory_lots l
        JOIN inventory i ON i.id = l.inventory_id
        WHERE i.org_id IS NOT DISTINCT FROM $2 AND l.serial = $1
        "#,
    )
    .bind(code).bind(org)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("lookup failed"))?;

    if let Some(r) = unit {
        return Ok(Json(json!({
            "matched": "serial",
            "code": code,
            "resource": { "id": r.get::<Uuid,_>("inventory_id"), "name": r.get::<String,_>("name"),
                          "partNumber": r.get::<Option<String>,_>("part_number"),
                          "unit": r.get::<String,_>("unit"), "location": r.get::<Option<String>,_>("location") },
            "unit": { "lotId": r.get::<Uuid,_>("lot_id"), "serial": r.get::<Option<String>,_>("serial"),
                      "status": r.get::<String,_>("status"),
                      "expiresAt": r.get::<Option<DateTime<Utc>>,_>("expires_at"),
                      "supplier": r.get::<Option<String>,_>("supplier"),
                      "pedigree": r.get::<Value,_>("pedigree") },
        })));
    }

    // 2. A lot code — a batch, possibly several rows.
    let batch = sqlx::query(
        r#"
        SELECT l.id AS lot_id, l.lot_code, l.status, l.quantity, l.expires_at,
               i.id AS inventory_id, i.name, i.part_number, i.unit, i.location
        FROM inventory_lots l
        JOIN inventory i ON i.id = l.inventory_id
        WHERE i.org_id IS NOT DISTINCT FROM $2 AND l.lot_code = $1
        ORDER BY l.received_at
        "#,
    )
    .bind(code).bind(org)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("lookup failed"))?;

    if !batch.is_empty() {
        let first = &batch[0];
        return Ok(Json(json!({
            "matched": "lot",
            "code": code,
            "resource": { "id": first.get::<Uuid,_>("inventory_id"), "name": first.get::<String,_>("name"),
                          "partNumber": first.get::<Option<String>,_>("part_number"),
                          "unit": first.get::<String,_>("unit"),
                          "location": first.get::<Option<String>,_>("location") },
            "lots": batch.iter().map(|r| json!({
                "lotId": r.get::<Uuid,_>("lot_id"),
                "quantity": r.get::<Decimal,_>("quantity").to_string(),
                "status": r.get::<String,_>("status"),
                "expiresAt": r.get::<Option<DateTime<Utc>>,_>("expires_at"),
            })).collect::<Vec<_>>(),
        })));
    }

    // 3. A barcode or part number — a kind of thing.
    let kind = sqlx::query(&format!(
        "{SELECT} WHERE i.org_id IS NOT DISTINCT FROM $2 AND (i.barcode = $1 OR i.part_number = $1)"
    ))
    .bind(code).bind(org)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("lookup failed"))?;

    if let Some(r) = kind {
        let e = entry(&r);
        let matched = if e.barcode.as_deref() == Some(code) { "barcode" } else { "part_number" };
        return Ok(Json(json!({ "matched": matched, "code": code, "resource": e })));
    }

    // Not found is an answer, not an error: at a shelf the useful next step is
    // registering what you are holding, and a 404 would make the UI treat a
    // perfectly ordinary scan as a failure.
    Ok(Json(json!({
        "matched": Value::Null,
        "code": code,
        "note": "nothing in this catalogue carries that code",
    })))
}

// ---------------------------------------------------------------------------
// Supplier part numbers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewSupplierPart {
    pub supplier_name: String,
    pub supplier_part_number: Option<String>,
    pub minimum_order_qty: Option<f64>,
    pub pack_size: Option<f64>,
    pub unit_cost: Option<f64>,
    pub currency: Option<String>,
    pub lead_time_days: Option<f64>,
    pub preferred: Option<bool>,
    pub note: Option<String>,
}

/// POST /api/catalogue/:id/suppliers
///
/// A manufacturer's part number is not a distributor's SKU, and multi-sourcing
/// is normal: the same part arrives under different codes, in different pack
/// sizes, at different prices and lead times. Reordering without that is
/// guessing at what an order will actually cost.
async fn add_supplier(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<NewSupplierPart>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if b.supplier_name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a supplier needs a name".into()));
    }
    let org = org_scope::caller_org(&state, &principal).await;
    owned(&state, id, org).await?;

    let mut tx = state.db.begin().await.map_err(server_err("could not start transaction"))?;

    // One preferred source per part. Demoting the incumbent here rather than
    // letting the unique index reject the insert means "make this the
    // preferred supplier" does what it says instead of erroring.
    if b.preferred.unwrap_or(false) {
        sqlx::query("UPDATE supplier_parts SET preferred = false WHERE inventory_id = $1")
            .bind(id).execute(&mut *tx).await
            .map_err(server_err("could not update suppliers"))?;
    }

    let row = sqlx::query(
        r#"
        INSERT INTO supplier_parts
          (inventory_id, supplier_name, supplier_part_number, minimum_order_qty, pack_size,
           unit_cost, currency, lead_time_days, preferred, note)
        VALUES ($1,$2,$3,$4,$5,$6,COALESCE($7,'TIDAT'),$8,COALESCE($9,false),$10)
        ON CONFLICT (inventory_id, supplier_name, supplier_part_number) DO UPDATE SET
          minimum_order_qty = EXCLUDED.minimum_order_qty, pack_size = EXCLUDED.pack_size,
          unit_cost = EXCLUDED.unit_cost, lead_time_days = EXCLUDED.lead_time_days,
          preferred = EXCLUDED.preferred, note = EXCLUDED.note, active = true
        RETURNING id
        "#,
    )
    .bind(id).bind(b.supplier_name.trim()).bind(clean(&b.supplier_part_number))
    .bind(dec(b.minimum_order_qty)).bind(dec(b.pack_size)).bind(dec(b.unit_cost))
    .bind(clean(&b.currency)).bind(dec(b.lead_time_days)).bind(b.preferred).bind(clean(&b.note))
    .fetch_one(&mut *tx)
    .await
    .map_err(server_err("could not record the supplier"))?;

    tx.commit().await.map_err(server_err("could not commit"))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": row.get::<Uuid,_>("id") }))))
}

async fn list_suppliers(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let org = org_scope::caller_org(&state, &principal).await;
    owned(&state, id, org).await?;

    let rows = sqlx::query(
        "SELECT * FROM supplier_parts WHERE inventory_id = $1 AND active \
         ORDER BY preferred DESC, supplier_name",
    )
    .bind(id).fetch_all(&state.db).await
    .map_err(server_err("supplier list failed"))?;

    Ok(Json(rows.iter().map(|r| {
        let moq: Option<Decimal> = r.get("minimum_order_qty");
        let pack: Option<Decimal> = r.get("pack_size");
        let cost: Option<Decimal> = r.get("unit_cost");
        json!({
            "id": r.get::<Uuid,_>("id"),
            "supplier": r.get::<String,_>("supplier_name"),
            "supplierPartNumber": r.get::<Option<String>,_>("supplier_part_number"),
            "minimumOrderQty": moq.map(|v| v.to_string()),
            "packSize": pack.map(|v| v.to_string()),
            "unitCost": cost.map(|v| v.to_string()),
            "currency": r.get::<String,_>("currency"),
            "leadTimeDays": r.get::<Option<Decimal>,_>("lead_time_days").map(|v| v.to_string()),
            "preferred": r.get::<bool,_>("preferred"),
            // What ordering one actually costs: a minimum of 50 makes a
            // single-unit reorder fifty units' worth of money.
            "minimumSpend": match (moq, cost) {
                (Some(q), Some(c)) => json!((q * c).round_dp(2).to_string()),
                _ => Value::Null,
            },
            "note": r.get::<Option<String>,_>("note"),
        })
    }).collect()))
}

// ---------------------------------------------------------------------------
// Supersession and alternates
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewRelation {
    /// The other part.
    pub to_id: Uuid,
    /// `supersedes` — this part is replaced by that one — or `alternate`.
    pub kind: String,
    pub note: Option<String>,
    pub effective_from: Option<DateTime<Utc>>,
}

/// POST /api/catalogue/:id/relations
async fn add_relation(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Path(id): Path<Uuid>,
    Json(b): Json<NewRelation>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !["supersedes", "alternate"].contains(&b.kind.as_str()) {
        return Err((StatusCode::BAD_REQUEST, "kind must be 'supersedes' or 'alternate'".into()));
    }
    if b.to_id == id {
        return Err((StatusCode::BAD_REQUEST, "a part cannot replace itself".into()));
    }
    let org = org_scope::caller_org(&state, &principal).await;
    // Both ends must be ours; relating to a part in another org would leak its
    // existence and make the graph unresolvable from either side.
    owned(&state, id, org).await?;
    owned(&state, b.to_id, org).await?;

    sqlx::query(
        "INSERT INTO part_relations (from_id, to_id, kind, note, effective_from) \
         VALUES ($1,$2,$3,$4,$5) ON CONFLICT (from_id, to_id, kind) DO UPDATE SET note = EXCLUDED.note",
    )
    .bind(id).bind(b.to_id).bind(&b.kind).bind(clean(&b.note)).bind(b.effective_from)
    .execute(&state.db).await
    .map_err(server_err("could not record the relation"))?;

    Ok((StatusCode::CREATED, Json(json!({ "from": id, "to": b.to_id, "kind": b.kind }))))
}

/// GET /api/catalogue/:id/relations
///
/// Resolves the supersession chain forward to whatever is current. Ordering a
/// superseded part should offer its replacement, and a chain of three is
/// common enough that stopping at the first hop would still leave an
/// unorderable number.
async fn list_relations(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_scope::caller_org(&state, &principal).await;
    owned(&state, id, org).await?;

    let brief = |r: &sqlx::postgres::PgRow| json!({
        "id": r.get::<Uuid,_>("id"),
        "name": r.get::<String,_>("name"),
        "partNumber": r.get::<Option<String>,_>("part_number"),
        "quantity": r.get::<i32,_>("quantity"),
    });

    // Forward through supersession, bounded so a cycle recorded by mistake
    // cannot spin forever.
    let chain = sqlx::query(
        r#"
        WITH RECURSIVE forward(id, depth) AS (
            SELECT to_id, 1 FROM part_relations WHERE from_id = $1 AND kind = 'supersedes'
            UNION
            SELECT r.to_id, f.depth + 1
            FROM part_relations r JOIN forward f ON r.from_id = f.id
            WHERE r.kind = 'supersedes' AND f.depth < 10
        )
        SELECT i.id, i.name, i.part_number, i.quantity, f.depth
        FROM forward f JOIN inventory i ON i.id = f.id
        ORDER BY f.depth
        "#,
    )
    .bind(id).fetch_all(&state.db).await
    .map_err(server_err("supersession lookup failed"))?;

    let superseded_by_us = sqlx::query(
        "SELECT i.id, i.name, i.part_number, i.quantity FROM part_relations r \
         JOIN inventory i ON i.id = r.from_id WHERE r.to_id = $1 AND r.kind = 'supersedes'",
    )
    .bind(id).fetch_all(&state.db).await
    .map_err(server_err("supersession lookup failed"))?;

    // Symmetric: stored once, read from either end.
    let alternates = sqlx::query(
        r#"
        SELECT i.id, i.name, i.part_number, i.quantity FROM part_relations r
        JOIN inventory i ON i.id = CASE WHEN r.from_id = $1 THEN r.to_id ELSE r.from_id END
        WHERE r.kind = 'alternate' AND ($1 IN (r.from_id, r.to_id))
        "#,
    )
    .bind(id).fetch_all(&state.db).await
    .map_err(server_err("alternate lookup failed"))?;

    Ok(Json(json!({
        "resourceId": id,
        // The end of the chain: what to actually order today.
        "currentPart": chain.last().map(brief),
        "supersededBy": chain.iter().map(brief).collect::<Vec<_>>(),
        "replaces": superseded_by_us.iter().map(brief).collect::<Vec<_>>(),
        "alternates": alternates.iter().map(brief).collect::<Vec<_>>(),
        "obsolete": !chain.is_empty(),
    })))
}

/// Confirm a resource belongs to the caller's organisation.
///
/// Not-found and not-yours give the same answer: telling a caller that a
/// resource exists in another organisation is itself a disclosure.
async fn owned(state: &AppState, id: Uuid, org: Option<Uuid>) -> Result<(), ApiError> {
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM inventory WHERE id = $1 AND org_id IS NOT DISTINCT FROM $2")
            .bind(id).bind(org)
            .fetch_optional(&state.db).await
            .map_err(server_err("resource lookup failed"))?;
    found.map(|_| ()).ok_or((StatusCode::NOT_FOUND, "no such resource".to_string()))
}
