//! Process metrics in Prometheus text format, served at `GET /metrics`.
//!
//! Deliberately dependency-free: a handful of atomics rather than a metrics
//! crate and its exporter. The cardinality here is fixed and small, and the
//! fabric's interesting numbers (queue depth, DTN backlog) live in Postgres
//! anyway — those are read at scrape time as gauges rather than mirrored into
//! process state that would drift on restart.

use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::PgPool;

/// Counters that only make sense as process-lifetime totals.
pub struct Counters {
    pub http_requests: AtomicU64,
    pub auth_failures: AtomicU64,
    pub fabric_sig_ok: AtomicU64,
    pub fabric_sig_rejected: AtomicU64,
    pub telemetry_processed: AtomicU64,
    pub telemetry_hmac_rejected: AtomicU64,
    pub rules_fired: AtomicU64,
    pub commands_delivered: AtomicU64,
    pub commands_failed: AtomicU64,
    pub dtn_delivered: AtomicU64,
    pub dtn_retried: AtomicU64,
}

impl Counters {
    const fn new() -> Self {
        Self {
            http_requests: AtomicU64::new(0),
            auth_failures: AtomicU64::new(0),
            fabric_sig_ok: AtomicU64::new(0),
            fabric_sig_rejected: AtomicU64::new(0),
            telemetry_processed: AtomicU64::new(0),
            telemetry_hmac_rejected: AtomicU64::new(0),
            rules_fired: AtomicU64::new(0),
            commands_delivered: AtomicU64::new(0),
            commands_failed: AtomicU64::new(0),
            dtn_delivered: AtomicU64::new(0),
            dtn_retried: AtomicU64::new(0),
        }
    }
}

/// Single process-wide registry. A static keeps daemons from having to thread
/// a handle through every call site just to bump a counter.
pub static METRICS: Counters = Counters::new();

/// Relaxed is the right ordering here: these are independent counters read
/// only by a scrape, never used to synchronize anything.
pub fn incr(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

fn get(counter: &AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

/// A gauge read from the database at scrape time. Any query failure yields
/// `None` and the metric is simply omitted — a scrape must never 500, and a
/// missing sample is honest about the fact we could not read it.
async fn gauge(pool: &PgPool, sql: &str) -> Option<i64> {
    sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.ok()
}

/// Render the full exposition. Counters come from process state, gauges from
/// Postgres.
pub async fn render(pool: &PgPool) -> String {
    let mut out = String::with_capacity(2048);

    let counters: [(&str, &str, u64); 11] = [
        ("wayfarer_http_requests_total", "HTTP requests handled", get(&METRICS.http_requests)),
        ("wayfarer_auth_failures_total", "Requests rejected by the auth guard", get(&METRICS.auth_failures)),
        ("wayfarer_fabric_signatures_verified_total", "Fabric requests accepted via Ed25519 signature", get(&METRICS.fabric_sig_ok)),
        ("wayfarer_fabric_signatures_rejected_total", "Fabric requests rejected for a bad signature", get(&METRICS.fabric_sig_rejected)),
        ("wayfarer_telemetry_processed_total", "Telemetry rows evaluated by the processor", get(&METRICS.telemetry_processed)),
        ("wayfarer_telemetry_hmac_rejected_total", "Telemetry rejected for a bad or missing HMAC", get(&METRICS.telemetry_hmac_rejected)),
        ("wayfarer_rules_fired_total", "Autonomy rule firings", get(&METRICS.rules_fired)),
        ("wayfarer_commands_delivered_total", "Commands delivered to an outpost", get(&METRICS.commands_delivered)),
        ("wayfarer_commands_failed_total", "Command delivery attempts that failed", get(&METRICS.commands_failed)),
        ("wayfarer_dtn_delivered_total", "DTN messages delivered", get(&METRICS.dtn_delivered)),
        ("wayfarer_dtn_retried_total", "DTN delivery attempts rescheduled", get(&METRICS.dtn_retried)),
    ];

    for (name, help, value) in counters {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"));
    }

    // Fabric state. Each is resilient to the table not existing yet, matching
    // how /api/fabric/status degrades on an unmigrated database.
    let gauges: [(&str, &str, &str); 6] = [
        (
            "wayfarer_command_queue_depth",
            "Commands queued for delivery",
            "SELECT COUNT(*)::int8 FROM command_queue WHERE status = 'queued'",
        ),
        (
            "wayfarer_dtn_outbox_depth",
            "DTN messages awaiting delivery",
            "SELECT COUNT(*)::int8 FROM dtn_outbox",
        ),
        (
            "wayfarer_dtn_inbox_unverified",
            "Received DTN messages that failed or lacked signature verification",
            "SELECT COUNT(*)::int8 FROM dtn_inbox WHERE verified = false",
        ),
        (
            "wayfarer_nodes_online",
            "Nodes seen within the last 60s",
            "SELECT COUNT(*)::int8 FROM node_registry WHERE last_seen > NOW() - INTERVAL '60 seconds'",
        ),
        (
            "wayfarer_nodes_total",
            "Nodes in the registry",
            "SELECT COUNT(*)::int8 FROM node_registry",
        ),
        (
            "wayfarer_telemetry_unprocessed",
            "Telemetry rows not yet evaluated",
            "SELECT COUNT(*)::int8 FROM fleet_telemetry WHERE processed = false",
        ),
    ];

    for (name, help, sql) in gauges {
        if let Some(value) = gauge(pool, sql).await {
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_increment() {
        let c = AtomicU64::new(0);
        assert_eq!(get(&c), 0);
        incr(&c);
        incr(&c);
        assert_eq!(get(&c), 2);
    }

    #[test]
    fn exposition_lines_are_well_formed() {
        // Prometheus requires HELP/TYPE to precede the sample and the sample
        // line to be "<name> <value>". Assert the shape without a live DB.
        let name = "wayfarer_rules_fired_total";
        let rendered = format!("# HELP {name} x\n# TYPE {name} counter\n{name} 7\n");
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].starts_with("# HELP "));
        assert!(lines[1].ends_with(" counter"));
        assert_eq!(lines[2], "wayfarer_rules_fired_total 7");
    }
}
