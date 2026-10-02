//! Search across the things an operator actually looks for.
//!
//! One box, several kinds of answer. A person hunting for "O2-BATCH-9" and a
//! person hunting for "oxygen" are asking the same question — *where is this
//! thing* — and should not have to know which table it lives in.
//!
//! Searches resources, lots and serials, orders, capsules and rate cards, each
//! result tagged with its kind and carrying enough context to act on without a
//! second lookup. Scoped to the outpost being asked: this is local data, and a
//! rollup-style fan-out would leak one site's holdings to anyone who could
//! reach another.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
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

pub fn search_routes() -> Router<AppState> {
    Router::new().route("/", get(search))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// Restrict to one kind: resource | lot | order | capsule | rate.
    pub kind: Option<String>,
    pub limit: Option<i64>,
}

/// GET /api/search?q=…
async fn search(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Caller(principal): Caller,
    Query(sq): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    // Fails closed: a caller whose organisation cannot be established searches
    // nothing rather than searching everything.
    let org = org_scope::caller_org(&state, &principal).await;
    let term = sq.q.trim();
    if term.len() < 2 {
        return Err((
            StatusCode::BAD_REQUEST,
            "give at least two characters — a one-character search returns everything".into(),
        ));
    }
    let limit = sq.limit.unwrap_or(25).clamp(1, 200);
    // Escape the LIKE wildcards so a literal % or _ in a lot code searches for
    // itself instead of matching everything.
    let pattern = format!("%{}%", term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
    let want = |k: &str| sq.kind.as_deref().map_or(true, |x| x.eq_ignore_ascii_case(k));

    let mut results: Vec<Value> = Vec::new();

    // ---- resources ----
    if want("resource") {
        let rows = sqlx::query(
            r#"
            SELECT id, name, description, category, location, unit, quantity, threshold
            FROM inventory
            WHERE org_id IS NOT DISTINCT FROM $3
              AND (name ILIKE $1 OR description ILIKE $1
                OR category ILIKE $1 OR location ILIKE $1)
            ORDER BY (quantity <= threshold) DESC, name
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(org)
        .fetch_all(&state.db)
        .await
        .map_err(server_err("resource search failed"))?;

        for r in &rows {
            let qty: i32 = r.get("quantity");
            let threshold: i32 = r.get("threshold");
            results.push(json!({
                "kind": "resource",
                "id": r.get::<Uuid, _>("id"),
                "title": r.get::<String, _>("name"),
                "subtitle": r.get::<Option<String>, _>("location"),
                "detail": format!("{} {} on hand", qty, r.get::<String, _>("unit")),
                "category": r.get::<Option<String>, _>("category"),
                // Carried so a result is actionable without a second request.
                "needsReorder": qty <= threshold,
                "quantity": qty,
                "unit": r.get::<String, _>("unit"),
            }));
        }
    }

    // ---- lots and serials ----
    // The reason a free-text box beats a filtered table: a recall notice names
    // a lot code, and nothing else in the system is keyed by it.
    if want("lot") {
        let rows = sqlx::query(
            r#"
            SELECT l.id, l.lot_code, l.serial, l.quantity, l.status, l.expires_at,
                   l.supplier, i.name AS item, i.unit, i.location
            FROM inventory_lots l
            JOIN inventory i ON i.id = l.inventory_id
            WHERE i.org_id IS NOT DISTINCT FROM $3
              AND (l.lot_code ILIKE $1 OR l.serial ILIKE $1
                OR l.supplier ILIKE $1 OR i.name ILIKE $1)
            ORDER BY l.expires_at NULLS LAST
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(org)
        .fetch_all(&state.db)
        .await
        .map_err(server_err("lot search failed"))?;

        for r in &rows {
            let lot: Option<String> = r.get("lot_code");
            let serial: Option<String> = r.get("serial");
            let expires: Option<DateTime<Utc>> = r.get("expires_at");
            results.push(json!({
                "kind": "lot",
                "id": r.get::<Uuid, _>("id"),
                "title": serial.clone().or(lot.clone()).unwrap_or_else(|| "(unidentified lot)".into()),
                "subtitle": format!("{} · {}", r.get::<String, _>("item"),
                                    r.get::<Option<String>, _>("location").unwrap_or_else(|| "—".into())),
                "detail": format!("{} {} · {}", r.get::<Decimal, _>("quantity"),
                                  r.get::<String, _>("unit"), r.get::<String, _>("status")),
                "lotCode": lot,
                "serial": serial,
                "supplier": r.get::<Option<String>, _>("supplier"),
                "expiresAt": expires,
                "expired": expires.map(|e| e <= Utc::now()).unwrap_or(false),
                "status": r.get::<String, _>("status"),
            }));
        }
    }

    // ---- orders ----
    if want("order") {
        let rows = sqlx::query(
            r#"
            SELECT id, description, status, quantity, unit, delivery_address, created_at
            FROM orders
            WHERE org_id IS NOT DISTINCT FROM $3
              AND (description ILIKE $1 OR delivery_address ILIKE $1
                OR target_part_number ILIKE $1 OR target_meta->>'resource' ILIKE $1)
            ORDER BY created_at DESC
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(org)
        .fetch_all(&state.db)
        .await
        .map_err(server_err("order search failed"))?;

        for r in &rows {
            results.push(json!({
                "kind": "order",
                "id": r.get::<Uuid, _>("id"),
                "title": r.get::<String, _>("description"),
                "subtitle": r.get::<Option<String>, _>("delivery_address"),
                "detail": format!("{} {} · {}", r.get::<Decimal, _>("quantity"),
                                  r.get::<String, _>("unit"), r.get::<String, _>("status")),
                "status": r.get::<String, _>("status"),
            }));
        }
    }

    // ---- capsules ----
    if want("capsule") {
        let rows = sqlx::query(
            r#"
            SELECT id, name, status, destination_address, departs_at,
                   mass_capacity_kg, volume_capacity_m3
            FROM capsules
            WHERE org_id IS NOT DISTINCT FROM $3
              AND (name ILIKE $1 OR destination_address ILIKE $1 OR notes ILIKE $1)
            ORDER BY departs_at NULLS LAST
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(org)
        .fetch_all(&state.db)
        .await
        .map_err(server_err("capsule search failed"))?;

        for r in &rows {
            results.push(json!({
                "kind": "capsule",
                "id": r.get::<Uuid, _>("id"),
                "title": r.get::<String, _>("name"),
                "subtitle": r.get::<Option<String>, _>("destination_address"),
                "detail": format!("{} · {} kg / {} m³", r.get::<String, _>("status"),
                                  r.get::<Decimal, _>("mass_capacity_kg"),
                                  r.get::<Decimal, _>("volume_capacity_m3")),
                "departsAt": r.get::<Option<DateTime<Utc>>, _>("departs_at"),
            }));
        }
    }

    // ---- rate cards ----
    if want("rate") {
        let rows = sqlx::query(
            r#"
            SELECT id, name, price_per_kg, price_per_m3, transit_days, currency
            FROM rate_cards
            WHERE active AND org_id IS NOT DISTINCT FROM $3
              AND (name ILIKE $1 OR notes ILIKE $1)
            ORDER BY name
            LIMIT $2
            "#,
        )
        .bind(&pattern)
        .bind(limit)
        .bind(org)
        .fetch_all(&state.db)
        .await
        .map_err(server_err("rate search failed"))?;

        for r in &rows {
            let kg: Option<Decimal> = r.get("price_per_kg");
            results.push(json!({
                "kind": "rate",
                "id": r.get::<Uuid, _>("id"),
                "title": r.get::<String, _>("name"),
                "subtitle": r.get::<Option<Decimal>, _>("transit_days")
                    .map(|d| format!("{d} days transit")),
                "detail": kg.map(|k| format!("{k} {}/kg", r.get::<String, _>("currency"))),
            }));
        }
    }

    // Things needing attention first — an expired lot or a resource below its
    // reorder point is why someone is searching, not a tidy alphabetical list.
    results.sort_by_key(|r| {
        let urgent = r["needsReorder"] == json!(true) || r["expired"] == json!(true);
        (!urgent, r["kind"].as_str().unwrap_or("").to_string())
    });

    let mut by_kind = serde_json::Map::new();
    for r in &results {
        let k = r["kind"].as_str().unwrap_or("other").to_string();
        let n = by_kind.get(&k).and_then(|v| v.as_i64()).unwrap_or(0) + 1;
        by_kind.insert(k, json!(n));
    }

    Ok(Json(json!({
        "query": term,
        "count": results.len(),
        "byKind": by_kind,
        "results": results,
    })))
}
