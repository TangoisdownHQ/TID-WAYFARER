use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::NaiveDateTime;
use sqlx::FromRow;

#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Asset {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub location: String,
    pub status: String,
    pub nft_token: Option<String>,
    pub nft_image_url: Option<String>,
    pub created_at: Option<NaiveDateTime>, // ✅ optional
    pub updated_at: Option<NaiveDateTime>, // ✅ optional
}

