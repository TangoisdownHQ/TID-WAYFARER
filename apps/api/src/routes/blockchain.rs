use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use uuid::Uuid;

use crate::AppState;

#[derive(Deserialize)]
pub struct NewAlert {
    pub chain: String,
    pub threat: String,
    pub address: Option<String>,
    pub tx_hash: Option<String>,
    pub severity: Option<String>,
    pub context: Value,            // jsonb in DB
    pub details: Option<Value>,    // will be stored as text(JSON)
    pub reported_by: Option<Uuid>, // nullable uuid
}

#[derive(Serialize)]
pub struct AlertAck {
    pub id: Uuid,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct AlertRow {
    pub id: String,
    pub chain: String,
    pub threat: String,
    pub address: Option<String>,
    pub tx_hash: Option<String>,
    pub severity: String,
    pub context: Value,
    pub details: Value,
    pub reported_by: Option<String>,
    pub created_at: DateTime<Utc>,
}

pub fn blockchain_routes() -> Router<AppState> {
    Router::new().route("/alerts", post(ingest_alert).get(list_alerts))
}

pub async fn ingest_alert(
    State(state): State<AppState>,
    Json(a): Json<NewAlert>,
) -> Json<AlertAck> {
    let id = Uuid::new_v4();

    // store details as JSON text in the text column
    let details_text: Option<String> = a.details.as_ref().map(|v| v.to_string());

    // reuse `threat` as `alert_type` for now
    let alert_type = a.threat.clone();

    sqlx::query!(
        r#"
        INSERT INTO blockchain_alerts 
            (id, chain, alert_type, threat, address, tx_hash, severity, context, reported_by, details)
        VALUES 
            ($1, $2, $3, $4, $5, $6, COALESCE($7, 'medium'), $8, $9, COALESCE($10, '{}'::text))
        "#,
        id,
        a.chain,
        alert_type,
        a.threat,
        a.address,
        a.tx_hash,
        a.severity,
        a.context,
        a.reported_by,
        details_text
    )
    .execute(&state.db)
    .await
    .unwrap();

    Json(AlertAck {
        id,
        created_at: Utc::now().to_rfc3339(),
    })
}

pub async fn list_alerts(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Vec<AlertRow>> {
    let chain_param: Option<&str> = q.get("chain").map(|s| s.as_str());

    let rows = sqlx::query!(
        r#"
        SELECT 
            id::text AS "id?",
            chain,
            threat,
            address,
            tx_hash,
            severity,
            context as "context!",
            details as "details?",
            reported_by::text AS "reported_by?",
            created_at as "created_at!"
        FROM blockchain_alerts
        WHERE ($1::text IS NULL OR chain = $1)
        ORDER BY created_at DESC
        LIMIT 200
        "#,
        chain_param
    )
    .fetch_all(&state.db)
    .await
    .unwrap();

    Json(
        rows
            .into_iter()
            .map(|r| {
                // id is from id::text AS "id?", so SQLx sees Option<String>
                let id = r.id.unwrap_or_default();

                // threat can be NULL; default "unknown"
                let threat = r.threat.unwrap_or_else(|| "unknown".to_string());

                // details is TEXT nullable; treat as JSON string, default {}
                let details_value: Value = r
                    .details
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or_else(|| json!({}));

                AlertRow {
                    id,
                    chain: r.chain,
                    threat,
                    address: r.address,
                    tx_hash: r.tx_hash,
                    severity: r.severity,
                    context: r.context, // jsonb -> Value
                    details: details_value,
                    reported_by: r.reported_by,
                    created_at: r.created_at,
                }
            })
            .collect(),
    )
}

