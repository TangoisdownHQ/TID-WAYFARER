//! Conditional Transfer Voucher flow against a real Postgres.
//!
//! The pure CTV logic is unit-tested in `services::voucher`. These assert the
//! assembled behaviour: that a voucher commits as an IOU from the order's
//! requester, that it links to the fulfilment, that only the requester may
//! commit, that a missing payee wallet is refused, and that a voucher
//! discharges only once the order's custody chain shows delivery.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use common::Harness;

const SHIPPER_WALLET: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";

/// Seed an order (delivered), its accepted bid, and a delivered fulfilment.
/// Returns (order_id, fulfillment_id, shipper_id).
async fn seed(h: &Harness, requester: Uuid, shipper: Uuid, price: &str) -> (Uuid, Uuid, Uuid) {
    let order = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, requester_id, description, item_kind, quantity, unit, status)
         VALUES ($1,$2,'voucher order','inventory',1,'each','delivered')",
    )
    .bind(order).bind(requester).execute(&h.db).await.expect("order");

    let bid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO bids (id, order_id, bidder_id, price, status)
         VALUES ($1,$2,$3,$4::numeric,'accepted')",
    )
    .bind(bid).bind(order).bind(shipper).bind(price).execute(&h.db).await.expect("bid");
    sqlx::query("UPDATE orders SET accepted_bid_id=$2 WHERE id=$1")
        .bind(order).bind(bid).execute(&h.db).await.unwrap();

    let ful = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fulfillments (id, order_id, bid_id, shipper_id, status)
         VALUES ($1,$2,$3,$4,'delivered')",
    )
    .bind(ful).bind(order).bind(bid).bind(shipper).execute(&h.db).await.expect("fulfillment");

    (order, ful, shipper)
}

async fn set_wallet(h: &Harness, user: Uuid, wallet: &str) {
    sqlx::query("UPDATE users SET wallet_address=$2 WHERE id=$1")
        .bind(user).bind(wallet).execute(&h.db).await.unwrap();
}

#[tokio::test]
async fn requester_commits_an_iou_voucher_linked_to_the_fulfillment() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    set_wallet(&h, shipper, SHIPPER_WALLET).await;
    let (order, ful, _) = seed(&h, requester, shipper, "100").await;

    let (st, body) = h.post("/api/vouchers", &req_tok, json!({ "order_id": order })).await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    assert_eq!(body["state"], "committed");
    assert!(body["escrow_ref"].is_null(), "IOU has no escrow");
    assert_eq!(body["payee_wallet"], SHIPPER_WALLET);
    assert_eq!(body["amount"], 100.0);
    assert_eq!(body["fulfillment_id"], json!(ful));
    assert_eq!(body["nonce"], 0);
    assert!(!body["payer_sig"].as_str().unwrap_or("").is_empty(), "voucher is signed");

    // The fulfilment points back at the voucher.
    let ctv: Option<Uuid> = sqlx::query_scalar("SELECT ctv_id FROM fulfillments WHERE id=$1")
        .bind(ful).fetch_one(&h.db).await.unwrap();
    assert_eq!(ctv.map(|c| c.to_string()), Some(body["id"].as_str().unwrap().to_string()));
}

#[tokio::test]
async fn only_the_requester_may_commit() {
    let h = require_db!();
    let (requester, _) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    set_wallet(&h, shipper, SHIPPER_WALLET).await;
    let (order, _, _) = seed(&h, requester, shipper, "100").await;

    let (_stranger, stranger_tok) = h.user("admin").await;
    let (st, _) = h.post("/api/vouchers", &stranger_tok, json!({ "order_id": order })).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_shipper_with_no_wallet_is_refused() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await; // no wallet set
    let (order, _, _) = seed(&h, requester, shipper, "100").await;

    let (st, _) = h.post("/api/vouchers", &req_tok, json!({ "order_id": order })).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
}

#[tokio::test]
async fn a_voucher_discharges_only_after_delivery() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    set_wallet(&h, shipper, SHIPPER_WALLET).await;
    let (order, _, _) = seed(&h, requester, shipper, "100").await;

    let (st, body) = h.post("/api/vouchers", &req_tok, json!({ "order_id": order })).await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    let vid = body["id"].as_str().unwrap().to_string();

    // No custody chain yet: delivery condition unmet.
    let (st_early, _) = h.post(&format!("/api/vouchers/{vid}/discharge"), &req_tok, json!({})).await;
    assert_eq!(st_early, StatusCode::PRECONDITION_FAILED);

    // A complete chain reaching a 'delivery' handoff discharges it.
    sqlx::query(
        "INSERT INTO custody_receipts (order_id, seq, from_node_id, to_node_id, event, condition)
         VALUES ($1, 0, NULL, $2, 'delivery', 'ok')",
    )
    .bind(order).bind(Uuid::new_v4()).execute(&h.db).await.unwrap();

    let (st_ok, body2) = h.post(&format!("/api/vouchers/{vid}/discharge"), &req_tok, json!({})).await;
    assert_eq!(st_ok, StatusCode::OK, "{body2:?}");
    assert_eq!(body2["state"], "discharged");
}

#[tokio::test]
async fn present_is_restricted_to_the_payer() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    set_wallet(&h, shipper, SHIPPER_WALLET).await;
    let (order, _, _) = seed(&h, requester, shipper, "100").await;

    let (st, body) = h.post("/api/vouchers", &req_tok, json!({ "order_id": order })).await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    let vid = body["id"].as_str().unwrap().to_string();

    // The payer can present it.
    let (st_self, _) = h.get(&format!("/api/vouchers/{vid}"), &req_tok).await;
    assert_eq!(st_self, StatusCode::OK);

    // A stranger cannot.
    let (_other, other_tok) = h.user("admin").await;
    let (st_other, _) = h.get(&format!("/api/vouchers/{vid}"), &other_tok).await;
    assert_eq!(st_other, StatusCode::FORBIDDEN);
}
