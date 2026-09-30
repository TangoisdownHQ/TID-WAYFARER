use axum::{
    Router,
    routing::{post, get},
    Json,
    body::Bytes,
    extract::{State, Path},
    http::{HeaderMap, StatusCode},
};
use serde::{Serialize, Deserialize};
use uuid::Uuid;
use chrono::{Utc, DateTime};
use crate::AppState;
use crate::routes::auth_middleware::{Caller, Principal};
use crate::services::signed_telemetry::verify_hmac;
use crate::services::metrics::{incr, METRICS};

/// Fleet asset registration request (from nodes/outposts)
#[derive(Deserialize)]
pub struct FleetAssetRegister {
    pub asset_id: Option<Uuid>,
    pub asset_type: String,
    pub name: String,
    pub node_id: String,
}

/// Universal telemetry schema (database uses f64!)
#[derive(Deserialize)]
pub struct TelemetryIngest {
    pub asset_id: Uuid,
    pub node_id: String,
    pub timestamp: Option<DateTime<Utc>>,

    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt: Option<f64>,

    pub speed: Option<f64>,
    pub heading: Option<f64>,
    pub inclination: Option<f64>,
    pub apogee: Option<f64>,
    pub perigee: Option<f64>,

    pub battery: Option<f64>,
    pub temperature: Option<f64>,

    pub signal_db: Option<f64>,
    pub latency_ms: Option<f64>,
    pub packet_loss: Option<f64>,

    pub anomaly_score: Option<f64>,
    pub tamper: Option<bool>,
    pub malware_flag: Option<bool>,
}

/// Fleet asset response struct
#[derive(Serialize)]
pub struct FleetAssetResponse {
    pub asset_id: Uuid,
    pub registered: bool,
    pub timestamp: String,
}

// Nested under /api/fleet in main.rs.
pub fn fleet_asset_routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register_fleet_asset))
        .route("/telemetry", post(ingest_telemetry))
        .route("/:id", get(get_asset_status))
}

pub async fn register_fleet_asset(
    State(state): State<AppState>,
    Json(payload): Json<FleetAssetRegister>,
) -> Result<Json<FleetAssetResponse>, StatusCode> {

    let asset_id = payload.asset_id.unwrap_or(Uuid::new_v4());

    sqlx::query!(
        r#"
        INSERT INTO fleet_assets (id, node_id, asset_type, name, last_seen)
        VALUES ($1, $2, $3, $4, NOW())
        ON CONFLICT (id) DO UPDATE
        SET node_id = EXCLUDED.node_id,
            name = EXCLUDED.name,
            last_seen = NOW()
        "#,
        asset_id,
        payload.node_id,
        payload.asset_type,
        payload.name,
    )
    .execute(&state.db)
    .await
    .map_err(|e| {
        // Malformed input (NOT NULL violation, oversized field) is the
        // caller's fault and must not take the worker task down, which is
        // what the previous .unwrap() did. Note fleet_assets.node_id is
        // unconstrained TEXT — an unknown node is accepted here today.
        tracing::warn!("[register_fleet_asset] insert failed for {asset_id}: {e}");
        StatusCode::BAD_REQUEST
    })?;

    Ok(Json(FleetAssetResponse {
        asset_id,
        registered: true,
        timestamp: Utc::now().to_rfc3339(),
    }))
}

/// Ingest telemetry.
///
/// Attribution comes from the authenticated caller, not the payload, and
/// content integrity is enforced per-node: a node with a `hmac_secret` on file
/// must send a valid `X-Telemetry-HMAC: <hex>` over the raw body
/// (HMAC-SHA256). The raw body is taken so the HMAC covers exactly what the
/// client sent.
///
/// Who may report as whom:
///   - a signing node    — only as itself; a mismatched `node_id` is a 403
///   - a user JWT        — on a node's behalf, but the node's HMAC must hold
///   - the shared secret — never; it cannot distinguish one holder from another
///
/// Unsigned telemetry is accepted only from a node that authenticated by
/// signature and has no secret issued yet. Since telemetry drives autonomy
/// (tamper → lockdown, malware → isolation), a caller that cannot prove which
/// node it is must not be able to move another outpost's state.
pub async fn ingest_telemetry(
    State(state): State<AppState>,
    Caller(principal): Caller,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let t: TelemetryIngest = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    // Whose telemetry this is, decided by the guard rather than the payload.
    //
    // This used to read `t.node_id` from the body and look up *that* node's
    // secret — so the HMAC gate was bypassed by naming any node without a
    // secret, or one that didn't exist at all (no row → no secret → accepted
    // unsigned). Telemetry drives autonomy, so that was a path to forcing
    // another outpost into LOCKDOWN or network isolation.
    let claimed = t.node_id.trim();
    match &principal {
        Principal::Node(id) => {
            // A signing node may only report as itself.
            if !claimed.is_empty() && claimed != id.to_string() {
                incr(&METRICS.telemetry_hmac_rejected);
                tracing::warn!(
                    signed_as = %id, claimed = %claimed,
                    "telemetry rejected: node reported as a different node"
                );
                return StatusCode::FORBIDDEN;
            }
        }
        // An operator may submit on a node's behalf (bench tests, backfill),
        // but the HMAC below still has to hold for the named node.
        Principal::User(_) => {}
        // Indistinguishable from every other holder, so it cannot vouch for a
        // node id. Sign the request instead.
        Principal::SharedSecret => {
            incr(&METRICS.telemetry_hmac_rejected);
            tracing::warn!(claimed = %claimed, "telemetry rejected: shared secret cannot assert a node id");
            return StatusCode::FORBIDDEN;
        }
    }

    // A signing node's own id wins; otherwise fall back to what was claimed.
    let node_id = principal
        .node_id()
        .map(|id| id.to_string())
        .unwrap_or_else(|| claimed.to_string());

    if node_id.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    // Content integrity for the node the telemetry is attributed to.
    let node_secret: Option<String> = sqlx::query_scalar(
        "SELECT hmac_secret FROM node_registry WHERE node_id::text = $1",
    )
    .bind(&node_id)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    match node_secret {
        Some(secret) => {
            let provided = headers
                .get("x-telemetry-hmac")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            if provided.is_empty() || !verify_hmac(&body, provided, &secret) {
                incr(&METRICS.telemetry_hmac_rejected);
                tracing::warn!(node_id = %node_id, "telemetry HMAC rejected");
                return StatusCode::UNAUTHORIZED;
            }
        }
        // No secret on file. Tolerated only for a node that authenticated as
        // itself by signature — its identity is already proven, and this is
        // the bootstrap window before a secret is issued. A user submitting on
        // behalf of a node with no secret has proven nothing about the source.
        None => {
            if principal.node_id().is_none() {
                incr(&METRICS.telemetry_hmac_rejected);
                tracing::warn!(
                    node_id = %node_id,
                    "telemetry rejected: no telemetry secret on file for the named node"
                );
                return StatusCode::UNAUTHORIZED;
            }
        }
    }

    let result = sqlx::query(
        r#"
        INSERT INTO fleet_telemetry (
            asset_id, node_id, timestamp,
            lat, lon, alt,
            speed, heading,
            inclination, apogee, perigee,
            battery, temperature,
            signal_db, latency_ms, packet_loss,
            anomaly_score, tamper, malware_flag
        )
        VALUES (
            $1, $2, COALESCE($3, NOW()),
            $4, $5, $6, $7, $8, $9, $10, $11,
            $12, $13, $14, $15, $16, $17, $18, $19
        )
        "#,
    )
    .bind(t.asset_id).bind(&node_id).bind(t.timestamp)
    .bind(t.lat).bind(t.lon).bind(t.alt)
    .bind(t.speed).bind(t.heading)
    .bind(t.inclination).bind(t.apogee).bind(t.perigee)
    .bind(t.battery).bind(t.temperature)
    .bind(t.signal_db).bind(t.latency_ms).bind(t.packet_loss)
    .bind(t.anomaly_score).bind(t.tamper).bind(t.malware_flag)
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => StatusCode::OK,
        Err(e) => {
            tracing::warn!("[ingest_telemetry] insert failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// ✅ Query asset + latest telemetry
pub async fn get_asset_status(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, StatusCode> {

    // An unknown asset id is a 404, not a panic — this is a public-shaped
    // lookup and any caller can supply an arbitrary UUID.
    let asset = sqlx::query!(
        "SELECT id, node_id, asset_type, name, last_seen FROM fleet_assets WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::warn!("[get_asset_status] asset lookup failed for {id}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .ok_or(StatusCode::NOT_FOUND)?;

    // Telemetry is optional: a registered asset that has never reported is
    // valid, so a query error degrades to "no telemetry" rather than a 500.
    let telemetry = sqlx::query!(
        r#"
        SELECT *
        FROM fleet_telemetry
        WHERE asset_id = $1
        ORDER BY timestamp DESC
        LIMIT 1
        "#,
        id
    )
    .fetch_optional(&state.db)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("[get_asset_status] telemetry lookup failed for {id}: {e}");
        None
    });

    let asset_json = serde_json::json!({
    "id": asset.id,
    "node_id": asset.node_id,
    "asset_type": asset.asset_type,
    "name": asset.name,
    "last_seen": asset.last_seen,
    });

    let telemetry_json = telemetry.map(|t| serde_json::json!({
    "timestamp": t.timestamp,
    "lat": t.lat,
    "lon": t.lon,
    "speed": t.speed,
    "battery": t.battery,
    "anomaly_score": t.anomaly_score,
    "tamper": t.tamper,
    "malware_flag": t.malware_flag
    }));

    Ok(Json(serde_json::json!({
    "asset": asset_json,
    "telemetry": telemetry_json
    })))
}

