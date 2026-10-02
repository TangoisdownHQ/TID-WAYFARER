//! People administration and conversations, at the boundaries.
//!
//! Both features add ways for one organisation's data to reach another's, so
//! these tests are mostly about what is *refused*. A messaging feature is the
//! classic place a carefully scoped application springs a leak: the access
//! rule looks obvious, it gets derived from org membership instead of thread
//! membership, and a year later someone who joined last week can read a
//! negotiation that closed before they arrived.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// An order raised by `requester` for `org`, with an optional bid against it.
async fn order_with_bid(
    h: &common::Harness,
    requester: Uuid,
    org: Uuid,
    bidder: Option<Uuid>,
) -> (Uuid, Option<Uuid>) {
    let order: Uuid = sqlx::query_scalar(
        "INSERT INTO orders (requester_id, org_id, description, item_kind, quantity, unit) \
         VALUES ($1, $2, 'test consignment', 'part', 1, 'each') RETURNING id",
    )
    .bind(requester)
    .bind(org)
    .fetch_one(&h.db)
    .await
    .expect("could not create order");

    let bid = match bidder {
        Some(b) => Some(
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO bids (order_id, bidder_id, price) VALUES ($1, $2, 100) RETURNING id",
            )
            .bind(order)
            .bind(b)
            .fetch_one(&h.db)
            .await
            .expect("could not create bid"),
        ),
        None => None,
    };

    (order, bid)
}

// ===== People administration =====

/// Only an owner or admin may add people.
///
/// The role comes from the membership row rather than the token, so a demoted
/// administrator loses the ability on their next request instead of at the end
/// of their session.
#[tokio::test]
async fn an_operator_cannot_create_accounts() {
    let h = require_db!();
    let org = h.org("depot").await;
    let (op, token) = h.user("user").await;
    h.join(org, op, "operator").await;

    let (status, body) = h
        .post("/api/people", &token, json!({ "email": "new@test.invalid" }))
        .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
}

/// An administrator's reach stops at their own organisation.
///
/// The target id is supplied by the caller, so without an explicit membership
/// check the UPDATE would happily edit a row belonging to another company.
#[tokio::test]
async fn an_admin_cannot_touch_another_orgs_people() {
    let h = require_db!();
    let mine = h.org("mine").await;
    let theirs = h.org("theirs").await;

    let (admin, token) = h.user("user").await;
    h.join(mine, admin, "admin").await;

    let (outsider, _) = h.user("user").await;
    h.join(theirs, outsider, "operator").await;

    let (status, body) = h
        .call(
            "PATCH",
            &format!("/api/people/{outsider}"),
            Some(&token),
            Some(json!({ "org_role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    let (status, body) = h
        .post(&format!("/api/people/{outsider}/password"), &token, json!({}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");
}

/// An administrator creating an account gets a password to pass on, once.
///
/// There is no mail server on an outpost that may be dark for days, so the
/// administrator reads it out. It is returned exactly here and stored only as
/// a hash, which is why `must_change_password` exists to remember it was
/// issued rather than chosen.
#[tokio::test]
async fn creating_a_person_returns_a_one_time_password_and_a_membership() {
    let h = require_db!();
    let org = h.org("foundry").await;
    let (admin, token) = h.user("user").await;
    h.join(org, admin, "owner").await;

    let email = format!("{}@test.invalid", Uuid::new_v4());
    let (status, body) = h
        .post(
            "/api/people",
            &token,
            json!({ "email": email, "full_name": "Rigger", "org_role": "operator" }),
        )
        .await;

    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    let temp = body["temporaryPassword"].as_str().expect("no password returned");
    assert!(temp.len() >= 20, "temporary password too short: {temp}");
    assert_eq!(body["mustChangePassword"], true);

    // The account and the membership are written together; a user belonging to
    // no org would sign in successfully and then see nothing, with no org
    // member list for an admin to find them in.
    let new_id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
    let joined: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM org_members WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org)
    .bind(new_id)
    .fetch_one(&h.db)
    .await
    .unwrap();
    assert_eq!(joined, 1, "created account has no organisation");

    // And the issued password actually works.
    let (status, login) = h
        .call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": email, "password": temp })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{login:?}");
    assert_eq!(login["mustChangePassword"], true);
}

/// A deactivated account cannot sign in, and is told why.
///
/// Checked after the password, so the endpoint still cannot be used to work
/// out who has an account here.
#[tokio::test]
async fn a_deactivated_account_cannot_sign_in() {
    let h = require_db!();
    let org = h.org("yard").await;
    let (admin, token) = h.user("user").await;
    h.join(org, admin, "owner").await;

    let email = format!("{}@test.invalid", Uuid::new_v4());
    let (_, created) = h.post("/api/people", &token, json!({ "email": email })).await;
    let temp = created["temporaryPassword"].as_str().unwrap().to_string();
    let id = created["id"].as_str().unwrap();

    let (status, _) = h
        .call("PATCH", &format!("/api/people/{id}/active"), Some(&token), Some(json!({"active": false})))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = h
        .call("POST", "/api/local-auth/login", None, Some(json!({ "email": email, "password": temp })))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");

    // Restoring brings them back rather than needing a new account, which is
    // the whole reason this is a flag and not a DELETE.
    h.call("PATCH", &format!("/api/people/{id}/active"), Some(&token), Some(json!({"active": true}))).await;
    let (status, _) = h
        .call("POST", "/api/local-auth/login", None, Some(json!({ "email": email, "password": temp })))
        .await;
    assert_eq!(status, StatusCode::OK);
}

/// An administrator cannot lock themselves out.
///
/// On an outpost that may be out of contact for days there is nobody to ring.
#[tokio::test]
async fn an_admin_cannot_deactivate_themselves() {
    let h = require_db!();
    let org = h.org("hab").await;
    let (me, token) = h.user("user").await;
    h.join(org, me, "admin").await;

    let (status, body) = h
        .call("PATCH", &format!("/api/people/{me}/active"), Some(&token), Some(json!({"active": false})))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body:?}");
}

// ===== Conversations =====

/// Thread membership is the access rule, and nothing else grants it.
///
/// Specifically not org membership: a colleague of a participant is still not
/// a participant. Deriving it from the org is the mistake this asserts against
/// — it would hand anyone who joins later a conversation that predates them.
#[tokio::test]
async fn a_colleague_of_a_participant_still_cannot_read_the_thread() {
    let h = require_db!();
    let org = h.org("crew").await;
    let (a, a_token) = h.user("user").await;
    let (b, b_token) = h.user("user").await;
    h.join(org, a, "admin").await;
    h.join(org, b, "operator").await;

    // A talks to nobody but themselves — a direct thread with one other
    // person would include B, so use an order-anchored thread A alone is on.
    let (order, _) = order_with_bid(&h, a, org, None).await;
    let (status, thread) = h
        .post("/api/chat", &a_token, json!({ "kind": "deal", "order_id": order, "body": "price?" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{thread:?}");
    let id = thread["id"].as_str().unwrap();

    // B is in the same organisation and is not in the room.
    let (status, body) = h.get(&format!("/api/chat/{id}"), &b_token).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    let (status, body) = h
        .post(&format!("/api/chat/{id}/messages"), &b_token, json!({"body": "hello"}))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body:?}");

    // And A can.
    let (status, read) = h.get(&format!("/api/chat/{id}"), &a_token).await;
    assert_eq!(status, StatusCode::OK, "{read:?}");
    assert_eq!(read["messages"].as_array().unwrap().len(), 1);
}

/// A cross-org conversation needs a transaction behind it.
///
/// Knowing an order id is not enough: a stranger who guessed or saw one would
/// otherwise get a channel into both organisations trading on it.
#[tokio::test]
async fn a_stranger_cannot_open_a_thread_on_someone_elses_order() {
    let h = require_db!();
    let buying = h.org("buyer-co").await;
    let selling = h.org("seller-co").await;
    let other = h.org("bystander-co").await;

    let (buyer, _) = h.user("user").await;
    h.join(buying, buyer, "operator").await;
    let (seller, seller_token) = h.user("user").await;
    h.join(selling, seller, "operator").await;
    let (stranger, stranger_token) = h.user("user").await;
    h.join(other, stranger, "admin").await;

    let (order, bid) = order_with_bid(&h, buyer, buying, Some(seller)).await;
    let bid = bid.unwrap();

    let (status, body) = h
        .post("/api/chat", &stranger_token, json!({ "kind": "deal", "bid_id": bid }))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");

    // The seller who bid on it may, and so is the buyer's counterpart.
    let (status, body) = h
        .post("/api/chat", &seller_token, json!({ "kind": "deal", "bid_id": bid, "body": "two pallets" }))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");

    let _ = (order, stranger);
}

/// Opening the chat for an order twice does not make two channels.
///
/// The realistic client behaviour is a button labelled "message the seller",
/// and a second channel would split the negotiation so each side could quote
/// from a different one.
#[tokio::test]
async fn opening_a_deal_thread_twice_returns_the_same_thread() {
    let h = require_db!();
    let buying = h.org("repeat-buyer").await;
    let selling = h.org("repeat-seller").await;
    let (buyer, buyer_token) = h.user("user").await;
    h.join(buying, buyer, "operator").await;
    let (seller, _) = h.user("user").await;
    h.join(selling, seller, "operator").await;

    let (_, bid) = order_with_bid(&h, buyer, buying, Some(seller)).await;
    let bid = bid.unwrap();

    let (s1, first) = h.post("/api/chat", &buyer_token, json!({"kind": "deal", "bid_id": bid})).await;
    let (s2, again) = h.post("/api/chat", &buyer_token, json!({"kind": "deal", "bid_id": bid})).await;

    assert_eq!(s1, StatusCode::CREATED);
    assert_eq!(s2, StatusCode::OK, "{again:?}");
    assert_eq!(first["id"], again["id"], "a second channel was opened");
    assert_eq!(again["existed"], true);
}

/// A direct conversation cannot be used to reach another organisation.
///
/// If it could, `direct` would be the unauthorised cross-org channel that
/// anchoring `deal` threads exists to avoid — and it would arrive with no
/// transaction to justify it.
#[tokio::test]
async fn a_direct_thread_cannot_cross_an_org_boundary() {
    let h = require_db!();
    let mine = h.org("inside").await;
    let theirs = h.org("outside").await;
    let (me, token) = h.user("user").await;
    h.join(mine, me, "admin").await;
    let (them, _) = h.user("user").await;
    h.join(theirs, them, "admin").await;

    let (status, body) = h
        .post("/api/chat", &token, json!({ "kind": "direct", "with": [them] }))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body:?}");
    assert!(
        body.as_str().unwrap_or_default().contains("order or bid"),
        "the refusal should point at the legitimate route: {body:?}"
    );
}

/// Unread counts are per-person and ignore your own messages.
#[tokio::test]
async fn unread_counts_the_other_persons_messages_only() {
    let h = require_db!();
    let org = h.org("watch").await;
    let (a, a_token) = h.user("user").await;
    let (b, b_token) = h.user("user").await;
    h.join(org, a, "admin").await;
    h.join(org, b, "operator").await;

    let (_, thread) = h
        .post("/api/chat", &a_token, json!({"kind": "direct", "with": [b], "body": "shift change at 0600"}))
        .await;
    let id = thread["id"].as_str().unwrap();

    // A wrote it, so A owes nothing; B has one to read.
    let (_, a_unread) = h.get("/api/chat/unread", &a_token).await;
    let (_, b_unread) = h.get("/api/chat/unread", &b_token).await;
    assert_eq!(a_unread["unread"], 0, "your own message counted against you");
    assert_eq!(b_unread["unread"], 1);

    // Reading clears it.
    h.post(&format!("/api/chat/{id}/read"), &b_token, json!({})).await;
    let (_, b_after) = h.get("/api/chat/unread", &b_token).await;
    assert_eq!(b_after["unread"], 0);
}

/// Changing your own password requires the current one.
///
/// The caller already holds a valid token, so this is the check that stops a
/// token lifted from a shared browser becoming a permanent takeover.
#[tokio::test]
async fn changing_your_password_requires_the_old_one() {
    let h = require_db!();
    let org = h.org("locker").await;
    let (admin, admin_token) = h.user("user").await;
    h.join(org, admin, "owner").await;

    let email = format!("{}@test.invalid", Uuid::new_v4());
    let (_, created) = h.post("/api/people", &admin_token, json!({"email": email})).await;
    let temp = created["temporaryPassword"].as_str().unwrap().to_string();

    let (_, login) = h
        .call("POST", "/api/local-auth/login", None, Some(json!({"email": email, "password": temp})))
        .await;
    let token = login["token"].as_str().unwrap().to_string();

    let (status, body) = h
        .post("/api/me/password", &token, json!({"current_password": "wrong-one-entirely", "new_password": "a-better-password"}))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body:?}");

    let (status, body) = h
        .post("/api/me/password", &token, json!({"current_password": temp, "new_password": "a-better-password"}))
        .await;
    assert_eq!(status, StatusCode::OK, "{body:?}");

    // And the must-change flag is cleared, so the UI stops insisting.
    let (_, relogin) = h
        .call("POST", "/api/local-auth/login", None, Some(json!({"email": email, "password": "a-better-password"})))
        .await;
    assert_eq!(relogin["mustChangePassword"], false);
}
