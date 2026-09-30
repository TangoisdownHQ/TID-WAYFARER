use crate::AppState;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

/// Backoff ceiling for a persistently failing queue read. An unmigrated DB
/// used to re-log every 2s forever, drowning the log; failing soft means
/// backing off, not spinning.
const MAX_BACKOFF_SECS: u64 = 60;

pub async fn run_blockchain_feeder(state: AppState) {
    let mut backoff_secs = 2u64;

    loop {
        // 1) Pull unprocessed feed items
        let rows = match sqlx::query!(
            r#"
            SELECT id, chain, payload
            FROM blockchain_feed_queue
            WHERE processed = false
            ORDER BY created_at ASC
            LIMIT 50
            "#
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(r) => {
                backoff_secs = 2;
                r
            }
            Err(e) => {
                tracing::warn!("[blockchain_feeder] fetch error (retry in {backoff_secs}s): {e}");
                sleep(Duration::from_secs(backoff_secs)).await;
                backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
                continue;
            }
        };

        if rows.is_empty() {
            sleep(Duration::from_millis(400)).await;
            continue;
        }

        for row in rows {
            let queue_id: Uuid = row.id;
            let chain: String = row.chain;
            let payload: Value = row.payload;

            // Try to normalize fields from payload
            let threat = payload
                .get("threat")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let alert_type = payload
                .get("alert_type")
                .and_then(|v| v.as_str())
                .unwrap_or("generic")
                .to_string();

            let address = payload
                .get("address")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let tx_hash = payload
                .get("tx_hash")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let severity = payload
                .get("severity")
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .to_string();

            // context: full payload
            let context: Value = payload.clone();

            // details: either nested "details" or whole payload
            let details_value: Value = payload
                .get("details")
                .cloned()
                .unwrap_or_else(|| json!({ "raw": payload }));

            let details_text: Option<String> = Some(details_value.to_string());

            // 2) Insert into blockchain_alerts
            let alert_id = Uuid::new_v4();

            let insert_result = sqlx::query!(
                r#"
                INSERT INTO blockchain_alerts 
                    (id, chain, alert_type, threat, address, tx_hash, severity, context, reported_by, details)
                VALUES 
                    ($1, $2, $3, $4, $5, $6, $7, $8, NULL, COALESCE($9, '{}'::text))
                "#,
                alert_id,
                chain,
                alert_type,
                threat,
                address,
                tx_hash,
                severity,
                context,
                details_text
            )
            .execute(&state.db)
            .await;

            match insert_result {
                Ok(_) => {
                    // 3) Mark queue row processed
                    let _ = sqlx::query!(
                        r#"
                        UPDATE blockchain_feed_queue
                        SET processed = true
                        WHERE id = $1
                        "#,
                        queue_id
                    )
                    .execute(&state.db)
                    .await;
                }
                Err(e) => {
                    tracing::warn!(
                        "[blockchain_feeder] insert error for queue_id {queue_id}: {e}"
                    );
                }
            }
        }
    }
}

