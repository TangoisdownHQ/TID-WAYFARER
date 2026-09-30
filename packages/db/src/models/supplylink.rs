use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use chrono::NaiveDateTime;

#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Assignment {
    pub id: Uuid,
    pub asset_id: Uuid,
    pub package_id: Option<Uuid>,
    pub inventory_id: Option<Uuid>,
    pub schedule_start: Option<NaiveDateTime>,
    pub schedule_end: Option<NaiveDateTime>,
    pub recurrence_rule: Option<String>,      // ✅ recurring schedules
    pub eta: Option<NaiveDateTime>,           // ✅ ETA prediction
    pub completed_at: Option<NaiveDateTime>,  // ✅ lifecycle tracking
    pub status: String,                       // scheduled, in_progress, completed
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Deserialize)]
pub struct NewAssignment {
    pub asset_id: Uuid,
    pub package_id: Option<Uuid>,
    pub inventory_id: Option<Uuid>,
    pub schedule_start: Option<NaiveDateTime>,
    pub schedule_end: Option<NaiveDateTime>,
    pub recurrence_rule: Option<String>,      // ✅ allow setting recurrence
    pub eta: Option<NaiveDateTime>,           // ✅ allow setting ETA
}

