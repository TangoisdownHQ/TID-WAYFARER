//! 🧾 Damage evidence — signed reports that suspend settlement on contested goods.
//!
//! A party to an order files a report of how a consignment arrived; the outpost
//! signs it (binding the hash of the evidence), and while it is open the money
//! is held — `mark_settled` and voucher discharge both refuse. Resolving or
//! withdrawing it lifts the hold. The cryptographic core is `services::damage`;
//! authorization reuses `documents::require_order_party` (a party is the
//! order's requester or its shipper). Reports travel over DTN like custody
//! receipts, so the evidence survives a blackout.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
use crate::routes::documents::require_order_party;
use crate::services::damage::{self, CONDITIONS, SEVERITIES};
use crate::AppState;

type ApiError = (StatusCode, String);

fn ise<E: std::fmt::Display>(e: E) -> ApiError {
    tracing::error!(error = %e, "damage query failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
}

fn uid(sub: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(sub).map_err(|_| (StatusCode::UNAUTHORIZED, "bad token".to_string()))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct DamageReport {
    pub id: Uuid,
    pub order_id: Uuid,
    pub fulfillment_id: Uuid,
    pub lot_id: Option<Uuid>,
    pub reporter_node_id: Uuid,
    pub reporter_user_id: Uuid,
    pub condition: String,
    pub severity: String,
    #[serde(with = "rust_decimal::serde::float_option")]
    pub quantity_affected: Option<Decimal>,
    pub description: String,
    pub evidence_hash: String,
    pub evidence_uri: Option<String>,
    pub occurred_at: DateTime<Utc>,
    pub reporter_sig: String,
    pub status: String,
    pub resolution: Option<String>,
    pub resolved_by: Option<Uuid>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewDamage {
    pub fulfillment_id: Uuid,
    pub lot_id: Option<Uuid>,
    pub condition: String,
    pub severity: String,
    pub quantity_affected: Option<f64>,
    pub description: Option<String>,
    /// Hash of the evidence file/bundle, computed by the client.
    pub evidence_hash: String,
    pub evidence_uri: Option<String>,
}

#[derive(Deserialize)]
pub struct ResolveDamage {
    /// "resolved" (the parties settled the dispute) or "withdrawn" (retracted).
    pub action: String,
    pub resolution: Option<String>,
}

/// Merged into `/orders` alongside `order_routes`.
pub fn damage_routes() -> Router<AppState> {
    Router::new()
        .route("/:id/damage", post(file_damage).get(list_damage))
        .route("/:id/damage/:rid/resolve", post(resolve_damage))
}

/// Whether an open damage report stands against a fulfilment. The settlement
/// and voucher-discharge paths call the equivalent query directly; this is the
/// shared predicate.
pub(crate) async fn fulfillment_has_open_damage(
    db: &sqlx::PgPool,
    fulfillment_id: Uuid,
) -> sqlx::Result<bool> {
    let row: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM damage_reports WHERE fulfillment_id = $1 AND status = 'open' LIMIT 1",
    )
    .bind(fulfillment_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

/// POST /orders/:id/damage — file a signed damage report.
pub async fn file_damage(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    State(state): State<AppState>,
    Json(body): Json<NewDamage>,
) -> Result<Json<DamageReport>, ApiError> {
    let caller = uid(&user.sub)?;
    require_order_party(&state, order_id, caller).await?;

    if !CONDITIONS.contains(&body.condition.as_str()) {
        return Err((StatusCode::BAD_REQUEST, format!("condition must be one of {CONDITIONS:?}")));
    }
    if !SEVERITIES.contains(&body.severity.as_str()) {
        return Err((StatusCode::BAD_REQUEST, format!("severity must be one of {SEVERITIES:?}")));
    }
    if body.evidence_hash.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "evidence_hash is required".to_string()));
    }

    // The named fulfilment must belong to this order.
    let fo: Option<Uuid> =
        sqlx::query_scalar("SELECT order_id FROM fulfillments WHERE id = $1")
            .bind(body.fulfillment_id)
            .fetch_optional(&state.db)
            .await
            .map_err(ise)?;
    if fo != Some(order_id) {
        return Err((StatusCode::BAD_REQUEST, "fulfillment does not belong to this order".to_string()));
    }

    let qty: Option<Decimal> = body
        .quantity_affected
        .map(|q| Decimal::try_from(q).map_err(|_| (StatusCode::BAD_REQUEST, "quantity_affected is not a usable number".to_string())))
        .transpose()?;

    let id = Uuid::new_v4();
    let reporter_node = uid(&state.identity.node_id)?;
    let occurred_at = Utc::now();
    let description = body.description.clone().unwrap_or_default();

    // Sign over exactly what is stored. occurred_at is signed as unix seconds so
    // a later re-verification reconstructs the same bytes from the stored
    // timestamp without an RFC3339 precision mismatch.
    let occurred_s = occurred_at.timestamp().to_string();
    let qty_s = qty.map(|q| q.to_string());
    let claim = damage::DamageClaim {
        id,
        order_id,
        fulfillment_id: body.fulfillment_id,
        lot_id: body.lot_id,
        reporter_node_id: reporter_node,
        reporter_user_id: caller,
        condition: &body.condition,
        severity: &body.severity,
        quantity_affected: qty_s.as_deref(),
        description: &description,
        evidence_hash: &body.evidence_hash,
        occurred_at: &occurred_s,
    };
    let sig = damage::sign_damage(&state.identity, &claim);

    let report = sqlx::query_as::<_, DamageReport>(
        r#"
        INSERT INTO damage_reports
          (id, order_id, fulfillment_id, lot_id, reporter_node_id, reporter_user_id,
           condition, severity, quantity_affected, description, evidence_hash,
           evidence_uri, occurred_at, reporter_sig, status)
        VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'open')
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(order_id)
    .bind(body.fulfillment_id)
    .bind(body.lot_id)
    .bind(reporter_node)
    .bind(caller)
    .bind(&body.condition)
    .bind(&body.severity)
    .bind(qty)
    .bind(&description)
    .bind(&body.evidence_hash)
    .bind(&body.evidence_uri)
    .bind(occurred_at)
    .bind(&sig)
    .fetch_one(&state.db)
    .await
    .map_err(ise)?;

    tracing::info!(report_id = %id, order_id = %order_id, condition = %body.condition, "damage report filed — settlement suspended");
    Ok(Json(report))
}

/// GET /orders/:id/damage — list the reports on an order (party-only).
pub async fn list_damage(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<DamageReport>>, ApiError> {
    let caller = uid(&user.sub)?;
    require_order_party(&state, order_id, caller).await?;
    let rows = sqlx::query_as::<_, DamageReport>(
        "SELECT * FROM damage_reports WHERE order_id = $1 ORDER BY created_at",
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(ise)?;
    Ok(Json(rows))
}

/// POST /orders/:id/damage/:rid/resolve — resolve or withdraw a report, lifting
/// the settlement suspension.
pub async fn resolve_damage(
    AuthenticatedUser(user): AuthenticatedUser,
    Path((order_id, rid)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
    Json(body): Json<ResolveDamage>,
) -> Result<Json<DamageReport>, ApiError> {
    let caller = uid(&user.sub)?;
    require_order_party(&state, order_id, caller).await?;

    if body.action != "resolved" && body.action != "withdrawn" {
        return Err((StatusCode::BAD_REQUEST, "action must be 'resolved' or 'withdrawn'".to_string()));
    }

    // Guarded update: only an open report on this order transitions, so a double
    // resolve or a wrong id gets a clean 409 rather than a silent no-op.
    let report = sqlx::query_as::<_, DamageReport>(
        r#"
        UPDATE damage_reports
        SET status = $3, resolution = $4, resolved_by = $5, resolved_at = NOW()
        WHERE id = $1 AND order_id = $2 AND status = 'open'
        RETURNING *
        "#,
    )
    .bind(rid)
    .bind(order_id)
    .bind(&body.action)
    .bind(&body.resolution)
    .bind(caller)
    .fetch_optional(&state.db)
    .await
    .map_err(ise)?
    .ok_or((StatusCode::CONFLICT, "no open report with that id on this order".to_string()))?;

    tracing::info!(report_id = %rid, action = %body.action, "damage report resolved — suspension lifted");
    Ok(Json(report))
}
