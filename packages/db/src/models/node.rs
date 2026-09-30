use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, sqlx::FromRow)]
pub struct Node {
    pub node_id: Uuid,
    pub name: String,
    pub api_endpoint: String,
    pub public_key: String,
    pub location: Option<String>,
    pub last_seen: Option<DateTime<Utc>>,
    pub status: Option<String>,
}

