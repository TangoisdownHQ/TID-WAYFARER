use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::NaiveDateTime;
use sqlx::FromRow;

#[derive(Debug, Serialize, Deserialize, FromRow, Clone)]
pub struct Assignment {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub package_id: Option<Uuid>,
    pub inventory_id: Option<Uuid>,
    pub schedule_start: Option<NaiveDateTime>,
    pub schedule_end: Option<NaiveDateTime>,
    pub recurrence_rule: Option<String>,
    pub eta: Option<NaiveDateTime>,
    pub completed_at: Option<NaiveDateTime>,
    pub status: String,
    pub created_at: NaiveDateTime,
}
