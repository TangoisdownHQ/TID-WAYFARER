use axum::{
    extract::{State, Json},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use crate::AppState;
use chrono::Utc;
use uuid::Uuid;
use std::borrow::Cow;
use base64::Engine as _;

/// Represents a node registration request. The signature proves possession
/// of the Ed25519 key the node claims: sign("register|{node_id}|{name}|
/// {api_endpoint}|{ts}") with the node's secret key.
#[derive(Deserialize)]
pub struct NodeRegisterRequest {
    pub node_id: String,
    pub public_key: String,
    pub name: String,
    pub api_endpoint: String,
    pub ts: i64,
    pub signature: String,
    /// ML-KEM public key so peers can seal DTN traffic for this node.
    /// Optional: a node on an older build simply won't send one, and DTN
    /// falls back to plaintext for it rather than refusing to register it.
    pub kem_public_key: Option<String>,
}

/// Registration replay window (seconds of clock skew tolerated).
const REGISTRATION_MAX_SKEW_SECS: i64 = 300;

/// Canonical byte string both sides sign/verify for registration.
pub fn registration_message(node_id: &str, name: &str, api_endpoint: &str, ts: i64) -> String {
    format!("register|{node_id}|{name}|{api_endpoint}|{ts}")
}

/// Represents a node in the registry
#[derive(Serialize)]
pub struct NodeInfo {
    pub node_id: String,
    pub name: String,
    pub api_endpoint: String,
    pub last_seen: String,
}

/// Response Core sends back. Includes core's own identity so registration
/// doubles as a mutual identity exchange — the outpost stores core in its
/// registry and can then verify core-signed traffic.
#[derive(Serialize)]
pub struct NodeRegistrationResponse {
    pub status: String,
    pub node_id: String,
    pub registered_at: String,
    pub core_node_id: Option<String>,
    pub core_public_key: Option<String>,
    pub core_name: Option<String>,
    pub core_kem_public_key: Option<String>,
}

/// Router
pub fn node_routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register_node))
        .route("/list", get(list_nodes))
        .route("/:id/telemetry-secret", post(set_telemetry_secret))
        .route("/:id/rotate-key", post(rotate_key))
        .route("/:id/revoke", post(revoke_node))
        .route("/:id/key-history", get(key_history))
}

/// Canonical message the *new* key signs to prove the rotation is genuine.
pub fn rotation_message(node_id: &str, new_public_key: &str, ts: i64) -> String {
    format!("rotate|{node_id}|{new_public_key}|{ts}")
}

#[derive(Deserialize)]
pub struct RotateKeyRequest {
    pub new_public_key: String,
    pub ts: i64,
    /// Signature over [`rotation_message`] made with the **new** secret key.
    pub signature: String,
    pub reason: Option<String>,
}

/// Admin-only: replace a node's Ed25519 public key.
///
/// Two independent checks have to pass. The admin JWT authorises *that* a
/// rotation may happen; the signature proves the caller actually holds the new
/// secret key, so a typo or an attacker-supplied key can't strand the node
/// with a key nobody has. The replay window matches registration.
pub async fn rotate_key(
    State(state): State<AppState>,
    admin: crate::routes::auth_middleware::AdminUser,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(body): Json<RotateKeyRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;

    let skew = (Utc::now().timestamp() - body.ts).abs();
    if skew > REGISTRATION_MAX_SKEW_SECS {
        return Err((StatusCode::BAD_REQUEST, format!("stale rotation timestamp ({skew}s)")));
    }

    // Proof of possession of the NEW key.
    let message = rotation_message(&id.to_string(), &body.new_public_key, body.ts);
    if !crate::services::identity::verify_signature(
        &body.new_public_key,
        message.as_bytes(),
        &body.signature,
    ) {
        tracing::warn!(node_id = %id, "key rotation rejected: new key did not sign the challenge");
        return Err((
            StatusCode::UNAUTHORIZED,
            "signature does not verify against the new public key".into(),
        ));
    }

    let old_key: Option<String> =
        sqlx::query_scalar("SELECT public_key FROM node_registry WHERE node_id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .flatten();

    let Some(old_key) = old_key else {
        return Err((StatusCode::NOT_FOUND, "node not in registry".into()));
    };

    if old_key == body.new_public_key {
        return Err((StatusCode::BAD_REQUEST, "new key is identical to the current key".into()));
    }

    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Rotating also lifts a revocation: a node with a fresh key the operator
    // has vouched for is trustworthy again, which is the whole recovery path.
    sqlx::query(
        r#"
        UPDATE node_registry
        SET public_key = $2, key_rotated_at = NOW(),
            revoked = false, revoked_at = NULL, revoked_reason = NULL
        WHERE node_id = $1
        "#,
    )
    .bind(id)
    .bind(&body.new_public_key)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    sqlx::query(
        r#"
        INSERT INTO node_key_history (node_id, old_public_key, new_public_key, reason, rotated_by)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(id)
    .bind(&old_key)
    .bind(&body.new_public_key)
    .bind(body.reason.as_deref().unwrap_or("rotated"))
    .bind(&admin.0.sub)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tracing::warn!(node_id = %id, rotated_by = %admin.0.sub, "node key rotated");
    Ok(Json(serde_json::json!({
        "status": "rotated",
        "nodeId": id,
        "rotatedBy": admin.0.sub,
    })))
}

#[derive(Deserialize)]
pub struct RevokeRequest {
    pub reason: Option<String>,
    /// Set false to lift a revocation without rotating the key.
    pub revoked: Option<bool>,
}

/// Admin-only: revoke (or restore) a node. A revoked node's signatures are
/// refused by the fabric guard, which is the fast lever during an incident —
/// rotation is the considered fix, revocation is the one you pull at 3am.
pub async fn revoke_node(
    State(state): State<AppState>,
    admin: crate::routes::auth_middleware::AdminUser,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(body): Json<RevokeRequest>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;

    let revoked = body.revoked.unwrap_or(true);
    let result = sqlx::query(
        r#"
        UPDATE node_registry
        SET revoked = $2,
            revoked_at = CASE WHEN $2 THEN NOW() ELSE NULL END,
            revoked_reason = CASE WHEN $2 THEN $3 ELSE NULL END
        WHERE node_id = $1
        "#,
    )
    .bind(id)
    .bind(revoked)
    .bind(body.reason.as_deref().unwrap_or("revoked by admin"))
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    if result.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "node not in registry".into()));
    }

    tracing::warn!(node_id = %id, revoked, by = %admin.0.sub, "node revocation changed");
    Ok(Json(serde_json::json!({
        "status": if revoked { "revoked" } else { "restored" },
        "nodeId": id,
    })))
}

/// Admin-only: the rotation trail for one node.
pub async fn key_history(
    State(state): State<AppState>,
    _admin: crate::routes::auth_middleware::AdminUser,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    use sqlx::Row;

    let rows = sqlx::query(
        r#"
        SELECT old_public_key, new_public_key, reason, rotated_by, rotated_at
        FROM node_key_history WHERE node_id = $1 ORDER BY rotated_at DESC
        "#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(serde_json::json!(rows
        .iter()
        .map(|r| serde_json::json!({
            "oldPublicKey": r.get::<Option<String>, _>("old_public_key"),
            "newPublicKey": r.get::<String, _>("new_public_key"),
            "reason": r.get::<Option<String>, _>("reason"),
            "rotatedBy": r.get::<Option<String>, _>("rotated_by"),
            "rotatedAt": r.get::<chrono::DateTime<chrono::Utc>, _>("rotated_at").to_rfc3339(),
        }))
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
pub struct TelemetrySecret {
    pub secret: String,
}

/// Admin-only: set/rotate the HMAC secret a node must sign telemetry with.
/// Once set, unsigned or mis-signed telemetry from that node is rejected.
pub async fn set_telemetry_secret(
    State(state): State<AppState>,
    _admin: crate::routes::auth_middleware::AdminUser,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    Json(body): Json<TelemetrySecret>,
) -> Result<Json<NodeRegistrationResponse>, axum::http::StatusCode> {
    let updated = sqlx::query("UPDATE node_registry SET hmac_secret = $2 WHERE node_id = $1")
        .bind(id)
        .bind(&body.secret)
        .execute(&state.db)
        .await
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    if updated.rows_affected() == 0 {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    Ok(reject("telemetry-secret-set", id.to_string()))
}

fn reject(status: &str, node_id: String) -> Json<NodeRegistrationResponse> {
    Json(NodeRegistrationResponse {
        status: status.into(),
        node_id,
        registered_at: Utc::now().to_rfc3339(),
        core_node_id: None,
        core_public_key: None,
        core_name: None,
        core_kem_public_key: None,
    })
}

/// Register or update node
pub async fn register_node(
    State(state): State<AppState>,
    Json(payload): Json<NodeRegisterRequest>,
) -> Json<NodeRegistrationResponse> {

    let node_uuid = match Uuid::parse_str(&payload.node_id) {
        Ok(id) => id,
        Err(_) => return reject("invalid-uuid", payload.node_id),
    };

    // Replay window: signed registrations expire.
    let skew = (Utc::now().timestamp() - payload.ts).abs();
    if skew > REGISTRATION_MAX_SKEW_SECS {
        tracing::error!("❌ Stale registration from {} (skew {skew}s)", payload.node_id);
        return reject("stale-timestamp", payload.node_id);
    }

    // Proof of key possession: signature over the canonical message must
    // verify against the public key being registered.
    let message = registration_message(
        &payload.node_id,
        &payload.name,
        &payload.api_endpoint,
        payload.ts,
    );
    if !crate::services::identity::verify_signature(
        &payload.public_key,
        message.as_bytes(),
        &payload.signature,
    ) {
        tracing::error!("❌ Bad registration signature from {}", payload.node_id);
        return reject("bad-signature", payload.node_id);
    }

    // Identity hijack protection: a known node_id may not swap its key by
    // re-registering. Changing a key is an admin operation with its own
    // proof-of-possession — see `rotate_key`.
    let existing: Option<(String, bool)> = sqlx::query_as(
        "SELECT public_key, revoked FROM node_registry WHERE node_id = $1",
    )
    .bind(node_uuid)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    if let Some((known, revoked)) = existing {
        // A revoked node cannot re-register its way back in; an admin must
        // rotate its key (which clears the revocation) or restore it.
        if revoked {
            tracing::warn!(node_id = %payload.node_id, "registration refused: node is revoked");
            return reject("revoked", payload.node_id);
        }
        if known != payload.public_key {
            tracing::error!(node_id = %payload.node_id, "key mismatch on re-registration — rejected");
            return reject("key-mismatch", payload.node_id);
        }
    }

    // Runtime API rather than query!: the macro's offline cache has no entry
    // for the kem_public_key column and sqlx-cli isn't available to regenerate.
    let result = sqlx::query(
        r#"
        INSERT INTO node_registry (node_id, name, api_endpoint, public_key, kem_public_key, last_seen)
        VALUES ($1, $2, $3, $4, $5, NOW())
        ON CONFLICT (node_id) DO UPDATE SET
            name = EXCLUDED.name,
            api_endpoint = EXCLUDED.api_endpoint,
            public_key = EXCLUDED.public_key,
            -- Keep the stored key if this registration omitted one, so an
            -- older-build heartbeat cannot erase a peer's ability to seal.
            kem_public_key = COALESCE(EXCLUDED.kem_public_key, node_registry.kem_public_key),
            last_seen = NOW()
        "#,
    )
    .bind(node_uuid)
    .bind(&payload.name)
    .bind(&payload.api_endpoint)
    .bind(&payload.public_key)
    .bind(&payload.kem_public_key)
    .execute(&state.db)
    .await;

    if let Err(e) = result {
        if let sqlx::Error::Database(db_err) = &e {
            if db_err.code() == Some(Cow::Borrowed("23505")) {
                // The endpoint is already claimed by a different node id.
                //
                // Two very different situations produce this, and the endpoint
                // alone cannot tell them apart: an outpost that regenerated its
                // identity (redeploy, lost key volume, rebuilt host) legitimately
                // reclaiming its own address, or an attacker trying to take over a
                // live peer's address so traffic routes to it.
                //
                // Refusing both meant a redeployed outpost was permanently locked
                // out of its own endpoint with no recovery path short of manual
                // SQL — which is what happened here. Allowing both would make the
                // uniqueness check pointless.
                //
                // So: let a *stale* claim be taken over, and refuse a live one. If
                // the incumbent has been heard from inside the window it is still
                // out there and its address is not up for grabs.
                let takeover_after = std::env::var("REGISTRATION_TAKEOVER_SECS")
                    .ok()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(900);

                let incumbent: Option<(uuid::Uuid, Option<chrono::DateTime<chrono::Utc>>)> =
                    sqlx::query_as(
                        "SELECT node_id, last_seen FROM node_registry WHERE api_endpoint = $1",
                    )
                    .bind(&payload.api_endpoint)
                    .fetch_optional(&state.db)
                    .await
                    .ok()
                    .flatten();

                if let Some((old_node, last_seen)) = incumbent {
                    let stale = last_seen
                        .map(|t| (Utc::now() - t).num_seconds() >= takeover_after)
                        .unwrap_or(true);

                    if stale {
                        tracing::warn!(
                            endpoint = %payload.api_endpoint,
                            previous_node = %old_node,
                            new_node = %payload.node_id,
                            "endpoint reclaimed from a stale registration"
                        );
                        let replaced = sqlx::query(
                            r#"
                            UPDATE node_registry
                            SET node_id = $1, name = $2, public_key = $3,
                                kem_public_key = COALESCE($4, kem_public_key),
                                revoked = false, last_seen = NOW()
                            WHERE api_endpoint = $5
                            "#,
                        )
                        .bind(node_uuid)
                        .bind(&payload.name)
                        .bind(&payload.public_key)
                        .bind(&payload.kem_public_key)
                        .bind(&payload.api_endpoint)
                        .execute(&state.db)
                        .await;

                        if replaced.is_ok() {
                            return Json(NodeRegistrationResponse {
                                status: "ok".into(),
                                node_id: payload.node_id.clone(),
                                registered_at: Utc::now().to_rfc3339(),
                                core_node_id: Some(state.identity.node_id.clone()),
                                core_public_key: Some(state.identity.public_key.clone()),
                                core_name: Some(
                                    std::env::var("OUTPOST_NAME")
                                        .unwrap_or_else(|_| "tid-wayfarer".into()),
                                ),
                                core_kem_public_key: Some(
                                    base64::engine::general_purpose::STANDARD.encode(
                                        pqcrypto_traits::kem::PublicKey::as_bytes(&state.commsec.pk),
                                    ),
                                ),
                            });
                        }
                    } else {
                        tracing::warn!(
                            endpoint = %payload.api_endpoint,
                            incumbent = %old_node,
                            claimant = %payload.node_id,
                            "refused: endpoint belongs to a node still in contact"
                        );
                        return reject(
                            "that endpoint is registered to another node that is still in contact",
                            payload.node_id,
                        );
                    }
                }
            }
        }

        tracing::error!(error = %e, node = %payload.node_id, "registration failed");
        return reject("registration failed", payload.node_id);
    }

    Json(NodeRegistrationResponse {
        status: "ok".into(),
        node_id: payload.node_id.clone(),
        registered_at: Utc::now().to_rfc3339(),
        core_node_id: Some(state.identity.node_id.clone()),
        core_public_key: Some(state.identity.public_key.clone()),
        core_name: Some(std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into())),
        core_kem_public_key: Some(base64::engine::general_purpose::STANDARD.encode(
            pqcrypto_traits::kem::PublicKey::as_bytes(&state.commsec.pk),
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::{registration_message, rotation_message};
    use crate::services::identity::{sign_message, verify_signature, NodeIdentity};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use uuid::Uuid;

    fn identity() -> NodeIdentity {
        let sk = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(sk.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(sk.to_bytes()),
        }
    }

    #[test]
    fn rotation_challenge_verifies_against_the_new_key() {
        let node_id = Uuid::new_v4().to_string();
        let new_key = identity();
        let ts = 1_760_000_000;

        let msg = rotation_message(&node_id, &new_key.public_key, ts);
        let sig = sign_message(&new_key, msg.as_bytes());

        assert!(verify_signature(&new_key.public_key, msg.as_bytes(), &sig));
    }

    #[test]
    fn old_key_cannot_authorise_its_own_replacement() {
        // The point of the challenge: possession of the *new* key is what is
        // proven. A signature from the outgoing key must not pass.
        let node_id = Uuid::new_v4().to_string();
        let old_key = identity();
        let new_key = identity();
        let ts = 1_760_000_000;

        let msg = rotation_message(&node_id, &new_key.public_key, ts);
        let sig_from_old = sign_message(&old_key, msg.as_bytes());

        assert!(!verify_signature(&new_key.public_key, msg.as_bytes(), &sig_from_old));
    }

    #[test]
    fn rotation_challenge_is_bound_to_the_node_and_key() {
        let new_key = identity();
        let ts = 1_760_000_000;
        let node_a = Uuid::new_v4().to_string();
        let node_b = Uuid::new_v4().to_string();

        let sig = sign_message(&new_key, rotation_message(&node_a, &new_key.public_key, ts).as_bytes());

        // Same key, different node id — must not transfer.
        let other = rotation_message(&node_b, &new_key.public_key, ts);
        assert!(!verify_signature(&new_key.public_key, other.as_bytes(), &sig));
    }

    #[test]
    fn rotation_and_registration_messages_do_not_collide() {
        // Distinct prefixes stop a registration signature from being replayed
        // as a rotation authorisation.
        let node_id = Uuid::new_v4().to_string();
        let ts = 1_760_000_000;
        assert_ne!(
            rotation_message(&node_id, "KEY", ts),
            registration_message(&node_id, "KEY", "", ts)
        );
        assert!(rotation_message(&node_id, "KEY", ts).starts_with("rotate|"));
    }
}

/// List all nodes
pub async fn list_nodes(State(state): State<AppState>) -> Json<Vec<NodeInfo>> {
    let rows = sqlx::query!(
        r#"
        SELECT node_id::text, name, api_endpoint, last_seen
        FROM node_registry
        ORDER BY last_seen DESC
        "#
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let nodes = rows
        .into_iter()
        .map(|r| NodeInfo {
            node_id: r.node_id.unwrap_or_default(),
            name: r.name,
            api_endpoint: r.api_endpoint,
            last_seen: r.last_seen
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "unknown".into()),
        })
        .collect();

    Json(nodes)
}

