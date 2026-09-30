use crate::services::metrics::{incr, METRICS};
use crate::AppState;
use reqwest::Client;
use sqlx::Row;
use tokio::time::{sleep, Duration};

pub async fn run_dtn_forwarder(state: AppState) {
    let http = Client::new();
    // Peer /api/dtn/receive endpoints sit behind the fabric guard.
    let node_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();

    loop {
        let rows = match sqlx::query(
            r#"
            SELECT id, dest_node_id::text AS dest, endpoint, payload, attempts
            FROM dtn_outbox
            WHERE next_try_at <= NOW()
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
            let ok = request.send().await
                .map(|resp| resp.status().is_success())
                .unwrap_or(false);

            if ok {
                let _ = sqlx::query!("DELETE FROM dtn_outbox WHERE id = $1", id)
                    .execute(&state.db).await;
                incr(&METRICS.dtn_delivered);
                tracing::info!(message_id = id, "DTN message delivered");
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

