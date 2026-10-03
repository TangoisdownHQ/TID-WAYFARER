//! Structured destinations.
//!
//! The order model expressed a destination as a body id, lat/lon/alt and a
//! free-text address. That is right for a lander on Mars and useless for a
//! carrier on Earth: free text has no country to rate against, no postcode to
//! zone, and no phone, which international services require. Carriers reject
//! malformed addresses, and free text is malformed by default.
//!
//! This does not replace the body model. `body_id` selects which half of a row
//! is meaningful — Earth (399) gets a postal address, anywhere else gets
//! coordinates — so a tool whose premise is cross-domain logistics does not
//! have to choose one.
//!
//! Every read is scoped to the caller's organisation through `org_scope`,
//! which fails closed. An address book is a customer list; leaking one leaks
//! who somebody's customers are.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::Caller;
use crate::services::org_scope::caller_org;
use crate::AppState;

type ApiError = (StatusCode, String);

/// The body id that means Earth, and therefore "a postal address applies".
/// Matches the convention already used by orders and rate cards.
pub const EARTH: i64 = 399;

pub fn address_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_addresses).post(create_address))
        .route("/:id", patch(update_address).get(get_address).delete(delete_address))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "address route failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "request failed".to_string())
}

async fn org_of(state: &AppState, caller: &Caller) -> Result<Uuid, ApiError> {
    let Caller(principal) = caller;
    caller_org(state, principal)
        .await
        .ok_or((StatusCode::FORBIDDEN, "no organisation for this caller".to_string()))
}

#[derive(Deserialize)]
pub struct NewAddress {
    pub label: Option<String>,
    pub body_id: Option<i64>,
    pub name: Option<String>,
    pub company: Option<String>,
    pub line1: Option<String>,
    pub line2: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub postcode: Option<String>,
    /// ISO 3166-1 alpha-2. Normalised to uppercase; a two-letter code is the
    /// only form a carrier accepts.
    pub country: Option<String>,
    pub phone: Option<String>,
    pub email: Option<String>,
    pub residential: Option<bool>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt: Option<f64>,
}

/// Check what a carrier would check, before a carrier is asked.
///
/// Doing this here rather than letting the provider reject it means an
/// operator sees "country is required" while the form is still open, instead
/// of a provider error minutes later — and it means an outpost with no link
/// still catches the mistake.
fn validate(a: &NewAddress) -> Result<(i64, Option<String>), ApiError> {
    let body = a.body_id.unwrap_or(EARTH);
    let bad = |m: String| (StatusCode::BAD_REQUEST, m);

    if body == EARTH {
        let missing: Vec<&str> = [
            ("line1", a.line1.as_deref()),
            ("city", a.city.as_deref()),
            ("country", a.country.as_deref()),
        ]
        .iter()
        .filter(|(_, v)| v.map(str::trim).unwrap_or("").is_empty())
        .map(|(k, _)| *k)
        .collect();

        if !missing.is_empty() {
            return Err(bad(format!(
                "an address on Earth needs {} — a carrier cannot rate without them",
                missing.join(", ")
            )));
        }

        let country = a.country.as_deref().unwrap_or("").trim().to_uppercase();
        if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(bad(format!(
                "country must be a two-letter ISO code (got '{country}') — 'US', not 'USA' or 'United States'"
            )));
        }
        return Ok((body, Some(country)));
    }

    // Off Earth, coordinates are the only thing that can locate anything.
    if a.lat.is_none() || a.lon.is_none() {
        return Err(bad(
            "a destination off Earth needs lat and lon — there is no postal network to address".into(),
        ));
    }
    if let Some(lat) = a.lat {
        if !(-90.0..=90.0).contains(&lat) {
            return Err(bad(format!("lat must be between -90 and 90 (got {lat})")));
        }
    }
    if let Some(lon) = a.lon {
        if !(-180.0..=180.0).contains(&lon) {
            return Err(bad(format!("lon must be between -180 and 180 (got {lon})")));
        }
    }
    Ok((body, a.country.as_deref().map(|c| c.trim().to_uppercase()).filter(|c| c.len() == 2)))
}

fn clean(v: Option<&String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

#[derive(Deserialize)]
pub struct AddressFilter {
    /// Free-text match on label, company, city or postcode.
    pub q: Option<String>,
    pub country: Option<String>,
    pub body_id: Option<i64>,
    pub limit: Option<i64>,
}

/// GET /api/addresses
async fn list_addresses(
    State(state): State<AppState>,
    caller: Caller,
    Query(f): Query<AddressFilter>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let pattern = f.q.as_deref().map(|q| format!("%{}%", q.trim().to_lowercase()));

    let rows = sqlx::query(
        r#"
        SELECT * FROM addresses
        WHERE org_id = $1
          AND ($2::text IS NULL OR
               lower(coalesce(label,'')) LIKE $2 OR
               lower(coalesce(company,'')) LIKE $2 OR
               lower(coalesce(city,'')) LIKE $2 OR
               lower(coalesce(postcode,'')) LIKE $2)
          AND ($3::text IS NULL OR country = $3)
          AND ($4::bigint IS NULL OR body_id = $4)
        ORDER BY coalesce(label, company, city, '') ASC
        LIMIT $5
        "#,
    )
    .bind(org)
    .bind(&pattern)
    .bind(f.country.as_deref().map(|c| c.to_uppercase()))
    .bind(f.body_id)
    .bind(f.limit.unwrap_or(200).clamp(1, 1000))
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let addresses: Vec<Value> = rows.iter().map(row_to_json).collect();
    Ok(Json(json!({ "count": addresses.len(), "addresses": addresses })))
}

pub fn row_to_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<Uuid, _>("id"),
        "label": r.get::<Option<String>, _>("label"),
        "bodyId": r.get::<i64, _>("body_id"),
        "name": r.get::<Option<String>, _>("name"),
        "company": r.get::<Option<String>, _>("company"),
        "line1": r.get::<Option<String>, _>("line1"),
        "line2": r.get::<Option<String>, _>("line2"),
        "city": r.get::<Option<String>, _>("city"),
        "region": r.get::<Option<String>, _>("region"),
        "postcode": r.get::<Option<String>, _>("postcode"),
        "country": r.get::<Option<String>, _>("country"),
        "phone": r.get::<Option<String>, _>("phone"),
        "email": r.get::<Option<String>, _>("email"),
        "residential": r.get::<Option<bool>, _>("residential"),
        "lat": r.get::<Option<f64>, _>("lat"),
        "lon": r.get::<Option<f64>, _>("lon"),
        "alt": r.get::<Option<f64>, _>("alt"),
        "validatedAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("validated_at")
            .map(|d| d.to_rfc3339()),
        "oneLine": one_line(r),
    })
}

/// A single-line rendering, because every list and every label needs one and
/// each client reinventing it produces four different formats.
fn one_line(r: &sqlx::postgres::PgRow) -> String {
    let g = |k: &str| r.get::<Option<String>, _>(k).filter(|s| !s.is_empty());
    if r.get::<i64, _>("body_id") != EARTH {
        let lat = r.get::<Option<f64>, _>("lat").unwrap_or_default();
        let lon = r.get::<Option<f64>, _>("lon").unwrap_or_default();
        return format!(
            "{} ({lat:.4}, {lon:.4})",
            g("label").unwrap_or_else(|| format!("body {}", r.get::<i64, _>("body_id")))
        );
    }
    [g("company"), g("line1"), g("line2"), g("city"), g("region"), g("postcode"), g("country")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
}

/// POST /api/addresses
async fn create_address(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<NewAddress>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let org = org_of(&state, &caller).await?;
    let (body_id, country) = validate(&body)?;

    let row = sqlx::query(
        r#"
        INSERT INTO addresses
          (org_id, label, body_id, name, company, line1, line2, city, region,
           postcode, country, phone, email, residential, lat, lon, alt)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)
        RETURNING *
        "#,
    )
    .bind(org)
    .bind(clean(body.label.as_ref()))
    .bind(body_id)
    .bind(clean(body.name.as_ref()))
    .bind(clean(body.company.as_ref()))
    .bind(clean(body.line1.as_ref()))
    .bind(clean(body.line2.as_ref()))
    .bind(clean(body.city.as_ref()))
    .bind(clean(body.region.as_ref()))
    .bind(clean(body.postcode.as_ref()))
    .bind(country)
    .bind(clean(body.phone.as_ref()))
    .bind(clean(body.email.as_ref()))
    .bind(body.residential)
    .bind(body.lat)
    .bind(body.lon)
    .bind(body.alt)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    Ok((StatusCode::CREATED, Json(row_to_json(&row))))
}

/// GET /api/addresses/:id
async fn get_address(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let row = sqlx::query("SELECT * FROM addresses WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        // 404 rather than 403: confirming an address exists in another
        // organisation leaks that they ship there.
        .ok_or((StatusCode::NOT_FOUND, "no such address".to_string()))?;
    Ok(Json(row_to_json(&row)))
}

/// PATCH /api/addresses/:id
///
/// A full replace rather than a field-by-field merge, because the Earth/body
/// validity rule is a property of the whole row: patching `country` to null
/// alone would otherwise leave a row the constraint rejects, and the error
/// would name the constraint rather than the mistake.
async fn update_address(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<NewAddress>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let (body_id, country) = validate(&body)?;

    let row = sqlx::query(
        r#"
        UPDATE addresses SET
          label=$3, body_id=$4, name=$5, company=$6, line1=$7, line2=$8, city=$9,
          region=$10, postcode=$11, country=$12, phone=$13, email=$14,
          residential=$15, lat=$16, lon=$17, alt=$18,
          -- Changing an address invalidates any previous validation: the
          -- carrier approved the old one.
          validated_at=NULL, validation_note=NULL, updated_at=NOW()
        WHERE id=$1 AND org_id=$2
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(org)
    .bind(clean(body.label.as_ref()))
    .bind(body_id)
    .bind(clean(body.name.as_ref()))
    .bind(clean(body.company.as_ref()))
    .bind(clean(body.line1.as_ref()))
    .bind(clean(body.line2.as_ref()))
    .bind(clean(body.city.as_ref()))
    .bind(clean(body.region.as_ref()))
    .bind(clean(body.postcode.as_ref()))
    .bind(country)
    .bind(clean(body.phone.as_ref()))
    .bind(clean(body.email.as_ref()))
    .bind(body.residential)
    .bind(body.lat)
    .bind(body.lon)
    .bind(body.alt)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such address".to_string()))?;

    Ok(Json(row_to_json(&row)))
}

/// DELETE /api/addresses/:id
///
/// Refused while a shipment still points at it. An address is the record of
/// where something was sent, and a shipment whose destination vanished cannot
/// be audited — the same reason a person who signed a custody receipt is
/// deactivated rather than deleted.
async fn delete_address(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;

    let used: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM carrier_shipments WHERE from_address_id = $1 OR to_address_id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    if used > 0 {
        return Err((
            StatusCode::CONFLICT,
            format!("{used} shipment(s) were sent to or from this address; it cannot be deleted"),
        ));
    }

    let done = sqlx::query("DELETE FROM addresses WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org)
        .execute(&state.db)
        .await
        .map_err(internal)?
        .rows_affected();

    if done == 0 {
        return Err((StatusCode::NOT_FOUND, "no such address".to_string()));
    }
    Ok(Json(json!({ "deleted": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn earth() -> NewAddress {
        NewAddress {
            label: Some("Chicago depot".into()), body_id: None, name: None,
            company: Some("TIDHQ".into()), line1: Some("1 Dock Rd".into()), line2: None,
            city: Some("Chicago".into()), region: Some("IL".into()),
            postcode: Some("60601".into()), country: Some("us".into()),
            phone: None, email: None, residential: Some(false),
            lat: None, lon: None, alt: None,
        }
    }

    /// An Earth address without the fields a carrier rates on is refused
    /// while the form is still open, not minutes later by a provider.
    #[test]
    fn an_earth_address_needs_what_a_carrier_needs() {
        let mut a = earth();
        a.country = None;
        let err = validate(&a).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("country"), "{}", err.1);

        let mut a = earth();
        a.line1 = Some("   ".into());
        a.city = None;
        let err = validate(&a).unwrap_err();
        // Both missing fields are named, so one round trip fixes both.
        assert!(err.1.contains("line1") && err.1.contains("city"), "{}", err.1);
    }

    /// Country is normalised, and the long forms people actually type are
    /// refused with the right answer rather than silently stored.
    #[test]
    fn country_is_normalised_to_an_iso_code() {
        assert_eq!(validate(&earth()).unwrap().1.as_deref(), Some("US"));

        let mut a = earth();
        a.country = Some("USA".into());
        let err = validate(&a).unwrap_err();
        assert!(err.1.contains("'US', not 'USA'"), "{}", err.1);
    }

    /// Off Earth there is no postal network, so coordinates are the address.
    #[test]
    fn an_off_earth_destination_needs_coordinates() {
        let mut a = earth();
        a.body_id = Some(499); // Mars
        a.line1 = None; a.city = None; a.country = None;
        let err = validate(&a).unwrap_err();
        assert!(err.1.contains("lat and lon"), "{}", err.1);

        a.lat = Some(18.38);
        a.lon = Some(77.58);
        assert_eq!(validate(&a).unwrap().0, 499);
    }

    /// The body model is extended, not replaced: an Earth row and a Mars row
    /// are both valid and validated differently.
    #[test]
    fn both_kinds_of_destination_are_valid() {
        assert_eq!(validate(&earth()).unwrap().0, EARTH);

        let mars = NewAddress {
            label: Some("Jezero cache".into()), body_id: Some(499),
            lat: Some(18.38), lon: Some(77.58), alt: Some(-2500.0),
            name: None, company: None, line1: None, line2: None, city: None,
            region: None, postcode: None, country: None, phone: None,
            email: None, residential: None,
        };
        assert_eq!(validate(&mars).unwrap().0, 499);
    }

    #[test]
    fn impossible_coordinates_are_refused() {
        let mut a = earth();
        a.body_id = Some(499);
        a.lat = Some(120.0);
        a.lon = Some(0.0);
        assert!(validate(&a).unwrap_err().1.contains("-90 and 90"));
    }
}
