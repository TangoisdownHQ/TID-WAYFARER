//! Shipments handed to a commercial carrier.
//!
//! # A carrier is a leg, not a participant
//!
//! The marketplace models a shipment as one outpost bidding to carry another's
//! order, which only works when both sides run Wayfarer. UPS will never run an
//! outpost, hold a TIDAT wallet, or sign a custody receipt — so a commercial
//! carrier is modelled as a subcontracted **leg** of a shipment that a real
//! participant stays accountable for. The depot that bid still signed the bid,
//! still gets paid in TIDAT, and still owes the delivery.
//!
//! That also means this works with no marketplace at all: an organisation
//! shipping its own stock to its own customers has no order and no bid, and
//! gets labels, tracking and customs paperwork anyway. That single-org case is
//! the point — the marketplace is what you grow into, not what you need before
//! anything works.
//!
//! # Where the custody chain picks it up
//!
//! A carrier cannot sign, so handing a parcel over produces a custody receipt
//! with `to_node_id` null, `to_label` naming the carrier and tracking number,
//! and `verified = false`. The tracking number is the evidence that stands in
//! for a signature, and the receipt says as much rather than pretending
//! otherwise. The existing chain already had the columns for this.
//!
//! # What settles in what
//!
//! Carrier money is fiat. A carrier invoices USD on net-30 terms; the bidding
//! outpost recovers that in its bid price and settles with the buyer in TIDAT.
//! Keeping the two apart is what keeps a bid comparable.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AuthenticatedUser, Caller};
use crate::services::carriers::{self, CarrierError, Provider};
use crate::services::org_scope::caller_org;
use crate::AppState;

type ApiError = (StatusCode, String);

pub fn carrier_routes() -> Router<AppState> {
    Router::new()
        .route("/accounts", get(list_accounts).post(add_account))
        .route("/rates", post(get_rates))
        .route("/shipments", get(list_shipments).post(create_shipment))
        .route("/shipments/:id", get(get_shipment))
        .route("/shipments/:id/purchase", post(purchase))
        .route("/shipments/:id/track", post(track_now))
        .route("/shipments/:id/handover", post(record_handover))
        .route("/check", post(check_acceptance))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "carrier route failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "request failed".to_string())
}

async fn org_of(state: &AppState, caller: &Caller) -> Result<Uuid, ApiError> {
    let Caller(principal) = caller;
    caller_org(state, principal)
        .await
        .ok_or((StatusCode::FORBIDDEN, "no organisation for this caller".to_string()))
}

/// Map a provider failure onto a status an operator can act on.
///
/// `NotSupported` is a 409 rather than a 501: it means "this account has no
/// API, do it by hand", which is a state of the account and not a gap in the
/// server.
fn carrier_status(e: &CarrierError) -> StatusCode {
    match e {
        CarrierError::NotSupported(_) => StatusCode::CONFLICT,
        CarrierError::Refused(_) => StatusCode::UNPROCESSABLE_ENTITY,
        CarrierError::Unreachable(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewAccount {
    carrier: String,
    /// manual | easypost. Defaults to manual, which always works.
    provider: Option<String>,
    nickname: Option<String>,
    account_ref: Option<String>,
}

async fn list_accounts(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let rows = sqlx::query(
        "SELECT id, carrier, provider, nickname, account_ref, active, created_at \
         FROM carrier_accounts WHERE org_id = $1 ORDER BY carrier",
    )
    .bind(org)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let accounts: Vec<Value> = rows
        .iter()
        .map(|r| {
            let provider: String = r.get("provider");
            json!({
                "id": r.get::<Uuid, _>("id"),
                "carrier": r.get::<String, _>("carrier"),
                "provider": provider,
                "nickname": r.get::<Option<String>, _>("nickname"),
                "accountRef": r.get::<Option<String>, _>("account_ref"),
                "active": r.get::<bool, _>("active"),
                // Whether the provider is *actually* usable, which is a
                // different question from which one was configured: a key can
                // be missing from the environment.
                "online": Provider::resolve(&provider).is_online(),
            })
        })
        .collect();

    Ok(Json(json!({ "count": accounts.len(), "accounts": accounts })))
}

async fn add_account(
    State(state): State<AppState>,
    caller: Caller,
    Json(b): Json<NewAccount>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let org = org_of(&state, &caller).await?;
    let carrier = b.carrier.trim().to_lowercase();
    if carrier.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "carrier is required".into()));
    }
    let provider = match b.provider.as_deref().map(str::trim).unwrap_or("manual") {
        p @ ("manual" | "easypost") => p.to_string(),
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("provider must be manual or easypost (got '{other}')"),
            ))
        }
    };

    let row = sqlx::query(
        "INSERT INTO carrier_accounts (org_id, carrier, provider, nickname, account_ref) \
         VALUES ($1,$2,$3,$4,$5) \
         ON CONFLICT (org_id, carrier, provider) DO UPDATE \
           SET nickname = EXCLUDED.nickname, account_ref = EXCLUDED.account_ref, active = TRUE \
         RETURNING id",
    )
    .bind(org)
    .bind(&carrier)
    .bind(&provider)
    .bind(b.nickname.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(b.account_ref.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": row.get::<Uuid, _>("id"),
            "carrier": carrier,
            "provider": provider,
            "online": Provider::resolve(&provider).is_online(),
        })),
    ))
}

// ---------------------------------------------------------------------------
// Acceptance: what the carrier will refuse to carry
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AcceptanceQuery {
    carrier: String,
    service: Option<String>,
    /// Catalogue items to check. Hazard class and UN number are read from the
    /// catalogue rather than taken from the request, so a caller cannot
    /// declare a lithium pack as harmless to get a label.
    inventory_ids: Vec<Uuid>,
}

/// POST /api/carriers/check — would this carrier take these items?
///
/// The catalogue already records `hazard_class` and `un_number`, and UN3480
/// (standalone lithium cells) is the single most common carrier refusal. So
/// the data to check against was already there; this checks it, before a label
/// is bought rather than when the parcel is rejected at the counter.
///
/// Deliberately *advisory as well as blocking*: `requires_declaration` is a
/// real outcome, not a failure, and an operator needs to know which of the two
/// they are facing.
async fn check_acceptance(
    State(state): State<AppState>,
    caller: Caller,
    Json(q): Json<AcceptanceQuery>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let findings = acceptance_findings(&state, org, &q.carrier, q.service.as_deref(), &q.inventory_ids).await?;

    let forbidden: Vec<&Value> = findings.iter().filter(|f| f["rule"] == "forbidden").collect();
    let declarations: Vec<&Value> =
        findings.iter().filter(|f| f["rule"] == "requires_declaration").collect();

    Ok(Json(json!({
        "carrier": q.carrier,
        "service": q.service,
        "acceptable": forbidden.is_empty(),
        "forbidden": forbidden,
        "requiresDeclaration": declarations,
        // Said plainly, because a rule table that looks exhaustive and is not
        // is worse than one that admits it.
        "note": "Checked against the rules recorded for this carrier. The table covers \
                 common refusals and is not a substitute for the carrier's own \
                 dangerous-goods guidance.",
    })))
}

async fn acceptance_findings(
    state: &AppState,
    org: Uuid,
    carrier: &str,
    service: Option<&str>,
    inventory_ids: &[Uuid],
) -> Result<Vec<Value>, ApiError> {
    if inventory_ids.is_empty() {
        return Ok(Vec::new());
    }

    let rows = sqlx::query(
        r#"
        SELECT i.id, i.name, i.hazard_class, i.un_number,
               r.rule, r.note, r.service AS rule_service
        FROM inventory i
        JOIN carrier_service_rules r
          ON r.carrier = $1
         AND (r.service IS NULL OR r.service = $2)
         AND ( (r.un_number    IS NOT NULL AND r.un_number    = i.un_number)
            OR (r.hazard_class IS NOT NULL AND r.hazard_class = i.hazard_class) )
        WHERE i.id = ANY($3) AND i.org_id = $4
        "#,
    )
    .bind(carrier.trim().to_lowercase())
    .bind(service)
    .bind(inventory_ids)
    .bind(org)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "inventoryId": r.get::<Uuid, _>("id"),
                "item": r.get::<String, _>("name"),
                "hazardClass": r.get::<Option<String>, _>("hazard_class"),
                "unNumber": r.get::<Option<String>, _>("un_number"),
                "rule": r.get::<String, _>("rule"),
                "service": r.get::<Option<String>, _>("rule_service"),
                "note": r.get::<Option<String>, _>("note"),
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Rating
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RateQuery {
    from_address_id: Uuid,
    to_address_id: Uuid,
    weight_kg: Decimal,
    length_cm: Option<Decimal>,
    width_cm: Option<Decimal>,
    height_cm: Option<Decimal>,
    order_id: Option<Uuid>,
    /// Which configured account to rate through. Defaults to any online one.
    provider: Option<String>,
}

/// POST /api/carriers/rates
///
/// Quotes are stored, not just returned. A bid composed against a quote has to
/// be able to say what it was quoted: a carrier rate moves, and a bid somebody
/// accepted does not.
async fn get_rates(
    State(state): State<AppState>,
    caller: Caller,
    Json(q): Json<RateQuery>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;

    if q.weight_kg <= Decimal::ZERO {
        return Err((StatusCode::BAD_REQUEST, "weight_kg must be greater than zero".into()));
    }

    let from = load_address(&state, org, q.from_address_id).await?;
    let to = load_address(&state, org, q.to_address_id).await?;

    // Pick the provider: the one asked for, else any online account, else
    // manual — which refuses with an instruction rather than an error.
    let provider_name = match q.provider.as_deref() {
        Some(p) => p.to_string(),
        None => sqlx::query_scalar::<_, String>(
            "SELECT provider FROM carrier_accounts \
             WHERE org_id = $1 AND active AND provider <> 'manual' LIMIT 1",
        )
        .bind(org)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .unwrap_or_else(|| "manual".into()),
    };
    let provider = Provider::resolve(&provider_name);

    let req = carriers::RateRequest {
        from,
        to,
        parcel: carriers::Parcel {
            weight_kg: q.weight_kg,
            length_cm: q.length_cm,
            width_cm: q.width_cm,
            height_cm: q.height_cm,
        },
    };

    let quotes = provider.rate(&req).await.map_err(|e| {
        tracing::info!(provider = provider.name(), error = %e, "carrier rating failed");
        (carrier_status(&e), e.to_string())
    })?;

    // Store each, so a purchase can reference one and a bid can cite it.
    let mut out = Vec::with_capacity(quotes.len());
    for quote in &quotes {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO carrier_rate_quotes
              (org_id, order_id, carrier, service, provider, provider_rate_id,
               from_address_id, to_address_id, amount, currency, transit_days,
               billable_weight_kg, expires_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12, NOW() + INTERVAL '1 hour')
            RETURNING id
            "#,
        )
        .bind(org)
        .bind(q.order_id)
        .bind(&quote.carrier)
        .bind(&quote.service)
        .bind(provider.name())
        .bind(&quote.provider_rate_id)
        .bind(q.from_address_id)
        .bind(q.to_address_id)
        .bind(quote.amount)
        .bind(&quote.currency)
        .bind(quote.transit_days)
        .bind(quote.billable_weight_kg)
        .fetch_one(&state.db)
        .await
        .map_err(internal)?;

        out.push(json!({
            "quoteId": id,
            "carrier": quote.carrier,
            "service": quote.service,
            "amount": quote.amount,
            "currency": quote.currency,
            "transitDays": quote.transit_days,
        }));
    }

    // Cheapest first, then fastest — the order an operator reads them in.
    out.sort_by(|a, b| {
        let f = |v: &Value| v["amount"].as_str().and_then(|s| s.parse::<f64>().ok())
            .or_else(|| v["amount"].as_f64()).unwrap_or(f64::MAX);
        f(a).partial_cmp(&f(b)).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(Json(json!({ "provider": provider.name(), "count": out.len(), "rates": out })))
}

async fn load_address(
    state: &AppState,
    org: Uuid,
    id: Uuid,
) -> Result<carriers::Address, ApiError> {
    let r = sqlx::query("SELECT * FROM addresses WHERE id = $1 AND org_id = $2")
        .bind(id)
        .bind(org)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, format!("no such address: {id}")))?;

    Ok(carriers::Address {
        name: r.get("name"),
        company: r.get("company"),
        line1: r.get("line1"),
        line2: r.get("line2"),
        city: r.get("city"),
        region: r.get("region"),
        postcode: r.get("postcode"),
        country: r.get("country"),
        phone: r.get("phone"),
        residential: r.get("residential"),
    })
}

// ---------------------------------------------------------------------------
// Shipments
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct NewShipment {
    carrier: String,
    service: Option<String>,
    from_address_id: Uuid,
    to_address_id: Uuid,
    order_id: Option<Uuid>,
    fulfillment_id: Option<Uuid>,
    quote_id: Option<Uuid>,
    weight_kg: Option<Decimal>,
    length_cm: Option<Decimal>,
    width_cm: Option<Decimal>,
    height_cm: Option<Decimal>,
    /// Supplied when the label was bought outside Wayfarer — the manual path,
    /// and the one that works with no link.
    tracking_number: Option<String>,
    cost_amount: Option<Decimal>,
    cost_currency: Option<String>,
    notes: Option<String>,
    /// Checked for carrier acceptance before the shipment is created.
    inventory_ids: Option<Vec<Uuid>>,
}

/// POST /api/carriers/shipments
///
/// Creating a shipment with a tracking number records a parcel already handed
/// over (status `purchased`); without one it is a `draft` to buy a label for.
async fn create_shipment(
    State(state): State<AppState>,
    caller: Caller,
    AuthenticatedUser(user): AuthenticatedUser,
    Json(b): Json<NewShipment>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let org = org_of(&state, &caller).await?;
    let me = Uuid::parse_str(&user.sub).ok();
    let carrier = b.carrier.trim().to_lowercase();

    // Both addresses must be ours. Checked before anything else, because the
    // ids come from the caller.
    load_address(&state, org, b.from_address_id).await?;
    load_address(&state, org, b.to_address_id).await?;

    // Would the carrier even take this? Refused before a label is bought,
    // rather than discovered when the parcel is rejected at the counter.
    if let Some(ids) = b.inventory_ids.as_deref() {
        let findings =
            acceptance_findings(&state, org, &carrier, b.service.as_deref(), ids).await?;
        let forbidden: Vec<&Value> = findings.iter().filter(|f| f["rule"] == "forbidden").collect();
        if !forbidden.is_empty() {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!(
                    "{carrier} will not carry this: {}",
                    forbidden
                        .iter()
                        .map(|f| format!(
                            "{} ({})",
                            f["item"].as_str().unwrap_or("item"),
                            f["note"].as_str().unwrap_or("forbidden")
                        ))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            ));
        }
    }

    let tracking = b.tracking_number.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let status = if tracking.is_some() { "purchased" } else { "draft" };

    let row = sqlx::query(
        r#"
        INSERT INTO carrier_shipments
          (org_id, order_id, fulfillment_id, carrier, service, provider, quote_id,
           from_address_id, to_address_id, parcel_weight_kg, parcel_length_cm,
           parcel_width_cm, parcel_height_cm, tracking_number, cost_amount,
           cost_currency, status, notes, created_by, shipped_at)
        VALUES ($1,$2,$3,$4,$5,'manual',$6,$7,$8,$9,$10,$11,$12,$13,$14,
                COALESCE($15,'USD'),$16,$17,$18,
                CASE WHEN $13 IS NOT NULL THEN NOW() ELSE NULL END)
        RETURNING id, status
        "#,
    )
    .bind(org)
    .bind(b.order_id)
    .bind(b.fulfillment_id)
    .bind(&carrier)
    .bind(b.service.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(b.quote_id)
    .bind(b.from_address_id)
    .bind(b.to_address_id)
    .bind(b.weight_kg)
    .bind(b.length_cm)
    .bind(b.width_cm)
    .bind(b.height_cm)
    .bind(tracking)
    .bind(b.cost_amount)
    .bind(b.cost_currency.as_deref())
    .bind(status)
    .bind(b.notes.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(me)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (
                    StatusCode::CONFLICT,
                    format!("tracking number is already recorded against another {carrier} shipment"),
                );
            }
        }
        internal(e)
    })?;

    let id: Uuid = row.get("id");

    // A parcel already handed over gets its custody receipt now.
    if tracking.is_some() {
        write_handover_receipt(&state, id).await;
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id, "status": row.get::<String, _>("status"), "carrier": carrier })),
    ))
}

#[derive(Deserialize)]
struct ShipmentFilter {
    status: Option<String>,
    order_id: Option<Uuid>,
    limit: Option<i64>,
}

async fn list_shipments(
    State(state): State<AppState>,
    caller: Caller,
    Query(f): Query<ShipmentFilter>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let rows = sqlx::query(
        r#"
        SELECT s.*, ta.label AS to_label, ta.city AS to_city, ta.country AS to_country,
               fa.label AS from_label
        FROM carrier_shipments s
        LEFT JOIN addresses ta ON ta.id = s.to_address_id
        LEFT JOIN addresses fa ON fa.id = s.from_address_id
        WHERE s.org_id = $1
          AND ($2::text IS NULL OR s.status = $2)
          AND ($3::uuid IS NULL OR s.order_id = $3)
        ORDER BY s.created_at DESC
        LIMIT $4
        "#,
    )
    .bind(org)
    .bind(f.status.as_deref())
    .bind(f.order_id)
    .bind(f.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let shipments: Vec<Value> = rows.iter().map(shipment_json).collect();
    let in_flight = shipments
        .iter()
        .filter(|s| matches!(s["status"].as_str(), Some("purchased" | "in_transit")))
        .count();

    Ok(Json(json!({
        "count": shipments.len(),
        "inFlight": in_flight,
        "shipments": shipments,
    })))
}

fn shipment_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<Uuid, _>("id"),
        "carrier": r.get::<String, _>("carrier"),
        "service": r.get::<Option<String>, _>("service"),
        "provider": r.get::<String, _>("provider"),
        "status": r.get::<String, _>("status"),
        "trackingNumber": r.get::<Option<String>, _>("tracking_number"),
        "trackingUrl": r.get::<Option<String>, _>("tracking_url"),
        "labelUrl": r.get::<Option<String>, _>("label_url"),
        "orderId": r.get::<Option<Uuid>, _>("order_id"),
        "fromLabel": r.try_get::<Option<String>, _>("from_label").ok().flatten(),
        "toLabel": r.try_get::<Option<String>, _>("to_label").ok().flatten(),
        "toCity": r.try_get::<Option<String>, _>("to_city").ok().flatten(),
        "toCountry": r.try_get::<Option<String>, _>("to_country").ok().flatten(),
        "costAmount": r.get::<Option<Decimal>, _>("cost_amount"),
        "costCurrency": r.get::<String, _>("cost_currency"),
        "weightKg": r.get::<Option<Decimal>, _>("parcel_weight_kg"),
        "shippedAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("shipped_at").map(|d| d.to_rfc3339()),
        "deliveredAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("delivered_at").map(|d| d.to_rfc3339()),
        "estimatedDelivery": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("estimated_delivery").map(|d| d.to_rfc3339()),
        "lastTrackedAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_tracked_at").map(|d| d.to_rfc3339()),
        "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
    })
}

async fn get_shipment(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let row = sqlx::query(
        "SELECT s.*, ta.label AS to_label, ta.city AS to_city, ta.country AS to_country, \
                fa.label AS from_label \
         FROM carrier_shipments s \
         LEFT JOIN addresses ta ON ta.id = s.to_address_id \
         LEFT JOIN addresses fa ON fa.id = s.from_address_id \
         WHERE s.id = $1 AND s.org_id = $2",
    )
    .bind(id)
    .bind(org)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such shipment".to_string()))?;

    let events = sqlx::query(
        "SELECT status, detail, location, occurred_at FROM carrier_tracking_events \
         WHERE shipment_id = $1 ORDER BY occurred_at DESC LIMIT 100",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let mut out = shipment_json(&row);
    out["events"] = json!(events
        .iter()
        .map(|e| json!({
            "status": e.get::<String, _>("status"),
            "detail": e.get::<Option<String>, _>("detail"),
            "location": e.get::<Option<String>, _>("location"),
            "occurredAt": e.get::<chrono::DateTime<chrono::Utc>, _>("occurred_at").to_rfc3339(),
        }))
        .collect::<Vec<_>>());
    Ok(Json(out))
}

/// POST /api/carriers/shipments/:id/purchase — buy the label.
async fn purchase(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;

    let row = sqlx::query(
        "SELECT s.status, s.quote_id, q.provider, q.provider_rate_id \
         FROM carrier_shipments s LEFT JOIN carrier_rate_quotes q ON q.id = s.quote_id \
         WHERE s.id = $1 AND s.org_id = $2",
    )
    .bind(id)
    .bind(org)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such shipment".to_string()))?;

    // Buying twice spends money twice. The status check is the guard, and it
    // is a 409 so a retried request is not mistaken for a new purchase.
    if row.get::<String, _>("status") != "draft" {
        return Err((
            StatusCode::CONFLICT,
            "this shipment is no longer a draft; a label has already been bought or recorded".into(),
        ));
    }

    let rate_id: String = row
        .try_get::<Option<String>, _>("provider_rate_id")
        .ok()
        .flatten()
        .ok_or((
            StatusCode::CONFLICT,
            "no stored rate to buy — request rates first, or record a tracking number by hand".to_string(),
        ))?;

    let provider = Provider::resolve(
        &row.try_get::<Option<String>, _>("provider").ok().flatten().unwrap_or_default(),
    );

    let label = provider.buy(&rate_id).await.map_err(|e| {
        tracing::warn!(shipment = %id, error = %e, "label purchase failed");
        (carrier_status(&e), e.to_string())
    })?;

    sqlx::query(
        "UPDATE carrier_shipments SET status='purchased', tracking_number=$2, \
         tracking_url=$3, label_url=$4, label_format=$5, cost_amount=COALESCE($6, cost_amount), \
         cost_currency=COALESCE($7, cost_currency), provider=$8, \
         provider_shipment_id=$9, shipped_at=NOW(), updated_at=NOW() \
         WHERE id=$1",
    )
    .bind(id)
    .bind(&label.tracking_number)
    .bind(&label.tracking_url)
    .bind(&label.label_url)
    .bind(&label.label_format)
    .bind(label.cost_amount)
    .bind(&label.cost_currency)
    .bind(provider.name())
    .bind(&label.provider_shipment_id)
    .execute(&state.db)
    .await
    .map_err(internal)?;

    write_handover_receipt(&state, id).await;

    Ok(Json(json!({
        "id": id,
        "status": "purchased",
        "trackingNumber": label.tracking_number,
        "labelUrl": label.label_url,
        "costAmount": label.cost_amount,
    })))
}

/// POST /api/carriers/shipments/:id/track — poll the carrier now.
async fn track_now(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let owned: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM carrier_shipments WHERE id=$1 AND org_id=$2")
            .bind(id)
            .bind(org)
            .fetch_optional(&state.db)
            .await
            .map_err(internal)?;
    if owned.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such shipment".into()));
    }

    let n = crate::services::carrier_tracking::refresh_one(&state, id)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, e))?;

    Ok(Json(json!({ "id": id, "newEvents": n })))
}

#[derive(Deserialize)]
struct Handover {
    /// Where the parcel physically changed hands, if known.
    lat: Option<f64>,
    lon: Option<f64>,
    notes: Option<String>,
}

/// POST /api/carriers/shipments/:id/handover — record the custody handover
/// explicitly.
///
/// Normally written automatically when a label is bought or a tracking number
/// recorded. This exists for the case where the parcel went to the carrier
/// later than the label was created, which happens whenever labels are printed
/// in a batch the night before.
async fn record_handover(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(h): Json<Handover>,
) -> Result<Json<Value>, ApiError> {
    let org = org_of(&state, &caller).await?;
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM carrier_shipments WHERE id=$1 AND org_id=$2")
            .bind(id)
            .bind(org)
            .fetch_optional(&state.db)
            .await
            .map_err(internal)?;
    if exists.is_none() {
        return Err((StatusCode::NOT_FOUND, "no such shipment".into()));
    }

    if let Some(notes) = h.notes.as_deref() {
        let _ = sqlx::query("UPDATE carrier_shipments SET notes = $2 WHERE id = $1")
            .bind(id)
            .bind(notes.trim())
            .execute(&state.db)
            .await;
    }
    let wrote = write_handover_receipt_at(&state, id, h.lat, h.lon).await;
    Ok(Json(json!({ "id": id, "receiptWritten": wrote })))
}

/// Record the handover to a carrier in the custody chain.
///
/// A commercial carrier cannot sign, so this is an **unverified** receipt with
/// the carrier and tracking number in `to_label` and a null `to_node_id`. That
/// is the honest shape: the tracking number is third-party-checkable evidence,
/// which is not the same as a signature and must not be filed as one.
///
/// Best-effort on purpose. A custody write that fails must not fail the label
/// purchase, because the money is already spent and the parcel is already
/// moving — losing the receipt is recoverable through `/handover`, losing the
/// shipment record is not.
pub async fn write_handover_receipt(state: &AppState, shipment_id: Uuid) -> bool {
    write_handover_receipt_at(state, shipment_id, None, None).await
}

async fn write_handover_receipt_at(
    state: &AppState,
    shipment_id: Uuid,
    lat: Option<f64>,
    lon: Option<f64>,
) -> bool {
    let Ok(Some(s)) = sqlx::query(
        "SELECT order_id, fulfillment_id, carrier, tracking_number, service \
         FROM carrier_shipments WHERE id = $1",
    )
    .bind(shipment_id)
    .fetch_optional(&state.db)
    .await
    else {
        return false;
    };

    let tracking: Option<String> = s.get("tracking_number");
    let Some(tracking) = tracking else { return false };
    let carrier: String = s.get("carrier");
    let order_id: Option<Uuid> = s.get("order_id");

    // Continue the chain for this order rather than starting a new one, so the
    // sequence stays meaningful across a hand-off from the fabric to a carrier
    // and back.
    let seq: i32 = match order_id {
        Some(o) => sqlx::query_scalar::<_, Option<i32>>(
            "SELECT max(seq) FROM custody_receipts WHERE order_id = $1",
        )
        .bind(o)
        .fetch_one(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
            + 1,
        None => 1,
    };

    let to_label = format!("{} {}", carrier.to_uppercase(), tracking);
    let from_node = Uuid::parse_str(&state.identity.node_id).ok();

    let result = sqlx::query(
        r#"
        INSERT INTO custody_receipts
          (order_id, fulfillment_id, carrier_shipment_id, seq, from_node_id, to_node_id,
           from_label, to_label, event, condition, notes, occurred_at, lat, lon, verified)
        VALUES ($1,$2,$3,$4,$5,NULL,$6,$7,'transfer','unknown',$8,NOW(),$9,$10,false)
        "#,
    )
    .bind(order_id)
    .bind(s.get::<Option<Uuid>, _>("fulfillment_id"))
    // The subject when there is no order: an organisation shipping its own
    // stock to its own customer still gets a provenance chain.
    .bind(shipment_id)
    .bind(seq)
    .bind(from_node)
    .bind(std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "this outpost".into()))
    .bind(&to_label)
    .bind(format!(
        "Handed to a commercial carrier. No signature is possible; tracking number {tracking} is the evidence."
    ))
    .bind(lat)
    .bind(lon)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => {
            tracing::info!(shipment = %shipment_id, to = %to_label, "custody handover to carrier recorded");
            true
        }
        Err(e) => {
            tracing::warn!(shipment = %shipment_id, error = %e, "could not record carrier handover");
            false
        }
    }
}
