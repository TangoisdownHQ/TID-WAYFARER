//! CRUD for autonomy_rules — the policies the rules engine evaluates against
//! incoming telemetry. Reads need any authenticated principal (the group sits
//! behind require_auth); mutations require an admin JWT.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::routes::auth_middleware::AdminUser;
use crate::AppState;

const ALLOWED_METRICS: &[&str] = &[
    "temperature",
    "anomaly_score",
    "speed",
    "battery",
    "signal_db",
    "heading",
];
const ALLOWED_OPS: &[&str] = &["gt", "gte", "lt", "lte", "eq"];

pub fn rules_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_rules).post(create_rule))
        .route("/:id", get(get_rule).put(update_rule).delete(delete_rule))
}

fn rule_to_json(r: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "name": r.get::<String, _>("name"),
        "description": r.get::<Option<String>, _>("description"),
        "enabled": r.get::<bool, _>("enabled"),
        "metric": r.get::<String, _>("metric"),
        "op": r.get::<String, _>("op"),
        "threshold": r.get::<f64, _>("threshold"),
        "command": r.get::<Value, _>("command"),
        "event_kind": r.get::<String, _>("event_kind"),
        "severity": r.get::<String, _>("severity"),
        "cooldown_secs": r.get::<i32, _>("cooldown_secs"),
        "created_at": r.get::<chrono::DateTime<chrono::Utc>, _>("created_at").to_rfc3339(),
        "updated_at": r.get::<chrono::DateTime<chrono::Utc>, _>("updated_at").to_rfc3339(),
    })
}

const RULE_COLUMNS: &str =
    "id, name, description, enabled, metric, op, threshold, command, \
     event_kind, severity, cooldown_secs, created_at, updated_at";

async fn list_rules(State(state): State<AppState>) -> Result<Json<Value>, (StatusCode, String)> {
    let rows = sqlx::query(&format!(
        "SELECT {RULE_COLUMNS} FROM autonomy_rules ORDER BY id"
    ))
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    Ok(Json(json!(rows.iter().map(rule_to_json).collect::<Vec<_>>())))
}

async fn get_rule(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let row = sqlx::query(&format!(
        "SELECT {RULE_COLUMNS} FROM autonomy_rules WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "rule not found".to_string()))?;

    Ok(Json(rule_to_json(&row)))
}

#[derive(Deserialize)]
struct CreateRule {
    name: String,
    description: Option<String>,
    metric: String,
    op: String,
    threshold: f64,
    command: Value,
    event_kind: Option<String>,
    severity: Option<String>,
    cooldown_secs: Option<i32>,
    enabled: Option<bool>,
}

async fn create_rule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<CreateRule>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, String)> {
    validate_metric_op(&body.metric, &body.op)?;

    let row = sqlx::query(&format!(
        r#"
        INSERT INTO autonomy_rules
            (name, description, metric, op, threshold, command,
             event_kind, severity, cooldown_secs, enabled)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
        RETURNING {RULE_COLUMNS}
        "#
    ))
    .bind(&body.name)
    .bind(&body.description)
    .bind(&body.metric)
    .bind(&body.op)
    .bind(body.threshold)
    .bind(&body.command)
    .bind(body.event_kind.as_deref().unwrap_or("rule_fired"))
    .bind(body.severity.as_deref().unwrap_or("warn"))
    .bind(body.cooldown_secs.unwrap_or(300))
    .bind(body.enabled.unwrap_or(true))
    .fetch_one(&state.db)
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("insert failed: {e}")))?;

    Ok((StatusCode::CREATED, Json(rule_to_json(&row))))
}

#[derive(Deserialize)]
struct UpdateRule {
    description: Option<String>,
    metric: Option<String>,
    op: Option<String>,
    threshold: Option<f64>,
    command: Option<Value>,
    event_kind: Option<String>,
    severity: Option<String>,
    cooldown_secs: Option<i32>,
    enabled: Option<bool>,
}

async fn update_rule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateRule>,
) -> Result<Json<Value>, (StatusCode, String)> {
    if let Some(metric) = &body.metric {
        validate_metric_op(metric, body.op.as_deref().unwrap_or("gt"))?;
    }
    if let Some(op) = &body.op {
        if !ALLOWED_OPS.contains(&op.as_str()) {
            return Err((StatusCode::BAD_REQUEST, format!("unknown op '{op}'")));
        }
    }

    let row = sqlx::query(&format!(
        r#"
        UPDATE autonomy_rules SET
            description   = COALESCE($2, description),
            metric        = COALESCE($3, metric),
            op            = COALESCE($4, op),
            threshold     = COALESCE($5, threshold),
            command       = COALESCE($6, command),
            event_kind    = COALESCE($7, event_kind),
            severity      = COALESCE($8, severity),
            cooldown_secs = COALESCE($9, cooldown_secs),
            enabled       = COALESCE($10, enabled),
            updated_at    = NOW()
        WHERE id = $1
        RETURNING {RULE_COLUMNS}
        "#
    ))
    .bind(id)
    .bind(&body.description)
    .bind(&body.metric)
    .bind(&body.op)
    .bind(body.threshold)
    .bind(&body.command)
    .bind(&body.event_kind)
    .bind(&body.severity)
    .bind(body.cooldown_secs)
    .bind(body.enabled)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::BAD_REQUEST, format!("update failed: {e}")))?
    .ok_or((StatusCode::NOT_FOUND, "rule not found".to_string()))?;

    Ok(Json(rule_to_json(&row)))
}

async fn delete_rule(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, (StatusCode, String)> {
    let result = sqlx::query("DELETE FROM autonomy_rules WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    if result.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "rule not found".to_string()));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validate_metric_op(metric: &str, op: &str) -> Result<(), (StatusCode, String)> {
    if !ALLOWED_METRICS.contains(&metric) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown metric '{metric}' (allowed: {ALLOWED_METRICS:?})"),
        ));
    }
    if !ALLOWED_OPS.contains(&op) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown op '{op}' (allowed: {ALLOWED_OPS:?})"),
        ));
    }
    Ok(())
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}
