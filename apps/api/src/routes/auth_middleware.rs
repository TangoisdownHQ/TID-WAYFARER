use axum::async_trait;
use axum::body::Body;
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{request::Parts, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use axum_extra::{
    extract::TypedHeader,
    headers::{authorization::Bearer, Authorization},
};
use chrono::Utc;
use jsonwebtoken::{decode, DecodingKey, Validation, Algorithm};
use uuid::Uuid;

use crate::services::actuators;
use crate::services::fabric_auth::{self, FabricAuthMode, SigError};
use crate::services::metrics::{incr, METRICS};
use crate::services::identity::verify_signature;
use crate::AppState;

/// JWT claims we expect inside tokens
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct Claims {
    pub sub: String,     // user ID or email
    pub exp: usize,      // expiration timestamp
    pub provider: String,
    pub role: String,    // "user" or "admin"
}

/// Extractor that validates JWT and injects claims into the handler
pub struct AuthenticatedUser(pub Claims);

#[async_trait]
impl axum::extract::FromRequestParts<AppState> for AuthenticatedUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        // Extract Authorization header
        let TypedHeader(Authorization(bearer)) =
            TypedHeader::<Authorization<Bearer>>::from_request_parts(parts, &())
                .await
                .map_err(|_| {
                    (StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header".into())
                })?;

        // 🔎 Explicit validation (HS256, check exp)
        let mut validation = Validation::new(Algorithm::HS256);
        validation.validate_exp = true;

        // Decode JWT using the secret from AppState.auth
        let token_data = decode::<Claims>(
            bearer.token(),
            &DecodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
            &validation,
        )
        .map_err(|err| {
            tracing::error!("❌ JWT decode error: {:?}", err);
            (StatusCode::UNAUTHORIZED, "Invalid or expired token".into())
        })?;

        Ok(AuthenticatedUser(token_data.claims))
    }
}

/// Router-level guard for route groups that are reached by humans *or* peer
/// nodes. Accepts, in order:
///   1. a per-node Ed25519 request signature (`X-Node-Id` / `X-Node-Timestamp`
///      / `X-Node-Signature`), verified against that node's registered public
///      key — see [`crate::services::fabric_auth`];
///   2. the legacy fabric shared secret (`X-Node-Token` vs `NODE_SHARED_SECRET`);
///   3. a valid user JWT (`Authorization: Bearer <token>`).
///
/// Which of (1) and (2) are live is controlled by `FABRIC_AUTH`
/// (`signed` | `both` | `legacy`, default `both`) so a running fabric can be
/// upgraded node-by-node. Rejects with 401 otherwise. If NODE_SHARED_SECRET is
/// unset, shared-token auth is disabled entirely (fail closed).
pub async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    let mode = FabricAuthMode::from_env();

    // ---- 1. Per-node signature ----
    // Only buffer the body when a signature is actually presented, so JWT and
    // token traffic keeps streaming as before. In `legacy` mode we fall
    // through rather than reject: rolling clients send the signature *and*
    // the legacy token together, and the token is what this mode honours.
    if mode.allows_signature() && req.headers().contains_key(fabric_auth::HEADER_SIGNATURE) {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, fabric_auth::MAX_SIGNED_BODY_BYTES)
            .await
            .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "Body too large to verify".to_string()))?;

        verify_signed_request(&state, &parts, &bytes).await.map_err(|e| {
            incr(&METRICS.fabric_sig_rejected);
            tracing::warn!(path = %parts.uri.path(), reason = %e, "fabric signature rejected");
            (StatusCode::UNAUTHORIZED, "Invalid node signature".to_string())
        })?;
        incr(&METRICS.fabric_sig_ok);

        return Ok(next.run(Request::from_parts(parts, Body::from(bytes))).await);
    }

    // ---- 2. Shared secret ----
    // In `signed` mode this survives only as a *bootstrap* credential: a node
    // registering for the first time is not yet in node_registry, so it has no
    // key on file to verify against and cannot sign its way in. The register
    // payload carries its own Ed25519 proof-of-possession, replay window and
    // key-mismatch check (see routes::nodes::register_node), so the token here
    // gates who may attempt registration at all — it does not vouch identity.
    if let Some(token) = req.headers().get("x-node-token").and_then(|v| v.to_str().ok()) {
        let is_bootstrap = req.uri().path().ends_with("/nodes/register");
        if !mode.allows_shared_token() && !is_bootstrap {
            return Err((
                StatusCode::UNAUTHORIZED,
                "Shared-token fabric auth disabled; sign the request".into(),
            ));
        }
        return match std::env::var("NODE_SHARED_SECRET") {
            Ok(secret) if !secret.is_empty() && constant_time_eq(token.as_bytes(), secret.as_bytes()) => {
                Ok(next.run(req).await)
            }
            _ => Err((StatusCode::UNAUTHORIZED, "Invalid node token".to_string())),
        };
    }

    // ---- 3. User JWT ----
    let (mut parts, body) = req.into_parts();
    AuthenticatedUser::from_request_parts(&mut parts, &state).await?;
    Ok(next.run(Request::from_parts(parts, body)).await)
}

/// Enforces LOCKDOWN. Without this the actuator would only flip a flag in a
/// table and a "locked down" outpost would keep accepting changes — the flag
/// has to actually refuse something to be worth the name.
///
/// Reads are still served (an operator must be able to see the state of a
/// locked outpost) and `/commands/execute` stays open, or the UNLOCK command
/// could never arrive and the lockdown would be unrecoverable remotely.
///
/// Costs one indexed lookup per mutating request; worth caching if the write
/// path ever gets hot.
pub async fn enforce_lockdown(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, (StatusCode, String)> {
    let is_mutating = matches!(req.method().as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    let is_command_receiver = req.uri().path().ends_with("/commands/execute");

    if is_mutating
        && !is_command_receiver
        && actuators::flag_is_set(&state, actuators::STATE_LOCKDOWN).await
    {
        tracing::warn!(
            path = %req.uri().path(),
            method = %req.method(),
            "refused: outpost is in LOCKDOWN"
        );
        return Err((
            StatusCode::LOCKED,
            "Outpost is in LOCKDOWN; mutating requests are refused".to_string(),
        ));
    }

    Ok(next.run(req).await)
}

/// Validate the three signature headers against the sending node's registered
/// Ed25519 key. A node that isn't in the registry cannot authenticate — the
/// registry is populated by the signed registration flow, so this is the same
/// trust root end to end.
async fn verify_signed_request(
    state: &AppState,
    parts: &Parts,
    body: &[u8],
) -> Result<(), SigError> {
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());

    let (Some(node_id), Some(ts_raw), Some(signature)) = (
        header(fabric_auth::HEADER_NODE_ID),
        header(fabric_auth::HEADER_TIMESTAMP),
        header(fabric_auth::HEADER_SIGNATURE),
    ) else {
        return Err(SigError::MissingHeaders);
    };

    let node_uuid = Uuid::parse_str(node_id).map_err(|_| SigError::BadNodeId)?;
    let ts: i64 = ts_raw.parse().map_err(|_| SigError::BadTimestamp)?;
    fabric_auth::check_skew(ts, Utc::now().timestamp())?;

    // Fetch key and revocation together: a revoked node must be refused even
    // though it still holds a key that would verify perfectly.
    let row: Option<(String, bool)> =
        sqlx::query_as("SELECT public_key, revoked FROM node_registry WHERE node_id = $1")
            .bind(node_uuid)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();

    let (public_key, revoked) = row.ok_or(SigError::UnknownNode)?;
    if revoked {
        return Err(SigError::RevokedNode);
    }

    // Sign/verify the path the *client* requested. `Router::nest` rewrites
    // `parts.uri` to strip the mount prefix, so an inner layer sees
    // "/nodes/list" where the caller sent "/api/nodes/list" — verifying
    // against that would require every client to know this node's mount
    // layout. OriginalUri preserves what actually came in over the wire.
    let path = parts
        .extensions
        .get::<axum::extract::OriginalUri>()
        .map(|original| original.0.path().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());

    let message = fabric_auth::canonical_request(parts.method.as_str(), &path, ts, body);

    if verify_signature(&public_key, message.as_bytes(), signature) {
        Ok(())
    } else {
        Err(SigError::BadSignature)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Extractor that ensures the user has admin role
pub struct AdminUser(pub Claims);

#[async_trait]
impl axum::extract::FromRequestParts<AppState> for AdminUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let AuthenticatedUser(claims) =
            AuthenticatedUser::from_request_parts(parts, state).await?;

        if claims.role == "admin" {
            Ok(AdminUser(claims))
        } else {
            Err((StatusCode::FORBIDDEN, "Admin access required".to_string()))
        }
    }
}

