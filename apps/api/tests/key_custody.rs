//! Key succession, and the rollover window that makes it survivable.
//!
//! Rotation already existed but needed an admin JWT *on the peer*, so on a
//! federated fabric an outpost could only rotate its own key with
//! administrative access to other companies' outposts — which it does not have.
//! The practical consequence was that nobody rotated.
//!
//! Succession fixes it by letting the outgoing key name its successor. Nothing
//! new is trusted: that key was already authoritative for the node. These tests
//! assert the parts that would be quietly wrong otherwise — that a *different*
//! key cannot nominate a successor, that a revoked node cannot reinstate
//! itself, and that the grace window which makes a multi-peer rotation possible
//! does not also keep a stolen key alive.

mod common;

use axum::http::StatusCode;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;
use uuid::Uuid;

use tid_wayfarer::routes::nodes::succession_message;
use tid_wayfarer::services::fabric_auth;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// A keypair derived from a uuid, so a failure is reproducible from the output.
fn keypair(seed: Uuid) -> SigningKey {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(seed.as_bytes());
    bytes[16..].copy_from_slice(seed.as_bytes());
    SigningKey::from_bytes(&bytes)
}

fn public(k: &SigningKey) -> String {
    B64.encode(k.verifying_key().to_bytes())
}

fn sign(k: &SigningKey, msg: &[u8]) -> String {
    B64.encode(k.sign(msg).to_bytes())
}

/// Register a peer with a known key.
async fn register(h: &common::Harness, key: &SigningKey) -> Uuid {
    let node_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO node_registry (node_id, name, api_endpoint, public_key, status) \
         VALUES ($1, $2, $3, $4, 'online')",
    )
    .bind(node_id)
    .bind(format!("peer-{}", &node_id.to_string()[..8]))
    .bind(format!("http://{node_id}.peer.invalid/api"))
    .bind(public(key))
    .execute(&h.db)
    .await
    .expect("could not register peer");
    node_id
}

/// Build a complete succession request, then let the caller break one thing.
fn succession(
    node_id: Uuid,
    old: &SigningKey,
    new: &SigningKey,
    reason: Option<&str>,
) -> serde_json::Value {
    let ts = chrono::Utc::now().timestamp();
    let msg = succession_message(&node_id.to_string(), &public(old), &public(new), ts);
    json!({
        "new_public_key": public(new),
        "ts": ts,
        "predecessor_signature": sign(old, &msg),
        "successor_signature": sign(new, &msg),
        "reason": reason,
    })
}

async fn current_key(h: &common::Harness, node_id: Uuid) -> String {
    sqlx::query_scalar("SELECT public_key FROM node_registry WHERE node_id = $1")
        .bind(node_id)
        .fetch_one(&h.db)
        .await
        .unwrap()
}

/// The outgoing key can hand its authority to a successor, with no
/// administrator anywhere.
///
/// This is the whole point. An admin-gated rotation is unusable on a fabric of
/// independent companies, so in practice keys were never rotated at all.
#[tokio::test]
async fn an_outgoing_key_can_nominate_its_successor() {
    let h = require_db!();
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    let (status, body) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None, // deliberately no token of any kind
            Some(succession(node, &old, &new, Some("annual rotation"))),
        )
        .await;

    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["status"], "succeeded");
    assert_eq!(current_key(&h, node).await, public(&new), "the registry still holds the old key");

    // The caller is told how long the old key keeps working, because their
    // rollout order depends on it.
    assert!(body["predecessorAcceptedForSecs"].as_i64().unwrap() > 0, "{body:?}");
}

/// Some other key cannot nominate a successor.
///
/// Without this the endpoint would be a way for anyone to take over any node,
/// which is precisely what the admin gate was protecting against.
#[tokio::test]
async fn an_unrelated_key_cannot_nominate_a_successor() {
    let h = require_db!();
    let real = keypair(Uuid::new_v4());
    let attacker = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &real).await;

    // The attacker signs a well-formed statement — with the wrong key.
    let ts = chrono::Utc::now().timestamp();
    let msg = succession_message(&node.to_string(), &public(&attacker), &public(&new), ts);
    let (status, body) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(json!({
                "new_public_key": public(&new),
                "ts": ts,
                "predecessor_signature": sign(&attacker, &msg),
                "successor_signature": sign(&new, &msg),
            })),
        )
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:?}");
    assert_eq!(current_key(&h, node).await, public(&real), "the key was changed by an outsider");
}

/// The successor must prove it exists.
///
/// Without the second signature, a typo in `new_public_key` would hand the
/// node's identity to a key nobody holds — and it could never sign its way
/// back, which is unrecoverable rather than merely inconvenient.
#[tokio::test]
async fn a_successor_that_cannot_sign_is_refused() {
    let h = require_db!();
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let typo = keypair(Uuid::new_v4()); // what the operator meant to type
    let node = register(&h, &old).await;

    let ts = chrono::Utc::now().timestamp();
    // The statement names `new`, and only `typo` signed as successor.
    let msg = succession_message(&node.to_string(), &public(&old), &public(&new), ts);
    let (status, body) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(json!({
                "new_public_key": public(&new),
                "ts": ts,
                "predecessor_signature": sign(&old, &msg),
                "successor_signature": sign(&typo, &msg),
            })),
        )
        .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:?}");
    assert!(
        body.as_str().unwrap_or_default().contains("successor_signature"),
        "the error should name which signature failed: {body:?}"
    );
    assert_eq!(current_key(&h, node).await, public(&old));
}

/// A revoked node cannot reinstate itself.
///
/// Revocation is an operator saying "this node is not to be trusted". If its
/// key could nominate a successor, it could undo that itself and revocation
/// would be decorative.
#[tokio::test]
async fn a_revoked_node_cannot_succeed_itself() {
    let h = require_db!();
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    sqlx::query("UPDATE node_registry SET revoked = true, revoked_at = NOW() WHERE node_id = $1")
        .bind(node)
        .execute(&h.db)
        .await
        .unwrap();

    let (status, body) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(succession(node, &old, &new, None)),
        )
        .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
    assert!(
        body.as_str().unwrap_or_default().contains("administrator"),
        "the refusal should point at the only legitimate route: {body:?}"
    );
}

/// A stale statement is refused.
#[tokio::test]
async fn a_stale_succession_statement_is_refused() {
    let h = require_db!();
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    let ts = chrono::Utc::now().timestamp() - 7200;
    let msg = succession_message(&node.to_string(), &public(&old), &public(&new), ts);
    let (status, _) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(json!({
                "new_public_key": public(&new),
                "ts": ts,
                "predecessor_signature": sign(&old, &msg),
                "successor_signature": sign(&new, &msg),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Post a fabric-signed request with a given key, to see whether it is accepted.
async fn signed_call(h: &common::Harness, node: Uuid, key: &SigningKey) -> StatusCode {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let path = "/api/nodes/list";
    let ts = chrono::Utc::now().timestamp();
    let canonical = fabric_auth::canonical_request("GET", path, ts, b"");

    let req = Request::builder()
        .method("GET")
        .uri(path)
        .header(fabric_auth::HEADER_NODE_ID, node.to_string())
        .header(fabric_auth::HEADER_TIMESTAMP, ts.to_string())
        .header(fabric_auth::HEADER_SIGNATURE, sign(key, canonical.as_bytes()))
        .body(Body::empty())
        .unwrap();

    h.app.clone().oneshot(req).await.unwrap().status()
}

/// The superseded key keeps working for the rollover window.
///
/// This is what makes a multi-peer rotation possible at all. A rotation cannot
/// be atomic across a fabric, and the only workable order is: tell every peer
/// first, install the new key last. Between those, a peer that has already
/// accepted the succession still sees requests signed by the old key — so it
/// accepts the predecessor, briefly.
#[tokio::test]
async fn a_superseded_key_still_works_during_the_rollover() {
    let h = require_db!();
    std::env::set_var("KEY_ROTATION_GRACE_SECS", "3600");
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    // Both keys work before: the old is current, the new is nothing yet.
    assert_eq!(signed_call(&h, node, &old).await, StatusCode::OK);
    assert_eq!(signed_call(&h, node, &new).await, StatusCode::UNAUTHORIZED);

    let (status, _) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(succession(node, &old, &new, Some("planned rotation"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // After: the new key is current, and the old one is still honoured — which
    // is the outpost's grace period to finish installing it.
    assert_eq!(signed_call(&h, node, &new).await, StatusCode::OK, "the new key should work");
    assert_eq!(
        signed_call(&h, node, &old).await,
        StatusCode::OK,
        "the superseded key should still be accepted inside the window"
    );
}

/// A key rotated because it was compromised stops working immediately.
///
/// The grace window is for an orderly rollover. On a stolen key it would be a
/// grace window for the thief, so the reason for the rotation decides — and it
/// is recorded per rotation rather than configured globally.
#[tokio::test]
async fn a_compromised_key_gets_no_rollover_window() {
    let h = require_db!();
    std::env::set_var("KEY_ROTATION_GRACE_SECS", "3600");
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    assert_eq!(signed_call(&h, node, &old).await, StatusCode::OK);

    let (status, body) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(succession(node, &old, &new, Some("laptop stolen, key compromised"))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["predecessorAcceptedForSecs"], 0, "{body:?}");

    assert_eq!(signed_call(&h, node, &new).await, StatusCode::OK);
    assert_eq!(
        signed_call(&h, node, &old).await,
        StatusCode::UNAUTHORIZED,
        "a compromised key must stop working at once"
    );
}

/// The rollover window closes.
///
/// A superseded key accepted forever would mean rotation never actually
/// retires anything.
#[tokio::test]
async fn the_rollover_window_expires() {
    let h = require_db!();
    std::env::set_var("KEY_ROTATION_GRACE_SECS", "3600");
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    h.call(
        "POST",
        &format!("/api/node-succession/{node}"),
        None,
        Some(succession(node, &old, &new, Some("planned"))),
    )
    .await;

    assert_eq!(signed_call(&h, node, &old).await, StatusCode::OK, "still inside the window");

    // Age the rotation past the window.
    sqlx::query(
        "UPDATE node_key_history SET rotated_at = NOW() - INTERVAL '2 hours' WHERE node_id = $1",
    )
    .bind(node)
    .execute(&h.db)
    .await
    .unwrap();

    assert_eq!(
        signed_call(&h, node, &old).await,
        StatusCode::UNAUTHORIZED,
        "the superseded key should stop working once the window passes"
    );
    assert_eq!(signed_call(&h, node, &new).await, StatusCode::OK);
}

/// Succession is recorded as such, so an auditor can tell a routine rollover
/// from an administrator overriding one.
#[tokio::test]
async fn a_succession_is_recorded_with_its_method_and_reason() {
    let h = require_db!();
    let old = keypair(Uuid::new_v4());
    let new = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    h.call(
        "POST",
        &format!("/api/node-succession/{node}"),
        None,
        Some(succession(node, &old, &new, Some("quarterly rotation"))),
    )
    .await;

    use sqlx::Row;
    let row = sqlx::query(
        "SELECT method, reason, rotated_by, grace, old_public_key, new_public_key \
         FROM node_key_history WHERE node_id = $1 ORDER BY rotated_at DESC LIMIT 1",
    )
    .bind(node)
    .fetch_one(&h.db)
    .await
    .unwrap();

    assert_eq!(row.get::<String, _>("method"), "succession");
    assert_eq!(row.get::<Option<String>, _>("reason").unwrap(), "quarterly rotation");
    assert_eq!(row.get::<Option<String>, _>("rotated_by").unwrap(), "self (succession)");
    assert!(row.get::<bool, _>("grace"));
    // Both ends of the transition, so the chain of keys is reconstructable.
    assert_eq!(row.get::<Option<String>, _>("old_public_key").unwrap(), public(&old));
    assert_eq!(row.get::<Option<String>, _>("new_public_key").unwrap(), public(&new));
}

/// Nominating the key already in use is refused.
#[tokio::test]
async fn succeeding_to_the_same_key_is_refused() {
    let h = require_db!();
    let old = keypair(Uuid::new_v4());
    let node = register(&h, &old).await;

    let (status, body) = h
        .call(
            "POST",
            &format!("/api/node-succession/{node}"),
            None,
            Some(succession(node, &old, &old, None)),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
}
