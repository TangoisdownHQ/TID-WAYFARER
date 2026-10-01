//! Organisations, node certificates and trust grants.
//!
//! Crypto and scope rules live in [`crate::services::org_trust`]; this is the
//! storage and the operator surface.
//!
//! One rule shapes the whole module: **an outpost never holds an org root
//! secret key.** It verifies certificates; it does not issue them. A root key
//! sitting on every outpost would make every outpost able to mint its own
//! authority, which is the single-root problem federation exists to escape.
//! Signing therefore happens wherever the org keeps its root — an air-gapped
//! machine, an HSM, a laptop in a safe — and only the resulting signature is
//! submitted here. `POST /orgs/:id/certificate-request` returns the exact
//! bytes to sign, so nobody has to reimplement the encoding.

use base64::Engine as _;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AdminUser, AuthenticatedUser};
use crate::services::org_trust::{self, GRANTABLE_SCOPES, NEVER_GRANTABLE};
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

pub fn org_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_orgs).post(create_org))
        .route("/scopes", get(scopes))
        .route("/:id", get(get_org))
        .route("/:id/certificate-request", post(certificate_request))
        .route("/:id/outposts", get(list_outposts).post(register_certificate))
        .route("/:id/trust", get(list_trust).post(grant_trust))
        .route("/:id/trust/:grant_id/revoke", post(revoke_trust))
        .route("/:id/trusts/:scope", get(check_trust))
}

/// GET /api/orgs/scopes — what may and may not cross a boundary.
async fn scopes(_user: AuthenticatedUser) -> Json<Value> {
    Json(json!({
        "grantable": GRANTABLE_SCOPES,
        "neverGrantable": NEVER_GRANTABLE,
        "why": {
            "inventory": "what you hold and how fast you burn it is the most commercially sensitive data here",
            "rollup": "same — a counterparty learning your reserve is thin learns when to raise prices",
            "commands": "one org must never actuate another's outpost; a safety boundary, not a business decision",
            "telemetry": "asset positions and condition",
            "rules": "policy that actuates carries the same risk as a command"
        }
    }))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct Organisation {
    pub id: Uuid,
    pub name: String,
    pub slug: Option<String>,
    pub root_public_key: String,
    pub wallet_address: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub struct NewOrg {
    pub name: String,
    pub slug: Option<String>,
    /// Base64 Ed25519 public half of the org root. The secret half stays with
    /// the org and is never sent here.
    pub root_public_key: String,
    pub wallet_address: Option<String>,
}

async fn create_org(
    State(state): State<AppState>,
    AdminUser(claims): AdminUser,
    Json(b): Json<NewOrg>,
) -> Result<(StatusCode, Json<Organisation>), ApiError> {
    if b.name.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "an organisation needs a name".into()));
    }
    // A root key that is not a usable Ed25519 key would make every certificate
    // under it unverifiable, and the failure would surface much later as a
    // mysterious signature rejection.
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b.root_public_key.trim())
        .map_err(|_| (StatusCode::BAD_REQUEST, "root_public_key is not valid base64".to_string()))?;
    if decoded.len() != 32 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("root_public_key must be a 32-byte Ed25519 key (got {} bytes)", decoded.len()),
        ));
    }

    let org = sqlx::query_as::<_, Organisation>(
        r#"
        INSERT INTO organisations (name, slug, root_public_key, wallet_address)
        VALUES ($1,$2,$3,$4) RETURNING *
        "#,
    )
    .bind(b.name.trim())
    .bind(&b.slug)
    .bind(b.root_public_key.trim())
    .bind(&b.wallet_address)
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (StatusCode::CONFLICT, "that slug or root key is already registered".into());
            }
        }
        tracing::error!(error = %e, "org insert failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not create the organisation".into())
    })?;

    // Whoever created it owns it; an org with no members is unadministrable.
    if let Ok(uid) = Uuid::parse_str(&claims.sub) {
        let _ = sqlx::query(
            "INSERT INTO org_members (org_id, user_id, role) VALUES ($1,$2,'owner') ON CONFLICT DO NOTHING",
        )
        .bind(org.id)
        .bind(uid)
        .execute(&state.db)
        .await;
    }

    tracing::info!(org = %org.id, name = %org.name, "organisation registered");
    Ok((StatusCode::CREATED, Json(org)))
}

async fn list_orgs(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
) -> Result<Json<Vec<Organisation>>, ApiError> {
    sqlx::query_as::<_, Organisation>("SELECT * FROM organisations ORDER BY name")
        .fetch_all(&state.db)
        .await
        .map(Json)
        .map_err(server_err("org list failed"))
}

async fn get_org(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Organisation>, ApiError> {
    sqlx::query_as::<_, Organisation>("SELECT * FROM organisations WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("org lookup failed"))?
        .map(Json)
        .ok_or((StatusCode::NOT_FOUND, "no such organisation".into()))
}

// ---------------------------------------------------------------------------
// Node certificates
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CertRequest {
    pub node_id: Uuid,
    pub node_public_key: String,
    pub not_after: Option<DateTime<Utc>>,
}

/// POST /api/orgs/:id/certificate-request
///
/// Returns the exact bytes to sign with the org root. Signing happens off this
/// machine; the outpost only ever verifies.
async fn certificate_request(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(org_id): Path<Uuid>,
    Json(b): Json<CertRequest>,
) -> Result<Json<Value>, ApiError> {
    let root: String = sqlx::query_scalar("SELECT root_public_key FROM organisations WHERE id = $1")
        .bind(org_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("org lookup failed"))?
        .ok_or((StatusCode::NOT_FOUND, "no such organisation".to_string()))?;

    Ok(Json(json!({
        "canonical": org_trust::canonical_certificate(
            &b.node_id.to_string(), &b.node_public_key, &org_id.to_string(), &root, b.not_after),
        "orgId": org_id,
        "nodeId": b.node_id,
        "notAfter": b.not_after,
        "note": "sign these exact bytes with the organisation root key, then POST the \
                 signature to /orgs/:id/outposts",
    })))
}

#[derive(Deserialize)]
pub struct RegisterCert {
    pub node_id: Uuid,
    pub node_public_key: String,
    pub certificate: String,
    pub not_after: Option<DateTime<Utc>>,
}

/// POST /api/orgs/:id/outposts — record a certificate, after verifying it.
///
/// Verified before storage rather than on use: a certificate that does not
/// check out is a mistake to surface now, not a silent failure that shows up
/// later as a peer mysteriously unable to authenticate.
async fn register_certificate(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(org_id): Path<Uuid>,
    Json(b): Json<RegisterCert>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let root: String = sqlx::query_scalar("SELECT root_public_key FROM organisations WHERE id = $1")
        .bind(org_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("org lookup failed"))?
        .ok_or((StatusCode::NOT_FOUND, "no such organisation".to_string()))?;

    org_trust::verify_certificate(
        &b.node_id.to_string(), &b.node_public_key, &org_id.to_string(),
        &root, b.not_after, false, &b.certificate, Utc::now(),
    )
    .map_err(|e| {
        tracing::warn!(org = %org_id, node = %b.node_id, reason = %e, "certificate refused");
        (StatusCode::BAD_REQUEST, format!("certificate refused: {e}"))
    })?;

    sqlx::query(
        r#"
        INSERT INTO org_outposts (org_id, node_id, node_public_key, certificate, not_after)
        VALUES ($1,$2,$3,$4,$5)
        ON CONFLICT (org_id, node_id) DO UPDATE SET
          node_public_key = EXCLUDED.node_public_key,
          certificate = EXCLUDED.certificate,
          not_after = EXCLUDED.not_after,
          revoked_at = NULL,
          issued_at = NOW()
        "#,
    )
    .bind(org_id).bind(b.node_id).bind(&b.node_public_key)
    .bind(&b.certificate).bind(b.not_after)
    .execute(&state.db)
    .await
    .map_err(|e| {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return (
                    StatusCode::CONFLICT,
                    "that node is already certified by a different organisation — a node \
                     certified by two orgs could act with either's authority".into(),
                );
            }
        }
        tracing::error!(error = %e, "certificate insert failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "could not record the certificate".into())
    })?;

    tracing::info!(org = %org_id, node = %b.node_id, "node certificate accepted");
    Ok((StatusCode::CREATED, Json(json!({
        "orgId": org_id, "nodeId": b.node_id, "verified": true, "notAfter": b.not_after,
    }))))
}

async fn list_outposts(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows = sqlx::query(
        "SELECT node_id, node_public_key, not_after, issued_at, revoked_at FROM org_outposts WHERE org_id = $1",
    )
    .bind(org_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("outpost list failed"))?;

    let now = Utc::now();
    Ok(Json(rows.iter().map(|r| {
        let exp: Option<DateTime<Utc>> = r.get("not_after");
        json!({
            "nodeId": r.get::<Uuid, _>("node_id"),
            "notAfter": exp,
            "issuedAt": r.get::<DateTime<Utc>, _>("issued_at"),
            "revokedAt": r.get::<Option<DateTime<Utc>>, _>("revoked_at"),
            "valid": r.get::<Option<DateTime<Utc>>, _>("revoked_at").is_none()
                      && exp.map(|e| e > now).unwrap_or(true),
        })
    }).collect()))
}

// ---------------------------------------------------------------------------
// Trust grants
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct NewGrant {
    pub counterparty_name: String,
    pub counterparty_root_key: String,
    pub scopes: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// Signed by this org's root over the canonical grant. Optional so a grant
    /// can be made locally, but one without a signature cannot be relayed to a
    /// third outpost and be believed.
    pub grant_signature: Option<String>,
    pub note: Option<String>,
}

/// POST /api/orgs/:id/trust — grant a counterparty access to certain scopes.
async fn grant_trust(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(org_id): Path<Uuid>,
    Json(b): Json<NewGrant>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    // Refused with a reason rather than filtered out silently: an operator who
    // tried to grant telemetry needs to know it did not happen.
    let forbidden: Vec<&String> = b.scopes.iter().filter(|s| !org_trust::is_grantable(s)).collect();
    if !forbidden.is_empty() {
        let never: Vec<&str> = forbidden.iter()
            .filter(|s| NEVER_GRANTABLE.contains(&s.as_str())).map(|s| s.as_str()).collect();
        let detail = if never.is_empty() {
            format!("unknown scope(s): {}", forbidden.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
        } else {
            format!(
                "{} can never cross an organisation boundary — actuation and holdings are \
                 safety and commercial boundaries, not grantable options",
                never.join(", ")
            )
        };
        return Err((StatusCode::BAD_REQUEST, format!(
            "{detail}. Grantable scopes are: {}", GRANTABLE_SCOPES.join(", "))));
    }
    if b.scopes.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a grant with no scopes grants nothing".into()));
    }

    let root: String = sqlx::query_scalar("SELECT root_public_key FROM organisations WHERE id = $1")
        .bind(org_id)
        .fetch_optional(&state.db)
        .await
        .map_err(server_err("org lookup failed"))?
        .ok_or((StatusCode::NOT_FOUND, "no such organisation".to_string()))?;

    // If a signature was supplied it must check out. A grant that claims to be
    // signed and is not is worse than an unsigned one, because it would be
    // relayed and believed.
    let mut signature_verified = false;
    if let Some(sig) = b.grant_signature.as_deref() {
        signature_verified = org_trust::verify_grant(
            &org_id.to_string(), &root, &b.counterparty_root_key, &b.scopes, b.expires_at, sig);
        if !signature_verified {
            return Err((StatusCode::BAD_REQUEST, "grant_signature does not verify against the organisation root".into()));
        }
    }

    let row = sqlx::query(
        r#"
        INSERT INTO org_trust (org_id, counterparty_name, counterparty_root_key, scopes,
                               expires_at, grant_signature, note)
        VALUES ($1,$2,$3,$4,$5,$6,$7)
        ON CONFLICT (org_id, counterparty_root_key) DO UPDATE SET
          counterparty_name = EXCLUDED.counterparty_name,
          scopes = EXCLUDED.scopes, expires_at = EXCLUDED.expires_at,
          grant_signature = EXCLUDED.grant_signature, note = EXCLUDED.note,
          revoked_at = NULL, granted_at = NOW()
        RETURNING id
        "#,
    )
    .bind(org_id).bind(&b.counterparty_name).bind(&b.counterparty_root_key)
    .bind(&b.scopes).bind(b.expires_at).bind(&b.grant_signature).bind(&b.note)
    .fetch_one(&state.db)
    .await
    .map_err(server_err("could not record the grant"))?;

    tracing::info!(org = %org_id, counterparty = %b.counterparty_name,
                   scopes = ?b.scopes, signed = signature_verified, "trust granted");

    Ok((StatusCode::CREATED, Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "counterparty": b.counterparty_name,
        "scopes": b.scopes,
        "expiresAt": b.expires_at,
        // Reported, because an unsigned grant is local-only: it cannot travel
        // over DTN and be believed by an outpost that was not present.
        "signatureVerified": signature_verified,
        "relayable": signature_verified,
    }))))
}

async fn list_trust(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(org_id): Path<Uuid>,
) -> Result<Json<Vec<Value>>, ApiError> {
    let rows = sqlx::query(
        "SELECT * FROM org_trust WHERE org_id = $1 ORDER BY counterparty_name",
    )
    .bind(org_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("trust list failed"))?;

    let now = Utc::now();
    Ok(Json(rows.iter().map(|r| {
        let scopes: Vec<String> = r.get("scopes");
        let exp: Option<DateTime<Utc>> = r.get("expires_at");
        let rev: Option<DateTime<Utc>> = r.get("revoked_at");
        json!({
            "id": r.get::<Uuid, _>("id"),
            "counterparty": r.get::<String, _>("counterparty_name"),
            "counterpartyRootKey": r.get::<String, _>("counterparty_root_key"),
            "scopes": scopes,
            "grantedAt": r.get::<DateTime<Utc>, _>("granted_at"),
            "expiresAt": exp,
            "revokedAt": rev,
            "live": rev.is_none() && exp.map(|e| e > now).unwrap_or(true),
            "relayable": r.get::<Option<String>, _>("grant_signature").is_some(),
        })
    }).collect()))
}

async fn revoke_trust(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path((org_id, grant_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let done = sqlx::query(
        "UPDATE org_trust SET revoked_at = NOW() WHERE id = $1 AND org_id = $2 AND revoked_at IS NULL",
    )
    .bind(grant_id)
    .bind(org_id)
    .execute(&state.db)
    .await
    .map_err(server_err("revoke failed"))?;

    if done.rows_affected() == 0 {
        return Err((StatusCode::CONFLICT, "no live grant with that id".into()));
    }
    // Revocation is unilateral by design — neither side needs the other's
    // agreement to stop trusting them.
    tracing::warn!(org = %org_id, grant = %grant_id, "trust revoked");
    Ok(Json(json!({ "id": grant_id, "revoked": true })))
}

#[derive(Deserialize)]
pub struct TrustCheck {
    pub counterparty_root_key: String,
}

/// GET /api/orgs/:id/trusts/:scope?counterparty_root_key=…
///
/// The decision procedure itself, exposed so it can be tested and reasoned
/// about rather than only happening implicitly inside a guard.
async fn check_trust(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path((org_id, scope)): Path<(Uuid, String)>,
    Query(q): Query<TrustCheck>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        "SELECT scopes, expires_at, revoked_at FROM org_trust WHERE org_id = $1 AND counterparty_root_key = $2",
    )
    .bind(org_id)
    .bind(&q.counterparty_root_key)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("trust lookup failed"))?;

    let permitted = match &row {
        Some(r) => org_trust::grant_permits(
            &r.get::<Vec<String>, _>("scopes"),
            r.get("expires_at"),
            r.get("revoked_at"),
            &scope,
            Utc::now(),
        ),
        None => false,
    };

    Ok(Json(json!({
        "orgId": org_id,
        "scope": scope,
        "permitted": permitted,
        "hasGrant": row.is_some(),
        "grantable": org_trust::is_grantable(&scope),
    })))
}
