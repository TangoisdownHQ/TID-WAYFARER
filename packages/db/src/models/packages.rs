use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::NaiveDateTime;
use sqlx::FromRow;

#[derive(Debug, Serialize, Deserialize, FromRow)]
pub struct Package {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub inventory_item_id: Option<Uuid>,   // 🔗 link to inventory item
    pub description: Option<String>,
    pub status: String,                    // pending, in-transit, delivered
    pub location: Option<String>,
    pub nft_token: Option<String>,         // optional NFT token ID
    pub nft_image_url: Option<String>,     // optional NFT image
    pub eta: Option<NaiveDateTime>,        // estimated arrival
    pub created_at: NaiveDateTime,
    pub completed_at: Option<NaiveDateTime>, // when marked delivered
}

