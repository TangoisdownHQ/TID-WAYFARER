//! The properties that decide whether this system is safe to run.
//!
//! Each of these was previously only asserted by reading the code. They are
//! here because a unit test cannot reach them: they are facts about the
//! assembled router, the guard and the schema together.

mod common;

use axum::http::StatusCode;
use serde_json::json;

// ---------------------------------------------------------------------------
// Authentication and authority
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unauthenticated_caller_reaches_nothing() {
    let h = require_db!();
    for path in [
        "/api/inventory",
        "/api/orders",
        "/api/rollup/inventory",
        "/api/movements/forecast",
        "/api/compliance/holds",
        "/api/orgs",
    ] {
        let (status, _) = h.call("GET", path, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} was reachable without a token");
    }
}

#[tokio::test]
async fn a_plain_user_cannot_actuate_an_outpost() {
    // The sharpest hole found in review: a role-user JWT could LOCKDOWN an
    // outpost, and worse UNLOCK one that autonomy had locked after physical
    // tamper — undoing the response with a credential the tampering party
    // might well hold.
    let h = require_db!();
    let (_, user) = h.user("user").await;

    let (status, _) = h.post("/api/commands/execute", &user, json!({"type":"LOCKDOWN"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = h.post("/api/commands/execute", &user, json!({"type":"UNLOCK"})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_dead_command_pull_flow_is_gone() {
    // It wrote rows nothing executed and let any caller drain another node's
    // queue. Deleting it closed three confused-deputy holes; this keeps it
    // deleted.
    let h = require_db!();
    let (_, user) = h.user("admin").await;
    for path in ["/api/commands/enqueue", "/api/commands/pull", "/api/commands/ack"] {
        let (status, _) = h.post(path, &user, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} is still mounted");
    }
}

#[tokio::test]
async fn telemetry_cannot_be_attributed_to_an_arbitrary_node() {
    // Telemetry drives autonomy, so forging it is a route to forcing another
    // outpost into lockdown. A user may submit on a node's behalf only when
    // that node's HMAC holds; naming a node with no secret must not work.
    let h = require_db!();
    let (_, user) = h.user("user").await;

    let (status, _) = h
        .post(
            "/api/fleet/telemetry",
            &user,
            json!({
                "asset_id": uuid::Uuid::new_v4(),
                "node_id": "00000000-0000-0000-0000-0000000000ff",
                "tamper": true
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ---------------------------------------------------------------------------
// Organisation boundaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_org_cannot_read_anothers_inventory() {
    // The property the whole federation model exists to provide. What a
    // counterparty holds, and how fast they burn it, is the most commercially
    // sensitive data in the system.
    let h = require_db!();

    let (alice, alice_token) = h.user("user").await;
    let (bob, bob_token) = h.user("user").await;
    let org_a = h.org("alpha-freight").await;
    let org_b = h.org("beta-mining").await;
    h.join(org_a, alice, "owner").await;
    h.join(org_b, bob, "owner").await;

    let secret = format!("alpha-reactor-coolant-{}", uuid::Uuid::new_v4());
    h.inventory(org_a, alice, &secret, 42).await;

    // Alice sees her own.
    let (status, mine) = h.get("/api/inventory", &alice_token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        mine.as_array().map_or(false, |a| a.iter().any(|i| i["name"] == json!(secret))),
        "an org could not see its own inventory"
    );

    // Bob must not.
    let (status, theirs) = h.get("/api/inventory", &bob_token).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        theirs.as_array().map_or(true, |a| !a.iter().any(|i| i["name"] == json!(secret))),
        "ORG BOUNDARY LEAK: beta-mining can see alpha-freight's inventory"
    );
}

#[tokio::test]
async fn search_does_not_cross_an_org_boundary() {
    // A free-text box over every table is exactly where a scoping mistake
    // surfaces, because it touches tables the author was not thinking about.
    let h = require_db!();

    let (alice, _) = h.user("user").await;
    let (bob, bob_token) = h.user("user").await;
    let org_a = h.org("alpha-search").await;
    let org_b = h.org("beta-search").await;
    h.join(org_a, alice, "owner").await;
    h.join(org_b, bob, "owner").await;

    let needle = format!("xenon-{}", uuid::Uuid::new_v4().simple());
    h.inventory(org_a, alice, &needle, 7).await;

    let (status, r) = h.get(&format!("/api/search?q={needle}"), &bob_token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        r["count"], json!(0),
        "ORG BOUNDARY LEAK: search returned another org's resource"
    );
}

#[tokio::test]
async fn the_rollup_does_not_cross_an_org_boundary() {
    let h = require_db!();

    let (alice, _) = h.user("user").await;
    let (bob, bob_token) = h.user("admin").await;
    let org_a = h.org("alpha-rollup").await;
    let org_b = h.org("beta-rollup").await;
    h.join(org_a, alice, "owner").await;
    h.join(org_b, bob, "owner").await;

    let secret = format!("alpha-fuel-{}", uuid::Uuid::new_v4().simple());
    h.inventory(org_a, alice, &secret, 500).await;

    let (status, r) = h.get("/api/rollup/inventory", &bob_token).await;
    assert_eq!(status, StatusCode::OK);
    let leaked = r["items"]
        .as_array()
        .map_or(false, |a| a.iter().any(|i| i["name"] == json!(secret)));
    assert!(!leaked, "ORG BOUNDARY LEAK: the rollup exposed another org's holdings");
}

#[tokio::test]
async fn actuation_scopes_cannot_be_granted_across_orgs() {
    // Not "off by default" — unrepresentable. One org actuating another's
    // outpost is a safety boundary, so no configuration mistake may grant it.
    let h = require_db!();
    let (admin_id, admin) = h.user("admin").await;
    let org = h.org("grantor").await;
    h.join(org, admin_id, "owner").await;

    for scope in ["commands", "telemetry", "rules", "inventory", "rollup"] {
        let (status, body) = h
            .post(
                &format!("/api/orgs/{org}/trust"),
                &admin,
                json!({
                    "counterparty_name": "Someone Else",
                    "counterparty_root_key": "Zm9yYmlkZGVuLWtleS1mb3ItdGVzdGluZy0xMjM0NQ==",
                    "scopes": [scope]
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "'{scope}' was accepted as a grantable scope");
        assert!(
            body.as_str().unwrap_or_default().contains("never cross"),
            "refusal for '{scope}' did not explain why"
        );
    }
}

// ---------------------------------------------------------------------------
// Business rules that must hold end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_compliance_hold_actually_blocks_a_shipment() {
    let h = require_db!();
    let (admin_id, admin) = h.user("admin").await;
    let org = h.org("compliance-org").await;
    h.join(org, admin_id, "owner").await;

    // A destination that demands an export licence nothing holds.
    let body = unique_body();
    let (status, _) = h
        .post("/api/compliance/requirements", &admin,
              json!({"destination_body_id": body, "required_kind": "export"}))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, order) = h
        .post("/api/orders", &admin, json!({
            "description": format!("controlled part {}", uuid::Uuid::new_v4().simple()),
            "item_kind": "inventory", "quantity": 1, "unit": "each",
            "delivery_body_id": body
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    let order_id = order["id"].as_str().unwrap();

    let (status, check) = h.get(&format!("/api/compliance/check/{order_id}"), &admin).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["mayShip"], json!(false), "a missing export licence did not block the shipment");
    assert!(check["activeHolds"].as_i64().unwrap_or(0) >= 1);
}

#[tokio::test]
async fn an_override_survives_the_next_check() {
    // Found in live testing: the duplicate-hold test looked only for active
    // holds, so an overridden one was invisible and the next check raised a
    // replacement — silently re-blocking a shipment an admin had released.
    let h = require_db!();
    let (admin_id, admin) = h.user("admin").await;
    let org = h.org("override-org").await;
    h.join(org, admin_id, "owner").await;

    let body = unique_body();
    h.post("/api/compliance/requirements", &admin,
           json!({"destination_body_id": body, "required_kind": "hazmat"})).await;

    let (_, order) = h.post("/api/orders", &admin, json!({
        "description": format!("hazmat load {}", uuid::Uuid::new_v4().simple()),
        "item_kind": "inventory", "quantity": 1, "unit": "each",
        "delivery_body_id": body
    })).await;
    let order_id = order["id"].as_str().unwrap().to_string();

    h.get(&format!("/api/compliance/check/{order_id}"), &admin).await;
    let (_, holds) = h.get(&format!("/api/compliance/holds?order_id={order_id}&status=active"), &admin).await;
    let hold_id = holds[0]["id"].as_str().unwrap().to_string();

    let (status, _) = h
        .post(&format!("/api/compliance/holds/{hold_id}/override"), &admin,
              json!({"reason": "written authorisation on file, ref TEST-0001"}))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Re-checking repeatedly must not resurrect it.
    for round in 1..=3 {
        let (_, check) = h.get(&format!("/api/compliance/check/{order_id}"), &admin).await;
        assert_eq!(check["mayShip"], json!(true), "the override was undone on round {round}");
    }
}

#[tokio::test]
async fn a_non_admin_cannot_override_a_compliance_hold() {
    let h = require_db!();
    let (admin_id, admin) = h.user("admin").await;
    let (_, plain) = h.user("user").await;
    let org = h.org("override-authz").await;
    h.join(org, admin_id, "owner").await;

    let body = unique_body();
    h.post("/api/compliance/requirements", &admin,
           json!({"destination_body_id": body, "required_kind": "customs"})).await;
    let (_, order) = h.post("/api/orders", &admin, json!({
        "description": format!("customs load {}", uuid::Uuid::new_v4().simple()),
        "item_kind": "inventory", "quantity": 1, "unit": "each", "delivery_body_id": body
    })).await;
    let order_id = order["id"].as_str().unwrap();
    h.get(&format!("/api/compliance/check/{order_id}"), &admin).await;
    let (_, holds) = h.get(&format!("/api/compliance/holds?order_id={order_id}&status=active"), &admin).await;
    let hold_id = holds[0]["id"].as_str().unwrap();

    let (status, _) = h
        .post(&format!("/api/compliance/holds/{hold_id}/override"), &plain,
              json!({"reason": "I would simply like this to ship, thank you"}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn stock_cannot_be_driven_negative() {
    let h = require_db!();
    let (owner, token) = h.user("admin").await;
    let org = h.org("movement-org").await;
    h.join(org, owner, "owner").await;
    let item = h.inventory(org, owner, &format!("bolts-{}", uuid::Uuid::new_v4().simple()), 10).await;

    let (status, _) = h.post("/api/movements", &token,
        json!({"inventory_id": item, "delta": -4, "reason": "issue"})).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = h.post("/api/movements", &token,
        json!({"inventory_id": item, "delta": -99, "reason": "issue"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "stock went negative");
    assert!(body.as_str().unwrap_or_default().contains("only 6"),
            "the refusal did not say what was actually on hand");
}

/// A destination body id no other test will use.
///
/// Compliance requirements are unique on (destination_body_id, item_category,
/// required_kind), so two tests sharing a body share requirements — and a test
/// that overrides its own hold then fails because it inherited someone else's.
/// That is exactly what happened: the first version drew from a 90-value space
/// and flaked roughly one run in three.
///
/// A per-process random base keeps concurrent `cargo test` invocations apart;
/// the atomic counter keeps tests within a process apart. Body ids are i32, so
/// there is no shortage.
fn unique_body() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicI32 = AtomicI32::new(0);
    static BASE: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

    let base = *BASE.get_or_init(|| {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        100_000 + (nanos % 1_000_000) as i32 * 100
    });
    base + NEXT.fetch_add(1, Ordering::Relaxed)
}
