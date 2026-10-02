//! Conversations between people, and between the two sides of a trade.
//!
//! # The access rule
//!
//! You may read a thread and post to it if and only if there is a row for you
//! in `chat_participants`. That is the entire check, and it is deliberately
//! not derived from anything else.
//!
//! The tempting alternative — "anyone in the buying organisation may read the
//! buyer's threads" — is wrong in a way that only shows up later: org
//! membership changes, so someone who joins next month would inherit a
//! conversation about a shipment that closed last month, including whatever
//! was said about price. Membership of the *thread* is a fact with a date on
//! it. Membership of the org is not.
//!
//! # Why a cross-org thread needs an anchor
//!
//! Buyers and sellers have to be able to talk — about a substitution, a
//! delivery window, a damaged pallet. But the org-boundary model holds that
//! `marketplace` is the one scope an organisation may grant another, and
//! `inventory` is never grantable. A free-for-all messaging directory would
//! quietly undo that: hand every participant a searchable list of every other
//! organisation's staff and you have leaked the org chart, which is
//! competitive information on a marketplace where the same companies bid
//! against each other.
//!
//! So a `deal` thread is authorised by the transaction that justifies it — an
//! order, or a bid on an order — and the participants are derived from the two
//! sides of that transaction at the moment it is opened. No order, no channel.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
use crate::AppState;

type ApiError = (StatusCode, String);

/// Longest message accepted. Generous for a conversation, small enough that
/// the thread view stays a conversation rather than a document store — there
/// is a documents module for the other thing.
const MAX_BODY_CHARS: usize = 4000;

pub fn chat_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_threads).post(open_thread))
        .route("/unread", get(unread_count))
        .route("/:id", get(read_thread))
        .route("/:id/messages", post(post_message))
        .route("/:id/read", post(mark_read))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "chat route failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "request failed".to_string())
}

fn me_of(claims: &crate::routes::auth_middleware::Claims) -> Result<Uuid, ApiError> {
    Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "malformed subject".to_string()))
}

/// The access check, in one place.
///
/// Returns 404 rather than 403 for a thread the caller is not in. A 403 would
/// confirm the thread exists, which on a marketplace tells one bidder that
/// another conversation is happening about the order they are bidding on.
async fn participant_of(state: &AppState, thread: Uuid, me: Uuid) -> Result<(), ApiError> {
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM chat_participants WHERE thread_id = $1 AND user_id = $2",
    )
    .bind(thread)
    .bind(me)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;

    found
        .map(|_| ())
        .ok_or((StatusCode::NOT_FOUND, "no such conversation".to_string()))
}

/// GET /api/chat — the caller's conversations, most recent first.
async fn list_threads(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
) -> Result<Json<Value>, ApiError> {
    let me = me_of(&claims)?;

    let rows = sqlx::query(
        r#"
        SELECT t.id, t.kind, t.subject, t.order_id, t.bid_id, t.last_message_at,
               p.last_read_at,
               -- Counted here rather than in the client: the client would need
               -- every message to count them, which is the thing this view
               -- exists to avoid.
               (SELECT count(*) FROM chat_messages m
                 WHERE m.thread_id = t.id
                   AND (p.last_read_at IS NULL OR m.created_at > p.last_read_at)
                   AND (m.sender_id IS NULL OR m.sender_id <> $1)) AS unread,
               (SELECT m.body FROM chat_messages m
                 WHERE m.thread_id = t.id ORDER BY m.id DESC LIMIT 1) AS last_body,
               (SELECT COALESCE(u.full_name, u.username) FROM chat_messages m
                 LEFT JOIN users u ON u.id = m.sender_id
                 WHERE m.thread_id = t.id ORDER BY m.id DESC LIMIT 1) AS last_from,
               -- Everyone else in the room, so the list can say who you are
               -- talking to without a request per thread.
               (SELECT string_agg(DISTINCT COALESCE(o.name, 'unaffiliated'), ', ')
                  FROM chat_participants cp
                  LEFT JOIN organisations o ON o.id = cp.org_id
                 WHERE cp.thread_id = t.id) AS orgs,
               (SELECT count(*) FROM chat_participants cp WHERE cp.thread_id = t.id) AS people
        FROM chat_participants p
        JOIN chat_threads t ON t.id = p.thread_id
        WHERE p.user_id = $1
        ORDER BY t.last_message_at DESC
        LIMIT 200
        "#,
    )
    .bind(me)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let threads: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "kind": r.get::<String, _>("kind"),
                "subject": r.get::<Option<String>, _>("subject"),
                "orderId": r.get::<Option<Uuid>, _>("order_id"),
                "bidId": r.get::<Option<Uuid>, _>("bid_id"),
                "orgs": r.get::<Option<String>, _>("orgs"),
                "people": r.get::<Option<i64>, _>("people").unwrap_or(0),
                "unread": r.get::<Option<i64>, _>("unread").unwrap_or(0),
                "lastBody": r.get::<Option<String>, _>("last_body"),
                "lastFrom": r.get::<Option<String>, _>("last_from"),
                "lastMessageAt": r.get::<chrono::DateTime<chrono::Utc>, _>("last_message_at").to_rfc3339(),
            })
        })
        .collect();

    let unread: i64 = threads.iter().filter_map(|t| t["unread"].as_i64()).sum();
    Ok(Json(json!({ "count": threads.len(), "unread": unread, "threads": threads })))
}

/// GET /api/chat/unread — just the badge number.
///
/// Its own route because every page polls it for the nav, and making them all
/// fetch the full thread list to add up a number would multiply the most
/// frequent read in the application by the number of open tabs.
async fn unread_count(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
) -> Result<Json<Value>, ApiError> {
    let me = me_of(&claims)?;
    let n: i64 = sqlx::query_scalar(
        r#"
        SELECT count(*)
        FROM chat_participants p
        JOIN chat_messages m ON m.thread_id = p.thread_id
        WHERE p.user_id = $1
          AND (m.sender_id IS NULL OR m.sender_id <> $1)
          AND (p.last_read_at IS NULL OR m.created_at > p.last_read_at)
        "#,
    )
    .bind(me)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    Ok(Json(json!({ "unread": n })))
}

#[derive(Deserialize)]
struct NewThread {
    /// org | direct | deal
    kind: String,
    subject: Option<String>,
    /// For `direct`: who to talk to. Must be in the caller's organisation.
    with: Option<Vec<Uuid>>,
    /// For `deal`: exactly one of these anchors the conversation.
    order_id: Option<Uuid>,
    bid_id: Option<Uuid>,
    /// Optional opening message, so starting a conversation is one request.
    body: Option<String>,
}

/// POST /api/chat — open a conversation, or return the existing one.
///
/// Idempotent for `deal` and `org` threads: both are unique by their anchor,
/// and the realistic client behaviour is "open the chat for this order", which
/// must not create a second channel every time someone clicks it.
async fn open_thread(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Json(body): Json<NewThread>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let me = me_of(&claims)?;
    let my_org: Option<Uuid> = sqlx::query_scalar(
        "SELECT org_id FROM org_members WHERE user_id = $1 ORDER BY added_at LIMIT 1",
    )
    .bind(me)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?;

    let mut tx = state.db.begin().await.map_err(internal)?;

    // (thread id, whether it already existed, participants to ensure)
    let (thread_id, existed, participants): (Uuid, bool, Vec<(Uuid, Option<Uuid>)>) =
        match body.kind.as_str() {
            // ---- Internal: everyone currently in my organisation ----
            "org" => {
                let org = my_org.ok_or((
                    StatusCode::FORBIDDEN,
                    "you are not in an organisation".to_string(),
                ))?;

                let existing: Option<Uuid> = sqlx::query_scalar(
                    "SELECT id FROM chat_threads WHERE kind = 'org' AND org_id = $1 LIMIT 1",
                )
                .bind(org)
                .fetch_optional(&mut *tx)
                .await
                .map_err(internal)?;

                let members: Vec<Uuid> = sqlx::query_scalar(
                    "SELECT m.user_id FROM org_members m JOIN users u ON u.id = m.user_id \
                     WHERE m.org_id = $1 AND u.active",
                )
                .bind(org)
                .fetch_all(&mut *tx)
                .await
                .map_err(internal)?;

                let id = match existing {
                    Some(id) => id,
                    None => new_thread(&mut tx, "org", body.subject.as_deref(), Some(org), None, None, me).await?,
                };
                (id, existing.is_some(), members.into_iter().map(|u| (u, Some(org))).collect())
            }

            // ---- Named people, inside one organisation ----
            "direct" => {
                let org = my_org.ok_or((
                    StatusCode::FORBIDDEN,
                    "you are not in an organisation".to_string(),
                ))?;
                let with = body.with.clone().unwrap_or_default();
                if with.is_empty() {
                    return Err((StatusCode::BAD_REQUEST, "'with' must name at least one person".into()));
                }

                // Every named person must be a colleague. Otherwise `direct`
                // becomes the unauthorised cross-org channel that `deal`
                // exists to keep narrow.
                let valid: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM org_members WHERE org_id = $1 AND user_id = ANY($2)",
                )
                .bind(org)
                .bind(&with)
                .fetch_one(&mut *tx)
                .await
                .map_err(internal)?;

                if valid != with.len() as i64 {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "a direct conversation can only include people in your organisation; \
                         talk to another company through the order or bid it concerns"
                            .into(),
                    ));
                }

                let id = new_thread(&mut tx, "direct", body.subject.as_deref(), Some(org), None, None, me).await?;
                let mut people: Vec<(Uuid, Option<Uuid>)> = with.into_iter().map(|u| (u, Some(org))).collect();
                people.push((me, Some(org)));
                (id, false, people)
            }

            // ---- The two sides of a trade ----
            "deal" => {
                let (order_id, bid_id) = match (body.order_id, body.bid_id) {
                    (Some(o), None) => (Some(o), None),
                    (None, Some(b)) => (None, Some(b)),
                    _ => {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            "a deal conversation needs exactly one of order_id or bid_id".into(),
                        ))
                    }
                };

                let sides = deal_sides(&mut tx, order_id, bid_id).await?;

                // The caller has to be on one side of it. This is the check
                // that makes the anchor mean something: without it, anyone who
                // could name an order id could open a channel into both of the
                // organisations trading on it.
                if !sides.iter().any(|(u, _)| *u == me) {
                    return Err((
                        StatusCode::FORBIDDEN,
                        "you are not a party to this order".into(),
                    ));
                }

                let existing: Option<Uuid> = match (order_id, bid_id) {
                    (Some(o), _) => sqlx::query_scalar(
                        "SELECT id FROM chat_threads WHERE order_id = $1 AND bid_id IS NULL",
                    )
                    .bind(o)
                    .fetch_optional(&mut *tx)
                    .await,
                    (_, Some(b)) => sqlx::query_scalar("SELECT id FROM chat_threads WHERE bid_id = $1")
                        .bind(b)
                        .fetch_optional(&mut *tx)
                        .await,
                    _ => Ok(None),
                }
                .map_err(internal)?;

                let id = match existing {
                    Some(id) => id,
                    None => {
                        new_thread(
                            &mut tx,
                            "deal",
                            body.subject.as_deref(),
                            None,
                            order_id,
                            bid_id,
                            me,
                        )
                        .await?
                    }
                };
                (id, existing.is_some(), sides)
            }

            other => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("kind must be org, direct or deal (got '{other}')"),
                ))
            }
        };

    for (user, org) in &participants {
        sqlx::query(
            "INSERT INTO chat_participants (thread_id, user_id, org_id) VALUES ($1, $2, $3) \
             ON CONFLICT (thread_id, user_id) DO NOTHING",
        )
        .bind(thread_id)
        .bind(user)
        .bind(org)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
    }

    if let Some(text) = body.body.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        insert_message(&mut tx, thread_id, me, text).await?;
    }

    tx.commit().await.map_err(internal)?;

    Ok((
        if existed { StatusCode::OK } else { StatusCode::CREATED },
        Json(json!({ "id": thread_id, "kind": body.kind, "existed": existed })),
    ))
}

async fn new_thread(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    kind: &str,
    subject: Option<&str>,
    org: Option<Uuid>,
    order_id: Option<Uuid>,
    bid_id: Option<Uuid>,
    creator: Uuid,
) -> Result<Uuid, ApiError> {
    sqlx::query_scalar(
        "INSERT INTO chat_threads (kind, subject, org_id, order_id, bid_id, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(kind)
    .bind(subject.map(str::trim).filter(|s| !s.is_empty()))
    .bind(org)
    .bind(order_id)
    .bind(bid_id)
    .bind(creator)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal)
}

/// Who the two parties to a trade are.
///
/// For a bid: the person who placed it, and the person who raised the order it
/// is against. For an order with no bid named: the requester, plus whoever
/// placed the accepted bid if one has been. An order nobody has bid on has one
/// side, and a thread on it is an internal note until it has two.
async fn deal_sides(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    order_id: Option<Uuid>,
    bid_id: Option<Uuid>,
) -> Result<Vec<(Uuid, Option<Uuid>)>, ApiError> {
    let row = match (order_id, bid_id) {
        (Some(o), None) => sqlx::query(
            r#"
            SELECT o.requester_id, o.org_id, b.bidder_id
            FROM orders o
            LEFT JOIN bids b ON b.id = o.accepted_bid_id
            WHERE o.id = $1
            "#,
        )
        .bind(o)
        .fetch_optional(&mut **tx)
        .await,
        (None, Some(b)) => sqlx::query(
            r#"
            SELECT o.requester_id, o.org_id, b.bidder_id
            FROM bids b
            JOIN orders o ON o.id = b.order_id
            WHERE b.id = $1
            "#,
        )
        .bind(b)
        .fetch_optional(&mut **tx)
        .await,
        _ => Ok(None),
    }
    .map_err(internal)?;

    let row = row.ok_or((StatusCode::NOT_FOUND, "no such order or bid".to_string()))?;

    let requester: Uuid = row.get("requester_id");
    let buyer_org: Option<Uuid> = row.try_get("org_id").ok().flatten();
    let bidder: Option<Uuid> = row.try_get("bidder_id").ok().flatten();

    let mut sides = vec![(requester, buyer_org)];
    if let Some(seller) = bidder {
        if seller != requester {
            // The seller's org is resolved the same way every other read
            // resolves it, so a seller in several orgs is attributed
            // consistently with the rest of the application.
            let seller_org: Option<Uuid> = sqlx::query_scalar(
                "SELECT org_id FROM org_members WHERE user_id = $1 ORDER BY added_at LIMIT 1",
            )
            .bind(seller)
            .fetch_optional(&mut **tx)
            .await
            .map_err(internal)?;
            sides.push((seller, seller_org));
        }
    }
    Ok(sides)
}

#[derive(Deserialize)]
struct Paging {
    /// Return only messages after this id, so a polling client transfers the
    /// new ones rather than the whole thread every few seconds.
    after: Option<i64>,
    limit: Option<i64>,
}

/// GET /api/chat/:id — a thread and a page of its messages.
async fn read_thread(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Path(id): Path<Uuid>,
    Query(p): Query<Paging>,
) -> Result<Json<Value>, ApiError> {
    let me = me_of(&claims)?;
    participant_of(&state, id, me).await?;

    let head = sqlx::query(
        "SELECT kind, subject, order_id, bid_id, created_at FROM chat_threads WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await
    .map_err(internal)?;

    let people = sqlx::query(
        r#"
        SELECT COALESCE(u.full_name, u.username) AS name, u.id, o.name AS org
        FROM chat_participants p
        JOIN users u ON u.id = p.user_id
        LEFT JOIN organisations o ON o.id = p.org_id
        WHERE p.thread_id = $1
        ORDER BY p.added_at
        "#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let rows = sqlx::query(
        r#"
        SELECT m.id, m.body, m.created_at, m.sender_id, m.via_node_id,
               COALESCE(u.full_name, u.username) AS sender,
               o.name AS sender_org
        FROM chat_messages m
        LEFT JOIN users u ON u.id = m.sender_id
        LEFT JOIN chat_participants p ON p.thread_id = m.thread_id AND p.user_id = m.sender_id
        LEFT JOIN organisations o ON o.id = p.org_id
        WHERE m.thread_id = $1 AND m.id > $2
        ORDER BY m.id ASC
        LIMIT $3
        "#,
    )
    .bind(id)
    .bind(p.after.unwrap_or(0))
    .bind(p.limit.unwrap_or(200).clamp(1, 500))
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let messages: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<i64, _>("id"),
                "body": r.get::<String, _>("body"),
                "sender": r.get::<Option<String>, _>("sender"),
                "senderOrg": r.get::<Option<String>, _>("sender_org"),
                "mine": r.get::<Option<Uuid>, _>("sender_id") == Some(me),
                // Non-null means it crossed a disconnected link to get here,
                // which is worth showing: it explains a delay that would
                // otherwise look like someone ignoring you.
                "viaNodeId": r.get::<Option<Uuid>, _>("via_node_id"),
                "createdAt": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(json!({
        "id": id,
        "kind": head.get::<String, _>("kind"),
        "subject": head.get::<Option<String>, _>("subject"),
        "orderId": head.get::<Option<Uuid>, _>("order_id"),
        "bidId": head.get::<Option<Uuid>, _>("bid_id"),
        "participants": people.iter().map(|r| json!({
            "id": r.get::<Uuid, _>("id"),
            "name": r.get::<Option<String>, _>("name"),
            "org": r.get::<Option<String>, _>("org"),
        })).collect::<Vec<_>>(),
        "messages": messages,
    })))
}

#[derive(Deserialize)]
struct NewMessage {
    body: String,
}

/// POST /api/chat/:id/messages
async fn post_message(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Path(id): Path<Uuid>,
    Json(body): Json<NewMessage>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let me = me_of(&claims)?;
    participant_of(&state, id, me).await?;

    let text = body.body.trim();
    if text.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "a message cannot be empty".into()));
    }
    if text.chars().count() > MAX_BODY_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("a message is at most {MAX_BODY_CHARS} characters; attach a document instead"),
        ));
    }

    let mut tx = state.db.begin().await.map_err(internal)?;
    let msg_id = insert_message(&mut tx, id, me, text).await?;

    // Posting is also reading: otherwise your own message counts against you
    // the moment the next poll runs.
    sqlx::query(
        "UPDATE chat_participants SET last_read_at = NOW() WHERE thread_id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(me)
    .execute(&mut *tx)
    .await
    .map_err(internal)?;

    tx.commit().await.map_err(internal)?;

    Ok((StatusCode::CREATED, Json(json!({ "id": msg_id }))))
}

async fn insert_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    thread: Uuid,
    sender: Uuid,
    body: &str,
) -> Result<i64, ApiError> {
    sqlx::query_scalar(
        "INSERT INTO chat_messages (thread_id, sender_id, body) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(thread)
    .bind(sender)
    .bind(body)
    .fetch_one(&mut **tx)
    .await
    .map_err(internal)
}

/// POST /api/chat/:id/read — clear the unread badge for this thread.
async fn mark_read(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let me = me_of(&claims)?;
    participant_of(&state, id, me).await?;

    sqlx::query(
        "UPDATE chat_participants SET last_read_at = NOW() WHERE thread_id = $1 AND user_id = $2",
    )
    .bind(id)
    .bind(me)
    .execute(&state.db)
    .await
    .map_err(internal)?;

    Ok(Json(json!({ "read": true })))
}
