//! Cross-location resource rollup — "what do we have, everywhere".
//!
//! Each outpost owns its own database, and nothing replicates between them
//! (`sync_daemon` gossips only the node registry). So the single question the
//! logistics product exists to answer — what resources are held across every
//! location — has no single place to be asked.
//!
//! This is the *read-only* answer: the coordinating outpost fans out signed
//! GETs to every peer's `/api/rollup/local`, merges what comes back, and
//! reports totals. It needs no schema change and no replication layer.
//!
//! What it deliberately does **not** do:
//!
//! - It does not pretend to be complete. A peer that is dark contributes
//!   nothing, and both the response and every affected line item say so
//!   (`complete: false`). A total you cannot trust has to announce itself —
//!   the alternative is an operator ordering against a number that silently
//!   omitted three sites.
//! - It does not return only a sum. 500 L spread over six locations is not
//!   500 L you can use anywhere, so every item carries its per-location
//!   breakdown alongside the total.
//! - It does not cache. Freshness here is the product; a stale rollup served
//!   fast would defeat the point. Cost is one concurrent round of peer GETs.
//!
//! Replacing this with real replication changes only where `gather` reads
//! from; the shape it returns is what a replicated view would return too.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::{extract::State, routing::get, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::task::JoinSet;

use crate::routes::auth_middleware::{AdminUser, AuthenticatedUser};
use crate::services::fabric_auth;
use crate::AppState;

/// Per-peer deadline. Generous enough for a satellite hop, short enough that
/// one dark outpost cannot hold the whole rollup open. `ROLLUP_TIMEOUT_SECS`
/// overrides for high-latency fabrics.
fn peer_timeout() -> Duration {
    let secs = std::env::var("ROLLUP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(6)
        .clamp(1, 120);
    Duration::from_secs(secs)
}

pub fn rollup_routes() -> Router<AppState> {
    Router::new()
        .route("/local", get(local_summary))
        .route("/inventory", get(inventory_rollup))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// One outpost's own stock, as a peer reports it.
#[derive(Serialize, Deserialize)]
pub struct OutpostSummary {
    pub node_id: String,
    pub outpost_name: String,
    pub body_id: i64,
    pub region: String,
    /// When this outpost produced the summary — the basis for its age.
    pub generated_at: DateTime<Utc>,
    pub items: Vec<SummaryItem>,
}

/// Stock of one thing at one place, already collapsed across inventory rows.
#[derive(Serialize, Deserialize, Clone)]
pub struct SummaryItem {
    pub name: String,
    pub unit: String,
    pub category: String,
    pub location: Option<String>,
    pub quantity: i64,
    /// Reorder point. Summed with the rows it was collapsed from, so it stays
    /// comparable to `quantity`.
    pub threshold: i64,
    pub below_threshold: bool,
}

/// Whether a source contributed, and how much to trust it if it did.
#[derive(Serialize)]
pub struct SourceStatus {
    pub node_id: String,
    pub name: String,
    /// `local` | `ok` | `unreachable`
    pub status: &'static str,
    /// Age of the data itself, from the peer's own `generated_at`.
    pub age_seconds: Option<i64>,
    /// Last contact per the node registry — the answer to "how dark is it?"
    /// when the fetch failed.
    pub last_seen: Option<DateTime<Utc>>,
    pub item_count: usize,
    pub error: Option<String>,
}

/// Stock of one thing at one place, tagged with which outpost reported it.
#[derive(Serialize)]
pub struct LocationQuantity {
    pub node_id: String,
    pub outpost_name: String,
    pub location: Option<String>,
    pub quantity: i64,
    /// This location's reorder point.
    pub threshold: i64,
    /// How much would bring this location back to its reorder point. This is
    /// the number a restock order should ask for — not the fabric-wide gap,
    /// because stock at another outpost is not stock you can use here.
    pub shortfall: i64,
    pub below_threshold: bool,
}

#[derive(Serialize)]
pub struct RolledItem {
    pub name: String,
    pub unit: String,
    pub category: String,
    /// Sum over the sources that answered — see `complete`.
    pub total_quantity: i64,
    /// False when any source was unreachable: this item may be held there too,
    /// so the total is a floor rather than a count.
    pub complete: bool,
    /// True if any reporting location is at or under its reorder point, even
    /// when the fabric-wide total looks healthy. Stock at the wrong location
    /// is not available stock.
    pub below_threshold_somewhere: bool,
    /// Sum of every location's own shortfall — what it would take to bring
    /// *every* site back to its reorder point, which is the only number that
    /// makes sense to reorder against.
    pub shortfall_total: i64,
    pub by_location: Vec<LocationQuantity>,
}

#[derive(Serialize)]
pub struct InventoryRollup {
    pub generated_at: DateTime<Utc>,
    /// False if any known peer failed to answer. Every total below is then a
    /// lower bound.
    pub complete: bool,
    pub sources_total: usize,
    pub sources_answered: usize,
    pub sources: Vec<SourceStatus>,
    pub distinct_items: usize,
    pub items_below_threshold: usize,
    pub items: Vec<RolledItem>,
}

// ---------------------------------------------------------------------------
// GET /api/rollup/local — this outpost's own stock
// ---------------------------------------------------------------------------

/// Peer-facing. Reachable by any fabric member (a signing peer or a user JWT),
/// because a rollup is exactly a peer asking this of everyone.
///
/// Unlike `/api/inventory`, this is **not** scoped to one user: it reports what
/// the outpost holds. User identity is per-outpost — the same person has a
/// different `users.id` at every site — so per-user scoping across the fabric
/// is not yet meaningful. That is why the aggregate endpoint is admin-gated.
async fn local_summary(
    State(state): State<AppState>,
    _user: Option<AuthenticatedUser>,
) -> Result<Json<OutpostSummary>, (axum::http::StatusCode, String)> {
    let items = local_items(&state).await.map_err(|e| {
        tracing::error!(error = %e, "local inventory summary failed");
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "inventory summary failed".to_string(),
        )
    })?;

    Ok(Json(OutpostSummary {
        node_id: state.identity.node_id.clone(),
        outpost_name: env_or("OUTPOST_NAME", "tid-wayfarer"),
        body_id: env_or("BODY_ID", "399").parse().unwrap_or(399),
        region: env_or("OUTPOST_REGION", ""),
        generated_at: Utc::now(),
        items,
    }))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Collapse this outpost's inventory rows into one line per
/// (name, unit, category, location). Several rows for the same thing at the
/// same place — different owners, separate deliveries — are one stock figure
/// for logistics purposes.
async fn local_items(state: &AppState) -> Result<Vec<SummaryItem>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT name,
               unit,
               category,
               location,
               SUM(quantity)::int8  AS quantity,
               SUM(threshold)::int8 AS threshold
        FROM inventory
        GROUP BY name, unit, category, location
        ORDER BY name, location
        "#,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .iter()
        .map(|r| {
            let quantity: i64 = r.get("quantity");
            let threshold: i64 = r.get("threshold");
            SummaryItem {
                name: r.get("name"),
                unit: r.get("unit"),
                category: r.get("category"),
                location: r.get("location"),
                quantity,
                threshold,
                below_threshold: quantity <= threshold,
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// GET /api/rollup/inventory — the aggregate
// ---------------------------------------------------------------------------

/// Admin-gated: this is a fabric-wide operational view, and inventory is not
/// federated per user (see [`local_summary`]).
async fn inventory_rollup(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<InventoryRollup>, (axum::http::StatusCode, String)> {
    let (sources, summaries) = gather(&state).await;
    Ok(Json(merge(sources, summaries)))
}

/// A peer worth asking: registered, reachable, not revoked, and not us.
struct Peer {
    node_id: String,
    /// Registry name. A peer that never answers still has to be nameable in
    /// the source list — "Jezero-Farm is dark" is actionable, an endpoint URL
    /// is not.
    name: String,
    endpoint: String,
    last_seen: Option<DateTime<Utc>>,
}

/// Fetch every source's summary concurrently. Returns per-source status
/// alongside whatever summaries arrived, so the caller can report both what it
/// learned and what it could not reach.
async fn gather(state: &AppState) -> (Vec<SourceStatus>, Vec<OutpostSummary>) {
    let mut sources = Vec::new();
    let mut summaries = Vec::new();

    // ---- this outpost, read straight from the DB ----
    match local_items(state).await {
        Ok(items) => {
            sources.push(SourceStatus {
                node_id: state.identity.node_id.clone(),
                name: env_or("OUTPOST_NAME", "tid-wayfarer"),
                status: "local",
                age_seconds: Some(0),
                last_seen: Some(Utc::now()),
                item_count: items.len(),
                error: None,
            });
            summaries.push(OutpostSummary {
                node_id: state.identity.node_id.clone(),
                outpost_name: env_or("OUTPOST_NAME", "tid-wayfarer"),
                body_id: env_or("BODY_ID", "399").parse().unwrap_or(399),
                region: env_or("OUTPOST_REGION", ""),
                generated_at: Utc::now(),
                items,
            });
        }
        Err(e) => sources.push(SourceStatus {
            node_id: state.identity.node_id.clone(),
            name: env_or("OUTPOST_NAME", "tid-wayfarer"),
            status: "unreachable",
            age_seconds: None,
            last_seen: None,
            item_count: 0,
            error: Some(format!("local query failed: {e}")),
        }),
    }

    // ---- peers ----
    let peers = match sqlx::query(
        r#"
        SELECT node_id::text AS node_id, name, api_endpoint, last_seen
        FROM node_registry
        WHERE COALESCE(revoked, false) = false
          AND api_endpoint IS NOT NULL
          AND api_endpoint <> ''
          AND node_id::text <> $1
        "#,
    )
    .bind(&state.identity.node_id)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows
            .iter()
            .map(|r| Peer {
                node_id: r.get("node_id"),
                name: r.get("name"),
                endpoint: r.get("api_endpoint"),
                last_seen: r.try_get("last_seen").ok(),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            tracing::error!(error = %e, "rollup could not list peers");
            Vec::new()
        }
    };

    if peers.is_empty() {
        return (sources, summaries);
    }

    let http = reqwest::Client::new();
    let legacy_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();
    let timeout = peer_timeout();

    let mut tasks: JoinSet<(Peer, Result<OutpostSummary, String>)> = JoinSet::new();
    for peer in peers {
        let http = http.clone();
        let identity = state.identity.clone();
        let token = legacy_token.clone();
        tasks.spawn(async move {
            let url = format!("{}/rollup/local", peer.endpoint.trim_end_matches('/'));
            let result = fetch_peer(&http, &identity, &url, &token, timeout).await;
            (peer, result)
        });
    }

    while let Some(joined) = tasks.join_next().await {
        let Ok((peer, result)) = joined else { continue };
        match result {
            Ok(summary) => {
                let age = (Utc::now() - summary.generated_at).num_seconds().max(0);
                sources.push(SourceStatus {
                    node_id: peer.node_id,
                    name: summary.outpost_name.clone(),
                    status: "ok",
                    age_seconds: Some(age),
                    last_seen: peer.last_seen,
                    item_count: summary.items.len(),
                    error: None,
                });
                summaries.push(summary);
            }
            Err(e) => {
                tracing::warn!(node_id = %peer.node_id, error = %e, "rollup peer unreachable");
                sources.push(SourceStatus {
                    node_id: peer.node_id,
                    name: peer.name,
                    status: "unreachable",
                    age_seconds: None,
                    last_seen: peer.last_seen,
                    item_count: 0,
                    error: Some(e),
                });
            }
        }
    }

    (sources, summaries)
}

async fn fetch_peer(
    http: &reqwest::Client,
    identity: &crate::services::identity::NodeIdentity,
    url: &str,
    legacy_token: &str,
    timeout: Duration,
) -> Result<OutpostSummary, String> {
    let resp = fabric_auth::signed_get(http, identity, url, legacy_token)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| if e.is_timeout() { "timed out".to_string() } else { e.to_string() })?;

    if !resp.status().is_success() {
        return Err(format!("http {}", resp.status()));
    }
    resp.json::<OutpostSummary>()
        .await
        .map_err(|e| format!("bad response: {e}"))
}

/// Merge summaries into one list, keyed case-insensitively on (name, unit) so
/// "Water"/"water" are one line. Category is taken from the first reporter.
fn merge(sources: Vec<SourceStatus>, summaries: Vec<OutpostSummary>) -> InventoryRollup {
    let answered = sources.iter().filter(|s| s.status != "unreachable").count();
    let complete = answered == sources.len();

    let mut grouped: BTreeMap<(String, String), RolledItem> = BTreeMap::new();

    for summary in &summaries {
        for item in &summary.items {
            let key = (item.name.to_lowercase(), item.unit.to_lowercase());
            let entry = grouped.entry(key).or_insert_with(|| RolledItem {
                name: item.name.clone(),
                unit: item.unit.clone(),
                category: item.category.clone(),
                total_quantity: 0,
                // Inherited from the fabric-wide result: if anything is dark,
                // no total can claim to be a full count.
                complete,
                below_threshold_somewhere: false,
                shortfall_total: 0,
                by_location: Vec::new(),
            });

            let shortfall = (item.threshold - item.quantity).max(0);

            entry.total_quantity += item.quantity;
            entry.below_threshold_somewhere |= item.below_threshold;
            entry.shortfall_total += shortfall;
            entry.by_location.push(LocationQuantity {
                node_id: summary.node_id.clone(),
                outpost_name: summary.outpost_name.clone(),
                location: item.location.clone(),
                quantity: item.quantity,
                threshold: item.threshold,
                shortfall,
                below_threshold: item.below_threshold,
            });
        }
    }

    let mut items: Vec<RolledItem> = grouped.into_values().collect();
    // Things needing attention first, then the largest holdings.
    items.sort_by(|a, b| {
        b.below_threshold_somewhere
            .cmp(&a.below_threshold_somewhere)
            .then(b.total_quantity.cmp(&a.total_quantity))
            .then(a.name.cmp(&b.name))
    });

    let items_below_threshold = items.iter().filter(|i| i.below_threshold_somewhere).count();

    InventoryRollup {
        generated_at: Utc::now(),
        complete,
        sources_total: sources.len(),
        sources_answered: answered,
        sources,
        distinct_items: items.len(),
        items_below_threshold,
        items,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, unit: &str, qty: i64, threshold: i64, loc: &str) -> SummaryItem {
        SummaryItem {
            name: name.into(),
            unit: unit.into(),
            category: "general".into(),
            location: Some(loc.into()),
            quantity: qty,
            threshold,
            below_threshold: qty <= threshold,
        }
    }

    fn summary(node: &str, name: &str, items: Vec<SummaryItem>) -> OutpostSummary {
        OutpostSummary {
            node_id: node.into(),
            outpost_name: name.into(),
            body_id: 399,
            region: String::new(),
            generated_at: Utc::now(),
            items,
        }
    }

    fn source(node: &str, status: &'static str) -> SourceStatus {
        SourceStatus {
            node_id: node.into(),
            name: node.into(),
            status,
            age_seconds: Some(0),
            last_seen: None,
            item_count: 0,
            error: None,
        }
    }

    #[test]
    fn totals_sum_across_outposts_and_keep_the_breakdown() {
        let r = merge(
            vec![source("a", "local"), source("b", "ok")],
            vec![
                summary("a", "HQ", vec![item("Water", "L", 300, 50, "tank-1")]),
                summary("b", "Farm", vec![item("Water", "L", 200, 50, "tank-2")]),
            ],
        );

        assert_eq!(r.distinct_items, 1);
        assert_eq!(r.items[0].total_quantity, 500);
        // The breakdown is the point: 500 L is not 500 L usable in one place.
        assert_eq!(r.items[0].by_location.len(), 2);
        assert!(r.complete);
    }

    #[test]
    fn the_same_item_named_differently_still_merges() {
        let r = merge(
            vec![source("a", "local"), source("b", "ok")],
            vec![
                summary("a", "HQ", vec![item("Water", "L", 10, 0, "x")]),
                summary("b", "Farm", vec![item("water", "l", 5, 0, "y")]),
            ],
        );
        assert_eq!(r.distinct_items, 1);
        assert_eq!(r.items[0].total_quantity, 15);
    }

    #[test]
    fn a_dark_outpost_makes_every_total_incomplete() {
        // The whole reason this endpoint reports provenance: an operator must
        // not reorder against a total that quietly omitted a site.
        let r = merge(
            vec![source("a", "local"), source("b", "unreachable")],
            vec![summary("a", "HQ", vec![item("Seed", "kg", 40, 10, "silo")])],
        );

        assert!(!r.complete);
        assert!(!r.items[0].complete);
        assert_eq!(r.sources_answered, 1);
        assert_eq!(r.sources_total, 2);
    }

    #[test]
    fn healthy_total_still_flags_a_starved_location() {
        // 1000 kg fabric-wide, but the farm that needs it has 5 kg left.
        let r = merge(
            vec![source("a", "local"), source("b", "ok")],
            vec![
                summary("a", "HQ", vec![item("Seed", "kg", 995, 10, "silo")]),
                summary("b", "Farm", vec![item("Seed", "kg", 5, 10, "shed")]),
            ],
        );

        assert_eq!(r.items[0].total_quantity, 1000);
        assert!(r.items[0].below_threshold_somewhere);
        assert_eq!(r.items_below_threshold, 1);
    }

    #[test]
    fn shortfall_is_per_location_not_fabric_wide() {
        // The number a restock order is placed against. A site that is over its
        // reorder point contributes nothing — its surplus must not cancel out
        // another site's gap, because stock at HQ is not stock at the farm.
        let r = merge(
            vec![source("a", "local"), source("b", "ok")],
            vec![
                summary("a", "HQ", vec![item("Seed", "kg", 995, 10, "silo")]),
                summary("b", "Farm", vec![item("Seed", "kg", 5, 50, "shed")]),
            ],
        );

        assert_eq!(r.items[0].shortfall_total, 45, "only the farm is short");
        let farm = r.items[0].by_location.iter().find(|l| l.outpost_name == "Farm").unwrap();
        let hq = r.items[0].by_location.iter().find(|l| l.outpost_name == "HQ").unwrap();
        assert_eq!(farm.shortfall, 45);
        assert_eq!(farm.threshold, 50);
        assert_eq!(hq.shortfall, 0, "a site above its threshold is never negative");
    }

    #[test]
    fn a_fully_stocked_fabric_has_nothing_to_reorder() {
        let r = merge(
            vec![source("a", "local")],
            vec![summary("a", "HQ", vec![item("Bolts", "each", 900, 10, "bin")])],
        );
        assert_eq!(r.items[0].shortfall_total, 0);
        assert!(!r.items[0].below_threshold_somewhere);
    }

    #[test]
    fn items_needing_attention_sort_first() {
        let r = merge(
            vec![source("a", "local")],
            vec![summary(
                "a",
                "HQ",
                vec![
                    item("Bolts", "each", 9000, 10, "bin"),
                    item("Oxygen", "kg", 2, 10, "tank"),
                ],
            )],
        );
        assert_eq!(r.items[0].name, "Oxygen");
    }

    #[test]
    fn an_empty_fabric_is_not_an_error() {
        let r = merge(vec![source("a", "local")], vec![summary("a", "HQ", vec![])]);
        assert_eq!(r.distinct_items, 0);
        assert!(r.complete);
    }
}
