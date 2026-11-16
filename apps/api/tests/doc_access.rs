//! Shipping documents are visible only to a party to the order.
//!
//! The document handlers extract the authenticated user but used to ignore it,
//! so any account could pull another organisation's packing list, invoice, lot
//! serials and custody chain by guessing an order id. These assert the fix: a
//! party (requester or shipper) sees the document; anyone else is refused.

mod common;

use axum::http::StatusCode;
use uuid::Uuid;

use common::Harness;

/// Seed an order with an accepted bid and a fulfillment. Returns
/// (order_id, requester_id, shipper_id).
async fn seed(h: &Harness, requester: Uuid, shipper: Uuid) -> Uuid {
    let order = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, requester_id, description, item_kind, quantity, unit, status)
         VALUES ($1,$2,'doc order','inventory',1,'each','delivered')",
    )
    .bind(order).bind(requester).execute(&h.db).await.expect("order");
    let bid = Uuid::new_v4();
    sqlx::query("INSERT INTO bids (id, order_id, bidder_id, price, status) VALUES ($1,$2,$3,1::numeric,'accepted')")
        .bind(bid).bind(order).bind(shipper).execute(&h.db).await.expect("bid");
    sqlx::query("UPDATE orders SET accepted_bid_id=$2 WHERE id=$1").bind(order).bind(bid).execute(&h.db).await.unwrap();
    sqlx::query("INSERT INTO fulfillments (id, order_id, bid_id, shipper_id, status) VALUES ($1,$2,$3,$4,'delivered')")
        .bind(Uuid::new_v4()).bind(order).bind(bid).bind(shipper).execute(&h.db).await.expect("fulfillment");
    order
}

#[tokio::test]
async fn a_party_to_the_order_can_fetch_its_documents() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, shp_tok) = h.user("admin").await;
    let order = seed(&h, requester, shipper).await;

    let (st_req, _) = h.get(&format!("/api/documents/packing-list/{order}"), &req_tok).await;
    assert_eq!(st_req, StatusCode::OK, "the requester is a party");

    let (st_shp, _) = h.get(&format!("/api/documents/invoice/{order}"), &shp_tok).await;
    assert_eq!(st_shp, StatusCode::OK, "the shipper is a party");
}

#[tokio::test]
async fn a_stranger_cannot_fetch_someone_elses_documents() {
    let h = require_db!();
    let (requester, _) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    let order = seed(&h, requester, shipper).await;

    let (_stranger, stranger_tok) = h.user("admin").await;
    for doc in ["packing-list", "bill-of-lading", "customs-declaration", "invoice"] {
        let (st, body) = h.get(&format!("/api/documents/{doc}/{order}"), &stranger_tok).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{doc} leaked to a non-party: {body:?}");
    }
}
