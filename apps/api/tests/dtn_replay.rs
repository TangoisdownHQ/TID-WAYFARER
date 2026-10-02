//! DTN replay protection, asserted by actually replaying things.
//!
//! The previous envelope signed the sender and the body and nothing else, so
//! these tests all used to pass in the wrong direction: the same bundle
//! verified every time it was posted, at any outpost, forever. Each case below
//! is one of the properties that absence cost.
//!
//! They forge envelopes deliberately — signing with a peer key the harness
//! controls and posting with the fabric transport headers — because a replay
//! is indistinguishable from a legitimate retransmit at the HTTP layer, and
//! only a real signed request proves the server tells them apart.

mod common;

use axum::http::StatusCode;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use uuid::Uuid;

use tid_wayfarer::routes::dtn::{plain_signing_bytes, Header, ENVELOPE_VERSION};
use tid_wayfarer::services::fabric_auth;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// A peer this outpost knows: a keypair, a registry row, and the ability to
/// sign both the envelope and the transport.
struct Peer {
    node_id: Uuid,
    key: SigningKey,
}

impl Peer {
    async fn register(h: &common::Harness) -> Self {
        // Deterministic from a fresh uuid rather than from an RNG, so a
        // failure can be reproduced from the test output alone.
        let node_id = Uuid::new_v4();
        let key = SigningKey::from_bytes(&{
            let mut seed = [0u8; 32];
            seed[..16].copy_from_slice(node_id.as_bytes());
            seed[16..].copy_from_slice(node_id.as_bytes());
            seed
        });

        sqlx::query(
            "INSERT INTO node_registry (node_id, name, api_endpoint, public_key, status) \
             VALUES ($1, $2, $3, $4, 'online')",
        )
        .bind(node_id)
        .bind(format!("peer-{}", &node_id.to_string()[..8]))
        // api_endpoint is unique, so each peer needs its own — several of
        // these tests register two, and the tests share one database.
        .bind(format!("http://{node_id}.peer.invalid/api"))
        .bind(B64.encode(key.verifying_key().to_bytes()))
        .execute(&h.db)
        .await
        .expect("could not register peer");

        Self { node_id, key }
    }

    fn sign(&self, bytes: &[u8]) -> String {
        B64.encode(self.key.sign(bytes).to_bytes())
    }

    /// Post an envelope the way the forwarder does: fabric-signed transport
    /// carrying the envelope as the body.
    async fn deliver(&self, h: &common::Harness, envelope: &Value) -> (StatusCode, Value) {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let body = serde_json::to_vec(envelope).unwrap();
        let ts = chrono::Utc::now().timestamp();
        let canonical = fabric_auth::canonical_request("POST", "/api/dtn/receive", ts, &body);

        let req = Request::builder()
            .method("POST")
            .uri("/api/dtn/receive")
            .header("content-type", "application/json")
            .header(fabric_auth::HEADER_NODE_ID, self.node_id.to_string())
            .header(fabric_auth::HEADER_TIMESTAMP, ts.to_string())
            .header(fabric_auth::HEADER_SIGNATURE, self.sign(canonical.as_bytes()))
            .body(Body::from(body))
            .unwrap();

        let res = h.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()));
        (status, json)
    }
}

/// Build a complete, correctly signed v2 envelope, then let the caller break
/// exactly one thing about it.
fn envelope(peer: &Peer, dest: &str, payload: Value, sent_offset: i64, ttl: i64) -> Value {
    let now = chrono::Utc::now() + chrono::Duration::seconds(sent_offset);
    let expires = now + chrono::Duration::seconds(ttl);

    let msg_id = Uuid::new_v4().to_string();
    let src = peer.node_id.to_string();
    let sent_at = now.to_rfc3339();
    let expires_at = expires.to_rfc3339();

    let header = Header {
        msg_id: &msg_id,
        src: &src,
        dest,
        sent_at: &sent_at,
        expires_at: &expires_at,
    };
    let signature = peer.sign(&plain_signing_bytes(&header, &payload));

    json!({
        "v": ENVELOPE_VERSION,
        "msg_id": msg_id,
        "src_node_id": src,
        "dest_node_id": dest,
        "sent_at": sent_at,
        "expires_at": expires_at,
        "payload": payload,
        "signature": signature,
    })
}

async fn inbox_rows(h: &common::Harness, msg_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM dtn_inbox WHERE msg_id = $1::uuid")
        .bind(msg_id)
        .fetch_one(&h.db)
        .await
        .unwrap()
}

/// The core property: posting the same envelope twice stores one message.
///
/// This is not only an attacker's path. The forwarder retries on a dropped
/// connection, so a bundle that arrived but whose acknowledgement was lost is
/// sent again as a matter of course — and before this, each retry became
/// another inbox row. For a chat message that is a duplicate; for a command it
/// is doing the thing twice.
#[tokio::test]
async fn the_same_envelope_delivered_twice_is_stored_once() {
    let h = require_db!();
    let peer = Peer::register(&h).await;
    let dest = h.node_id.clone();

    let env = envelope(&peer, &dest, json!({"kind": "test", "n": 1}), 0, 3600);
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (s1, b1) = peer.deliver(&h, &env).await;
    assert_eq!(s1, StatusCode::OK, "first delivery: {b1:?}");
    assert_eq!(b1["status"], "received");

    let (s2, b2) = peer.deliver(&h, &env).await;
    assert_eq!(s2, StatusCode::OK, "a retransmit must be acknowledged, not refused: {b2:?}");
    assert_eq!(b2["status"], "duplicate");

    assert_eq!(inbox_rows(&h, &msg_id).await, 1, "replay produced a second inbox row");
}

/// A retransmit is answered 2xx on purpose.
///
/// A 409 would read as correct and be wrong: the forwarder retries on any
/// non-2xx, so refusing a duplicate would keep the bundle in the sender's
/// outbox and have it redelivered on the backoff schedule until it expired —
/// turning the defence into a traffic generator.
#[tokio::test]
async fn a_duplicate_is_acknowledged_so_the_sender_stops() {
    let h = require_db!();
    let peer = Peer::register(&h).await;
    let dest = h.node_id.clone();
    let env = envelope(&peer, &dest, json!({"n": 2}), 0, 3600);

    peer.deliver(&h, &env).await;
    let (status, _) = peer.deliver(&h, &env).await;
    assert!(status.is_success(), "duplicate answered {status}, which the forwarder reads as retry");
}

/// A captured envelope cannot be re-aimed at a different outpost.
///
/// `dest` was outside the signature, so the same bundle verified anywhere. On
/// a fabric where one outpost may act on a message another was meant to
/// receive, that is a redirect, not a formality.
#[tokio::test]
async fn an_envelope_addressed_elsewhere_is_refused() {
    let h = require_db!();
    let peer = Peer::register(&h).await;

    let elsewhere = Uuid::new_v4().to_string();
    let env = envelope(&peer, &elsewhere, json!({"n": 3}), 0, 3600);
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (status, body) = peer.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert_eq!(inbox_rows(&h, &msg_id).await, 0);
}

/// Editing the body invalidates the signature.
///
/// Held before too — this one asserts the rewrite did not lose it while adding
/// the header fields.
#[tokio::test]
async fn a_tampered_payload_is_refused() {
    let h = require_db!();
    let peer = Peer::register(&h).await;
    let dest = h.node_id.clone();

    let mut env = envelope(&peer, &dest, json!({"qty": 1}), 0, 3600);
    env["payload"] = json!({"qty": 1000});
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (status, body) = peer.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:?}");
    assert_eq!(inbox_rows(&h, &msg_id).await, 0);
}

/// Giving a captured bundle a fresh lifetime invalidates it.
///
/// Without `expires_at` under the signature, an attacker holding an expired
/// envelope could extend it indefinitely and the replay window would never
/// close.
#[tokio::test]
async fn extending_the_lifetime_of_a_captured_bundle_is_refused() {
    let h = require_db!();
    let peer = Peer::register(&h).await;
    let dest = h.node_id.clone();

    let mut env = envelope(&peer, &dest, json!({"n": 4}), 0, 60);
    env["expires_at"] = json!((chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339());

    let (status, body) = peer.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:?}");
}

/// A dead bundle is absorbed, not refused.
///
/// Same reasoning as the duplicate case: it can never be accepted, so a
/// non-2xx would have the sender retry a hopeless delivery for the rest of the
/// backoff schedule.
#[tokio::test]
async fn an_expired_bundle_is_absorbed_rather_than_retried() {
    let h = require_db!();
    let peer = Peer::register(&h).await;
    let dest = h.node_id.clone();

    // Written two hours ago with a one-hour lifetime: validly signed, dead.
    let env = envelope(&peer, &dest, json!({"n": 5}), -7200, 3600);
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (status, body) = peer.deliver(&h, &env).await;
    assert!(status.is_success(), "expired answered {status}: {body:?}");
    assert_eq!(body["status"], "expired");
    assert_eq!(inbox_rows(&h, &msg_id).await, 0, "a dead bundle must not be filed");
}

/// A sender cannot hold the replay window open indefinitely.
///
/// The message id has to be remembered for the whole lifetime, so an uncapped
/// lifetime is an uncapped table. Refused rather than trimmed: trimming would
/// mean forgetting the id while the sender still believed the bundle live,
/// which is the gap a replay needs.
#[tokio::test]
async fn an_absurd_lifetime_is_refused() {
    let h = require_db!();
    let peer = Peer::register(&h).await;
    let dest = h.node_id.clone();

    let env = envelope(&peer, &dest, json!({"n": 6}), 0, 400 * 24 * 3600);
    let (status, body) = peer.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert!(
        body.as_str().unwrap_or_default().contains("lifetime_too_long"),
        "reason should name the cap: {body:?}"
    );
}

/// The old envelope shape is refused rather than accepted unverified.
///
/// Accepting v1 alongside v2 would make every guarantee above optional for
/// anyone claiming to be an older build — including an attacker.
#[tokio::test]
async fn a_v1_envelope_is_refused() {
    let h = require_db!();
    let peer = Peer::register(&h).await;

    // Exactly what the previous implementation sent.
    let payload = json!({"n": 7});
    let legacy = format!("dtn|{}|{}", peer.node_id, payload);
    let env = json!({
        "src_node_id": peer.node_id.to_string(),
        "payload": payload,
        "signature": peer.sign(legacy.as_bytes()),
    });

    let (status, body) = peer.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
}

/// A signed-in person is not a peer.
///
/// `/dtn/receive` sat behind the general guard, which accepts a user token —
/// so any authenticated account could post envelopes and have them filed for
/// consumers to read. Unverified rows were stored with `verified=false`, and
/// filtering on that column was a convention every consumer had to remember.
#[tokio::test]
async fn a_user_token_cannot_deliver_bundles() {
    let h = require_db!();
    let (_id, token) = h.user("admin").await;

    let (status, body) = h
        .post(
            "/api/dtn/receive",
            &token,
            json!({"v": 2, "payload": {"n": 8}, "src_node_id": Uuid::new_v4()}),
        )
        .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
}

/// A registered peer cannot deliver an envelope authored by another peer.
///
/// There is no relaying in this implementation — the forwarder posts straight
/// to the destination — so the transport identity and the claimed author must
/// agree. Otherwise one compromised outpost could replay another's traffic
/// under its own transport credentials.
#[tokio::test]
async fn a_peer_cannot_present_another_peers_envelope() {
    let h = require_db!();
    let author = Peer::register(&h).await;
    let courier = Peer::register(&h).await;
    let dest = h.node_id.clone();

    let env = envelope(&author, &dest, json!({"n": 9}), 0, 3600);
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (status, body) = courier.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
    assert_eq!(inbox_rows(&h, &msg_id).await, 0);
}

/// Every refusal is visible to an operator.
///
/// A rejected envelope is no longer stored in the inbox, which is correct but
/// would otherwise make the whole class invisible — the sender's forwarder
/// reports only a failed attempt. The difference between "a peer needs
/// upgrading" and "something is replaying our traffic" is only legible here.
#[tokio::test]
async fn refusals_are_recorded_with_a_reason() {
    let h = require_db!();
    let peer = Peer::register(&h).await;

    let mut env = envelope(&peer, &h.node_id, json!({"n": 10}), 0, 3600);
    env["payload"] = json!({"n": "tampered"});
    peer.deliver(&h, &env).await;

    let reason: Option<String> = sqlx::query_scalar(
        "SELECT reason FROM dtn_rejected WHERE src_node_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(peer.node_id)
    .fetch_optional(&h.db)
    .await
    .unwrap();

    assert_eq!(reason.as_deref(), Some("bad_signature"));
}
