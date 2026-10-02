//! Replication: a dark site still counts, and says so.
//!
//! The property under test is easy to state and easy to get subtly wrong. A
//! peer that cannot be reached should contribute its last reported stock
//! rather than nothing — but the result must never read as though that figure
//! were current, must stop counting once it is too old to mean anything, and
//! must never leak across an organisation boundary just because it is cached
//! locally.
//!
//! Peers are made dark by registering them on a port nothing listens on, which
//! fails immediately rather than after a timeout.

mod common;

use axum::http::StatusCode;
use chrono::{Duration, Utc};
use serde_json::Value;
use uuid::Uuid;

use tid_wayfarer::routes::rollup::{OutpostSummary, SummaryItem};
use tid_wayfarer::services::replication::store_snapshot;

/// A registered peer nothing can connect to.
async fn dark_peer(h: &common::Harness, name: &str) -> Uuid {
    let node_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO node_registry (node_id, name, api_endpoint, public_key, status, last_seen) \
         VALUES ($1, $2, $3, $4, 'dark', NOW() - INTERVAL '2 days')",
    )
    .bind(node_id)
    .bind(format!("{name}-{}", &node_id.to_string()[..8]))
    // Port 1: connection refused at once, so a dark peer costs no timeout.
    .bind(format!("http://127.0.0.1:1/{node_id}/api"))
    .bind(format!("key-{node_id}"))
    .execute(&h.db)
    .await
    .expect("could not register dark peer");
    node_id
}

fn summary(node: Uuid, name: &str, age: Duration, items: Vec<(&str, i64, i64)>) -> OutpostSummary {
    summary_in(node, name, age, "L", items)
}

/// As above with an explicit unit. `merge` keys on (name, unit), so a snapshot
/// only lands on the same line as a local row when the units agree — which is
/// correct, and is a thing to get right in a test rather than to discover as a
/// silent split.
fn summary_in(
    node: Uuid,
    name: &str,
    age: Duration,
    unit: &str,
    items: Vec<(&str, i64, i64)>,
) -> OutpostSummary {
    OutpostSummary {
        node_id: node.to_string(),
        outpost_name: name.to_string(),
        body_id: 499,
        region: "Jezero".into(),
        // The peer's own clock. Everything about staleness is measured from
        // here, not from when the snapshot was written locally.
        generated_at: Utc::now() - age,
        items: items
            .into_iter()
            .map(|(item, qty, threshold)| SummaryItem {
                name: item.to_string(),
                unit: unit.to_string(),
                category: "consumable".into(),
                location: Some("tank-1".into()),
                quantity: qty,
                threshold,
                below_threshold: qty <= threshold,
            })
            .collect(),
    }
}

/// Enrol this outpost in an organisation, as a node certificate would.
///
/// Required for any test that expects peers to be asked at all: peers answer on
/// behalf of the org named by this outpost's certificate, so the rollup only
/// fans out when the caller is in that same org.
async fn enrol_outpost(h: &common::Harness, org: Uuid) {
    sqlx::query(
        "INSERT INTO org_outposts (org_id, node_id, node_public_key, certificate, issued_at) \
         VALUES ($1, $2::uuid, 'test-key', '{}'::jsonb, NOW()) ON CONFLICT DO NOTHING",
    )
    .bind(org)
    .bind(&h.node_id)
    .execute(&h.db)
    .await
    .expect("could not enrol this outpost");
}

/// An admin in an org that this outpost also belongs to — the ordinary case,
/// and the only one in which the rollup asks peers.
async fn admin_in_org(h: &common::Harness, org_name: &str) -> (Uuid, String) {
    let (org, token) = admin_in_unenrolled_org(h, org_name).await;
    enrol_outpost(h, org).await;
    (org, token)
}

/// An admin in an org without enrolling the outpost into it.
async fn admin_in_unenrolled_org(h: &common::Harness, org_name: &str) -> (Uuid, String) {
    let org = h.org(org_name).await;
    let (user, token) = h.user("admin").await;
    h.join(org, user, "owner").await;
    (org, token)
}

fn item<'a>(r: &'a Value, name: &str) -> Option<&'a Value> {
    r["items"].as_array()?.iter().find(|i| i["name"] == name)
}

fn source<'a>(r: &'a Value, node: Uuid) -> Option<&'a Value> {
    r["sources"].as_array()?.iter().find(|s| s["node_id"] == node.to_string())
}

/// The whole point: a site out of contact contributes its last known stock.
///
/// Before this it contributed zero, so a farm office on a weekly uplink was
/// invisible six days out of seven and the fabric-wide total quietly meant
/// "what we have at the places answering the phone".
#[tokio::test]
async fn a_dark_site_still_counts() {
    let h = require_db!();
    let (org, token) = admin_in_org(&h, "jezero-co").await;
    let peer = dark_peer(&h, "Jezero-Farm").await;

    store_snapshot(
        &h.db,
        Some(org),
        &summary(peer, "Jezero-Farm", Duration::days(3), vec![("hydrazine", 212, 50)]),
    )
    .await
    .expect("could not store snapshot");

    let (status, r) = h.get("/api/rollup/inventory", &token).await;
    assert_eq!(status, StatusCode::OK, "{r:?}");

    let src = source(&r, peer).expect("dark peer missing from sources");
    assert_eq!(src["status"], "stale", "{src:?}");
    assert!(
        (src["age_seconds"].as_i64().unwrap() - 3 * 86_400).abs() < 60,
        "age should be the data's age, ~3 days: {src:?}"
    );

    let it = item(&r, "hydrazine").expect("the dark site's stock did not count");
    assert_eq!(it["total_quantity"], 212);
    // And it is unmistakably not a live figure.
    assert_eq!(it["stale_quantity"], 212, "the total must say how much is stale");
    assert_eq!(it["complete"], false);

    // Fabric-wide counts are deliberately not asserted here: these tests
    // share one database and node_registry accumulates every peer every test
    // registers, so `sources_missing` is not this test's to predict. That a
    // snapshot leaves the total uncertain-but-not-a-floor is asserted directly
    // on `merge` in rollup.rs, where it is pure logic.
    assert!(r["sources_stale"].as_i64().unwrap() >= 1, "{r:?}");
}

/// A site that has never reported leaves the total a lower bound.
///
/// This is the case `complete: false` always meant, and it still has to be
/// distinguishable from the snapshot case above — otherwise replication would
/// have blurred "we might have more somewhere" into "these numbers are a bit
/// old", which are not the same warning.
#[tokio::test]
async fn a_dark_site_with_no_snapshot_makes_the_total_a_floor() {
    let h = require_db!();
    let (_org, token) = admin_in_org(&h, "floor-co").await;
    let peer = dark_peer(&h, "Never-Heard-From").await;

    let (_, r) = h.get("/api/rollup/inventory", &token).await;

    let src = source(&r, peer).expect("peer missing from sources");
    assert_eq!(src["status"], "unreachable", "{src:?}");
    assert_eq!(r["floor"], true);
    assert_eq!(r["complete"], false);
    assert!(r["sources_missing"].as_i64().unwrap() >= 1);
}

/// A snapshot past the horizon is listed but not summed.
///
/// A figure from six months ago is worse than no figure, because somebody will
/// act on it. Keeping the site visible with its age attached is useful;
/// adding it into a total is not.
#[tokio::test]
async fn an_ancient_snapshot_is_listed_but_not_counted() {
    let h = require_db!();
    let (org, token) = admin_in_org(&h, "ancient-co").await;
    let peer = dark_peer(&h, "Long-Gone").await;

    store_snapshot(
        &h.db,
        Some(org),
        &summary(peer, "Long-Gone", Duration::days(400), vec![("ancient-water", 9999, 10)]),
    )
    .await
    .unwrap();

    let (_, r) = h.get("/api/rollup/inventory", &token).await;

    let src = source(&r, peer).expect("expired peer should still be listed");
    assert_eq!(src["status"], "expired", "{src:?}");
    assert!(
        src["error"].as_str().unwrap_or_default().contains("too old"),
        "the reason should say why it did not count: {src:?}"
    );

    assert!(item(&r, "ancient-water").is_none(), "a 400-day-old figure was summed");
    assert_eq!(r["floor"], true, "an uncounted site makes the total a floor");
}

/// `?fresh_only=true` gives a number nobody has to caveat.
///
/// Most of the time "roughly what is out there" is the question. Anything that
/// commits — drafting a reorder, promising a delivery — wants only what is
/// confirmed right now.
#[tokio::test]
async fn fresh_only_excludes_snapshots() {
    let h = require_db!();
    let (org, token) = admin_in_org(&h, "freshonly-co").await;
    let peer = dark_peer(&h, "Weekly-Uplink").await;

    store_snapshot(
        &h.db,
        Some(org),
        &summary(peer, "Weekly-Uplink", Duration::hours(6), vec![("argon", 400, 20)]),
    )
    .await
    .unwrap();

    let (_, with_replicas) = h.get("/api/rollup/inventory", &token).await;
    assert_eq!(item(&with_replicas, "argon").unwrap()["total_quantity"], 400);

    let (_, fresh) = h.get("/api/rollup/inventory?fresh_only=true", &token).await;
    assert!(item(&fresh, "argon").is_none(), "fresh_only still counted a snapshot");
    assert_eq!(source(&fresh, peer).unwrap()["status"], "unreachable");
    assert_eq!(fresh["floor"], true);
}

/// A cached snapshot must not cross an organisation boundary.
///
/// This is the risk replication introduces that the live rollup did not have:
/// `/rollup/local` is org-scoped at the *peer*, so a snapshot is one
/// organisation's holdings sitting in a local table. Reading it without the
/// org filter would make this the one place where one company's stock could be
/// served to another — and it would look like a caching detail, not a leak.
#[tokio::test]
async fn a_snapshot_is_scoped_to_the_organisation_it_came_from() {
    let h = require_db!();
    // The outpost belongs to "theirs", so their rollup genuinely does fan out
    // to peers and genuinely does consult snapshots. The only thing standing
    // between them and our cached figures is the org filter on the snapshot
    // read — which is exactly what this test is for.
    let (mine, _my_token) = admin_in_unenrolled_org(&h, "ours").await;
    let (_theirs, their_token) = admin_in_org(&h, "theirs").await;
    let peer = dark_peer(&h, "Shared-Depot").await;

    // A snapshot of *our* holdings at that depot.
    store_snapshot(
        &h.db,
        Some(mine),
        &summary(peer, "Shared-Depot", Duration::hours(2), vec![("our-secret-reagent", 77, 1)]),
    )
    .await
    .unwrap();

    // The other organisation asks the same question of the same fabric.
    let (status, theirs) = h.get("/api/rollup/inventory", &their_token).await;
    assert_eq!(status, StatusCode::OK, "{theirs:?}");

    assert!(
        item(&theirs, "our-secret-reagent").is_none(),
        "another organisation's cached holdings were served: {theirs:?}"
    );
    // They see the site as dark with nothing usable, which is correct: there
    // is no snapshot *for them*.
    assert_eq!(source(&theirs, peer).unwrap()["status"], "unreachable");
}

/// Re-storing a snapshot replaces it rather than merging into it.
///
/// An upsert per row would leave the last-known quantity of something the peer
/// has since consumed entirely — the worst kind of stale, because it keeps
/// being re-confirmed by fetches that never mention it.
#[tokio::test]
async fn a_replaced_snapshot_drops_what_the_peer_no_longer_holds() {
    let h = require_db!();
    let (org, token) = admin_in_org(&h, "replace-co").await;
    let peer = dark_peer(&h, "Changing-Depot").await;

    store_snapshot(
        &h.db,
        Some(org),
        &summary(peer, "Changing-Depot", Duration::hours(1),
                 vec![("kept-item", 10, 1), ("consumed-item", 500, 1)]),
    )
    .await
    .unwrap();

    let (_, before) = h.get("/api/rollup/inventory", &token).await;
    assert!(item(&before, "consumed-item").is_some(), "setup failed");

    // The peer reports again, and no longer holds the second item at all.
    store_snapshot(
        &h.db,
        Some(org),
        &summary(peer, "Changing-Depot", Duration::minutes(1), vec![("kept-item", 12, 1)]),
    )
    .await
    .unwrap();

    let (_, after) = h.get("/api/rollup/inventory", &token).await;
    assert!(
        item(&after, "consumed-item").is_none(),
        "a consumed item survived a snapshot replacement: {after:?}"
    );
    assert_eq!(item(&after, "kept-item").unwrap()["total_quantity"], 12);
}

/// A live answer is never reported as stale, even alongside one that is.
#[tokio::test]
async fn the_local_outpost_is_always_live() {
    let h = require_db!();
    let (org, token) = admin_in_org(&h, "mixed-co").await;
    let (owner, _) = h.user("user").await;
    h.join(org, owner, "operator").await;
    h.inventory(org, owner, "local-stock", 42).await;

    let peer = dark_peer(&h, "Dark-Annex").await;
    store_snapshot(
        &h.db,
        Some(org),
        // 'each' to match what Harness::inventory writes, so the two land on
        // one line instead of two.
        &summary_in(peer, "Dark-Annex", Duration::days(1), "each", vec![("local-stock", 8, 1)]),
    )
    .await
    .unwrap();

    let (_, r) = h.get("/api/rollup/inventory", &token).await;

    let me = source(&r, Uuid::parse_str(&h.node_id).unwrap()).expect("this outpost is missing");
    assert_eq!(me["status"], "local");
    assert_eq!(me["age_seconds"], 0);

    // 42 live here plus 8 as last reported there — and the split is visible,
    // which is the difference between a number and a number you can act on.
    let it = item(&r, "local-stock").expect("merged item missing");
    assert_eq!(it["total_quantity"], 50);
    assert_eq!(it["stale_quantity"], 8);
    assert!(
        (it["oldest_contribution_seconds"].as_i64().unwrap() - 86_400).abs() < 60,
        "{it:?}"
    );
}

/// A caller outside this outpost's own organisation does not get peer figures.
///
/// Found while wiring replication, and it predates it. The peer fetch is signed
/// with the *node* identity, so a peer resolves the asker's org from the node
/// certificate and returns that organisation's holdings. Attributing that
/// answer to whichever admin triggered the rollup showed one organisation what
/// a peer had released to another — and a snapshot would then have cached it
/// under the wrong org and kept serving it after the link went down.
///
/// A caller from another org now gets a local-only rollup, and is told so
/// rather than being shown a fabric that appears to have one site in it.
#[tokio::test]
async fn a_caller_outside_this_outposts_org_gets_no_peer_figures() {
    let h = require_db!();

    // This outpost belongs to one organisation...
    let outpost_org = h.org("the-outpost-org").await;
    enrol_outpost(&h, outpost_org).await;

    // ...and the caller belongs to another.
    let (_other, their_token) = admin_in_unenrolled_org(&h, "a-different-org").await;
    let peer = dark_peer(&h, "Not-Theirs").await;
    store_snapshot(
        &h.db,
        Some(outpost_org),
        &summary(peer, "Not-Theirs", Duration::hours(1), vec![("not-your-stock", 500, 1)]),
    )
    .await
    .unwrap();

    let (status, r) = h.get("/api/rollup/inventory", &their_token).await;
    assert_eq!(status, StatusCode::OK, "{r:?}");

    assert!(
        item(&r, "not-your-stock").is_none(),
        "a peer's figures leaked to another organisation: {r:?}"
    );
    assert!(source(&r, peer).is_none(), "the peer should not even be listed");
    assert!(
        r["scope_note"].as_str().unwrap_or_default().contains("this outpost only"),
        "a narrowed rollup must say so: {r:?}"
    );
}
