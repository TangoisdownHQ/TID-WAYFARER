use crate::services::metrics::{incr, METRICS};
use tracing::Instrument;
use crate::services::rules_engine;
use crate::AppState;
use serde_json::json;
use sqlx::Row;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

pub async fn run_telemetry_processor(state: AppState) {
    loop {
        let rows = match sqlx::query(
            r#"
            SELECT id, asset_id, node_id,
                   anomaly_score, tamper, malware_flag,
                   lat, lon, speed, heading, temperature,
                   battery, signal_db
            FROM fleet_telemetry
            WHERE processed = false
            ORDER BY timestamp ASC
            LIMIT 50
            "#,
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[telemetry_processor] fetch error: {e}");
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if rows.is_empty() {
            sleep(Duration::from_millis(350)).await;
            continue;
        }

        // One rules fetch per batch keeps policy current without a query per row.
        let rules = rules_engine::load_rules(&state).await;

        for r in rows {
            let id: Uuid = r.get("id");
            let asset_id: Option<Uuid> = r.get("asset_id");
            let node_id: Option<String> = r.get("node_id");

            // One trace id per telemetry row, carried onto every event and
            // command this row causes — including across the wire to the
            // outpost that executes them. This is what makes "why did rover-7
            // lock down?" a single indexed query instead of a timestamp hunt.
            let trace_id = Uuid::new_v4();
            let span = tracing::info_span!(
                "telemetry",
                %trace_id,
                telemetry_id = %id,
                asset_id = asset_id.map(|a| a.to_string()).unwrap_or_default(),
                node_id = node_id.clone().unwrap_or_default(),
            );
            incr(&METRICS.telemetry_processed);

            // The whole per-row pipeline runs inside the span via Instrument
            // rather than an entered guard: an EnteredSpan held across .await
            // makes the future !Send and tokio::spawn rejects it.
            async {

            let anomaly: Option<f64> = r.get("anomaly_score");
            let tamper: Option<bool> = r.get("tamper");
            let malware: Option<bool> = r.get("malware_flag");

            if let (Some(asset), Some(lat), Some(lon)) =
                (asset_id,
                 r.try_get::<Option<f64>, _>("lat").ok().flatten(),
                 r.try_get::<Option<f64>, _>("lon").ok().flatten())
            {
                let _ = sqlx::query!(
                    r#"
                    UPDATE fleet_assets
                    SET dynamic_location = $1, updated_at = NOW(), last_seen = NOW()
                    WHERE id = $2
                    "#,
                    format!("{lat:.6},{lon:.6}"),
                    asset
                )
                .execute(&state.db)
                .await;
            }

            // always log baseline event
            record_event(
                &state,
                asset_id,
                node_id.clone(),
                "telemetry_ok",
                "info",
                json!({ "anomaly": anomaly, "tamper": tamper, "malware": malware }),
                trace_id,
            )
            .await;

            // Configurable policies (autonomy_rules) — thresholds on any metric
            let metrics = json!({
                "anomaly_score": anomaly,
                "lat": r.try_get::<Option<f64>, _>("lat").ok().flatten(),
                "lon": r.try_get::<Option<f64>, _>("lon").ok().flatten(),
                "speed": r.try_get::<Option<f64>, _>("speed").ok().flatten(),
                "heading": r.try_get::<Option<f64>, _>("heading").ok().flatten(),
                "temperature": r.try_get::<Option<f64>, _>("temperature").ok().flatten(),
                "battery": r.try_get::<Option<f64>, _>("battery").ok().flatten(),
                "signal_db": r.try_get::<Option<f64>, _>("signal_db").ok().flatten(),
            });
            rules_engine::evaluate(&state, &rules, asset_id, node_id.clone(), &metrics, trace_id).await;

            // Built-in hard responses — not configurable, security-critical
            if tamper == Some(true) {
                tracing::warn!("tamper flag set; queueing built-in LOCKDOWN");
                queue_command(&state, asset_id, node_id.clone(), json!({"type": "LOCKDOWN"}), trace_id).await;
                record_event(&state, asset_id, node_id.clone(), "tamper", "high", json!({}), trace_id).await;
            }

            if malware == Some(true) {
                tracing::warn!("malware flag set; queueing built-in ISOLATE_NETWORK");
                queue_command(&state, asset_id, node_id.clone(), json!({"type": "ISOLATE_NETWORK"}), trace_id).await;
                record_event(&state, asset_id, node_id.clone(), "malware", "critical", json!({}), trace_id).await;
            }

            let _ = sqlx::query("UPDATE fleet_telemetry SET processed = true WHERE id = $1")
                .bind(id)
                .execute(&state.db)
                .await;
            }
            .instrument(span)
            .await;
        }
    }
}

async fn queue_command(
    state: &AppState,
    asset_id: Option<Uuid>,
    node_id: Option<String>,
    command: serde_json::Value,
    trace_id: Uuid,
) {
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO command_queue (asset_id, node_id, command, status, trace_id)
        VALUES ($1, $2, $3, 'queued', $4)
        "#,
    )
    .bind(asset_id)
    .bind(node_id)
    .bind(&command)
    .bind(trace_id)
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %e, "failed to queue built-in command");
    }
}

async fn record_event(
    state: &AppState,
    asset_id: Option<Uuid>,
    node_id: Option<String>,
    kind: &str,
    severity: &str,
    details: serde_json::Value,
    trace_id: Uuid,
) {
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO ops_events (asset_id, node_id, kind, severity, details, trace_id)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(asset_id)
    .bind(node_id)
    .bind(kind)
    .bind(severity)
    .bind(details)
    .bind(trace_id)
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %e, kind, "failed to record ops event");
    }
}

