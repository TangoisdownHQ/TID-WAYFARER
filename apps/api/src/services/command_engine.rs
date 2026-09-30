use crate::services::metrics::{incr, METRICS};
use crate::AppState;
use axum::http::StatusCode;
use chrono::Utc;
use reqwest::Client;
use serde_json::Value;
use sqlx::Row;
use tokio::time::{sleep, Duration};
use tracing::Instrument;

/// Give up on a command after this many delivery attempts.
const MAX_ATTEMPTS: i32 = 8;
/// Cap the exponential backoff (seconds).
const MAX_BACKOFF_SECS: f64 = 300.0;
/// Re-check interval when a node has no registered endpoint yet.
const NO_ENDPOINT_RETRY_SECS: f64 = 30.0;

/// Reads due commands and attempts delivery to the node endpoint, with
/// exponential backoff on failure (same semantics as the DTN outbox).
/// Expects each node to expose POST {API_ENDPOINT}/commands/execute
pub async fn run_command_engine(state: AppState) {
    let http = Client::new();
    // Target /commands/execute endpoints sit behind the fabric guard.
    let node_token = std::env::var("NODE_SHARED_SECRET").unwrap_or_default();

    loop {
        let rows = match sqlx::query(
            r#"
            SELECT cq.id, cq.asset_id, cq.node_id, cq.command, cq.attempts,
                   cq.trace_id, nr.api_endpoint
            FROM command_queue cq
            LEFT JOIN node_registry nr
              ON nr.node_id::text = cq.node_id
            WHERE cq.status = 'queued'
              AND cq.next_try_at <= NOW()
            ORDER BY cq.next_try_at
            LIMIT 25
            "#,
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "command queue fetch failed");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if rows.is_empty() {
            sleep(Duration::from_millis(400)).await;
            continue;
        }

        for r in rows {
            let id: i64 = r.get("id");
            let node_id: Option<String> = r.get("node_id");
            let endpoint: Option<String> = r.get("api_endpoint");
            let mut command: Value = r.get("command");
            let attempts: i32 = r.get("attempts");
            let trace_id: Option<uuid::Uuid> = r.try_get("trace_id").ok().flatten();

            let span = tracing::info_span!(
                "command_delivery",
                command_id = id,
                trace_id = trace_id.map(|t| t.to_string()).unwrap_or_default(),
                node_id = node_id.clone().unwrap_or_default(),
            );

            // Carry the trace across the wire: the receiving outpost records
            // it on its own ops_event, so one id spans both sides of the push.
            // The command id goes too, so the outpost can name what it is
            // acknowledging in the response.
            if let Some(obj) = command.as_object_mut() {
                if let Some(t) = trace_id {
                    obj.insert("trace_id".into(), Value::String(t.to_string()));
                }
                obj.insert("command_id".into(), Value::from(id));
            }

            // Instrument rather than enter: an EnteredSpan across .await is
            // !Send. `return` inside this block is the loop's old `continue`.
            async {
            let Some(api) = endpoint else {
                // No endpoint known yet — check again later without burning
                // an attempt (the node may simply not have registered yet).
                let _ = sqlx::query(
                    r#"
                    UPDATE command_queue
                    SET next_try_at = NOW() + make_interval(secs => $2)
                    WHERE id = $1
                    "#,
                )
                .bind(id)
                .bind(NO_ENDPOINT_RETRY_SECS)
                .execute(&state.db)
                .await;
                return;
            };

            let url = format!("{}/commands/execute", api.trim_end_matches('/'));

            // Command push is the highest-consequence call in the fabric —
            // it actuates remote assets — so it is signed with this node's
            // key rather than a secret every node shares.
            let request = crate::services::fabric_auth::signed_post_json(
                &http,
                &state.identity,
                &url,
                &command,
                &node_token,
            );
            // The response body carries the outpost's ack. Transport success
            // is not execution success: an outpost can accept the request and
            // still report that it could not run the command.
            let ack = match request.send().await {
                Ok(resp) if resp.status().is_success() => {
                    let body = resp.json::<Value>().await.unwrap_or_else(|_| Value::Null);
                    Some((
                        body.get("ackStatus")
                            .and_then(Value::as_str)
                            .unwrap_or("executed")
                            .to_string(),
                        body.get("detail").and_then(Value::as_str).unwrap_or("").to_string(),
                    ))
                }
                // Reached the node but it refused, or never reached it: both
                // are retryable, so they fall through to the backoff paths.
                Ok(resp) => {
                    tracing::warn!(status = resp.status().as_u16(), "outpost rejected command push");
                    None
                }
                Err(e) => {
                    tracing::debug!(error = %e, "command push transport error");
                    None
                }
            };

            if let Some((ack_status, ack_detail)) = ack {
                // 'executed' is the only outcome that ends the command's life
                // successfully; 'failed'/'unsupported' are terminal too, but
                // recorded as such so an operator can see the difference.
                let _ = sqlx::query(
                    r#"
                    UPDATE command_queue
                    SET status = $2, sent_at = NOW(), acked_at = NOW(),
                        ack_status = $3, ack_detail = $4
                    WHERE id = $1
                    "#,
                )
                .bind(id)
                .bind(if ack_status == "executed" { "acked" } else { "failed" })
                .bind(&ack_status)
                .bind(&ack_detail)
                .execute(&state.db)
                .await;

                if ack_status == "executed" {
                    incr(&METRICS.commands_delivered);
                    tracing::info!(ack_status, detail = %ack_detail, "command executed by outpost");
                } else {
                    incr(&METRICS.commands_failed);
                    tracing::warn!(ack_status, detail = %ack_detail, "outpost could not execute command");
                }
            } else if attempts + 1 >= MAX_ATTEMPTS {
                let _ = sqlx::query(
                    r#"
                    UPDATE command_queue
                    SET status='failed', attempts = attempts + 1, last_error = $2
                    WHERE id=$1
                    "#,
                )
                .bind(id)
                .bind(format!("gave up after {} attempts delivering to {url}", attempts + 1))
                .execute(&state.db)
                .await;
                incr(&METRICS.commands_failed);
                tracing::error!(attempts = attempts + 1, url, "giving up on command delivery");
            } else {
                let delay = (2_f64).powi(attempts.min(10)).min(MAX_BACKOFF_SECS);
                let _ = sqlx::query(
                    r#"
                    UPDATE command_queue
                    SET attempts = attempts + 1,
                        next_try_at = NOW() + make_interval(secs => $2),
                        last_error = $3
                    WHERE id=$1
                    "#,
                )
                .bind(id)
                .bind(delay)
                .bind(format!("deliver to {url}"))
                .execute(&state.db)
                .await;
                incr(&METRICS.commands_failed);
                tracing::warn!(attempts = attempts + 1, delay_secs = delay, "command delivery failed; retry scheduled");
            }
            }
            .instrument(span)
            .await;
        }
    }
}

/// Node may call this to ACK a command it executed.
pub async fn ack_command(state: &AppState, command_id: i64) -> Result<(), StatusCode> {
    sqlx::query!(
        r#"UPDATE command_queue SET status='acked', acked_at=NOW() WHERE id=$1"#,
        command_id
    )
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(())
}

