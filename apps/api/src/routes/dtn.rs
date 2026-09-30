//! DTN messaging API — store-and-forward payloads between outposts.
//!
//! Send: POST /api/dtn/send enqueues into dtn_outbox; the DTN forwarder
//! daemon delivers with exponential backoff whenever a route exists.
//! Receive: peers POST to /api/dtn/receive (behind the fabric guard), which
//! lands the payload in dtn_inbox for local consumption.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::services::dtn_crypto;
use crate::services::identity::{sign_message, verify_signature};
use crate::AppState;

/// Canonical bytes signed for a DTN envelope. serde_json's default map is
/// ordered (BTreeMap), so serializing the payload is deterministic across
/// nodes running the same schema.
fn dtn_signing_bytes(src_node_id: &str, payload: &Value) -> Vec<u8> {
    format!("dtn|{src_node_id}|{payload}").into_bytes()
}

/// Canonical bytes signed for a *sealed* envelope. Distinct prefix from the
/// plaintext form so a signature over one shape can never be reinterpreted as
/// the other.
fn sealed_signing_bytes(src_node_id: &str, ciphertext_b64: &str) -> Vec<u8> {
    format!("dtn-sealed|{src_node_id}|{ciphertext_b64}").into_bytes()
}

pub fn dtn_routes() -> Router<AppState> {
    Router::new()
        .route("/send", post(send_message))
        .route("/receive", post(receive_message))
        .route("/outbox", get(list_outbox))
        .route("/inbox", get(list_inbox))
}

#[derive(Deserialize)]
struct SendRequest {
    dest_node_id: Uuid,
    payload: Value,
    /// Explicit delivery URL. Defaults to the destination's registered
    /// api_endpoint + /dtn/receive.
    endpoint: Option<String>,
}

async fn send_message(
    State(state): State<AppState>,
    Json(body): Json<SendRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, String)> {
    // One lookup for both routing and sealing: the endpoint to deliver to and
    // the recipient's ML-KEM key to encrypt for.
    let registry_row = sqlx::query(
        "SELECT api_endpoint, kem_public_key FROM node_registry WHERE node_id = $1",
    )
    .bind(body.dest_node_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let recipient_kem: Option<String> = registry_row
        .as_ref()
        .and_then(|r| r.try_get("kem_public_key").ok())
        .flatten();

    let endpoint = match body.endpoint {
        Some(e) => e,
        None => {
            let row = registry_row.as_ref().ok_or((
                StatusCode::NOT_FOUND,
                format!("node {} not in registry and no endpoint given", body.dest_node_id),
            ))?;
            let api: String = row.get("api_endpoint");
            format!("{}/dtn/receive", api.trim_end_matches('/'))
        }
    };

    let dest = body.dest_node_id.to_string();

    // Seal the payload when the recipient has published a KEM key. A peer on
    // an older build has none; that message still goes (plaintext, flagged)
    // rather than being dropped, so a partially-upgraded fabric keeps working.
    let (envelope, encrypted) = match recipient_kem.as_deref() {
        Some(kem_key) => match dtn_crypto::seal(kem_key, &state.identity.node_id, &dest, &body.payload) {
            Ok(sealed) => {
                // Sign the ciphertext, not the plaintext: the recipient can
                // then authenticate the sender before doing any decryption.
                let signature = sign_message(
                    &state.identity,
                    &sealed_signing_bytes(&state.identity.node_id, &sealed.ciphertext),
                );
                (
                    json!({
                        "src_node_id": state.identity.node_id,
                        "dest_node_id": dest,
                        "scheme": sealed.scheme,
                        "kem_ciphertext": sealed.kem_ciphertext,
                        "nonce": sealed.nonce,
                        "ciphertext": sealed.ciphertext,
                        "signature": signature,
                    }),
                    true,
                )
            }
            Err(e) => {
                tracing::error!(dest = %dest, error = ?e, "could not seal DTN payload");
                return Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "recipient key present but sealing failed".to_string(),
                ));
            }
        },
        None => {
            tracing::warn!(
                dest = %dest,
                "peer has no ML-KEM key on file; sending DTN payload unencrypted"
            );
            let signature = sign_message(
                &state.identity,
                &dtn_signing_bytes(&state.identity.node_id, &body.payload),
            );
            (
                json!({
                    "src_node_id": state.identity.node_id,
                    "payload": body.payload,
                    "signature": signature,
                }),
                false,
            )
        }
    };

    let row = sqlx::query(
        r#"
        INSERT INTO dtn_outbox (dest_node_id, endpoint, payload, encrypted)
        VALUES ($1, $2, $3, $4)
        RETURNING id
        "#,
    )
    .bind(body.dest_node_id)
    .bind(&endpoint)
    .bind(&envelope)
    .bind(encrypted)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "id": row.get::<i64, _>("id"),
            "destNodeId": body.dest_node_id,
            "endpoint": endpoint,
            "encrypted": encrypted,
            "status": "queued",
        })),
    ))
}

#[derive(Deserialize)]
struct ReceiveEnvelope {
    src_node_id: Option<Uuid>,
    /// Present on plaintext envelopes only.
    payload: Option<Value>,
    signature: Option<String>,
    /// Sealed-envelope fields (see [`dtn_crypto`]).
    scheme: Option<String>,
    kem_ciphertext: Option<String>,
    nonce: Option<String>,
    ciphertext: Option<String>,
    dest_node_id: Option<String>,
}

async fn receive_message(
    State(state): State<AppState>,
    Json(envelope): Json<ReceiveEnvelope>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sealed = envelope.ciphertext.is_some();

    // Authenticate before decrypting. The signature covers the ciphertext on
    // a sealed envelope and the plaintext on a legacy one.
    let mut verified = false;
    if let (Some(src), Some(sig)) = (envelope.src_node_id, envelope.signature.as_deref()) {
        let known_key: Option<String> =
            sqlx::query_scalar("SELECT public_key FROM node_registry WHERE node_id = $1")
                .bind(src)
                .fetch_optional(&state.db)
                .await
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        if let Some(pk) = known_key {
            let signed_bytes = match envelope.ciphertext.as_deref() {
                Some(ct) => sealed_signing_bytes(&src.to_string(), ct),
                None => dtn_signing_bytes(
                    &src.to_string(),
                    envelope.payload.as_ref().unwrap_or(&Value::Null),
                ),
            };
            verified = verify_signature(&pk, &signed_bytes, sig);
            if !verified {
                tracing::warn!(src = %src, "DTN envelope failed signature verification");
            }
        }
    }

    // Open the payload. A failure here is recorded rather than fatal: the
    // message is still stored so an operator can see that something arrived
    // that this node could not read, which is exactly the signal you want if
    // a peer is sealing to a stale key.
    let (stored_payload, decrypted) = if sealed {
        let src = envelope.src_node_id.map(|s| s.to_string()).unwrap_or_default();
        let dest = envelope
            .dest_node_id
            .clone()
            .unwrap_or_else(|| state.identity.node_id.clone());

        match dtn_crypto::open(
            &state.commsec.sk,
            envelope.kem_ciphertext.as_deref().unwrap_or(""),
            envelope.nonce.as_deref().unwrap_or(""),
            envelope.ciphertext.as_deref().unwrap_or(""),
            &src,
            &dest,
        ) {
            Ok(plaintext) => (plaintext, true),
            Err(e) => {
                tracing::error!(src = %src, error = %e, scheme = ?envelope.scheme, "could not open sealed DTN payload");
                (
                    json!({
                        "error": "undecryptable",
                        "detail": e.to_string(),
                        "scheme": envelope.scheme,
                    }),
                    false,
                )
            }
        }
    } else {
        (envelope.payload.clone().unwrap_or(Value::Null), false)
    };

    sqlx::query(
        "INSERT INTO dtn_inbox (src_node_id, payload, signature, verified, encrypted) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(envelope.src_node_id)
    .bind(&stored_payload)
    .bind(&envelope.signature)
    .bind(verified)
    .bind(sealed)
    .execute(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "status": "received",
        "verified": verified,
        "sealed": sealed,
        "decrypted": decrypted,
    })))
}

#[derive(Deserialize)]
struct ListFilter {
    limit: Option<i64>,
}

async fn list_outbox(
    State(state): State<AppState>,
    Query(f): Query<ListFilter>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = sqlx::query(
        r#"
        SELECT id, dest_node_id, endpoint, payload, attempts, next_try_at, created_at
        FROM dtn_outbox
        ORDER BY id DESC
        LIMIT $1
        "#,
    )
    .bind(f.limit.unwrap_or(100).clamp(1, 1000))
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let msgs: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "destNodeId": r.get::<Uuid, _>("dest_node_id"),
                "endpoint": r.get::<String, _>("endpoint"),
                "payload": r.get::<Value, _>("payload"),
                "attempts": r.get::<i32, _>("attempts"),
                "nextTryAt": r.get::<chrono::DateTime<chrono::Utc>, _>("next_try_at").to_rfc3339(),
                "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!(msgs)))
}

async fn list_inbox(
    State(state): State<AppState>,
    Query(f): Query<ListFilter>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = sqlx::query(
        r#"
        SELECT id, src_node_id, payload, verified, received_at
        FROM dtn_inbox
        ORDER BY id DESC
        LIMIT $1
        "#,
    )
    .bind(f.limit.unwrap_or(100).clamp(1, 1000))
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let msgs: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "srcNodeId": r.get::<Option<Uuid>, _>("src_node_id"),
                "payload": r.get::<Value, _>("payload"),
                "verified": r.get::<bool, _>("verified"),
                "receivedAt": r.get::<chrono::DateTime<chrono::Utc>, _>("received_at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!(msgs)))
}
