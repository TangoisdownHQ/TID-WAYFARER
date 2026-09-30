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

/// Who the guard actually authenticated.
///
/// `require_auth` used to verify a node signature or a JWT and then drop the
/// identity on the floor — `extensions.insert` appeared nowhere in the crate.
/// Membership was proven; *identity* never reached a handler. So anything
/// needing an actor read it from the request body, and the fabric had no way
/// to tell one member from another: any authenticated caller could pull
/// another node's commands, acknowledge them on its behalf, post telemetry as
/// any node, or push a LOCKDOWN.
///
/// The guard now stores this in request extensions, and [`Caller`] reads it
/// back out. A handler that needs to know who is calling must take it from
/// here, never from the payload.
#[derive(Debug, Clone)]
pub enum Principal {
    /// A peer outpost that proved possession of its registered Ed25519 key.
    Node(Uuid),
    /// A human bearing a valid JWT.
    User(Claims),
    /// The legacy fabric-wide shared secret. It authenticates *membership* and
    /// nothing else — every holder looks identical — so it can never satisfy
    /// "is the caller this specific node". Kept only for bootstrap and
    /// rollback; see `FABRIC_AUTH`.
    SharedSecret,
}

impl Principal {
    /// The node id this caller may act as, if any.
    pub fn node_id(&self) -> Option<Uuid> {
        match self {
            Self::Node(id) => Some(*id),
            _ => None,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Self::User(c) if c.role == "admin")
    }

    /// Short label for logs and audit rows.
    pub fn describe(&self) -> String {
        match self {
            Self::Node(id) => format!("node:{id}"),
            Self::User(c) => format!("user:{}", c.sub),
            Self::SharedSecret => "shared-secret".to_string(),
        }
    }
}

/// Extractor for the verified caller. Infallible where the route sits behind
/// `require_auth`; a missing principal means the route was mounted outside the
/// guard, which is a wiring bug rather than a client error.
pub struct Caller(pub Principal);

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .map(Caller)
            .ok_or_else(|| {
                tracing::error!(
                    path = %parts.uri.path(),
                    "no Principal in extensions — route is mounted outside require_auth"
                );
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "caller identity unavailable".to_string(),
                )
            })
    }
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
        let (mut parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, fabric_auth::MAX_SIGNED_BODY_BYTES)
            .await
            .map_err(|_| (StatusCode::PAYLOAD_TOO_LARGE, "Body too large to verify".to_string()))?;

        let node_id = verify_signed_request(&state, &parts, &bytes).await.map_err(|e| {
            incr(&METRICS.fabric_sig_rejected);
            tracing::warn!(path = %parts.uri.path(), reason = %e, "fabric signature rejected");
            (StatusCode::UNAUTHORIZED, "Invalid node signature".to_string())
        })?;
        incr(&METRICS.fabric_sig_ok);

        // The whole point of verifying: downstream must be able to ask *which*
        // node this is, not merely that it is one.
        parts.extensions.insert(Principal::Node(node_id));

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
                // Deliberately NOT Principal::Node: every holder of this secret
                // is indistinguishable, so it cannot answer "which node is
                // this". Handlers needing a specific identity will refuse it.
                let mut req = req;
                req.extensions_mut().insert(Principal::SharedSecret);
                Ok(next.run(req).await)
            }
            _ => Err((StatusCode::UNAUTHORIZED, "Invalid node token".to_string())),
        };
    }

    // ---- 3. User JWT ----
    let (mut parts, body) = req.into_parts();
    let AuthenticatedUser(claims) = AuthenticatedUser::from_request_parts(&mut parts, &state).await?;
    parts.extensions.insert(Principal::User(claims));
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
) -> Result<Uuid, SigError> {
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
        Ok(node_uuid)
    } else {
        Err(SigError::BadSignature)
    }
}

#[cfg(test)]
mod principal_tests {
    use super::*;

    fn claims(role: &str) -> Claims {
        Claims {
            sub: "11111111-1111-1111-1111-111111111111".into(),
            exp: 0,
            provider: "local".into(),
            role: role.into(),
        }
    }

    #[test]
    fn only_a_signing_node_can_act_as_a_node() {
        let id = Uuid::nil();
        assert_eq!(Principal::Node(id).node_id(), Some(id));
        // A user is a person, not an outpost.
        assert_eq!(Principal::User(claims("admin")).node_id(), None);
        // The decisive one: the shared secret authenticates membership only.
        // Every holder is identical, so it can never answer "which node".
        // Treating it as a node id is exactly how one compromised outpost
        // would have impersonated the whole fabric.
        assert_eq!(Principal::SharedSecret.node_id(), None);
    }

    #[test]
    fn admin_is_a_user_role_not_a_fabric_property() {
        assert!(Principal::User(claims("admin")).is_admin());
        assert!(!Principal::User(claims("user")).is_admin());
        assert!(!Principal::Node(Uuid::nil()).is_admin());
        assert!(!Principal::SharedSecret.is_admin());
    }

    #[test]
    fn command_execution_accepts_only_nodes_and_admins() {
        // Mirrors routes::commands::execute. A plain user must not be able to
        // LOCKDOWN an outpost — or UNLOCK one that autonomy locked after
        // physical tamper, which was the sharper half of the hole.
        let may_actuate =
            |p: &Principal| matches!(p, Principal::Node(_)) || p.is_admin();

        assert!(may_actuate(&Principal::Node(Uuid::nil())));
        assert!(may_actuate(&Principal::User(claims("admin"))));
        assert!(!may_actuate(&Principal::User(claims("user"))));
        assert!(!may_actuate(&Principal::SharedSecret));
    }

    #[test]
    fn describe_identifies_the_caller_for_audit() {
        let id = Uuid::nil();
        assert_eq!(Principal::Node(id).describe(), format!("node:{id}"));
        assert!(Principal::User(claims("user")).describe().starts_with("user:"));
        assert_eq!(Principal::SharedSecret.describe(), "shared-secret");
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

