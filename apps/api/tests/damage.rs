//! Damage evidence suspends settlement until it is resolved.
//!
//! The pure signing is unit-tested in `services::damage`. These assert the loop
//! that makes the feature matter: a party files a signed report, and while it
//! is open neither on-chain settlement nor an offline voucher can complete;
//! resolving it lifts the hold. A non-party can neither file nor resolve.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use common::Harness;

/// A well-formed (88-char base58) Solana signature, unique per call — the
/// settlement_tx unique index is global and the test DB is shared across runs.
fn sig() -> String {
    const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let bytes = *Uuid::new_v4().as_bytes();
    (0..88).map(|i| B58[(bytes[i % 16] as usize + i) % B58.len()] as char).collect()
}

/// Seed an order with an accepted bid and a delivered fulfillment. The shipper
/// gets a wallet so a voucher/settlement has a payee. Returns (order, fulfillment).
async fn seed(h: &Harness, requester: Uuid, shipper: Uuid) -> (Uuid, Uuid) {
    sqlx::query("UPDATE users SET wallet_address='9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM' WHERE id=$1")
        .bind(shipper).execute(&h.db).await.unwrap();
    let order = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, requester_id, description, item_kind, quantity, unit, status)
         VALUES ($1,$2,'dmg order','inventory',1,'each','delivered')",
    ).bind(order).bind(requester).execute(&h.db).await.expect("order");
    let bid = Uuid::new_v4();
    sqlx::query("INSERT INTO bids (id, order_id, bidder_id, price, status) VALUES ($1,$2,$3,100::numeric,'accepted')")
        .bind(bid).bind(order).bind(shipper).execute(&h.db).await.expect("bid");
    sqlx::query("UPDATE orders SET accepted_bid_id=$2 WHERE id=$1").bind(order).bind(bid).execute(&h.db).await.unwrap();
    let ful = Uuid::new_v4();
    sqlx::query("INSERT INTO fulfillments (id, order_id, bid_id, shipper_id, status) VALUES ($1,$2,$3,$4,'delivered')")
        .bind(ful).bind(order).bind(bid).bind(shipper).execute(&h.db).await.expect("fulfillment");
    (order, ful)
}

async fn file_report(h: &Harness, tok: &str, order: Uuid, ful: Uuid) -> (StatusCode, serde_json::Value) {
    h.post(&format!("/api/orders/{order}/damage"), tok, json!({
        "fulfillment_id": ful, "condition": "damaged", "severity": "major",
        "quantity_affected": 3.0, "description": "crushed in transit",
        "evidence_hash": format!("sha256:{}", Uuid::new_v4())
    })).await
}

#[tokio::test]
async fn an_open_damage_report_suspends_settlement_until_resolved() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, shp_tok) = h.user("admin").await;
    let (order, ful) = seed(&h, requester, shipper).await;

    // A party files a signed report.
    let (st, body) = file_report(&h, &req_tok, order, ful).await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    assert_eq!(body["status"], "open");
    assert!(!body["reporter_sig"].as_str().unwrap_or("").is_empty(), "it is signed");
    let rid = body["id"].as_str().unwrap().to_string();

    // Settlement is refused while the report is open.
    let (st_blocked, _) = h.post(&format!("/api/fulfillments/{ful}/settle"), &shp_tok,
        json!({ "settlement_tx": sig(), "rail": "tidat" })).await;
    assert_eq!(st_blocked, StatusCode::CONFLICT, "settlement must be suspended");

    // The counterparty resolves it.
    let (st_res, body_res) = h.post(&format!("/api/orders/{order}/damage/{rid}/resolve"), &shp_tok,
        json!({ "action": "resolved", "resolution": "replaced free of charge" })).await;
    assert_eq!(st_res, StatusCode::OK, "{body_res:?}");
    assert_eq!(body_res["status"], "resolved");

    // Now settlement goes through (harness runs SETTLEMENT_VERIFY=off → skipped).
    let (st_ok, body_ok) = h.post(&format!("/api/fulfillments/{ful}/settle"), &shp_tok,
        json!({ "settlement_tx": sig(), "rail": "tidat" })).await;
    assert_eq!(st_ok, StatusCode::OK, "suspension lifted: {body_ok:?}");
}

#[tokio::test]
async fn an_open_damage_report_suspends_voucher_discharge() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _shp_tok) = h.user("admin").await;
    let (order, _ful) = seed(&h, requester, shipper).await;

    // Commit an IOU voucher for the order.
    let (stv, vbody) = h.post("/api/vouchers", &req_tok, json!({ "order_id": order })).await;
    assert_eq!(stv, StatusCode::OK, "{vbody:?}");
    let vid = vbody["id"].as_str().unwrap().to_string();
    let ful = Uuid::parse_str(vbody["fulfillment_id"].as_str().unwrap()).unwrap();

    // File a damage report, then discharge is refused with 409 (not the 412 it
    // would be for an undelivered-but-uncontested voucher).
    let (st, _) = file_report(&h, &req_tok, order, ful).await;
    assert_eq!(st, StatusCode::OK);
    let (st_blocked, _) = h.post(&format!("/api/vouchers/{vid}/discharge"), &req_tok, json!({})).await;
    assert_eq!(st_blocked, StatusCode::CONFLICT, "discharge suspended by the open report");
}

#[tokio::test]
async fn only_a_party_can_file_a_report() {
    let h = require_db!();
    let (requester, _req_tok) = h.user("admin").await;
    let (shipper, _shp_tok) = h.user("admin").await;
    let (order, ful) = seed(&h, requester, shipper).await;

    let (_stranger, stranger_tok) = h.user("admin").await;
    let (st, _) = file_report(&h, &stranger_tok, order, ful).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_unknown_condition_is_refused() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    let (order, ful) = seed(&h, requester, shipper).await;

    let (st, _) = h.post(&format!("/api/orders/{order}/damage"), &req_tok, json!({
        "fulfillment_id": ful, "condition": "exploded", "severity": "major",
        "evidence_hash": "sha256:x"
    })).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}
