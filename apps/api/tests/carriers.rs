//! Carrier legs, at the boundaries.
//!
//! The feature's whole claim is that a commercial carrier can be a leg of a
//! shipment without becoming a participant in the fabric. These assert the
//! parts of that claim which would be silently wrong otherwise: that an
//! address book does not leak across organisations, that a handover to a
//! carrier is recorded as *unverified* rather than filed as though somebody
//! signed for it, and that a carrier's own refusals are caught before a label
//! is bought rather than at the counter.

mod common;

use axum::http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

/// An operator in an org, which is all a carrier shipment needs — notably
/// *not* an admin, because shipping is an operator's job.
async fn operator(h: &common::Harness, org_name: &str) -> (Uuid, Uuid, String) {
    let org = h.org(org_name).await;
    let (user, token) = h.user("user").await;
    h.join(org, user, "operator").await;
    (org, user, token)
}

async fn earth_address(h: &common::Harness, token: &str, label: &str) -> Uuid {
    let (status, body) = h
        .post(
            "/api/addresses",
            token,
            json!({
                "label": label,
                "company": "TIDHQ",
                "line1": "1 Dock Road",
                "city": "Chicago",
                "region": "IL",
                "postcode": "60601",
                "country": "us",
                "residential": false
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    Uuid::parse_str(body["id"].as_str().unwrap()).unwrap()
}

/// An Earth address is validated against what a carrier needs, before a
/// carrier is asked — so an outpost with no link still catches the mistake.
#[tokio::test]
async fn an_earth_address_without_a_country_is_refused() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "addr-co").await;

    let (status, body) = h
        .post("/api/addresses", &token, json!({ "line1": "1 Dock Rd", "city": "Chicago" }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert!(body.as_str().unwrap_or_default().contains("country"), "{body:?}");
}

/// The body model is extended, not replaced: an off-Earth destination is
/// addressed by coordinates and is perfectly valid.
#[tokio::test]
async fn an_off_earth_destination_is_addressed_by_coordinates() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "mars-co").await;

    let (status, body) = h
        .post(
            "/api/addresses",
            &token,
            json!({ "label": "Jezero cache", "body_id": 499, "lat": 18.38, "lon": 77.58 }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(body["bodyId"], 499);
    // And it renders as something an operator can read, not an empty string.
    assert!(body["oneLine"].as_str().unwrap().contains("Jezero cache"), "{body:?}");

    // The same row without coordinates is refused — there is no postal
    // network out there to fall back on.
    let (status, body) = h
        .post("/api/addresses", &token, json!({ "label": "nowhere", "body_id": 499 }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
    assert!(body.as_str().unwrap_or_default().contains("lat and lon"), "{body:?}");
}

/// An address book is a customer list. It must not cross an org boundary.
#[tokio::test]
async fn an_address_book_does_not_leak_between_organisations() {
    let h = require_db!();
    let (_mine, _u1, my_token) = operator(&h, "ours-addr").await;
    let (_theirs, _u2, their_token) = operator(&h, "theirs-addr").await;

    let mine = earth_address(&h, &my_token, "Our secret depot").await;

    let (status, body) = h.get("/api/addresses", &their_token).await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    let leaked = body["addresses"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["id"] == mine.to_string());
    assert!(!leaked, "another organisation's address book was listed: {body:?}");

    // And 404, not 403 — confirming it exists would tell them we ship there.
    let (status, _) = h.get(&format!("/api/addresses/{mine}"), &their_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A shipment cannot be created against someone else's address.
///
/// The ids come from the caller, so without an explicit ownership check the
/// insert would happily reference another organisation's depot.
#[tokio::test]
async fn a_shipment_cannot_use_another_orgs_address() {
    let h = require_db!();
    let (_mine, _u1, my_token) = operator(&h, "ship-ours").await;
    let (_theirs, _u2, their_token) = operator(&h, "ship-theirs").await;

    let theirs = earth_address(&h, &their_token, "Their depot").await;
    let mine = earth_address(&h, &my_token, "Our depot").await;

    let (status, body) = h
        .post(
            "/api/carriers/shipments",
            &my_token,
            json!({
                "carrier": "ups",
                "from_address_id": mine,
                "to_address_id": theirs,
                "tracking_number": "1Z999AA10123456784"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
}

/// Recording a parcel handed to a carrier writes an **unverified** custody
/// receipt naming the carrier and the tracking number.
///
/// This is the heart of the design. A commercial carrier cannot sign, so the
/// tracking number is the evidence that stands in for a signature — and the
/// receipt has to say so. Filing it as verified would weaken every other
/// receipt in the chain, because a reader could no longer tell which ones
/// somebody actually signed for.
#[tokio::test]
async fn handing_a_parcel_to_a_carrier_records_an_unverified_custody_receipt() {
    let h = require_db!();
    let (org, user, token) = operator(&h, "custody-co").await;

    let from = earth_address(&h, &token, "Chicago depot").await;
    let to = earth_address(&h, &token, "Phoenix customer").await;

    let order: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (requester_id, org_id, description, item_kind, quantity, unit) \
         VALUES ($1,$2,'two pallets','part',2,'each') RETURNING id",
    )
    .bind(user)
    .bind(org)
    .fetch_one(&h.db)
    .await
    .unwrap();

    let tracking = format!("1Z{}", &Uuid::new_v4().simple().to_string()[..16]);
    let (status, body) = h
        .post(
            "/api/carriers/shipments",
            &token,
            json!({
                "carrier": "ups",
                "service": "ups_ground",
                "from_address_id": from,
                "to_address_id": to,
                "order_id": order,
                "tracking_number": tracking,
                "cost_amount": "24.80",
                "weight_kg": "12.5"
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    // A tracking number means the parcel is already gone, not a draft.
    assert_eq!(body["status"], "purchased");

    let receipt = sqlx::query(
        "SELECT to_label, to_node_id, verified, event, notes FROM custody_receipts \
         WHERE order_id = $1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(order)
    .fetch_optional(&h.db)
    .await
    .unwrap()
    .expect("no custody receipt was written for the carrier handover");

    use sqlx::Row;
    assert_eq!(receipt.get::<bool, _>("verified"), false, "a carrier cannot sign");
    assert!(receipt.get::<Option<Uuid>, _>("to_node_id").is_none(), "a carrier is not a node");
    let label: String = receipt.get("to_label");
    assert!(label.contains("UPS") && label.contains(&tracking), "{label}");
    // The reason is recorded on the receipt, so a reader a year later knows
    // why this one is unsigned.
    assert!(
        receipt.get::<Option<String>, _>("notes").unwrap_or_default().contains("evidence"),
        "the receipt should say what stands in for a signature"
    );
}

/// A shipment needs no order and no bid.
///
/// This is the single-org case the whole feature exists for: an organisation
/// shipping its own stock to its own customers has no marketplace counterparty
/// and must still get labels, tracking and paperwork.
#[tokio::test]
async fn a_shipment_works_with_no_marketplace_order_at_all() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "solo-co").await;
    let from = earth_address(&h, &token, "Warehouse").await;
    let to = earth_address(&h, &token, "Customer").await;

    let (status, body) = h
        .post(
            "/api/carriers/shipments",
            &token,
            json!({
                "carrier": "usps",
                "from_address_id": from,
                "to_address_id": to,
                "tracking_number": format!("94001{}", &Uuid::new_v4().simple().to_string()[..15])
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    assert_eq!(body["status"], "purchased");
}

/// One tracking number is one shipment.
///
/// Recording it twice would show two handovers for one parcel in the custody
/// chain, which is the same class of error as a replayed DTN envelope.
#[tokio::test]
async fn a_tracking_number_cannot_be_recorded_twice() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "dupe-co").await;
    let from = earth_address(&h, &token, "A").await;
    let to = earth_address(&h, &token, "B").await;
    let tracking = format!("1Z{}", &Uuid::new_v4().simple().to_string()[..16]);

    let body = json!({
        "carrier": "ups", "from_address_id": from, "to_address_id": to,
        "tracking_number": tracking
    });

    let (first, _) = h.post("/api/carriers/shipments", &token, body.clone()).await;
    assert_eq!(first, StatusCode::CREATED);

    let (second, msg) = h.post("/api/carriers/shipments", &token, body).await;
    assert_eq!(second, StatusCode::CONFLICT, "{msg:?}");
    assert!(msg.as_str().unwrap_or_default().contains("already recorded"), "{msg:?}");
}

/// A carrier's own refusals are caught before a label is bought.
///
/// The catalogue already records `hazard_class` and `un_number` — UN3480,
/// standalone lithium cells, is the most common carrier refusal there is — so
/// the data to check against already existed. This checks it at the point it
/// is still cheap to act on.
#[tokio::test]
async fn a_carrier_that_will_not_carry_something_refuses_before_the_label() {
    let h = require_db!();
    let (org, user, token) = operator(&h, "hazmat-co").await;
    let from = earth_address(&h, &token, "Battery store").await;
    let to = earth_address(&h, &token, "Field site").await;

    // An EV cell pack: the real-world case this rule exists for.
    let cell: Uuid = sqlx::query_scalar(
        "INSERT INTO inventory (id, owner_id, org_id, name, quantity, location, category, unit, \
                                threshold, un_number, hazard_class) \
         VALUES (gen_random_uuid(), $1, $2, 'EV cell module', 4, 'bay', 'battery', 'each', 1, \
                 'UN3480', '9') RETURNING id",
    )
    .bind(user)
    .bind(org)
    .fetch_one(&h.db)
    .await
    .unwrap();

    // Advisory check first: an operator should be able to ask.
    let (status, check) = h
        .post(
            "/api/carriers/check",
            &token,
            json!({ "carrier": "usps", "service": "usps_priority_air", "inventory_ids": [cell] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{check:?}");
    assert_eq!(check["acceptable"], false, "{check:?}");
    assert!(!check["forbidden"].as_array().unwrap().is_empty(), "{check:?}");

    // And creating the shipment is refused, with the reason.
    let (status, body) = h
        .post(
            "/api/carriers/shipments",
            &token,
            json!({
                "carrier": "usps", "service": "usps_priority_air",
                "from_address_id": from, "to_address_id": to,
                "tracking_number": format!("94001{}", &Uuid::new_v4().simple().to_string()[..15]),
                "inventory_ids": [cell]
            }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body:?}");
    assert!(body.as_str().unwrap_or_default().contains("will not carry"), "{body:?}");

    // A carrier that *does* take it with paperwork is a different answer, not
    // the same refusal — an operator needs to be able to tell those apart.
    let (status, check) = h
        .post(
            "/api/carriers/check",
            &token,
            json!({ "carrier": "fedex", "inventory_ids": [cell] }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(check["acceptable"], true, "{check:?}");
    assert!(
        !check["requiresDeclaration"].as_array().unwrap().is_empty(),
        "fedex should require a declaration rather than refuse: {check:?}"
    );
}

/// With no provider configured, rating says what to do instead of failing
/// opaquely.
///
/// `manual` is the default provider precisely because it works with no link,
/// and a missing API key must not read as a server fault.
#[tokio::test]
async fn rating_with_no_provider_tells_the_operator_what_to_do() {
    let h = require_db!();
    std::env::remove_var("CARRIER_EASYPOST_KEY");
    let (_org, _u, token) = operator(&h, "norate-co").await;
    let from = earth_address(&h, &token, "A").await;
    let to = earth_address(&h, &token, "B").await;

    let (status, body) = h
        .post(
            "/api/carriers/rates",
            &token,
            json!({ "from_address_id": from, "to_address_id": to, "weight_kg": "5" }),
        )
        .await;

    // 409: a state of the account, not a gap in the server.
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert!(
        body.as_str().unwrap_or_default().contains("record the shipment"),
        "the refusal should name the manual path: {body:?}"
    );
}

/// A zero-weight parcel is refused locally rather than by the carrier.
#[tokio::test]
async fn a_weightless_parcel_is_refused() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "weight-co").await;
    let from = earth_address(&h, &token, "A").await;
    let to = earth_address(&h, &token, "B").await;

    let (status, body) = h
        .post(
            "/api/carriers/rates",
            &token,
            json!({ "from_address_id": from, "to_address_id": to, "weight_kg": "0" }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
}

/// An address a shipment was sent to cannot be deleted.
///
/// It is the record of where something went. The same reason a person who
/// signed a custody receipt is deactivated rather than deleted.
#[tokio::test]
async fn an_address_with_shipment_history_cannot_be_deleted() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "history-co").await;
    let from = earth_address(&h, &token, "A").await;
    let to = earth_address(&h, &token, "B").await;

    h.post(
        "/api/carriers/shipments",
        &token,
        json!({
            "carrier": "ups", "from_address_id": from, "to_address_id": to,
            "tracking_number": format!("1Z{}", &Uuid::new_v4().simple().to_string()[..16])
        }),
    )
    .await;

    let (status, body) = h.call("DELETE", &format!("/api/addresses/{to}"), Some(&token), None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert!(body.as_str().unwrap_or_default().contains("cannot be deleted"), "{body:?}");

    // An unused address deletes fine.
    let spare = earth_address(&h, &token, "Unused").await;
    let (status, _) = h.call("DELETE", &format!("/api/addresses/{spare}"), Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
}

/// Shipments are listed per organisation, with the in-flight count the
/// shipping page leads on.
#[tokio::test]
async fn shipments_are_listed_per_organisation() {
    let h = require_db!();
    let (_mine, _u1, my_token) = operator(&h, "list-ours").await;
    let (_theirs, _u2, their_token) = operator(&h, "list-theirs").await;

    let from = earth_address(&h, &my_token, "A").await;
    let to = earth_address(&h, &my_token, "B").await;
    h.post(
        "/api/carriers/shipments",
        &my_token,
        json!({
            "carrier": "ups", "from_address_id": from, "to_address_id": to,
            "tracking_number": format!("1Z{}", &Uuid::new_v4().simple().to_string()[..16])
        }),
    )
    .await;

    let (_, mine) = h.get("/api/carriers/shipments", &my_token).await;
    assert_eq!(mine["count"], 1, "{mine:?}");
    assert_eq!(mine["inFlight"], 1);

    let (_, theirs) = h.get("/api/carriers/shipments", &their_token).await;
    assert_eq!(theirs["count"], 0, "another org's shipments were listed: {theirs:?}");
}

/// A draft with no stored rate cannot be purchased, and says why.
#[tokio::test]
async fn a_draft_with_no_rate_cannot_be_purchased() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "draft-co").await;
    let from = earth_address(&h, &token, "A").await;
    let to = earth_address(&h, &token, "B").await;

    let (_, created) = h
        .post(
            "/api/carriers/shipments",
            &token,
            json!({ "carrier": "ups", "from_address_id": from, "to_address_id": to }),
        )
        .await;
    assert_eq!(created["status"], "draft");
    let id = created["id"].as_str().unwrap();

    let (status, body) = h.post(&format!("/api/carriers/shipments/{id}/purchase"), &token, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert!(
        body.as_str().unwrap_or_default().contains("record a tracking number by hand"),
        "{body:?}"
    );
}

/// An already-purchased shipment cannot be purchased again.
///
/// Buying twice spends money twice, so a retried request must not be mistaken
/// for a new purchase.
#[tokio::test]
async fn a_purchased_shipment_cannot_be_bought_again() {
    let h = require_db!();
    let (_org, _u, token) = operator(&h, "nodouble-co").await;
    let from = earth_address(&h, &token, "A").await;
    let to = earth_address(&h, &token, "B").await;

    let (_, created) = h
        .post(
            "/api/carriers/shipments",
            &token,
            json!({
                "carrier": "ups", "from_address_id": from, "to_address_id": to,
                "tracking_number": format!("1Z{}", &Uuid::new_v4().simple().to_string()[..16])
            }),
        )
        .await;
    let id = created["id"].as_str().unwrap();

    let (status, body) = h.post(&format!("/api/carriers/shipments/{id}/purchase"), &token, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body:?}");
    assert!(body.as_str().unwrap_or_default().contains("already been bought"), "{body:?}");
}

fn _unused(_: Value) {}
