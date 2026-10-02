// apps/api/src/lib.rs
pub mod app;
pub mod routes;
pub mod services;

use sqlx::PgPool;

use crate::routes::{auth, commsec};
use crate::services::identity::NodeIdentity;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub auth: auth::AuthState,
    pub commsec: commsec::CommSecState,
    /// This outpost's node identity (UUID + Ed25519 keypair), loaded once at
    /// boot. Never expose `identity.secret_key` in a response.
    pub identity: NodeIdentity,
}
