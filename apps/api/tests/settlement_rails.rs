//! Settlement-rail behaviour against a real Postgres.
//!
//! The unit tests in `services::settlement` cover the rail logic in isolation
//! (resolution, per-rail validation, mint, payee precedence). These assert the
//! facts that only the assembled application and schema can show: that a fiat
//! settlement is recorded pending and reaches 'settled' only once attested,
//! that attestation cannot be used to wave through an on-chain settlement, and
//! that the payee stored is the organisation's wallet, not the employee's.
//!
//! The harness sets `SETTLEMENT_VERIFY=off`, so an on-chain settlement here is
//! stamped 'skipped' rather than reaching a chain — which is exactly what lets
//! these assert the recorded payee and the confirm guard without an RPC.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use common::Harness;

/// A distinct, well-formed (88-char base58) Solana signature per call. The
/// unique index on `settlement_tx` is global and the test DB is shared across
/// runs, so a fixed signature would 409 on the second test and on every rerun —
/// a stored fixture value, not a bug. Deriving from a fresh UUID sidesteps both.
fn sig() -> String {
    const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let bytes = *Uuid::new_v4().as_bytes();
    (0..88).map(|i| B58[(bytes[i % 16] as usize + i) % B58.len()] as char).collect()
}

/// A per-run-unique invoice reference, for the same reason.
fn invoice() -> String {
    format!("INV-{}", Uuid::new_v4())
}

/// Seed an order/bid/fulfillment already at 'delivered', so a settlement can be
/// posted against it without driving the whole marketplace flow.
async fn seed_delivered(h: &Harness, requester: Uuid, shipper: Uuid, price: &str) -> Uuid {
    let order = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO orders (id, requester_id, description, item_kind, quantity, unit, status)
         VALUES ($1,$2,'test order','inventory',1,'each','delivered')",
    )
    .bind(order)
    .bind(requester)
    .execute(&h.db)
    .await
    .expect("insert order");

    let bid = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO bids (id, order_id, bidder_id, price, status)
         VALUES ($1,$2,$3,$4::numeric,'accepted')",
    )
    .bind(bid)
    .bind(order)
    .bind(shipper)
    .bind(price)
    .execute(&h.db)
    .await
    .expect("insert bid");

    sqlx::query("UPDATE orders SET accepted_bid_id=$2 WHERE id=$1")
        .bind(order)
        .bind(bid)
        .execute(&h.db)
        .await
        .expect("set accepted bid");

    let ful = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO fulfillments (id, order_id, bid_id, shipper_id, status)
         VALUES ($1,$2,$3,$4,'delivered')",
    )
    .bind(ful)
    .bind(order)
    .bind(bid)
    .bind(shipper)
    .execute(&h.db)
    .await
    .expect("insert fulfillment");

    ful
}

#[tokio::test]
async fn fiat_settlement_is_recorded_pending_then_attested() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, shp_tok) = h.user("admin").await;
    let ful = seed_delivered(&h, requester, shipper, "100").await;

    // Fiat records immediately, needs no wallet, and does not reach 'settled':
    // it waits for the counterparty to attest the invoice.
    let (st, body) = h
        .post(
            &format!("/api/fulfillments/{ful}/settle"),
            &req_tok,
            json!({ "settlement_tx": invoice(), "rail": "fiat" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    assert_eq!(body["settlement_status"], "pending");
    assert_eq!(body["status"], "settling");

    // The counterparty attests → 'attested' and the order settles.
    let (st2, body2) = h
        .post(&format!("/api/fulfillments/{ful}/settle/confirm"), &shp_tok, json!({}))
        .await;
    assert_eq!(st2, StatusCode::OK, "{body2:?}");
    assert_eq!(body2["settlement_status"], "attested");
    assert_eq!(body2["status"], "settled");

    let order_status: String = sqlx::query_scalar(
        "SELECT status FROM orders WHERE id=(SELECT order_id FROM fulfillments WHERE id=$1)",
    )
    .bind(ful)
    .fetch_one(&h.db)
    .await
    .unwrap();
    assert_eq!(order_status, "settled");
}

#[tokio::test]
async fn an_unknown_rail_is_refused() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    let ful = seed_delivered(&h, requester, shipper, "100").await;

    let (st, _) = h
        .post(
            &format!("/api/fulfillments/{ful}/settle"),
            &req_tok,
            json!({ "settlement_tx": "INV-1", "rail": "dogecoin" }),
        )
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn attestation_cannot_confirm_an_on_chain_settlement() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, shp_tok) = h.user("admin").await;
    let ful = seed_delivered(&h, requester, shipper, "100").await;

    // On-chain rail under SETTLEMENT_VERIFY=off records as 'skipped', not a
    // pending fiat settlement.
    let (st, body) = h
        .post(
            &format!("/api/fulfillments/{ful}/settle"),
            &req_tok,
            json!({ "settlement_tx": sig(), "rail": "tidat" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    assert_eq!(body["settlement_status"], "skipped");

    // Confirming it must be refused — attestation is not a backdoor around the
    // chain.
    let (st2, _) = h
        .post(&format!("/api/fulfillments/{ful}/settle/confirm"), &shp_tok, json!({}))
        .await;
    assert_eq!(st2, StatusCode::CONFLICT);
}

#[tokio::test]
async fn payee_is_the_org_wallet_when_the_shipper_has_one() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;
    let org = h.org("acme-logistics").await;
    h.join(org, shipper, "operator").await;

    sqlx::query("UPDATE users SET wallet_address=$2 WHERE id=$1")
        .bind(shipper)
        .bind("USERwa11et111111111111111111111111111111111")
        .execute(&h.db)
        .await
        .unwrap();
    sqlx::query("UPDATE organisations SET wallet_address=$2 WHERE id=$1")
        .bind(org)
        .bind("ORGwa11et1111111111111111111111111111111111")
        .execute(&h.db)
        .await
        .unwrap();

    let ful = seed_delivered(&h, requester, shipper, "100").await;
    let (st, _) = h
        .post(
            &format!("/api/fulfillments/{ful}/settle"),
            &req_tok,
            json!({ "settlement_tx": sig(), "rail": "tidat" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);

    let payee: Option<String> =
        sqlx::query_scalar("SELECT settlement_payee FROM fulfillments WHERE id=$1")
            .bind(ful)
            .fetch_one(&h.db)
            .await
            .unwrap();
    assert_eq!(payee.as_deref(), Some("ORGwa11et1111111111111111111111111111111111"));
}

#[tokio::test]
async fn payee_falls_back_to_the_user_wallet_for_a_sole_trader() {
    let h = require_db!();
    let (requester, req_tok) = h.user("admin").await;
    let (shipper, _) = h.user("admin").await;

    // No organisation: a sole trader settles to their own wallet.
    sqlx::query("UPDATE users SET wallet_address=$2 WHERE id=$1")
        .bind(shipper)
        .bind("SOLEtrader1111111111111111111111111111111111")
        .execute(&h.db)
        .await
        .unwrap();

    let ful = seed_delivered(&h, requester, shipper, "100").await;
    let (st, _) = h
        .post(
            &format!("/api/fulfillments/{ful}/settle"),
            &req_tok,
            json!({ "settlement_tx": sig(), "rail": "tidat" }),
        )
        .await;
    assert_eq!(st, StatusCode::OK);

    let payee: Option<String> =
        sqlx::query_scalar("SELECT settlement_payee FROM fulfillments WHERE id=$1")
            .bind(ful)
            .fetch_one(&h.db)
            .await
            .unwrap();
    assert_eq!(payee.as_deref(), Some("SOLEtrader1111111111111111111111111111111111"));
}
