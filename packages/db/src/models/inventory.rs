use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::NaiveDateTime;
use sqlx::FromRow;

#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Inventory {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub quantity: i32,
    pub location: Option<String>,
    pub token_id: Option<String>,
    pub token_image_url: Option<String>,
    pub category: String,        // ✅ new
    pub unit: String,            // ✅ new
    pub threshold: i32,          // ✅ new
    pub created_at: NaiveDateTime,
}

