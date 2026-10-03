//! Polling carriers for where a parcel is, and closing the custody loop when
//! it arrives.
//!
//! # Idempotency, again
//!
//! Polling is at-least-once by nature: the same tracking history comes back
//! every time it is asked for. Without a per-event key, a second poll
//! duplicates the whole history — the same problem DTN had, one layer up, and
//! solved the same way: a `source_ref` with a unique index, and
//! `ON CONFLICT DO NOTHING`.
//!
//! # Delivery is attested, not asserted
//!
//! When a carrier reports delivery, that closes the custody chain — but the
//! receipt it writes is **unverified**, because a carrier cannot sign. A
//! tracking event is third-party-checkable evidence, which is a genuinely
//! useful thing and not the same as a signature. Filing it as verified would
//! quietly weaken every other receipt in the chain, since a reader could no
//! longer tell which ones somebody actually signed for.

use std::time::Duration;

use sqlx::Row;
use tokio::time::sleep;
use uuid::Uuid;

use crate::services::carriers::Provider;
use crate::AppState;

/// How often to sweep live shipments.
///
/// Carriers rate-limit, and a parcel's status changes a handful of times a
/// day, not a minute. Fifteen minutes is frequent enough that an operator
/// refreshing a page sees current information and sparse enough to stay well
/// inside any provider's limits.
fn interval() -> Duration {
    Duration::from_secs(
        std::env::var("CARRIER_TRACK_INTERVAL_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|s| *s > 0)
            .unwrap_or(900),
    )
}

/// How many shipments to poll per sweep. Bounded so a backlog of a thousand
/// parcels does not become a thousand HTTP calls in one burst.
fn batch() -> i64 {
    std::env::var("CARRIER_TRACK_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|s: &i64| *s > 0)
        .unwrap_or(25)
        .clamp(1, 200)
}

/// Statuses a carrier reports that mean the parcel arrived.
fn is_delivered(status: &str) -> bool {
    matches!(status.to_ascii_lowercase().as_str(), "delivered")
}

/// Statuses that mean somebody needs to look at it.
fn is_exception(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "error" | "failure" | "return_to_sender" | "exception" | "cancelled"
    )
}

/// Refresh one shipment. Returns how many events were new.
///
/// Shared by the daemon and the "track now" route so both record the same
/// thing — a second implementation of this would drift from the first.
pub async fn refresh_one(state: &AppState, shipment_id: Uuid) -> Result<usize, String> {
    let row = sqlx::query(
        "SELECT carrier, tracking_number, provider, status FROM carrier_shipments WHERE id = $1",
    )
    .bind(shipment_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| e.to_string())?
    .ok_or_else(|| "no such shipment".to_string())?;

    let tracking: Option<String> = row.get("tracking_number");
    let Some(tracking) = tracking else {
        return Err("this shipment has no tracking number yet".into());
    };
    let carrier: String = row.get("carrier");
    let provider = Provider::resolve(&row.get::<String, _>("provider"));

    let events = provider
        .track(&carrier, &tracking)
        .await
        .map_err(|e| e.to_string())?;

    let mut new = 0usize;
    let mut delivered_at: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut latest: Option<(chrono::DateTime<chrono::Utc>, String)> = None;

    for e in &events {
        let inserted = sqlx::query(
            "INSERT INTO carrier_tracking_events \
               (shipment_id, status, detail, location, occurred_at, source_ref) \
             VALUES ($1,$2,$3,$4,$5,$6) \
             ON CONFLICT (shipment_id, source_ref) DO NOTHING",
        )
        .bind(shipment_id)
        .bind(&e.status)
        .bind(&e.detail)
        .bind(&e.location)
        .bind(e.occurred_at)
        .bind(&e.source_ref)
        .execute(&state.db)
        .await
        .map_err(|err| err.to_string())?
        .rows_affected();

        if inserted > 0 {
            new += 1;
        }
        if is_delivered(&e.status) {
            delivered_at = Some(e.occurred_at);
        }
        if latest.as_ref().map(|(t, _)| e.occurred_at > *t).unwrap_or(true) {
            latest = Some((e.occurred_at, e.status.clone()));
        }
    }

    // Derive the shipment's own status from the newest event rather than from
    // whichever event happened to arrive last — carriers do not always return
    // them in order.
    let status = match (&delivered_at, latest.as_ref()) {
        (Some(_), _) => "delivered",
        (None, Some((_, s))) if is_exception(s) => "exception",
        (None, Some(_)) => "in_transit",
        (None, None) => "purchased",
    };

    sqlx::query(
        "UPDATE carrier_shipments \
         SET status = $2, delivered_at = COALESCE($3, delivered_at), \
             last_tracked_at = NOW(), updated_at = NOW() \
         WHERE id = $1",
    )
    .bind(shipment_id)
    .bind(status)
    .bind(delivered_at)
    .execute(&state.db)
    .await
    .map_err(|e| e.to_string())?;

    if delivered_at.is_some() {
        close_custody(state, shipment_id, &carrier, &tracking).await;
    }

    Ok(new)
}

/// Write the delivery end of the custody chain.
///
/// Guarded against running twice: a delivered parcel keeps reporting delivered
/// on every subsequent poll, and two delivery receipts for one parcel would
/// make the chain contradict itself.
async fn close_custody(state: &AppState, shipment_id: Uuid, carrier: &str, tracking: &str) {
    let to_label = format!("{} {}", carrier.to_uppercase(), tracking);

    let already: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM custody_receipts WHERE from_label = $1 AND event = 'delivery' LIMIT 1",
    )
    .bind(&to_label)
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if already.is_some() {
        return;
    }

    let Ok(Some(s)) = sqlx::query(
        "SELECT order_id, fulfillment_id, to_address_id FROM carrier_shipments WHERE id = $1",
    )
    .bind(shipment_id)
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };

    let order_id: Option<Uuid> = s.get("order_id");
    let seq: i32 = match order_id {
        Some(o) => sqlx::query_scalar::<_, Option<i32>>(
            "SELECT max(seq) FROM custody_receipts WHERE order_id = $1",
        )
        .bind(o)
        .fetch_one(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(0)
            + 1,
        None => 2,
    };

    // Who it reached, in words, from the address book.
    let consignee: Option<String> = match s.get::<Option<Uuid>, _>("to_address_id") {
        Some(a) => sqlx::query_scalar(
            "SELECT COALESCE(label, company, city) FROM addresses WHERE id = $1",
        )
        .bind(a)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten(),
        None => None,
    };

    let result = sqlx::query(
        r#"
        INSERT INTO custody_receipts
          (order_id, fulfillment_id, carrier_shipment_id, seq, from_node_id, to_node_id,
           from_label, to_label, event, condition, notes, occurred_at, verified)
        VALUES ($1,$2,$3,$4,NULL,NULL,$5,$6,'delivery','unknown',$7,NOW(),false)
        "#,
    )
    .bind(order_id)
    .bind(s.get::<Option<Uuid>, _>("fulfillment_id"))
    .bind(shipment_id)
    .bind(seq)
    .bind(&to_label)
    .bind(consignee.clone().unwrap_or_else(|| "consignee".into()))
    .bind(format!(
        "Carrier reported delivery. Unverified: a carrier cannot sign a receipt — \
         tracking number {tracking} is the evidence."
    ))
    .execute(&state.db)
    .await;

    match result {
        Ok(_) => tracing::info!(shipment = %shipment_id, "carrier delivery closed the custody chain"),
        Err(e) => tracing::warn!(shipment = %shipment_id, error = %e, "could not record carrier delivery"),
    }
}

/// Poll live shipments on a timer.
pub async fn run_carrier_tracker(state: AppState) {
    // Nothing to do until an account exists, and on a fresh outpost that may
    // be never — so the first sweep waits rather than racing startup.
    sleep(Duration::from_secs(30)).await;

    loop {
        // Only shipments whose provider can actually fetch. A manually
        // recorded parcel is updated by a person; polling it would be a
        // guaranteed failure every sweep, and the log noise would bury real
        // ones.
        let due = sqlx::query(
            r#"
            SELECT id FROM carrier_shipments
            WHERE status IN ('purchased', 'in_transit')
              AND tracking_number IS NOT NULL
              AND provider <> 'manual'
              AND (last_tracked_at IS NULL OR last_tracked_at < NOW() - make_interval(secs => $1))
            ORDER BY last_tracked_at NULLS FIRST
            LIMIT $2
            "#,
        )
        .bind(interval().as_secs() as f64)
        .bind(batch())
        .fetch_all(&state.db)
        .await;

        match due {
            Ok(rows) if !rows.is_empty() => {
                let mut events = 0usize;
                let mut failed = 0usize;
                for r in &rows {
                    let id: Uuid = r.get("id");
                    match refresh_one(&state, id).await {
                        Ok(n) => events += n,
                        Err(e) => {
                            failed += 1;
                            // Expected whenever the link is down. Debug, not
                            // warn, or a week of being dark fills the log.
                            tracing::debug!(shipment = %id, error = %e, "carrier tracking failed");
                            // Stamp the attempt so one unreachable shipment
                            // does not monopolise every sweep.
                            let _ = sqlx::query(
                                "UPDATE carrier_shipments SET last_tracked_at = NOW() WHERE id = $1",
                            )
                            .bind(id)
                            .execute(&state.db)
                            .await;
                        }
                    }
                }
                tracing::debug!(
                    polled = rows.len(), new_events = events, failed,
                    "[carrier_tracker] sweep complete"
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "[carrier_tracker] could not list shipments"),
        }

        sleep(interval()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_and_exception_statuses_are_recognised() {
        assert!(is_delivered("delivered"));
        assert!(is_delivered("Delivered"));
        assert!(!is_delivered("in_transit"));
        assert!(!is_delivered("out_for_delivery"));

        assert!(is_exception("return_to_sender"));
        assert!(is_exception("Failure"));
        assert!(!is_exception("in_transit"));
        // "pre_transit" is a label printed and not yet collected — normal, and
        // specifically not an exception.
        assert!(!is_exception("pre_transit"));
    }

    #[test]
    fn the_sweep_is_bounded() {
        // A backlog of a thousand parcels must not become a thousand
        // simultaneous HTTP calls.
        assert!(batch() <= 200);
        assert!(batch() >= 1);
        assert!(interval() >= Duration::from_secs(1));
    }
}
