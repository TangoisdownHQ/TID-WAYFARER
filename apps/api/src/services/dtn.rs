use crate::services::metrics::{incr, METRICS};
use crate::AppState;
use reqwest::Client;
use sqlx::Row;
use tokio::time::{sleep, Duration};

pub async fn run_dtn_forwarder(state: AppState) {
    let http = Client::new();
    // Peer /api/dtn/receive endpoints sit behind the fabric guard.
    let node_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();

    // Housekeeping runs on a counter rather than its own task: one loop that
    // is already waking every second does not need a second timer, and the
    // two jobs must not interleave with a half-applied migration on startup.
    let mut tick: u64 = 0;

    loop {
        tick = tick.wrapping_add(1);

        // Every ~5 minutes: retire what can no longer be delivered, and
        // forget message ids whose bundles can no longer arrive.
        //
        // The second half is the part that has to be right. A row dropped
        // from dtn_seen while the sender still considers the bundle live
        // reopens the replay window for exactly that message, so the cutoff
        // is the bundle's own expiry — not an age, and not a fixed retention.
        if tick % 300 == 0 {
            match sqlx::query("DELETE FROM dtn_seen WHERE expires_at < NOW()")
                .execute(&state.db)
                .await
            {
                Ok(r) if r.rows_affected() > 0 => {
                    tracing::debug!(rows = r.rows_affected(), "pruned expired DTN replay window")
                }
                Err(e) => tracing::warn!("[dtn_forwarder] replay-window prune failed: {e}"),
                _ => {}
            }

            // An undeliverable bundle used to retry at a 64-second ceiling
            // forever — a peer that would never accept it (wrong address, old
            // envelope format) produced traffic indefinitely. Its lifetime now
            // ends the attempt, which is the same clock the receiver enforces.
            match sqlx::query(
                "DELETE FROM dtn_outbox WHERE expires_at IS NOT NULL AND expires_at < NOW()",
            )
            .execute(&state.db)
            .await
            {
                Ok(r) if r.rows_affected() > 0 => tracing::warn!(
                    rows = r.rows_affected(),
                    "dropped DTN bundles that passed their lifetime undelivered"
                ),
                Err(e) => tracing::warn!("[dtn_forwarder] outbox expiry sweep failed: {e}"),
                _ => {}
            }
        }

        let rows = match sqlx::query(
            r#"
            SELECT id, dest_node_id::text AS dest, endpoint, payload, attempts
            FROM dtn_outbox
            WHERE next_try_at <= NOW()
              -- Dead bundles are swept on a timer; this keeps one from being
              -- attempted in the window before the sweep reaches it.
              AND (expires_at IS NULL OR expires_at > NOW())
            ORDER BY next_try_at ASC
            LIMIT 20
            "#,
        )
        .fetch_all(&state.db)
        .await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[dtn_forwarder] fetch error: {e}");
                sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if rows.is_empty() {
            sleep(Duration::from_secs(1)).await;
            continue;
        }

        for r in rows {
            let id: i64 = r.get("id");
            let endpoint: String = r.get("endpoint");
            let payload: serde_json::Value = r.get("payload");
            let attempts: i32 = r.get("attempts");

            let request = crate::services::fabric_auth::signed_post_json(
                &http,
                &state.identity,
                &endpoint,
                &payload,
                &node_token,
            );
            // The receiver distinguishes a first acceptance from a repeat, and
            // both are successes: a retransmit happens whenever our
            // acknowledgement was lost rather than the bundle, which on a
            // link that comes and goes is routine. Reading the outcome back
            // means the log says which one it was instead of implying every
            // delivery was new.
            let (ok, outcome) = match request.send().await {
                Ok(resp) => {
                    let success = resp.status().is_success();
                    let label = resp
                        .json::<serde_json::Value>()
                        .await
                        .ok()
                        .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(String::from))
                        .unwrap_or_else(|| "accepted".into());
                    (success, label)
                }
                Err(e) => (false, e.to_string()),
            };

            if ok {
                let _ = sqlx::query!("DELETE FROM dtn_outbox WHERE id = $1", id)
                    .execute(&state.db).await;
                incr(&METRICS.dtn_delivered);
                tracing::info!(message_id = id, outcome = %outcome, "DTN message delivered");
            } else {
                // ✅ Fix: cast delay_secs to f64 for Postgres make_interval
                let delay_secs = (2_i64).saturating_pow((attempts as u32).min(6)) as f64;
                let _ = sqlx::query!(
                    r#"
                    UPDATE dtn_outbox
                    SET attempts = attempts + 1,
                        next_try_at = NOW() + make_interval(secs => $2)
                    WHERE id = $1
                    "#,
                    id,
                    delay_secs
                )
                .execute(&state.db)
                .await;
                incr(&METRICS.dtn_retried);
                tracing::warn!(message_id = id, delay_secs = delay_secs, "DTN delivery failed; retry scheduled");
            }
        }
    }
}

