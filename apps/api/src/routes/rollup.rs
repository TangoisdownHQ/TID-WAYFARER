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
//! When a peer cannot be reached, its **last reported summary** is used
//! instead — see [`crate::services::replication`]. That was the one thing
//! missing: a live-only rollup silently redefined "what we have" as "what we
//! have at the places answering the phone", so a farm office on a weekly
//! uplink was invisible six days out of seven.
//!
//! What it deliberately does **not** do:
//!
//! - It does not pretend a snapshot is live. Three separate counts come back —
//!   `sources_live`, `sources_stale`, `sources_missing` — and every line item
//!   reports how much of its total came from a snapshot and how old the oldest
//!   contribution was. A total you cannot fully trust has to say which part.
//! - It does not blur the two ways a total can be wrong. If a site contributed
//!   nothing at all the total is a **floor** (`floor: true`). If a site
//!   contributed a snapshot, the total is neither a floor nor a ceiling — the
//!   stock may have moved either way since — which is a different thing to
//!   tell an operator and so is reported differently.
//! - It does not count a snapshot forever. Past `REPLICA_MAX_AGE_SECS` a
//!   snapshot is still listed but stops being added up: a figure from six
//!   months ago is worse than no figure, because someone will act on it.
//! - It does not return only a sum. 500 L spread over six locations is not
//!   500 L you can use anywhere, so every item carries its per-location
//!   breakdown alongside the total.
//! - It does not serve a cached *aggregate*. Live peers are always re-read;
//!   the snapshot is a fallback for a specific peer, not a shortcut for the
//!   whole request.
//!
//! A replica is never authoritative. It must not back a reservation, a custody
//! transfer or a settlement — only the question "roughly what is out there".

use std::collections::BTreeMap;
use std::time::Duration;

use axum::{extract::State, routing::get, Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::task::JoinSet;

use crate::routes::auth_middleware::{AdminUser, AuthenticatedUser, Caller};
use crate::services::org_scope;
use crate::services::fabric_auth;
use crate::services::replication;
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
    /// `local`  — this outpost, read from its own database
    /// `ok`     — answered live
    /// `stale`  — unreachable, but a recent enough snapshot was used
    /// `expired` — unreachable, snapshot too old to count; listed, not summed
    /// `unreachable` — unreachable and never heard from
    pub status: &'static str,
    /// Age of the data itself, from the peer's own `generated_at`. Present for
    /// a snapshot too, which is the point: it is the age of the stock figure,
    /// not of the row holding it.
    pub age_seconds: Option<i64>,
    /// Last contact per the node registry — the answer to "how dark is it?"
    /// when the fetch failed.
    pub last_seen: Option<DateTime<Utc>>,
    /// When this outpost last managed to ask. For a `stale` source this is how
    /// long the link has been down, which is a different question from how old
    /// the stock figure is.
    pub fetched_at: Option<DateTime<Utc>>,
    pub item_count: usize,
    pub error: Option<String>,
}

/// One source's contribution, with whether it came from a snapshot.
///
/// Carried separately from [`OutpostSummary`] because the summary is a wire
/// type shared with peers: a peer reporting its own stock has no business
/// describing it as stale, since from where it sits it is not.
struct Contribution {
    summary: OutpostSummary,
    stale: bool,
    age_seconds: i64,
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
    /// True when this line came from a snapshot rather than a live read. Shown
    /// per location rather than only per item, because "we have 400 units" and
    /// "we had 400 units here a week ago" are different claims and an operator
    /// deciding where to pull from needs to see which is which.
    pub stale: bool,
    /// Age of this line's figure, in seconds. Zero for a live read.
    pub age_seconds: i64,
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
    /// Sum over the sources that contributed — live or from a snapshot.
    pub total_quantity: i64,
    /// How much of `total_quantity` came from a snapshot rather than a live
    /// read. Zero means every contributor answered just now.
    pub stale_quantity: i64,
    /// Age of the oldest figure in this total, in seconds. The honest headline
    /// for "how current is this number".
    pub oldest_contribution_seconds: i64,
    /// True only when every source answered live. A snapshot contributing
    /// makes this false even though the total is no longer missing anything.
    pub complete: bool,
    /// True when a source contributed nothing at all, so this item may be held
    /// somewhere unaccounted for and the total is a lower bound. Distinct from
    /// `complete`: a total built partly from snapshots is uncertain in both
    /// directions, not merely low.
    pub floor: bool,
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
    /// True only when every known source answered live.
    pub complete: bool,
    /// True when at least one source contributed nothing — no live answer and
    /// no usable snapshot. Every total below is then a lower bound.
    pub floor: bool,
    pub sources_total: usize,
    /// Live + stale: sources that contributed a figure of any age.
    pub sources_answered: usize,
    /// Answered just now.
    pub sources_live: usize,
    /// Dark, but a snapshot inside the contributing horizon was used.
    pub sources_stale: usize,
    /// Dark with nothing usable — never heard from, or a snapshot too old to
    /// count. These are the sites missing from the totals.
    pub sources_missing: usize,
    /// Age of the oldest figure anywhere in this rollup. `None` when
    /// everything is live.
    pub oldest_contribution_seconds: Option<i64>,
    /// Horizon past which a snapshot stops being summed, so a client can
    /// explain why a listed source contributed nothing.
    pub replica_horizon_seconds: i64,
    /// Set when the rollup was narrowed for a reason the caller cannot see
    /// from the numbers — currently only "you are not in this outpost's
    /// organisation, so its peers were not asked". A silently local-only
    /// rollup would read as a fabric with one site in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_note: Option<String>,
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
    Caller(principal): Caller,
    _user: Option<AuthenticatedUser>,
) -> Result<Json<OutpostSummary>, (axum::http::StatusCode, String)> {
    // A peer asks on behalf of its own organisation; it is shown that org's
    // holdings here and nothing else. Holdings never cross an org boundary —
    // `inventory` is not a grantable scope, by design.
    let org = org_scope::caller_org(&state, &principal).await;
    let items = local_items(&state, org).await.map_err(|e| {
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
async fn local_items(
    state: &AppState,
    org: Option<uuid::Uuid>,
) -> Result<Vec<SummaryItem>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT name,
               unit,
               category,
               location,
               SUM(quantity)::int8  AS quantity,
               SUM(threshold)::int8 AS threshold
        FROM inventory
        WHERE org_id IS NOT DISTINCT FROM $1
        GROUP BY name, unit, category, location
        ORDER BY name, location
        "#,
    )
    .bind(org)
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
    Caller(principal): Caller,
    axum::extract::Query(q): axum::extract::Query<RollupQuery>,
    _admin: AdminUser,
) -> Result<Json<InventoryRollup>, (axum::http::StatusCode, String)> {
    let org = org_scope::caller_org(&state, &principal).await;

    // Peers may only be asked on behalf of *this outpost's own* organisation.
    //
    // This is not a nicety. The peer fetch is signed with the node identity,
    // so a peer resolves `caller_org` to the org its node certificate names —
    // this outpost's — and returns that org's holdings. Attributing the answer
    // to whichever user happened to trigger the rollup would show an admin in
    // one organisation what a peer released to another: a cross-org
    // disclosure, and one that a snapshot would then cache under the wrong
    // org and keep serving.
    //
    // So a caller from a different org gets a local-only rollup. Fewer
    // numbers, never another company's numbers.
    let own = org_scope::own_org(&state).await;
    let may_ask_peers = org.is_some() && org == own;

    let (sources, contributions) =
        gather(&state, org, q.fresh_only.unwrap_or(false), may_ask_peers).await;
    let mut rolled = merge(sources, contributions);

    if !may_ask_peers {
        rolled.scope_note = Some(
            "Showing this outpost only. Other outposts answer on behalf of the organisation \
             this outpost belongs to, which is not yours — so their figures are not this \
             rollup's to report."
                .to_string(),
        );
    }
    Ok(Json(rolled))
}

#[derive(Deserialize)]
pub struct RollupQuery {
    /// Exclude snapshots, giving a strict lower bound built only from sources
    /// answering right now.
    ///
    /// Worth having as an explicit option rather than a philosophy: most of
    /// the time "roughly what is out there" is the question, but anything that
    /// commits — drafting a reorder against a figure, promising a delivery —
    /// wants the number nobody has to caveat.
    pub fresh_only: Option<bool>,
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
async fn gather(
    state: &AppState,
    org: Option<uuid::Uuid>,
    fresh_only: bool,
    may_ask_peers: bool,
) -> (Vec<SourceStatus>, Vec<Contribution>) {
    let mut sources = Vec::new();
    let mut summaries: Vec<Contribution> = Vec::new();

    // ---- this outpost, read straight from the DB ----
    match local_items(state, org).await {
        Ok(items) => {
            sources.push(SourceStatus {
                node_id: state.identity.node_id.clone(),
                name: env_or("OUTPOST_NAME", "tid-wayfarer"),
                status: "local",
                age_seconds: Some(0),
                last_seen: Some(Utc::now()),
                fetched_at: Some(Utc::now()),
                item_count: items.len(),
                error: None,
            });
            summaries.push(Contribution {
                summary: OutpostSummary {
                    node_id: state.identity.node_id.clone(),
                    outpost_name: env_or("OUTPOST_NAME", "tid-wayfarer"),
                    body_id: env_or("BODY_ID", "399").parse().unwrap_or(399),
                    region: env_or("OUTPOST_REGION", ""),
                    generated_at: Utc::now(),
                    items,
                },
                stale: false,
                age_seconds: 0,
            });
        }
        Err(e) => sources.push(SourceStatus {
            node_id: state.identity.node_id.clone(),
            name: env_or("OUTPOST_NAME", "tid-wayfarer"),
            status: "unreachable",
            age_seconds: None,
            last_seen: None,
            fetched_at: None,
            item_count: 0,
            error: Some(format!("local query failed: {e}")),
        }),
    }

    // ---- peers ----
    if !may_ask_peers {
        return (sources, summaries);
    }

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
                    fetched_at: Some(Utc::now()),
                    item_count: summary.items.len(),
                    error: None,
                });

                // Keep the snapshot warm off the back of a read someone was
                // making anyway. The daemon does this on a timer, but an
                // operator watching the resources page is the most reliable
                // clock there is — and a failure to store must not fail the
                // rollup they asked for.
                if let Err(e) = replication::store_snapshot(&state.db, org, &summary).await {
                    tracing::warn!(error = %e, "rollup could not refresh snapshot");
                }

                summaries.push(Contribution { summary, stale: false, age_seconds: age });
            }
            Err(e) => {
                // The condition this feature exists for. Before falling back
                // to "contributes nothing", look for what this peer last said.
                let snapshot = if fresh_only {
                    None
                } else {
                    replication::read_snapshot(&state.db, org, &peer.node_id).await
                };

                match snapshot {
                    Some(snap) if snap.contributes() => {
                        let age = snap.age_seconds();
                        tracing::info!(
                            node_id = %peer.node_id, age_seconds = age,
                            "rollup peer dark; using its last reported summary"
                        );
                        sources.push(SourceStatus {
                            node_id: peer.node_id,
                            name: snap.summary.outpost_name.clone(),
                            status: "stale",
                            age_seconds: Some(age),
                            last_seen: peer.last_seen,
                            fetched_at: Some(snap.fetched_at),
                            item_count: snap.item_count,
                            // Not an error — the fetch failed and the fallback
                            // worked — but the reason the link is down is still
                            // the useful detail for whoever has to fix it.
                            error: Some(format!("out of contact ({e}); figures as last reported")),
                        });
                        summaries.push(Contribution {
                            summary: snap.summary,
                            stale: true,
                            age_seconds: age,
                        });
                    }
                    // A snapshot exists but is past the horizon. Listed so the
                    // site is nameable and its age visible, deliberately not
                    // summed: a figure from six months ago is worse than none,
                    // because someone will act on it.
                    Some(snap) => {
                        let age = snap.age_seconds();
                        tracing::warn!(
                            node_id = %peer.node_id, age_seconds = age,
                            "rollup peer dark and its snapshot is too old to count"
                        );
                        sources.push(SourceStatus {
                            node_id: peer.node_id,
                            name: snap.summary.outpost_name,
                            status: "expired",
                            age_seconds: Some(age),
                            last_seen: peer.last_seen,
                            fetched_at: Some(snap.fetched_at),
                            item_count: 0,
                            error: Some(format!(
                                "out of contact ({e}); last reported {} days ago, too old to count",
                                age / 86_400
                            )),
                        });
                    }
                    None => {
                        tracing::warn!(node_id = %peer.node_id, error = %e, "rollup peer unreachable, no snapshot");
                        sources.push(SourceStatus {
                            node_id: peer.node_id,
                            name: peer.name,
                            status: "unreachable",
                            age_seconds: None,
                            last_seen: peer.last_seen,
                            fetched_at: None,
                            item_count: 0,
                            error: Some(e),
                        });
                    }
                }
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
fn merge(sources: Vec<SourceStatus>, summaries: Vec<Contribution>) -> InventoryRollup {
    let live = sources.iter().filter(|s| matches!(s.status, "local" | "ok")).count();
    let stale = sources.iter().filter(|s| s.status == "stale").count();
    // `expired` and `unreachable` both contributed nothing. They are separate
    // statuses because the fix differs — one site has never been heard from,
    // the other has been dark long enough that what it said no longer counts —
    // but for the arithmetic they are the same: a gap.
    let missing = sources.len() - live - stale;

    let answered = live + stale;
    let complete = missing == 0 && stale == 0;
    let floor = missing > 0;

    let mut grouped: BTreeMap<(String, String), RolledItem> = BTreeMap::new();

    for contribution in &summaries {
        let summary = &contribution.summary;
        for item in &summary.items {
            let key = (item.name.to_lowercase(), item.unit.to_lowercase());
            let entry = grouped.entry(key).or_insert_with(|| RolledItem {
                name: item.name.clone(),
                unit: item.unit.clone(),
                category: item.category.clone(),
                total_quantity: 0,
                stale_quantity: 0,
                oldest_contribution_seconds: 0,
                // Inherited from the fabric-wide result: an item's total
                // cannot be more trustworthy than the coverage behind it.
                complete,
                floor,
                below_threshold_somewhere: false,
                shortfall_total: 0,
                by_location: Vec::new(),
            });

            let shortfall = (item.threshold - item.quantity).max(0);

            entry.total_quantity += item.quantity;
            if contribution.stale {
                entry.stale_quantity += item.quantity;
            }
            entry.oldest_contribution_seconds =
                entry.oldest_contribution_seconds.max(contribution.age_seconds);
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
                stale: contribution.stale,
                age_seconds: contribution.age_seconds,
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

    let oldest = sources.iter().filter_map(|s| s.age_seconds).max().filter(|a| *a > 0);

    InventoryRollup {
        generated_at: Utc::now(),
        complete,
        floor,
        sources_total: sources.len(),
        sources_answered: answered,
        sources_live: live,
        sources_stale: stale,
        sources_missing: missing,
        oldest_contribution_seconds: oldest,
        replica_horizon_seconds: replication::max_contributing_age().num_seconds(),
        scope_note: None,
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
            fetched_at: None,
            item_count: 0,
            error: None,
        }
    }

    /// A source that answered just now.
    fn live(s: OutpostSummary) -> Contribution {
        Contribution { summary: s, stale: false, age_seconds: 0 }
    }

    /// A source that is dark, answered from its last reported summary.
    fn stale(s: OutpostSummary, age_seconds: i64) -> Contribution {
        Contribution { summary: s, stale: true, age_seconds }
    }

    #[test]
    fn totals_sum_across_outposts_and_keep_the_breakdown() {
        let r = merge(
            vec![source("a", "local"), source("b", "ok")],
            vec![
                live(summary("a", "HQ", vec![item("Water", "L", 300, 50, "tank-1")])),
                live(summary("b", "Farm", vec![item("Water", "L", 200, 50, "tank-2")])),
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
                live(summary("a", "HQ", vec![item("Water", "L", 10, 0, "x")])),
                live(summary("b", "Farm", vec![item("water", "l", 5, 0, "y")])),
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
            vec![live(summary("a", "HQ", vec![item("Seed", "kg", 40, 10, "silo")]))],
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
                live(summary("a", "HQ", vec![item("Seed", "kg", 995, 10, "silo")])),
                live(summary("b", "Farm", vec![item("Seed", "kg", 5, 10, "shed")])),
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
                live(summary("a", "HQ", vec![item("Seed", "kg", 995, 10, "silo")])),
                live(summary("b", "Farm", vec![item("Seed", "kg", 5, 50, "shed")])),
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
            vec![live(summary("a", "HQ", vec![item("Bolts", "each", 900, 10, "bin")]))],
        );
        assert_eq!(r.items[0].shortfall_total, 0);
        assert!(!r.items[0].below_threshold_somewhere);
    }

    #[test]
    fn items_needing_attention_sort_first() {
        let r = merge(
            vec![source("a", "local")],
            vec![live(summary(
                "a",
                "HQ",
                vec![
                    item("Bolts", "each", 9000, 10, "bin"),
                    item("Oxygen", "kg", 2, 10, "tank"),
                ],
            ))],
        );
        assert_eq!(r.items[0].name, "Oxygen");
    }

    /// A dark site contributing its last figures is not the same as a dark
    /// site contributing nothing, and the response must not blur them.
    ///
    /// `floor` says a site is missing entirely, so the total is a lower bound.
    /// `complete` says every figure is current. A snapshot makes the second
    /// false without making the first true — the stock may have moved either
    /// way since, which is a different thing to tell an operator.
    #[test]
    fn a_snapshot_makes_a_total_uncertain_but_not_a_floor() {
        let r = merge(
            vec![source("a", "local"), source("b", "stale")],
            vec![
                live(summary("a", "HQ", vec![item("Water", "L", 300, 50, "tank-1")])),
                stale(summary("b", "Farm", vec![item("Water", "L", 200, 50, "tank-2")]), 3 * 86_400),
            ],
        );

        assert_eq!(r.items[0].total_quantity, 500, "the dark site must still count");
        assert_eq!(r.items[0].stale_quantity, 200, "and the total must say how much is stale");
        assert!(!r.complete, "not every figure is current");
        assert!(!r.floor, "nothing is missing, so this is not a lower bound");
        assert_eq!(r.sources_live, 1);
        assert_eq!(r.sources_stale, 1);
        assert_eq!(r.sources_missing, 0);
        assert_eq!(r.sources_answered, 2);
    }

    /// A site that contributed nothing makes the total a floor, whether it was
    /// never heard from or its snapshot aged out.
    #[test]
    fn a_site_contributing_nothing_makes_the_total_a_floor() {
        for status in ["unreachable", "expired"] {
            let r = merge(
                vec![source("a", "local"), source("b", status)],
                vec![live(summary("a", "HQ", vec![item("Seed", "kg", 40, 10, "silo")]))],
            );
            assert!(r.floor, "{status} should make the total a floor");
            assert!(!r.complete);
            assert!(r.items[0].floor);
            assert_eq!(r.sources_missing, 1);
            assert_eq!(r.sources_answered, 1);
        }
    }

    /// The reported age is the oldest figure in the total, not an average.
    ///
    /// Averaging would let one very stale site hide behind several fresh ones,
    /// which is exactly the number an operator must not be given.
    #[test]
    fn the_oldest_contribution_is_what_gets_reported() {
        let r = merge(
            vec![source("a", "local"), source("b", "stale"), source("c", "stale")],
            vec![
                live(summary("a", "HQ", vec![item("Water", "L", 10, 1, "t")])),
                stale(summary("b", "Near", vec![item("Water", "L", 10, 1, "t")]), 3_600),
                stale(summary("c", "Far", vec![item("Water", "L", 10, 1, "t")]), 9 * 86_400),
            ],
        );
        assert_eq!(r.items[0].oldest_contribution_seconds, 9 * 86_400);
        assert_eq!(r.items[0].stale_quantity, 20);
        // The rollup-level figure is taken from the source list rather than
        // from the items, so that a site reporting nothing at all still counts
        // toward "how old is the oldest thing here".
        assert!(r.oldest_contribution_seconds.is_none(), "the helper's sources are all age 0");
    }

    #[test]
    fn an_empty_fabric_is_not_an_error() {
        let r = merge(vec![source("a", "local")], vec![live(summary("a", "HQ", vec![]))]);
        assert_eq!(r.distinct_items, 0);
        assert!(r.complete);
    }
}
