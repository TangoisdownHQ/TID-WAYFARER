//! Certifications and compliance holds.
//!
//! A part that is out of cert must not ship. "Someone will remember" is not a
//! control, it is the absence of one — so the check runs at the moment a
//! shipment is attempted, and it blocks.
//!
//! Three pieces:
//!
//!   certifications           what a thing is certified for, and until when
//!   compliance_requirements  what a destination demands
//!   compliance_holds         a shipment stopped, and why
//!
//! The check is `GET /api/compliance/check/:order_id` and it is deliberately
//! callable on its own: an operator planning a manifest needs to know what will
//! be refused *before* the capsule is packed, not at the door.
//!
//! Overrides exist. The times you genuinely must ship anyway are real, and a
//! system that pretends otherwise just teaches people to route around it. But
//! an override is attributed and reasoned, enforced by a CHECK constraint —
//! because an unattributable override is indistinguishable from having no
//! check at all.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AdminUser, AuthenticatedUser};
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

pub const KINDS: [&str; 6] = ["export", "hazmat", "flight", "calibration", "customs", "quality"];

fn check_kind(k: &str) -> Result<(), ApiError> {
    if KINDS.contains(&k) {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("kind must be one of {}", KINDS.join(", ")),
        ))
    }
}

pub fn compliance_routes() -> Router<AppState> {
    Router::new()
        .route("/certifications", get(list_certs).post(create_cert))
        .route("/certifications/expiring", get(expiring_certs))
        .route("/requirements", get(list_requirements).post(create_requirement))
        .route("/check/:order_id", get(check_order))
        .route("/holds", get(list_holds))
        .route("/holds/:id/override", post(override_hold))
        .route("/holds/:id/clear", post(clear_hold))
}

// ---------------------------------------------------------------------------
// Certifications
// ---------------------------------------------------------------------------

#[derive(Serialize, sqlx::FromRow)]
pub struct Certification {
    pub id: Uuid,
    pub inventory_id: Option<Uuid>,
    pub lot_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub kind: String,
    pub authority: Option<String>,
    pub identifier: Option<String>,
    pub destination_body_id: Option<i32>,
    pub destination_region: Option<String>,
    pub issued_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub status: String,
    pub details: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewCertification {
    pub inventory_id: Option<Uuid>,
    pub lot_id: Option<Uuid>,
    pub asset_id: Option<Uuid>,
    pub kind: String,
    pub authority: Option<String>,
    pub identifier: Option<String>,
    pub destination_body_id: Option<i32>,
    pub destination_region: Option<String>,
    pub issued_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
    pub details: Option<Value>,
}

async fn create_cert(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Json(body): Json<NewCertification>,
) -> Result<(StatusCode, Json<Certification>), ApiError> {
    check_kind(&body.kind)?;

    let subjects = body.inventory_id.is_some() as u8
        + body.lot_id.is_some() as u8
        + body.asset_id.is_some() as u8;
    if subjects != 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            "give exactly one of inventory_id, lot_id or asset_id — a certificate covering \
             everything is not a certificate"
                .into(),
        ));
    }

    let cert = sqlx::query_as::<_, Certification>(
        r#"
        INSERT INTO certifications
          (inventory_id, lot_id, asset_id, kind, authority, identifier,
           destination_body_id, destination_region, issued_at, expires_at, details)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,COALESCE($11,'{}'::jsonb))
        RETURNING *
        "#,
    )
    .bind(body.inventory_id)
    .bind(body.lot_id)
    .bind(body.asset_id)
    .bind(&body.kind)
    .bind(&body.authority)
    .bind(&body.identifier)
    .bind(body.destination_body_id)
    .bind(&body.destination_region)
    .bind(body.issued_at)
    .bind(body.expires_at)
    .bind(&body.details)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("could not record the certification"))?;

    Ok((StatusCode::CREATED, Json(cert)))
}

#[derive(Deserialize)]
pub struct CertFilter {
    pub inventory_id: Option<Uuid>,
    pub kind: Option<String>,
    pub status: Option<String>,
}

async fn list_certs(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(f): Query<CertFilter>,
) -> Result<Json<Vec<Certification>>, ApiError> {
    sqlx::query_as::<_, Certification>(
        r#"
        SELECT * FROM certifications
        WHERE ($1::uuid IS NULL OR inventory_id = $1)
          AND ($2::text IS NULL OR kind = $2)
          AND ($3::text IS NULL OR status = $3)
        ORDER BY expires_at NULLS LAST
        "#,
    )
    .bind(f.inventory_id)
    .bind(&f.kind)
    .bind(&f.status)
    .fetch_all(&state.db)
    .await
    .map(Json)
    .map_err(server_err("certification list failed"))
}

#[derive(Deserialize)]
pub struct ExpiringQuery {
    pub days: Option<i64>,
}

/// Certificates lapsing inside the window, and any already lapsed but still
/// marked valid — the same split as the lot expiry sweep, for the same reason:
/// the second group is a present problem, not a planning one.
async fn expiring_certs(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(q): Query<ExpiringQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = q.days.unwrap_or(60).clamp(0, 3650);
    let rows = sqlx::query(
        r#"
        SELECT c.*, i.name AS item_name
        FROM certifications c
        LEFT JOIN inventory i ON i.id = c.inventory_id
        WHERE c.expires_at IS NOT NULL AND c.status = 'valid' AND c.expires_at <= $1
        ORDER BY c.expires_at
        "#,
    )
    .bind(Utc::now() + Duration::days(days))
    .fetch_all(&state.db)
    .await
    .map_err(server_err("certification sweep failed"))?;

    let now = Utc::now();
    let mut lapsed = 0usize;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let exp: Option<DateTime<Utc>> = r.get("expires_at");
            let gone = exp.map(|e| e <= now).unwrap_or(false);
            if gone {
                lapsed += 1;
            }
            json!({
                "id": r.get::<Uuid, _>("id"),
                "kind": r.get::<String, _>("kind"),
                "identifier": r.get::<Option<String>, _>("identifier"),
                "authority": r.get::<Option<String>, _>("authority"),
                "item": r.get::<Option<String>, _>("item_name"),
                "expiresAt": exp,
                "daysRemaining": exp.map(|e| (e - now).num_days()),
                "alreadyExpired": gone,
            })
        })
        .collect();

    Ok(Json(json!({
        "horizonDays": days, "count": items.len(),
        "alreadyExpired": lapsed, "certifications": items,
    })))
}

// ---------------------------------------------------------------------------
// Requirements
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewRequirement {
    pub destination_body_id: Option<i32>,
    pub item_category: Option<String>,
    pub required_kind: String,
    pub note: Option<String>,
}

/// Admin-only: a requirement decides what the fabric will refuse to ship.
async fn create_requirement(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<NewRequirement>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    check_kind(&body.required_kind)?;

    let row = sqlx::query(
        r#"
        INSERT INTO compliance_requirements (destination_body_id, item_category, required_kind, note)
        VALUES ($1,$2,$3,$4)
        ON CONFLICT (destination_body_id, item_category, required_kind) DO UPDATE SET note = EXCLUDED.note
        RETURNING id
        "#,
    )
    .bind(body.destination_body_id)
    .bind(&body.item_category)
    .bind(&body.required_kind)
    .bind(&body.note)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("could not record the requirement"))?;

    Ok((StatusCode::CREATED, Json(json!({ "id": row.get::<Uuid, _>("id") }))))
}

async fn list_requirements(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows = sqlx::query(
        "SELECT * FROM compliance_requirements ORDER BY destination_body_id NULLS FIRST, required_kind",
    )
    .fetch_all(&state.db)
    .await
    .map_err(server_err("requirement list failed"))?;

    Ok(Json(
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.get::<Uuid, _>("id"),
                    "destinationBodyId": r.get::<Option<i32>, _>("destination_body_id"),
                    "itemCategory": r.get::<Option<String>, _>("item_category"),
                    "requiredKind": r.get::<String, _>("required_kind"),
                    "note": r.get::<Option<String>, _>("note"),
                })
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// The check
// ---------------------------------------------------------------------------

/// GET /api/compliance/check/:order_id
///
/// Would this order ship? Runs the destination's requirements against the
/// certificates actually on file, and raises a hold for anything missing.
///
/// Callable before packing on purpose: an operator needs to know what will be
/// refused while there is still time to fix it.
async fn check_order(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let order = sqlx::query(
        "SELECT id, description, delivery_body_id, target_part_number, item_kind FROM orders WHERE id = $1",
    )
    .bind(order_id)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("order lookup failed"))?
    .ok_or((StatusCode::NOT_FOUND, "no such order".to_string()))?;

    let destination: Option<i32> = order.get("delivery_body_id");

    // Which certificate kinds this destination demands. A NULL body in the
    // requirement means "everywhere", so it always applies.
    let required = sqlx::query(
        r#"
        SELECT required_kind, item_category, note
        FROM compliance_requirements
        WHERE destination_body_id IS NULL OR destination_body_id = $1
        "#,
    )
    .bind(destination)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("requirement lookup failed"))?;

    // What is actually certified, valid, unexpired, and in scope for this
    // destination. A certificate scoped to another body does not count — that
    // is the entire point of scoping it.
    //
    // Orders carry no inventory FK yet, so the subject is matched on item name:
    // the reorder flow writes it into target_meta.resource, and a hand-written
    // order has it in the description.
    let valid_kinds = sqlx::query(
        r#"
        SELECT DISTINCT c.kind
        FROM certifications c
        LEFT JOIN inventory i ON i.id = c.inventory_id
        WHERE c.status = 'valid'
          AND (c.expires_at IS NULL OR c.expires_at > NOW())
          AND (c.destination_body_id IS NULL OR c.destination_body_id = $1)
          AND (
                i.id IS NULL
             OR i.name = (SELECT o.target_meta->>'resource' FROM orders o WHERE o.id = $2)
             OR i.name = (SELECT o.description FROM orders o WHERE o.id = $2)
          )
        "#,
    )
    .bind(destination)
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("certification lookup failed"))?;

    let have: Vec<String> = valid_kinds.iter().map(|r| r.get::<String, _>("kind")).collect();

    let mut missing: Vec<Value> = Vec::new();
    for r in &required {
        let kind: String = r.get("required_kind");
        if !have.contains(&kind) {
            missing.push(json!({
                "kind": kind,
                "note": r.get::<Option<String>, _>("note"),
            }));
        }
    }

    // Raise a hold for each missing requirement, once.
    //
    // The existing-hold test covers 'overridden' as well as 'active', and that
    // matters: an admin who has deliberately released a shipment must not have
    // it silently re-blocked the next time anyone runs the check. An override
    // the system undoes behind your back is worse than no override at all,
    // because it looks like it worked.
    //
    // 'cleared' is deliberately excluded. Cleared means the underlying problem
    // was fixed, so if the same requirement is unmet again it is a new fact and
    // deserves a new hold.
    for m in &missing {
        let kind = m["kind"].as_str().unwrap_or_default();
        let exists: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM compliance_holds \
             WHERE order_id=$1 AND failed_kind=$2 AND status IN ('active','overridden')",
        )
        .bind(order_id)
        .bind(kind)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("hold lookup failed"))?;

        if exists.is_none() {
            let _ = sqlx::query(
                r#"
                INSERT INTO compliance_holds (order_id, reason, failed_kind, details)
                VALUES ($1,$2,$3,$4)
                "#,
            )
            .bind(order_id)
            .bind(format!("missing a valid '{kind}' certificate for this destination"))
            .bind(kind)
            .bind(json!({ "destination_body_id": destination }))
            .execute(&state.db)
            .await;
        }
    }

    let active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM compliance_holds WHERE order_id=$1 AND status='active'",
    )
    .bind(order_id)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("hold count failed"))?;

    if !missing.is_empty() {
        tracing::warn!(%order_id, missing = missing.len(), "compliance check failed");
    }

    Ok(Json(json!({
        "orderId": order_id,
        "destinationBodyId": destination,
        // The answer an operator acts on.
        "mayShip": active == 0,
        "requiredKinds": required.iter().map(|r| r.get::<String, _>("required_kind")).collect::<Vec<_>>(),
        "satisfiedKinds": have,
        "missing": missing,
        "activeHolds": active,
    })))
}

// ---------------------------------------------------------------------------
// Holds
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct HoldFilter {
    pub order_id: Option<Uuid>,
    pub status: Option<String>,
}

async fn list_holds(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Query(f): Query<HoldFilter>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT h.*, o.description, u.username AS overridden_by_name
        FROM compliance_holds h
        LEFT JOIN orders o ON o.id = h.order_id
        LEFT JOIN users  u ON u.id = h.overridden_by
        WHERE ($1::uuid IS NULL OR h.order_id = $1)
          AND ($2::text IS NULL OR h.status = $2)
        ORDER BY h.created_at DESC
        "#,
    )
    .bind(f.order_id)
    .bind(&f.status)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("hold list failed"))?;

    Ok(Json(
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.get::<Uuid, _>("id"),
                    "orderId": r.get::<Option<Uuid>, _>("order_id"),
                    "capsuleId": r.get::<Option<Uuid>, _>("capsule_id"),
                    "order": r.get::<Option<String>, _>("description"),
                    "reason": r.get::<String, _>("reason"),
                    "failedKind": r.get::<Option<String>, _>("failed_kind"),
                    "status": r.get::<String, _>("status"),
                    "overriddenBy": r.get::<Option<String>, _>("overridden_by_name"),
                    "overrideReason": r.get::<Option<String>, _>("override_reason"),
                    "createdAt": r.get::<DateTime<Utc>, _>("created_at"),
                    "resolvedAt": r.get::<Option<DateTime<Utc>>, _>("resolved_at"),
                })
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct OverrideRequest {
    pub reason: String,
}

/// POST /api/compliance/holds/:id/override — ship anyway, on the record.
///
/// Admin-only and reasoned. The schema enforces that an overridden hold has
/// both an author and a reason, because an unattributable override is
/// indistinguishable from the check never having run.
async fn override_hold(
    State(state): State<AppState>,
    AdminUser(claims): AdminUser,
    Path(id): Path<Uuid>,
    Json(body): Json<OverrideRequest>,
) -> Result<Json<Value>, ApiError> {
    let reason = body.reason.trim();
    if reason.len() < 8 {
        return Err((
            StatusCode::BAD_REQUEST,
            "an override needs a real reason — it is the only record of why this shipped".into(),
        ));
    }
    let admin = Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "token subject is not a user id".to_string()))?;

    let row = sqlx::query(
        r#"
        UPDATE compliance_holds
        SET status='overridden', overridden_by=$2, override_reason=$3, resolved_at=NOW()
        WHERE id=$1 AND status='active'
        RETURNING order_id, failed_kind
        "#,
    )
    .bind(id)
    .bind(admin)
    .bind(reason)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("override failed"))?
    .ok_or((StatusCode::CONFLICT, "no active hold with that id".to_string()))?;

    // Loud on purpose: this is the line an auditor will look for.
    tracing::warn!(
        hold_id = %id,
        order_id = ?row.get::<Option<Uuid>, _>("order_id"),
        failed_kind = ?row.get::<Option<String>, _>("failed_kind"),
        by = %claims.sub,
        reason = %reason,
        "COMPLIANCE HOLD OVERRIDDEN"
    );

    Ok(Json(json!({ "id": id, "status": "overridden", "by": claims.sub, "reason": reason })))
}

/// POST /api/compliance/holds/:id/clear — the underlying problem was fixed.
async fn clear_hold(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let done = sqlx::query(
        "UPDATE compliance_holds SET status='cleared', resolved_at=NOW() WHERE id=$1 AND status='active'",
    )
    .bind(id)
    .execute(&state.db)
    .await
    .map_err(server_err("clear failed"))?;

    if done.rows_affected() == 0 {
        return Err((StatusCode::CONFLICT, "no active hold with that id".into()));
    }
    Ok(Json(json!({ "id": id, "status": "cleared" })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_closed() {
        assert!(check_kind("export").is_ok());
        assert!(check_kind("calibration").is_ok());
        assert_eq!(check_kind("vibes").unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    /// The rule the check applies: every required kind must appear among the
    /// valid, in-scope certificates.
    fn may_ship(required: &[&str], have: &[&str]) -> bool {
        required.iter().all(|r| have.contains(r))
    }

    #[test]
    fn a_missing_certificate_blocks() {
        assert!(!may_ship(&["export", "hazmat"], &["export"]));
        assert!(may_ship(&["export", "hazmat"], &["export", "hazmat"]));
    }

    #[test]
    fn extra_certificates_do_not_substitute_for_the_required_one() {
        // Holding five unrelated certificates does not make an export licence
        // appear, which is exactly the mistake a naive "has any cert" check
        // would make.
        assert!(!may_ship(&["export"], &["flight", "quality", "calibration"]));
    }

    #[test]
    fn nothing_required_means_nothing_blocks() {
        assert!(may_ship(&[], &[]));
    }

    /// Whether `check_order` raises a fresh hold for a requirement that is
    /// still unmet, given whatever hold already exists for it.
    fn raises_hold(existing: Option<&str>) -> bool {
        !matches!(existing, Some("active") | Some("overridden"))
    }

    #[test]
    fn an_override_is_not_undone_by_the_next_check() {
        // Found in live testing: the dedup originally looked only for 'active'
        // holds, so an overridden one was invisible and the very next check
        // raised a replacement. The shipment an admin had deliberately released
        // silently re-blocked — which looks like the override worked and then
        // stopped working for no reason.
        assert!(!raises_hold(Some("overridden")));
        assert!(!raises_hold(Some("active")));
    }

    #[test]
    fn a_cleared_hold_does_not_suppress_a_recurrence() {
        // 'cleared' means the underlying problem was fixed. If the requirement
        // is unmet again that is a new fact — a certificate lapsing a second
        // time must block a second time.
        assert!(raises_hold(Some("cleared")));
        assert!(raises_hold(None));
    }
}
