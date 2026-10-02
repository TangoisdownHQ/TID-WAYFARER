//! Keeping a usable answer for an outpost that is not answering.
//!
//! The rollup fans out live reads, so a dark site contributes nothing and a
//! farm office on a weekly uplink is invisible six days out of seven. The
//! total silently became "what we have at the places answering the phone".
//!
//! This daemon keeps each peer's last reported summary locally, so the rollup
//! can fall back to it — with the age attached and counted separately, because
//! three-day-old stock is much closer to the truth than zero and nothing like
//! the truth itself.
//!
//! # What a replica is not
//!
//! It is a cache of another outpost's authoritative data. It must never back a
//! decision that needs the real figure — a reservation, a custody transfer, a
//! settlement — and nothing outside this module writes it. The distinction is
//! load-bearing: the moment a replica is treated as stock you can allocate,
//! two disconnected outposts will both allocate the same last unit, and the
//! system has no way to notice.
//!
//! # Which organisation's data this is
//!
//! `/api/rollup/local` is org-scoped: a peer asking resolves to an
//! organisation through its node certificate, and is shown that org's holdings
//! and nothing else. This daemon signs as *this* outpost, so it caches this
//! outpost's own organisation — which is the only org whose data it is
//! entitled to see at a peer.
//!
//! A snapshot therefore carries its org, and the rollup only ever reads
//! snapshots for the org it is asking about. A user from some other
//! organisation finds no snapshots and gets a live-only rollup: fewer numbers
//! rather than another company's numbers.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use tokio::time::sleep;
use uuid::Uuid;

use crate::routes::rollup::{OutpostSummary, SummaryItem};
use crate::services::{fabric_auth, org_scope};
use crate::AppState;

/// How often to refresh snapshots.
///
/// This is a background freshness task, not a live read — the rollup still
/// fetches live on every request and only falls back here. Five minutes keeps
/// snapshots recent without making a fabric of twenty outposts spend its
/// bandwidth on inventory summaries nobody asked for.
fn interval() -> Duration {
    Duration::from_secs(
        std::env::var("REPLICATION_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(300),
    )
}

/// Per-peer deadline for a refresh.
fn fetch_timeout() -> Duration {
    Duration::from_secs(
        std::env::var("REPLICATION_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(10)
            .clamp(1, 120),
    )
}

/// Past this, a snapshot stops contributing to totals.
///
/// A number from six months ago is worse than no number, because someone will
/// act on it. The snapshot is still *reported* — "Jezero-Farm last reported
/// 212 days ago" is useful — it just stops being added up.
///
/// Thirty days by default: long enough for a seasonal uplink or a vehicle that
/// visits monthly, short enough that it still describes the same operation.
pub fn max_contributing_age() -> chrono::Duration {
    chrono::Duration::seconds(
        std::env::var("REPLICA_MAX_AGE_SECS")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|s| *s > 0)
            .unwrap_or(30 * 24 * 3600),
    )
}

/// A snapshot read back out, with enough context to be judged.
pub struct Snapshot {
    pub summary: OutpostSummary,
    /// When this outpost last managed to ask. Distinct from the summary's own
    /// `generated_at`, which is the age of the data.
    pub fetched_at: DateTime<Utc>,
    pub item_count: usize,
}

impl Snapshot {
    /// Age of the data, from the peer's clock.
    pub fn age_seconds(&self) -> i64 {
        (Utc::now() - self.summary.generated_at).num_seconds().max(0)
    }

    /// Whether this is recent enough to be added into a total.
    pub fn contributes(&self) -> bool {
        self.age_seconds() <= max_contributing_age().num_seconds()
    }
}

/// Replace one peer's snapshot for one organisation.
///
/// Delete-then-insert inside a transaction, rather than an upsert per row.
/// An item the peer no longer holds has to *disappear* — an upsert would leave
/// the last-known quantity of something that has since been consumed entirely,
/// which is the worst kind of stale: a number that looks live because it keeps
/// being re-confirmed by a fetch that never mentions it.
pub async fn store_snapshot(
    db: &PgPool,
    org: Option<Uuid>,
    summary: &OutpostSummary,
) -> Result<(), sqlx::Error> {
    let node_id = match Uuid::parse_str(&summary.node_id) {
        Ok(id) => id,
        Err(_) => {
            tracing::warn!(node_id = %summary.node_id, "snapshot discarded: unparseable node id");
            return Ok(());
        }
    };

    let mut tx = db.begin().await?;

    sqlx::query(
        "DELETE FROM replica_inventory WHERE node_id = $1 \
         AND org_key = COALESCE($2, '00000000-0000-0000-0000-000000000000'::uuid)",
    )
    .bind(node_id)
    .bind(org)
    .execute(&mut *tx)
    .await?;

    for item in &summary.items {
        sqlx::query(
            r#"
            INSERT INTO replica_inventory
              (node_id, org_id, name, unit, category, location, quantity,
               threshold, below_threshold, generated_at)
            VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
            "#,
        )
        .bind(node_id)
        .bind(org)
        .bind(&item.name)
        .bind(&item.unit)
        .bind(&item.category)
        .bind(&item.location)
        .bind(item.quantity)
        .bind(item.threshold)
        .bind(item.below_threshold)
        .bind(summary.generated_at)
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        r#"
        INSERT INTO replica_sources
          (node_id, org_id, outpost_name, body_id, region, generated_at, fetched_at, item_count)
        VALUES ($1,$2,$3,$4,$5,$6,NOW(),$7)
        ON CONFLICT (node_id, org_key) DO UPDATE SET
          outpost_name = EXCLUDED.outpost_name,
          body_id      = EXCLUDED.body_id,
          region       = EXCLUDED.region,
          generated_at = EXCLUDED.generated_at,
          fetched_at   = NOW(),
          item_count   = EXCLUDED.item_count
        "#,
    )
    .bind(node_id)
    .bind(org)
    .bind(&summary.outpost_name)
    .bind(summary.body_id)
    .bind(&summary.region)
    .bind(summary.generated_at)
    .bind(summary.items.len() as i32)
    .execute(&mut *tx)
    .await?;

    tx.commit().await
}

/// The stored snapshot for one peer and one organisation, if there is one.
pub async fn read_snapshot(
    db: &PgPool,
    org: Option<Uuid>,
    node_id: &str,
) -> Option<Snapshot> {
    let parsed = Uuid::parse_str(node_id).ok()?;

    let head = sqlx::query(
        "SELECT outpost_name, body_id, region, generated_at, fetched_at, item_count \
         FROM replica_sources WHERE node_id = $1 \
         AND org_key = COALESCE($2, '00000000-0000-0000-0000-000000000000'::uuid)",
    )
    .bind(parsed)
    .bind(org)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()?;

    let rows = sqlx::query(
        "SELECT name, unit, category, location, quantity, threshold, below_threshold \
         FROM replica_inventory WHERE node_id = $1 \
         AND org_key = COALESCE($2, '00000000-0000-0000-0000-000000000000'::uuid) \
         ORDER BY name, location",
    )
    .bind(parsed)
    .bind(org)
    .fetch_all(db)
    .await
    .ok()?;

    let items: Vec<SummaryItem> = rows
        .iter()
        .map(|r| SummaryItem {
            name: r.get("name"),
            unit: r.get("unit"),
            category: r.get("category"),
            location: r.get("location"),
            quantity: r.get("quantity"),
            threshold: r.get("threshold"),
            below_threshold: r.get("below_threshold"),
        })
        .collect();

    Some(Snapshot {
        summary: OutpostSummary {
            node_id: node_id.to_string(),
            outpost_name: head.get("outpost_name"),
            body_id: head.try_get::<Option<i64>, _>("body_id").ok().flatten().unwrap_or(399),
            region: head.try_get::<Option<String>, _>("region").ok().flatten().unwrap_or_default(),
            // The peer's clock, not ours. This is the figure the whole feature
            // turns on: re-fetching an unchanged summary must not make it look
            // newer than the stock actually is.
            generated_at: head.get("generated_at"),
            items,
        },
        fetched_at: head.get("fetched_at"),
        item_count: head.try_get::<i32, _>("item_count").unwrap_or(0) as usize,
    })
}

/// Refresh every reachable peer's snapshot, for this outpost's own org.
///
/// Runs on a timer and on demand (the rollup also stores whatever it fetches
/// live, so an operator looking at the page keeps the cache warm for free).
pub async fn run_replication_daemon(state: AppState) {
    // Give registration a moment to settle; a peer list read at second zero on
    // a cold start is usually empty and the first pass would do nothing.
    sleep(Duration::from_secs(15)).await;

    let http = reqwest::Client::new();
    let legacy_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();

    loop {
        // The org this outpost belongs to — the only one whose holdings it is
        // entitled to read at a peer. `None` is a legitimate answer on an
        // outpost not yet enrolled in an organisation, and caches the
        // unaffiliated bucket.
        let org = org_scope::own_org(&state).await;

        let peers = match sqlx::query(
            r#"
            SELECT node_id::text AS node_id, name, api_endpoint
            FROM node_registry
            WHERE COALESCE(revoked, false) = false
              AND api_endpoint IS NOT NULL AND api_endpoint <> ''
              AND node_id::text <> $1
            "#,
        )
        .bind(&state.identity.node_id)
        .fetch_all(&state.db)
        .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "[replication] could not list peers");
                sleep(interval()).await;
                continue;
            }
        };

        let timeout = fetch_timeout();
        let mut stored = 0usize;
        let mut dark = 0usize;

        for row in &peers {
            let node_id: String = row.get("node_id");
            let name: String = row.get("name");
            let endpoint: String = row.get("api_endpoint");
            let url = format!("{}/rollup/local", endpoint.trim_end_matches('/'));

            let fetched = fabric_auth::signed_get(&http, &state.identity, &url, &legacy_token)
                .timeout(timeout)
                .send()
                .await;

            match fetched {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<OutpostSummary>().await {
                        Ok(summary) => match store_snapshot(&state.db, org, &summary).await {
                            Ok(()) => stored += 1,
                            Err(e) => tracing::warn!(peer = %name, error = %e, "[replication] could not store snapshot"),
                        },
                        Err(e) => tracing::warn!(peer = %name, error = %e, "[replication] unreadable summary"),
                    }
                }
                Ok(resp) => {
                    // A 403 here is worth distinguishing from a dark link: it
                    // means the peer does not recognise this outpost's
                    // certificate, which is a trust problem and will not fix
                    // itself by waiting.
                    dark += 1;
                    tracing::debug!(peer = %name, status = %resp.status(), "[replication] peer refused");
                }
                Err(_) => {
                    // Expected. This is the condition the whole feature exists
                    // for, so it is not a warning — the snapshot already held
                    // is the answer.
                    dark += 1;
                    tracing::trace!(peer = %name, "[replication] peer unreachable; keeping snapshot");
                }
            }
        }

        if !peers.is_empty() {
            tracing::debug!(
                refreshed = stored,
                unreachable = dark,
                "[replication] snapshot pass complete"
            );
        }

        sleep(interval()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(age_secs: i64) -> Snapshot {
        Snapshot {
            summary: OutpostSummary {
                node_id: Uuid::new_v4().to_string(),
                outpost_name: "Jezero-Farm".into(),
                body_id: 499,
                region: "Jezero".into(),
                generated_at: Utc::now() - chrono::Duration::seconds(age_secs),
                items: vec![],
            },
            fetched_at: Utc::now(),
            item_count: 0,
        }
    }

    /// Age is measured from the peer's clock, so a snapshot does not become
    /// fresher by being read.
    #[test]
    fn age_comes_from_the_data_not_the_row() {
        let s = snap(3 * 24 * 3600);
        assert!((s.age_seconds() - 3 * 24 * 3600).abs() <= 2);
        // fetched_at is now, and must not affect the answer.
        assert!(s.age_seconds() > 1000);
    }

    /// A recent snapshot counts; an ancient one is reported but not added up.
    ///
    /// Six-month-old stock is worse than no figure, because someone will act
    /// on it.
    #[test]
    fn a_snapshot_stops_counting_once_it_is_too_old() {
        assert!(snap(3600).contributes(), "an hour old should count");
        assert!(snap(29 * 24 * 3600).contributes(), "inside 30 days should count");
        assert!(!snap(200 * 24 * 3600).contributes(), "200 days should not count");
    }

    #[test]
    fn the_contributing_horizon_is_positive() {
        // A zero or negative horizon would silently disable the feature while
        // leaving every snapshot in place, which is the confusing failure.
        assert!(max_contributing_age() > chrono::Duration::zero());
    }
}
