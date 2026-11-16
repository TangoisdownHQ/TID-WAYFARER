//! Multi-hop DTN relaying, with `DTN_RELAY` on.
//!
//! A separate test binary from `dtn_replay` on purpose: `DTN_RELAY` is read
//! from the process environment, and the relay-off behaviour (a misdirected
//! bundle is refused) is asserted over there. Keeping the two in one binary
//! would have them race on the same env var. Here relaying is on for every
//! case, so there is no contention.
//!
//! The security guarantees are unchanged from the single-hop path: a relay
//! only carries a bundle whose author it can authenticate and whose
//! destination it can reach, and the replay dedupe means it carries each bundle
//! at most once.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use tid_wayfarer::routes::dtn::{plain_signing_bytes, Header, ENVELOPE_VERSION};
use tid_wayfarer::services::fabric_auth;

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

struct Peer {
    node_id: Uuid,
    key: SigningKey,
}

impl Peer {
    async fn register(h: &common::Harness) -> Self {
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

    /// Deliver an envelope with this peer as the fabric transport identity.
    async fn deliver(&self, h: &common::Harness, envelope: &Value) -> (StatusCode, Value) {
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
        let jsonv = serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).to_string()));
        (status, jsonv)
    }
}

/// A validly signed v2 plaintext envelope authored by `peer`, addressed to `dest`.
fn envelope(peer: &Peer, dest: &str, payload: Value) -> Value {
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(3600);
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

async fn outbox_rows(h: &common::Harness, msg_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM dtn_outbox WHERE msg_id = $1::uuid")
        .bind(msg_id)
        .fetch_one(&h.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_relay_carries_a_bundle_toward_a_destination_it_can_reach() {
    std::env::set_var("DTN_RELAY", "1");
    let h = require_db!();
    let author = Peer::register(&h).await;
    let courier = Peer::register(&h).await;
    let third = Peer::register(&h).await; // the destination, reachable from us

    // Addressed to `third`, authored by `author`, carried by `courier`.
    let env = envelope(&author, &third.node_id.to_string(), json!({"n": 1}));
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (status, body) = courier.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["status"], "relayed");
    assert_eq!(body["to"], third.node_id.to_string());

    // Enqueued for forwarding toward third's registered endpoint.
    assert_eq!(outbox_rows(&h, &msg_id).await, 1, "a relayed bundle is queued once");
    let (dest, endpoint): (Uuid, String) = sqlx::query_as(
        "SELECT dest_node_id, endpoint FROM dtn_outbox WHERE msg_id = $1::uuid",
    )
    .bind(&msg_id)
    .fetch_one(&h.db)
    .await
    .unwrap();
    assert_eq!(dest, third.node_id);
    assert!(endpoint.contains(&third.node_id.to_string()) && endpoint.ends_with("/dtn/receive"));
}

#[tokio::test]
async fn a_bundle_for_an_unknown_destination_is_unroutable() {
    std::env::set_var("DTN_RELAY", "1");
    let h = require_db!();
    let author = Peer::register(&h).await;
    let courier = Peer::register(&h).await;

    // Addressed to a node this relay has never heard of: no route.
    let env = envelope(&author, &Uuid::new_v4().to_string(), json!({"n": 2}));
    let (status, body) = courier.deliver(&h, &env).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert!(body.as_str().unwrap_or_default().contains("unroutable"), "{body:?}");
}

#[tokio::test]
async fn a_relay_carries_each_bundle_at_most_once() {
    std::env::set_var("DTN_RELAY", "1");
    let h = require_db!();
    let author = Peer::register(&h).await;
    let courier = Peer::register(&h).await;
    let third = Peer::register(&h).await;

    let env = envelope(&author, &third.node_id.to_string(), json!({"n": 3}));
    let msg_id = env["msg_id"].as_str().unwrap().to_string();

    let (s1, _) = courier.deliver(&h, &env).await;
    assert_eq!(s1, StatusCode::OK);
    // A second arrival of the same bundle (a loop, or the sender's retransmit)
    // is absorbed by the replay dedupe, not forwarded again.
    let (s2, b2) = courier.deliver(&h, &env).await;
    assert_eq!(s2, StatusCode::OK, "{b2:?}");
    assert_eq!(b2["status"], "duplicate");
    assert_eq!(outbox_rows(&h, &msg_id).await, 1, "a loop must not multiply the bundle");
}
