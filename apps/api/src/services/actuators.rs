//! Command actuators — what actually happens when a pushed command arrives.
//!
//! The autonomy pipeline used to end at a log line: `/api/commands/execute`
//! recorded an ops_event and returned "received", so a tamper flag produced a
//! LOCKDOWN row and no lockdown. This module is the missing half — a registry
//! mapping a command type to a handler with a durable effect, and an honest
//! outcome for types nobody implements.
//!
//! Effects land in `outpost_state`, a key/value table, so adding an actuator
//! does not require a migration.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::AppState;

/// Local state keys an actuator may set. Kept as constants because the auth
/// guard reads `LOCKDOWN` on every request.
pub const STATE_LOCKDOWN: &str = "lockdown";
pub const STATE_NETWORK_ISOLATED: &str = "network_isolated";

/// What a handler did. `Unsupported` is deliberately distinct from `Failed`:
/// an outpost that does not implement a command must say so rather than
/// silently report success, or core learns nothing from the ack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Executed(String),
    Failed(String),
    Unsupported(String),
}

impl Outcome {
    /// Wire form recorded on command_queue.ack_status.
    pub fn status(&self) -> &'static str {
        match self {
            Self::Executed(_) => "executed",
            Self::Failed(_) => "failed",
            Self::Unsupported(_) => "unsupported",
        }
    }

    pub fn detail(&self) -> &str {
        match self {
            Self::Executed(d) | Self::Failed(d) | Self::Unsupported(d) => d,
        }
    }
}

/// Persist a piece of local state, tagged with the trace that caused it.
async fn set_state(
    state: &AppState,
    key: &str,
    value: Value,
    trace_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        INSERT INTO outpost_state (key, value, updated_at, trace_id)
        VALUES ($1, $2, NOW(), $3)
        ON CONFLICT (key) DO UPDATE
        SET value = EXCLUDED.value, updated_at = NOW(), trace_id = EXCLUDED.trace_id
        "#,
    )
    .bind(key)
    .bind(value)
    .bind(trace_id)
    .execute(&state.db)
    .await
    .map(|_| ())
}

/// Read a boolean flag from local state. Defaults to false so an unmigrated
/// or empty table means "not locked down" rather than bricking the outpost.
pub async fn flag_is_set(state: &AppState, key: &str) -> bool {
    sqlx::query_scalar::<_, Value>("SELECT value FROM outpost_state WHERE key = $1")
        .bind(key)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.get("active").and_then(Value::as_bool))
        .unwrap_or(false)
}

/// Dispatch one command to its handler.
///
/// Handlers must be idempotent: the command engine retries on transport
/// failure, so the same command can legitimately arrive twice.
pub async fn dispatch(state: &AppState, command: &Value, trace_id: Option<Uuid>) -> Outcome {
    let Some(command_type) = command.get("type").and_then(Value::as_str) else {
        return Outcome::Failed("command has no \"type\" field".into());
    };

    match command_type {
        "LOCKDOWN" => set_flag(state, STATE_LOCKDOWN, true, trace_id, command).await,
        "UNLOCK" => set_flag(state, STATE_LOCKDOWN, false, trace_id, command).await,
        "ISOLATE_NETWORK" => set_flag(state, STATE_NETWORK_ISOLATED, true, trace_id, command).await,
        "REJOIN_NETWORK" => set_flag(state, STATE_NETWORK_ISOLATED, false, trace_id, command).await,
        "DIAGNOSTIC_SNAPSHOT" => diagnostic_snapshot(state, trace_id).await,

        other => Outcome::Unsupported(format!("no actuator registered for '{other}'")),
    }
}

/// Set/clear a boolean flag, recording the reason the command carried.
async fn set_flag(
    state: &AppState,
    key: &str,
    active: bool,
    trace_id: Option<Uuid>,
    command: &Value,
) -> Outcome {
    let reason = command
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("commanded");

    match set_state(state, key, json!({ "active": active, "reason": reason }), trace_id).await {
        Ok(()) => Outcome::Executed(format!("{key}={active}")),
        Err(e) => Outcome::Failed(format!("could not persist {key}: {e}")),
    }
}

/// Capture a point-in-time snapshot of local fabric state. Counts only — a
/// snapshot travels back to core and must not carry payloads.
async fn diagnostic_snapshot(state: &AppState, trace_id: Option<Uuid>) -> Outcome {
    let count = |sql: &'static str| async move {
        sqlx::query_scalar::<_, i64>(sql)
            .fetch_one(&state.db)
            .await
            .unwrap_or(-1)
    };

    let snapshot = json!({
        "queued_commands": count("SELECT COUNT(*)::int8 FROM command_queue WHERE status='queued'").await,
        "dtn_outbox": count("SELECT COUNT(*)::int8 FROM dtn_outbox").await,
        "unprocessed_telemetry": count("SELECT COUNT(*)::int8 FROM fleet_telemetry WHERE processed=false").await,
        "known_nodes": count("SELECT COUNT(*)::int8 FROM node_registry").await,
    });

    match set_state(state, "last_diagnostic", snapshot.clone(), trace_id).await {
        Ok(()) => Outcome::Executed(snapshot.to_string()),
        Err(e) => Outcome::Failed(format!("could not store snapshot: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_command_is_unsupported_not_failed() {
        // The distinction matters: core must be able to tell "this outpost
        // cannot do that" from "it tried and broke".
        let outcome = Outcome::Unsupported("no actuator registered for 'WARP'".into());
        assert_eq!(outcome.status(), "unsupported");
        assert_ne!(outcome.status(), Outcome::Failed(String::new()).status());
    }

    #[test]
    fn outcome_status_strings_are_stable() {
        // These are persisted to command_queue.ack_status and read by
        // operators, so they are part of the interface.
        assert_eq!(Outcome::Executed(String::new()).status(), "executed");
        assert_eq!(Outcome::Failed(String::new()).status(), "failed");
        assert_eq!(Outcome::Unsupported(String::new()).status(), "unsupported");
    }

    #[test]
    fn detail_is_preserved_across_variants() {
        assert_eq!(Outcome::Executed("lockdown=true".into()).detail(), "lockdown=true");
        assert_eq!(Outcome::Failed("db down".into()).detail(), "db down");
    }
}
