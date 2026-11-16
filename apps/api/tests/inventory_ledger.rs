//! The inventory level only ever moves through the movement ledger.
//!
//! This is the CRDT prerequisite from the roadmap: before stock counters can
//! be reconciled across offline outposts, every change to `inventory.quantity`
//! has to leave a movement behind, so the level can be reconstructed from the
//! ledger rather than being an opaque number that changed for no recorded
//! reason. The editor endpoints used to set the level directly; these assert
//! they no longer can.

mod common;

use axum::http::StatusCode;
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

use common::Harness;

/// Movements recorded against an item, newest first, as (delta, reason).
async fn movements(h: &Harness, inv: Uuid) -> Vec<(Decimal, String)> {
    sqlx::query_as::<_, (Decimal, String)>(
        "SELECT delta, reason FROM inventory_movements WHERE inventory_id=$1 ORDER BY occurred_at DESC, id DESC",
    )
    .bind(inv)
    .fetch_all(&h.db)
    .await
    .unwrap()
}

async fn level(h: &Harness, inv: Uuid) -> i32 {
    sqlx::query_scalar::<_, i32>("SELECT quantity FROM inventory WHERE id=$1")
        .bind(inv)
        .fetch_one(&h.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn setting_the_quantity_records_an_adjustment_movement() {
    let h = require_db!();
    let (uid, tok) = h.user("admin").await;
    let org = h.org("ledger-co").await;
    let inv = h.inventory(org, uid, "bolts", 10).await;

    let (st, body) = h
        .call("PUT", &format!("/api/inventory/{inv}/quantity"), Some(&tok), Some(json!({ "new_quantity": 7 })))
        .await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    assert_eq!(level(&h, inv).await, 7);

    // The -3 change left a ledger row rather than a silent overwrite.
    let ms = movements(&h, inv).await;
    assert_eq!(ms.len(), 1, "exactly one movement for the edit: {ms:?}");
    assert_eq!(ms[0].0, Decimal::from(-3));
    assert_eq!(ms[0].1, "adjustment");
}

#[tokio::test]
async fn a_full_item_edit_logs_the_quantity_delta_and_updates_metadata() {
    let h = require_db!();
    let (uid, tok) = h.user("admin").await;
    let org = h.org("ledger-co").await;
    let inv = h.inventory(org, uid, "washers", 5).await;

    let (st, body) = h
        .call("PUT", &format!("/api/inventory/{inv}"), Some(&tok),
              Some(json!({ "name": "washers (M6)", "quantity": 12 })))
        .await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    assert_eq!(body["name"], "washers (M6)");
    assert_eq!(level(&h, inv).await, 12);

    let ms = movements(&h, inv).await;
    assert_eq!(ms.len(), 1, "{ms:?}");
    assert_eq!(ms[0].0, Decimal::from(7)); // 5 -> 12
    assert_eq!(ms[0].1, "adjustment");
}

#[tokio::test]
async fn an_unchanged_quantity_records_no_movement() {
    let h = require_db!();
    let (uid, tok) = h.user("admin").await;
    let org = h.org("ledger-co").await;
    let inv = h.inventory(org, uid, "nuts", 8).await;

    // Editing only metadata, leaving quantity where it is, is not a stock event.
    let (st, _) = h
        .call("PUT", &format!("/api/inventory/{inv}"), Some(&tok),
              Some(json!({ "name": "nuts (M6)", "quantity": 8 })))
        .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(movements(&h, inv).await.len(), 0, "a no-op must not record a movement");
}

#[tokio::test]
async fn a_direct_movement_applies_and_is_ledgered() {
    let h = require_db!();
    let (uid, tok) = h.user("admin").await;
    let org = h.org("ledger-co").await;
    let inv = h.inventory(org, uid, "rivets", 10).await;

    let (st, body) = h
        .post("/api/movements", &tok, json!({ "inventory_id": inv, "delta": -3.0, "reason": "issue" }))
        .await;
    assert_eq!(st, StatusCode::CREATED, "{body:?}");
    assert_eq!(body["quantityBefore"], 10);
    assert_eq!(body["quantityAfter"], 7);
    assert_eq!(level(&h, inv).await, 7);
}

#[tokio::test]
async fn a_fractional_delta_is_refused_not_rounded() {
    let h = require_db!();
    let (uid, tok) = h.user("admin").await;
    let org = h.org("ledger-co").await;
    let inv = h.inventory(org, uid, "cable", 10).await;

    // The old code rounded 0.5 into the integer level, so the ledger and the
    // level drifted apart. A whole-unit item must refuse it instead.
    let (st, body) = h
        .post("/api/movements", &tok, json!({ "inventory_id": inv, "delta": -0.5, "reason": "issue" }))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{body:?}");
    assert!(body.as_str().unwrap_or_default().to_lowercase().contains("whole"), "{body:?}");
    // Nothing moved.
    assert_eq!(level(&h, inv).await, 10);
    assert_eq!(movements(&h, inv).await.len(), 0);
}

#[tokio::test]
async fn creating_an_item_records_its_opening_balance() {
    let h = require_db!();
    let (_uid, tok) = h.user("admin").await;

    let (st, body) = h
        .post("/api/inventory", &tok, json!({ "name": "pumps", "quantity": 15 }))
        .await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    let inv = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(level(&h, inv).await, 15);

    // The opening balance is a genesis 'receipt', so the level equals the sum
    // of the ledger — the exact invariant a stock CRDT needs.
    let ms = movements(&h, inv).await;
    assert_eq!(ms.len(), 1, "{ms:?}");
    assert_eq!(ms[0].0, Decimal::from(15));
    assert_eq!(ms[0].1, "receipt");
    let sum: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(delta),0) FROM inventory_movements WHERE inventory_id=$1",
    )
    .bind(inv)
    .fetch_one(&h.db)
    .await
    .unwrap();
    assert_eq!(sum, Decimal::from(15), "level must equal the sum of movements");
}

#[tokio::test]
async fn creating_an_empty_item_records_no_opening_movement() {
    let h = require_db!();
    let (_uid, tok) = h.user("admin").await;

    let (st, body) = h
        .post("/api/inventory", &tok, json!({ "name": "spares", "quantity": 0 }))
        .await;
    assert_eq!(st, StatusCode::OK, "{body:?}");
    let inv = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    assert_eq!(movements(&h, inv).await.len(), 0, "an empty item has no opening movement");
}

#[tokio::test]
async fn a_movement_cannot_drive_stock_negative() {
    let h = require_db!();
    let (uid, tok) = h.user("admin").await;
    let org = h.org("ledger-co").await;
    let inv = h.inventory(org, uid, "fuses", 2).await;

    let (st, _) = h
        .post("/api/movements", &tok, json!({ "inventory_id": inv, "delta": -5.0, "reason": "issue" }))
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(level(&h, inv).await, 2);
    assert_eq!(movements(&h, inv).await.len(), 0);
}
