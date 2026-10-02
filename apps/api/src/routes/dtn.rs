//! DTN messaging API — store-and-forward payloads between outposts.
//!
//! Send: POST /api/dtn/send enqueues into dtn_outbox; the DTN forwarder
//! daemon delivers with exponential backoff whenever a route exists.
//! Receive: peers POST to /api/dtn/receive, which authenticates the envelope,
//! refuses a repeat, and lands the payload in dtn_inbox for local consumption.
//!
//! # Why reception is idempotent
//!
//! The forwarder retries on any non-2xx *and* on a dropped connection, so a
//! delivery that arrived but whose response was lost is sent again. On a link
//! that stays up for minutes at a time this is not an edge case, it is how the
//! protocol works. The receiver therefore has to be able to say "I already
//! have this one" — and once it can, a replayed envelope and an honest
//! retransmit are the same thing and both are harmless.
//!
//! That is the whole shape of the replay defence: a message identity the
//! signature covers, remembered for as long as the message could legitimately
//! still arrive.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::Caller;
use crate::services::dtn_crypto;
use crate::services::identity::{sign_message, verify_signature};
use crate::AppState;

/// Envelope format this node emits and accepts.
///
/// v1 envelopes signed only `src|payload`: no message id, no clock, no
/// recipient. They are refused rather than downgraded to, because accepting
/// both shapes would mean every v2 guarantee is optional for anyone who
/// claims to be old.
pub const ENVELOPE_VERSION: u8 = 2;

/// How far ahead of our clock a sender may claim to be.
///
/// Distinct from a delivery deadline: a bundle may legitimately take days to
/// arrive, but it cannot have been *written* in the future. Matches the
/// fabric transport's window so the two layers agree about clocks.
const MAX_FUTURE_SKEW_SECS: i64 = 300;

/// Longest bundle lifetime this node will accept, and therefore how long a
/// message id is remembered.
///
/// The sender chooses the lifetime and signs it, so it cannot be trimmed in
/// flight — but it is capped on arrival, because the replay window has to be
/// held open for the whole of it and an uncapped lifetime is an uncapped
/// table. A week is generous for a relay chain; interplanetary one-way light
/// time is minutes, not days.
fn max_lifetime() -> Duration {
    let secs = std::env::var("DTN_MAX_LIFETIME_SECS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(7 * 24 * 3600);
    Duration::seconds(secs)
}

/// Default lifetime stamped on outgoing bundles when the caller names none.
fn default_lifetime() -> Duration {
    let secs = std::env::var("DTN_DEFAULT_LIFETIME_SECS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(24 * 3600);
    Duration::seconds(secs)
}

/// Length-prefixed concatenation under a domain label.
///
/// Public because this is a wire format, not an implementation detail: another
/// outpost — or a test forging an envelope on purpose — has to be able to
/// produce exactly these bytes.
///
/// The previous form was `format!("dtn|{src}|{payload}")`. With fixed-width
/// fields either side of a separator that happened to be unambiguous, but it
/// stops being so the moment a variable-length field is added — and this
/// change adds four. Prefixing each field with its length means no
/// combination of field contents can be re-cut into a different message that
/// hashes the same, so a signature means exactly one thing.
pub fn canonical(domain: &str, fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(domain.as_bytes());
    for f in fields {
        out.extend_from_slice(&(f.len() as u32).to_be_bytes());
        out.extend_from_slice(f);
    }
    out
}

/// Fields every envelope binds regardless of whether the body is sealed.
///
/// `dest` is in here because without it an envelope addressed to one outpost
/// verified at any other: the signature said who wrote the message but not who
/// it was for, so a captured bundle could be re-aimed. Sealed bodies already
/// bound the recipient through the AEAD's associated data; plaintext ones bound
/// nothing.
pub struct Header<'a> {
    pub msg_id: &'a str,
    pub src: &'a str,
    pub dest: &'a str,
    pub sent_at: &'a str,
    pub expires_at: &'a str,
}

impl Header<'_> {
    pub fn parts(&self) -> [&[u8]; 5] {
        [
            self.msg_id.as_bytes(),
            self.src.as_bytes(),
            self.dest.as_bytes(),
            self.sent_at.as_bytes(),
            self.expires_at.as_bytes(),
        ]
    }
}

/// Canonical bytes for a plaintext envelope.
pub fn plain_signing_bytes(h: &Header<'_>, payload: &Value) -> Vec<u8> {
    let body = payload.to_string();
    let p = h.parts();
    canonical(
        "dtn-v2|plain",
        &[p[0], p[1], p[2], p[3], p[4], body.as_bytes()],
    )
}

/// Canonical bytes for a sealed envelope. A distinct domain from the plaintext
/// form, so a signature over one shape can never be read as the other.
pub fn sealed_signing_bytes(h: &Header<'_>, kem_ct: &str, nonce: &str, ct: &str) -> Vec<u8> {
    let p = h.parts();
    canonical(
        "dtn-v2|sealed",
        &[
            p[0],
            p[1],
            p[2],
            p[3],
            p[4],
            kem_ct.as_bytes(),
            nonce.as_bytes(),
            ct.as_bytes(),
        ],
    )
}

pub fn dtn_routes() -> Router<AppState> {
    Router::new()
        .route("/send", post(send_message))
        .route("/receive", post(receive_message))
        .route("/outbox", get(list_outbox))
        .route("/inbox", get(list_inbox))
        .route("/rejected", get(list_rejected))
}

#[derive(Deserialize)]
struct SendRequest {
    dest_node_id: Uuid,
    payload: Value,
    /// Explicit delivery URL. Defaults to the destination's registered
    /// api_endpoint + /dtn/receive.
    endpoint: Option<String>,
    /// Bundle lifetime. Past this the forwarder stops trying and the receiver
    /// refuses it, which is what makes the replay window finite.
    ttl_secs: Option<i64>,
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

    // Stamped once, here, and signed. Every later hop and the receiver all
    // read the same values; nothing downstream may re-derive them.
    let msg_id = Uuid::new_v4();
    let now = Utc::now();
    let lifetime = match body.ttl_secs {
        Some(s) if s > 0 => Duration::seconds(s).min(max_lifetime()),
        _ => default_lifetime(),
    };
    let expires = now + lifetime;

    let msg_id_s = msg_id.to_string();
    let sent_at_s = now.to_rfc3339();
    let expires_s = expires.to_rfc3339();
    let header = Header {
        msg_id: &msg_id_s,
        src: &state.identity.node_id,
        dest: &dest,
        sent_at: &sent_at_s,
        expires_at: &expires_s,
    };

    // Seal the payload when the recipient has published a KEM key. A peer
    // without one gets plaintext (flagged and logged) rather than nothing, so
    // a partially-upgraded fabric keeps working.
    let (envelope, encrypted) = match recipient_kem.as_deref() {
        Some(kem_key) => match dtn_crypto::seal(kem_key, &state.identity.node_id, &dest, &body.payload) {
            Ok(sealed) => {
                // Sign the ciphertext, not the plaintext: the recipient can
                // then authenticate the sender before doing any decryption.
                let signature = sign_message(
                    &state.identity,
                    &sealed_signing_bytes(&header, &sealed.kem_ciphertext, &sealed.nonce, &sealed.ciphertext),
                );
                (
                    json!({
                        "v": ENVELOPE_VERSION,
                        "msg_id": msg_id_s,
                        "src_node_id": state.identity.node_id,
                        "dest_node_id": dest,
                        "sent_at": sent_at_s,
                        "expires_at": expires_s,
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
            let signature =
                sign_message(&state.identity, &plain_signing_bytes(&header, &body.payload));
            (
                json!({
                    "v": ENVELOPE_VERSION,
                    "msg_id": msg_id_s,
                    "src_node_id": state.identity.node_id,
                    "dest_node_id": dest,
                    "sent_at": sent_at_s,
                    "expires_at": expires_s,
                    "payload": body.payload,
                    "signature": signature,
                }),
                false,
            )
        }
    };

    let row = sqlx::query(
        r#"
        INSERT INTO dtn_outbox (dest_node_id, endpoint, payload, encrypted, msg_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id
        "#,
    )
    .bind(body.dest_node_id)
    .bind(&endpoint)
    .bind(&envelope)
    .bind(encrypted)
    .bind(msg_id)
    .bind(expires)
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "id": row.get::<i64, _>("id"),
            "msgId": msg_id,
            "destNodeId": body.dest_node_id,
            "endpoint": endpoint,
            "encrypted": encrypted,
            "expiresAt": expires_s,
            "status": "queued",
        })),
    ))
}

#[derive(Deserialize)]
struct ReceiveEnvelope {
    v: Option<u8>,
    msg_id: Option<Uuid>,
    src_node_id: Option<Uuid>,
    dest_node_id: Option<String>,
    sent_at: Option<String>,
    expires_at: Option<String>,
    signature: Option<String>,
    /// Present on plaintext envelopes only.
    payload: Option<Value>,
    /// Sealed-envelope fields (see [`dtn_crypto`]).
    scheme: Option<String>,
    kem_ciphertext: Option<String>,
    nonce: Option<String>,
    ciphertext: Option<String>,
}

/// Record and return a refusal.
///
/// Every rejection path goes through here so that turning an envelope away is
/// never silent: an operator watching /api/dtn/rejected sees a peer that has
/// not been upgraded, a clock that has drifted, and a replay attempt as three
/// distinguishable things.
async fn refuse(
    state: &AppState,
    src: Option<Uuid>,
    msg_id: Option<Uuid>,
    code: StatusCode,
    reason: &str,
    detail: impl std::fmt::Display,
) -> (StatusCode, String) {
    let detail = detail.to_string();
    tracing::warn!(src = ?src, msg_id = ?msg_id, reason, detail = %detail, "DTN envelope refused");
    let _ = sqlx::query(
        "INSERT INTO dtn_rejected (src_node_id, msg_id, reason, detail) VALUES ($1, $2, $3, $4)",
    )
    .bind(src)
    .bind(msg_id)
    .bind(reason)
    .bind(&detail)
    .execute(&state.db)
    .await;
    (code, format!("{reason}: {detail}"))
}

async fn receive_message(
    State(state): State<AppState>,
    Caller(principal): Caller,
    Json(envelope): Json<ReceiveEnvelope>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let src_opt = envelope.src_node_id;
    let msg_opt = envelope.msg_id;

    // --- Who is allowed to hand us a bundle at all ---
    //
    // This endpoint used to sit behind the general guard, which accepts a user
    // token: any signed-in person could POST an envelope and have it stored
    // for consumers to read. It is a peer-to-peer endpoint, so it now requires
    // a *named* peer — the fabric's shared secret is not enough, because every
    // holder of it is indistinguishable and the next check needs to know whose
    // key to verify against.
    let transport_node = match principal.node_id() {
        Some(id) => id,
        None => {
            return Err(refuse(
                &state,
                src_opt,
                msg_opt,
                StatusCode::FORBIDDEN,
                "not_a_peer",
                format!("{} may not deliver DTN bundles", principal.describe()),
            )
            .await)
        }
    };

    // --- Shape ---
    if envelope.v != Some(ENVELOPE_VERSION) {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::BAD_REQUEST,
            "unsupported_envelope",
            format!(
                "need v{ENVELOPE_VERSION}, got {:?} — the sending outpost predates replay protection and needs upgrading",
                envelope.v
            ),
        )
        .await);
    }

    let (Some(src), Some(msg_id), Some(sig)) =
        (envelope.src_node_id, envelope.msg_id, envelope.signature.as_deref())
    else {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::BAD_REQUEST,
            "incomplete_envelope",
            "src_node_id, msg_id and signature are all required",
        )
        .await);
    };

    // The transport identity and the claimed author must agree. There is no
    // relaying in this implementation — the forwarder posts straight to the
    // destination — so a mismatch is a forgery attempt, not a hop. When
    // multi-hop relaying lands this check moves to the outer hop and the
    // envelope signature plus the dedupe below carry the inner guarantee.
    if transport_node != src {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::FORBIDDEN,
            "src_mismatch",
            format!("peer {transport_node} presented an envelope authored by {src}"),
        )
        .await);
    }

    // --- Addressed to us ---
    let dest = envelope.dest_node_id.clone().unwrap_or_default();
    if dest != state.identity.node_id {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::CONFLICT,
            "misdirected",
            format!("addressed to {dest:?}, this node is {}", state.identity.node_id),
        )
        .await);
    }

    // --- Clock ---
    let sent_at_s = envelope.sent_at.clone().unwrap_or_default();
    let expires_s = envelope.expires_at.clone().unwrap_or_default();

    let parse = |s: &str| DateTime::parse_from_rfc3339(s).map(|d| d.with_timezone(&Utc));
    let (Ok(sent_at), Ok(expires_at)) = (parse(&sent_at_s), parse(&expires_s)) else {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::BAD_REQUEST,
            "bad_timestamps",
            "sent_at and expires_at must both be RFC3339",
        )
        .await);
    };

    let now = Utc::now();

    if sent_at > now + Duration::seconds(MAX_FUTURE_SKEW_SECS) {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::BAD_REQUEST,
            "future_dated",
            format!("sent_at is {}s ahead of this clock", (sent_at - now).num_seconds()),
        )
        .await);
    }

    if expires_at <= sent_at {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::BAD_REQUEST,
            "expired_on_arrival",
            "expires_at is not after sent_at",
        )
        .await);
    }

    // A lifetime longer than the cap is refused rather than trimmed. Trimming
    // would mean forgetting the message id while the sender still considered
    // the bundle live, which is precisely the gap a replay needs.
    let cap = max_lifetime();
    if expires_at - sent_at > cap {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::BAD_REQUEST,
            "lifetime_too_long",
            format!(
                "{}s exceeds the {}s this node will hold a replay window open for",
                (expires_at - sent_at).num_seconds(),
                cap.num_seconds()
            ),
        )
        .await);
    }

    // Past its lifetime: the bundle is dead, and saying so with a 2xx is what
    // makes the sender stop. A non-2xx would have the forwarder retry a
    // message that can never be accepted until the retry schedule gave up.
    if now > expires_at {
        let _ = sqlx::query(
            "INSERT INTO dtn_rejected (src_node_id, msg_id, reason, detail) VALUES ($1,$2,'expired',$3)",
        )
        .bind(src)
        .bind(msg_id)
        .bind(format!("{}s past expiry", (now - expires_at).num_seconds()))
        .execute(&state.db)
        .await;
        return Ok(Json(json!({ "status": "expired", "msgId": msg_id })));
    }

    // --- Authenticity ---
    let known_key: Option<String> =
        sqlx::query_scalar("SELECT public_key FROM node_registry WHERE node_id = $1")
            .bind(src)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let Some(pk) = known_key else {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::UNAUTHORIZED,
            "unknown_sender",
            format!("no public key on file for {src}"),
        )
        .await);
    };

    let msg_id_s = msg_id.to_string();
    let src_s = src.to_string();
    let header = Header {
        msg_id: &msg_id_s,
        src: &src_s,
        dest: &dest,
        sent_at: &sent_at_s,
        expires_at: &expires_s,
    };

    let sealed = envelope.ciphertext.is_some();
    let signed_bytes = if sealed {
        sealed_signing_bytes(
            &header,
            envelope.kem_ciphertext.as_deref().unwrap_or(""),
            envelope.nonce.as_deref().unwrap_or(""),
            envelope.ciphertext.as_deref().unwrap_or(""),
        )
    } else {
        plain_signing_bytes(&header, envelope.payload.as_ref().unwrap_or(&Value::Null))
    };

    // Unverified envelopes are refused, not filed. Storing them was the real
    // hole: anything reading the inbox had to remember to filter on
    // `verified`, and a single consumer that forgot acted on attacker input.
    if !verify_signature(&pk, &signed_bytes, sig) {
        return Err(refuse(
            &state,
            src_opt,
            msg_opt,
            StatusCode::UNAUTHORIZED,
            "bad_signature",
            "signature does not cover this envelope",
        )
        .await);
    }

    // --- Replay ---
    //
    // Claiming the id and storing the payload are one transaction. Split, a
    // crash between them would leave the message remembered but never
    // delivered, and the sender's retransmit — the one mechanism that could
    // have recovered it — would be absorbed as a duplicate.
    let mut tx = state
        .db
        .begin()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let claimed = sqlx::query(
        "INSERT INTO dtn_seen (src_node_id, msg_id, expires_at) VALUES ($1, $2, $3) \
         ON CONFLICT (src_node_id, msg_id) DO NOTHING",
    )
    .bind(src)
    .bind(msg_id)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .rows_affected();

    if claimed == 0 {
        tx.rollback().await.ok();
        // 200, deliberately. A retransmit is correct behaviour by a sender
        // that never saw our acknowledgement, and the honest and hostile cases
        // are indistinguishable from here — so both get the same answer, and
        // neither produces a second inbox row.
        tracing::debug!(src = %src, msg_id = %msg_id, "DTN envelope already seen");
        return Ok(Json(json!({
            "status": "duplicate",
            "msgId": msg_id,
            "verified": true,
        })));
    }

    // --- Open ---
    // A decryption failure is recorded rather than fatal: the envelope was
    // authentic, so an operator needs to see that a peer is sealing to a stale
    // key. The payload slot carries the diagnosis instead of the message.
    let (stored_payload, decrypted) = if sealed {
        match dtn_crypto::open(
            &state.commsec.sk,
            envelope.kem_ciphertext.as_deref().unwrap_or(""),
            envelope.nonce.as_deref().unwrap_or(""),
            envelope.ciphertext.as_deref().unwrap_or(""),
            &src_s,
            &dest,
        ) {
            Ok(plaintext) => (plaintext, true),
            Err(e) => {
                tracing::error!(src = %src, error = %e, scheme = ?envelope.scheme, "could not open sealed DTN payload");
                (
                    json!({ "error": "undecryptable", "detail": e.to_string(), "scheme": envelope.scheme }),
                    false,
                )
            }
        }
    } else {
        (envelope.payload.clone().unwrap_or(Value::Null), false)
    };

    sqlx::query(
        "INSERT INTO dtn_inbox (src_node_id, payload, signature, verified, encrypted, msg_id, sent_at, expires_at) \
         VALUES ($1, $2, $3, true, $4, $5, $6, $7)",
    )
    .bind(src)
    .bind(&stored_payload)
    .bind(&envelope.signature)
    .bind(sealed)
    .bind(msg_id)
    .bind(sent_at)
    .bind(expires_at)
    .execute(&mut *tx)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    tx.commit()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(json!({
        "status": "received",
        "msgId": msg_id,
        "verified": true,
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
        SELECT id, dest_node_id, endpoint, payload, attempts, next_try_at, created_at,
               msg_id, expires_at
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
                "msgId": r.get::<Option<Uuid>, _>("msg_id"),
                "destNodeId": r.get::<Uuid, _>("dest_node_id"),
                "endpoint": r.get::<String, _>("endpoint"),
                "payload": r.get::<Value, _>("payload"),
                "attempts": r.get::<i32, _>("attempts"),
                "nextTryAt": r.get::<DateTime<Utc>, _>("next_try_at").to_rfc3339(),
                "expiresAt": r.get::<Option<DateTime<Utc>>, _>("expires_at").map(|d| d.to_rfc3339()),
                "createdAt": r.get::<DateTime<Utc>, _>("created_at").to_rfc3339(),
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
        SELECT id, src_node_id, payload, verified, received_at, msg_id, sent_at
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
                "msgId": r.get::<Option<Uuid>, _>("msg_id"),
                "srcNodeId": r.get::<Option<Uuid>, _>("src_node_id"),
                "payload": r.get::<Value, _>("payload"),
                "verified": r.get::<bool, _>("verified"),
                "sentAt": r.get::<Option<DateTime<Utc>>, _>("sent_at").map(|d| d.to_rfc3339()),
                "receivedAt": r.get::<DateTime<Utc>, _>("received_at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!(msgs)))
}

/// What was turned away, and why.
///
/// Worth a route of its own: a refusal is invisible to both sides otherwise —
/// the sender's forwarder just reports a failed attempt — and the difference
/// between "a peer needs upgrading" and "something is replaying our traffic"
/// is only legible here.
async fn list_rejected(
    State(state): State<AppState>,
    Query(f): Query<ListFilter>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = sqlx::query(
        "SELECT id, src_node_id, msg_id, reason, detail, at FROM dtn_rejected ORDER BY id DESC LIMIT $1",
    )
    .bind(f.limit.unwrap_or(100).clamp(1, 1000))
    .fetch_all(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let out: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "srcNodeId": r.get::<Option<Uuid>, _>("src_node_id"),
                "msgId": r.get::<Option<Uuid>, _>("msg_id"),
                "reason": r.get::<String, _>("reason"),
                "detail": r.get::<Option<String>, _>("detail"),
                "at": r.get::<DateTime<Utc>, _>("at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!(out)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header<'static> {
        Header {
            msg_id: "11111111-1111-1111-1111-111111111111",
            src: "22222222-2222-2222-2222-222222222222",
            dest: "33333333-3333-3333-3333-333333333333",
            sent_at: "2026-10-02T12:00:00+00:00",
            expires_at: "2026-10-03T12:00:00+00:00",
        }
    }

    /// The two envelope shapes must never produce the same signed bytes.
    ///
    /// If they could, a signature taken from a sealed envelope would
    /// authenticate a plaintext one whose body the attacker chose.
    #[test]
    fn the_two_envelope_shapes_are_domain_separated() {
        let h = header();
        let plain = plain_signing_bytes(&h, &json!("x"));
        let sealed = sealed_signing_bytes(&h, "x", "", "");
        assert_ne!(plain, sealed);
    }

    /// Changing any header field changes what was signed.
    ///
    /// This is what stops a captured bundle being re-aimed at another outpost
    /// or given a fresh lifetime: each of these was outside the signature
    /// before, so each of these was free to edit in flight.
    #[test]
    fn every_header_field_is_covered_by_the_signature() {
        let base = plain_signing_bytes(&header(), &json!({"a": 1}));

        let mut other = header();
        other.dest = "44444444-4444-4444-4444-444444444444";
        assert_ne!(base, plain_signing_bytes(&other, &json!({"a": 1})), "dest");

        let mut other = header();
        other.msg_id = "55555555-5555-5555-5555-555555555555";
        assert_ne!(base, plain_signing_bytes(&other, &json!({"a": 1})), "msg_id");

        let mut other = header();
        other.expires_at = "2030-01-01T00:00:00+00:00";
        assert_ne!(base, plain_signing_bytes(&other, &json!({"a": 1})), "expires_at");

        let mut other = header();
        other.sent_at = "2026-10-02T12:00:01+00:00";
        assert_ne!(base, plain_signing_bytes(&other, &json!({"a": 1})), "sent_at");

        assert_ne!(base, plain_signing_bytes(&header(), &json!({"a": 2})), "payload");
    }

    /// Length prefixing means no two different field splits collide.
    ///
    /// With a `|` separator and variable-length fields, a sender could move
    /// the boundary — `dest="a", sent="b|c"` and `dest="a|b", sent="c"` signed
    /// identical bytes, so one signature covered two different routings.
    #[test]
    fn field_boundaries_cannot_be_moved() {
        let a = canonical("d", &[b"a", b"b|c"]);
        let b = canonical("d", &[b"a|b", b"c"]);
        assert_ne!(a, b);
    }

    /// An envelope is only remembered for as long as it can arrive, and the
    /// cap is what bounds that. A sender asking for longer is refused in
    /// `receive_message`; here we only assert the cap is positive and finite,
    /// since a zero or negative value would disable the dedupe table entirely.
    #[test]
    fn the_lifetime_cap_is_sane() {
        assert!(max_lifetime() > Duration::zero());
        assert!(default_lifetime() <= max_lifetime());
    }
}
