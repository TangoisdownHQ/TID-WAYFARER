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

// ===========================================================================
// Lots, batches and serials
//
// This whole module shipped with no organisation scoping at all — nine
// handlers, none filtering. It survived because this suite was written before
// those routes existed and never covered them; a smoke test against seeded
// data found it in one line, by showing a user in one organisation the other's
// battery serials.
//
// The writes were the worse half: `quarantine` and `usage` let one
// organisation condemn or re-clock another's stock, which is a safety decision
// about somebody else's equipment.
// ===========================================================================

/// A serialised unit with a life limit, owned by one organisation.
///
/// `serial` is a prefix: a random suffix is appended because serials are
/// globally unique and this suite shares one database across repeated runs.
/// Fixed fixture serials collided on the second run — the same trap the
/// `unique_body` helper exists for. The full serial is returned so assertions
/// match on what was actually written.
async fn limited_unit(
    h: &common::Harness,
    org: uuid::Uuid,
    owner: uuid::Uuid,
    name: &str,
    serial_prefix: &str,
    lot_code: &str,
    cycles_used: f64,
) -> (uuid::Uuid, uuid::Uuid, String) {
    let serial = format!("{serial_prefix}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let item: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO inventory (id, owner_id, org_id, name, quantity, location, category, unit, \
                                threshold, serialised, life_limit_cycles) \
         VALUES (gen_random_uuid(), $1, $2, $3, 1, 'bay', 'battery', 'each', 1, true, 3000) \
         RETURNING id",
    )
    .bind(owner)
    .bind(org)
    .bind(name)
    .fetch_one(&h.db)
    .await
    .expect("could not create item");

    let lot: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO inventory_lots (inventory_id, lot_code, serial, quantity, status, \
                                     cycles_used, expires_at) \
         VALUES ($1, $2, $3, 1, 'available', $4, NOW() + INTERVAL '5 days') \
         RETURNING id",
    )
    .bind(item)
    .bind(lot_code)
    .bind(&serial)
    .bind(cycles_used)
    .fetch_one(&h.db)
    .await
    .expect("could not create lot");

    (item, lot, serial)
}

/// A lot code unique to this test run, but shared between the two fixtures in
/// one test — so a trace against it is the realistic guess an attacker makes.
fn shared_lot_code() -> String {
    format!("BATCH-{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

/// The life-limit sweep does not show another organisation's units.
///
/// This is the exact leak the smoke test caught: serials, locations and usage
/// of somebody else's serialised stock.
#[tokio::test]
async fn the_life_sweep_does_not_cross_an_org_boundary() {
    let h = require_db!();
    let mine = h.org("life-ours").await;
    let theirs = h.org("life-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, their_token) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let code = shared_lot_code();
    let (_item, _lot, ours) = limited_unit(&h, mine, me, "Our pack", "SN-OURS", &code, 2950.0).await;
    let (_, _, theirs_serial) =
        limited_unit(&h, theirs, them, "Their pack", "SN-THEIRS", &code, 2990.0).await;

    let (status, mine_view) = h.get("/api/lots/life?at_pct=0.5", &my_token).await;
    assert_eq!(status, StatusCode::OK, "{mine_view:?}");
    let serials: Vec<String> = mine_view["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["serial"].as_str().map(String::from))
        .collect();
    assert!(serials.contains(&ours), "our own unit is missing: {serials:?}");
    assert!(
        !serials.contains(&theirs_serial),
        "another organisation's serial leaked: {serials:?}"
    );

    // And symmetrically.
    let (_, their_view) = h.get("/api/lots/life?at_pct=0.5", &their_token).await;
    let theirs_serials: Vec<String> = their_view["units"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|u| u["serial"].as_str().map(String::from))
        .collect();
    assert!(!theirs_serials.contains(&ours), "{theirs_serials:?}");
}

/// The expiry sweep is scoped too.
#[tokio::test]
async fn the_expiry_sweep_does_not_cross_an_org_boundary() {
    let h = require_db!();
    let mine = h.org("exp-ours").await;
    let theirs = h.org("exp-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, _) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let code = shared_lot_code();
    let (_, _, ours) = limited_unit(&h, mine, me, "Our coolant", "SN-EXP-OURS", &code, 0.0).await;
    let (_, _, theirs_serial) =
        limited_unit(&h, theirs, them, "Their coolant", "SN-EXP-THEIRS", &code, 0.0).await;

    let (_, view) = h.get("/api/lots/expiring?days=30", &my_token).await;
    let body = serde_json::to_string(&view).unwrap();
    assert!(body.contains(&ours), "our own lapsing lot is missing");
    assert!(!body.contains(&theirs_serial), "another organisation's expiry leaked: {body}");
}

/// A recall trace cannot be used to look up somebody else's batch.
///
/// Worse than the sweeps, because the caller supplies the lot code: knowing or
/// guessing a batch number would otherwise return the item, the location and
/// the quantity still outstanding.
#[tokio::test]
async fn a_trace_cannot_read_another_orgs_batch() {
    let h = require_db!();
    let mine = h.org("trace-ours").await;
    let theirs = h.org("trace-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, _) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let code = shared_lot_code();
    let (_, _, theirs_serial) =
        limited_unit(&h, theirs, them, "Their pack", "SN-TRACE-THEIRS", &code, 10.0).await;

    // Tracing the batch code directly, which is exactly the guess an attacker
    // makes: a lot code is printed on a label and is not a secret.
    let (status, view) = h.get(&format!("/api/lots/trace?lot_code={code}"), &my_token).await;
    assert_eq!(status, StatusCode::OK, "{view:?}");
    let body = serde_json::to_string(&view).unwrap();
    assert!(
        !body.contains(&theirs_serial),
        "a trace returned another organisation's batch: {body}"
    );
}

/// One organisation cannot quarantine another's stock.
///
/// The write half, and the serious half: quarantining somebody else's lot pulls
/// it out of their usable stock, which is a decision about their operation.
#[tokio::test]
async fn one_org_cannot_quarantine_anothers_lot() {
    let h = require_db!();
    let mine = h.org("q-ours").await;
    let theirs = h.org("q-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, _) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let (_item, their_lot, _) =
        limited_unit(&h, theirs, them, "Their pack", "SN-Q-THEIRS", &shared_lot_code(), 10.0).await;

    let (status, body) = h
        .post(
            &format!("/api/lots/{their_lot}/quarantine"),
            &my_token,
            serde_json::json!({ "reason": "not mine to condemn" }),
        )
        .await;
    // 404, not 403: confirming it exists would disclose that another
    // organisation holds that serial.
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    let still: String = sqlx::query_scalar("SELECT status FROM inventory_lots WHERE id = $1")
        .bind(their_lot)
        .fetch_one(&h.db)
        .await
        .unwrap();
    assert_eq!(still, "available", "another organisation's lot was quarantined");
}

/// One organisation cannot re-clock another's unit.
///
/// Usage decides when a part retires, so writing it moves somebody else's
/// safety decision.
#[tokio::test]
async fn one_org_cannot_record_usage_on_anothers_unit() {
    let h = require_db!();
    let mine = h.org("u-ours").await;
    let theirs = h.org("u-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, _) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let (_item, their_lot, _) =
        limited_unit(&h, theirs, them, "Their motor", "SN-U-THEIRS", &shared_lot_code(), 100.0).await;

    let (status, body) = h
        .call(
            "PATCH",
            &format!("/api/lots/{their_lot}/usage"),
            Some(&my_token),
            Some(serde_json::json!({ "cycles_used": 2999 })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    let cycles: rust_decimal::Decimal =
        sqlx::query_scalar("SELECT cycles_used FROM inventory_lots WHERE id = $1")
            .bind(their_lot)
            .fetch_one(&h.db)
            .await
            .unwrap();
    assert_eq!(cycles.to_string(), "100", "another organisation's usage was rewritten");
}

/// A lot cannot be attached to another organisation's resource.
///
/// It would then appear in their lot totals and their expiry sweep, as stock
/// they never received.
#[tokio::test]
async fn one_org_cannot_add_a_lot_to_anothers_item() {
    let h = require_db!();
    let mine = h.org("c-ours").await;
    let theirs = h.org("c-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, _) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let (their_item, _, _) =
        limited_unit(&h, theirs, them, "Their pack", "SN-C-THEIRS", &shared_lot_code(), 10.0).await;

    let (status, body) = h
        .post(
            &format!("/api/inventory/{their_item}/lots"),
            &my_token,
            serde_json::json!({ "lot_code": "INJECTED", "quantity": 1 }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM inventory_lots WHERE inventory_id = $1 AND lot_code = 'INJECTED'",
    )
    .bind(their_item)
    .fetch_one(&h.db)
    .await
    .unwrap();
    assert_eq!(n, 0, "a lot was attached to another organisation's resource");
}

/// Reading one lot directly is scoped as well.
#[tokio::test]
async fn one_org_cannot_read_anothers_lot_by_id() {
    let h = require_db!();
    let mine = h.org("r-ours").await;
    let theirs = h.org("r-theirs").await;
    let (me, my_token) = h.user("user").await;
    let (them, _) = h.user("user").await;
    h.join(mine, me, "operator").await;
    h.join(theirs, them, "operator").await;

    let (their_item, their_lot, _) =
        limited_unit(&h, theirs, them, "Their pack", "SN-R-THEIRS", &shared_lot_code(), 10.0).await;

    let (status, _) = h.get(&format!("/api/lots/{their_lot}"), &my_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // And the per-item listing.
    let (status, _) = h.get(&format!("/api/inventory/{their_item}/lots"), &my_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
