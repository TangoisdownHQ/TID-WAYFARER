use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;
use chrono::NaiveDateTime;

#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub role: String,                // ✅ must exist
    pub nft_token_id: Option<String>,
    pub nft_image_url: Option<String>,
    pub identity_hash: Option<String>,
    pub created_at: NaiveDateTime,
}

