//! DB-driven autonomous ops policies.
//!
//! Rules live in `autonomy_rules` (managed via /api/rules). The telemetry
//! processor calls [`load_rules`] once per batch and [`evaluate`] per row;
//! a firing queues the rule's command into `command_queue` and records an
//! `ops_events` entry. Cooldowns are enforced atomically per (rule, asset)
//! in `autonomy_rule_firings`, so restarts and replicas can't double-fire.

use crate::services::metrics::{incr, METRICS};
use crate::AppState;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: i64,
    pub name: String,
    pub metric: String,
    pub op: String,
    pub threshold: f64,
    pub command: Value,
    pub event_kind: String,
    pub severity: String,
    pub cooldown_secs: i32,
}

/// Fetch all enabled rules. Errors degrade to "no rules" so a missing table
/// (unmigrated DB) never takes the telemetry pipeline down.
pub async fn load_rules(state: &AppState) -> Vec<Rule> {
    let rows = match sqlx::query(
        r#"
        SELECT id, name, metric, op, threshold, command,
               event_kind, severity, cooldown_secs
        FROM autonomy_rules
        WHERE enabled = true
        "#,
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[rules_engine] failed to load rules: {e}");
            return Vec::new();
        }
    };

    rows.iter()
        .map(|r| Rule {
            id: r.get("id"),
            name: r.get("name"),
            metric: r.get("metric"),
            op: r.get("op"),
            threshold: r.get("threshold"),
            command: r.get("command"),
            event_kind: r.get("event_kind"),
            severity: r.get("severity"),
            cooldown_secs: r.get("cooldown_secs"),
        })
        .collect()
}

/// Evaluate one telemetry row (as a metric map) against the rule set.
/// Returns the number of rules that fired.
pub async fn evaluate(
    state: &AppState,
    rules: &[Rule],
    asset_id: Option<Uuid>,
    node_id: Option<String>,
    metrics: &Value,
    trace_id: Uuid,
) -> usize {
    let mut fired = 0;

    for rule in rules {
        let Some(value) = metrics.get(&rule.metric).and_then(Value::as_f64) else {
            continue; // metric absent from this row
        };

        let hit = match rule.op.as_str() {
            "gt" => value > rule.threshold,
            "gte" => value >= rule.threshold,
            "lt" => value < rule.threshold,
            "lte" => value <= rule.threshold,
            "eq" => (value - rule.threshold).abs() < f64::EPSILON,
            other => {
                tracing::warn!("[rules_engine] rule {} has unknown op '{other}'", rule.name);
                false
            }
        };
        if !hit {
            continue;
        }

        if !try_claim_firing(state, rule, asset_id).await {
            continue; // still cooling down
        }

        queue_command(state, asset_id, node_id.clone(), rule.command.clone(), trace_id).await;
        record_event(
            state,
            asset_id,
            node_id.clone(),
            &rule.event_kind,
            &rule.severity,
            json!({
                "rule": rule.name,
                "metric": rule.metric,
                "value": value,
                "threshold": rule.threshold,
            }),
            trace_id,
        )
        .await;

        incr(&METRICS.rules_fired);
        tracing::info!(
            rule = %rule.name,
            metric = %rule.metric,
            op = %rule.op,
            threshold = rule.threshold,
            value,
            severity = %rule.severity,
            "autonomy rule fired"
        );
        fired += 1;
    }

    fired
}

/// Atomic cooldown gate: inserts/updates the firing row only when the
/// cooldown has elapsed. Returns true when this caller won the claim.
async fn try_claim_firing(state: &AppState, rule: &Rule, asset_id: Option<Uuid>) -> bool {
    let asset_key = asset_id.unwrap_or_else(Uuid::nil);

    let claimed = sqlx::query(
        r#"
        INSERT INTO autonomy_rule_firings (rule_id, asset_id, fired_at)
        VALUES ($1, $2, NOW())
        ON CONFLICT (rule_id, asset_id) DO UPDATE SET fired_at = NOW()
        WHERE autonomy_rule_firings.fired_at <= NOW() - make_interval(secs => $3)
        RETURNING rule_id
        "#,
    )
    .bind(rule.id)
    .bind(asset_key)
    .bind(rule.cooldown_secs as f64)
    .fetch_optional(&state.db)
    .await;

    match claimed {
        Ok(row) => row.is_some(),
        Err(e) => {
            tracing::warn!("[rules_engine] cooldown check failed for {}: {e}", rule.name);
            false
        }
    }
}

async fn queue_command(
    state: &AppState,
    asset_id: Option<Uuid>,
    node_id: Option<String>,
    command: Value,
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
    .bind(command)
    .bind(trace_id)
    .execute(&state.db)
    .await
    {
        tracing::error!(error = %e, "failed to queue rule command");
    }
}

async fn record_event(
    state: &AppState,
    asset_id: Option<Uuid>,
    node_id: Option<String>,
    kind: &str,
    severity: &str,
    details: Value,
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
        tracing::error!(error = %e, kind, "failed to record rule event");
    }
}
