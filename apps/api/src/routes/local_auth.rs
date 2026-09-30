use axum::{
    routing::post,
    Json, Router, extract::State,
};
use jsonwebtoken::{encode, Header, EncodingKey};
use serde::{Serialize, Deserialize};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use sqlx::Row;

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, rand_core::OsRng, PasswordHash};

use crate::AppState;
use crate::routes::auth_middleware::Claims;

#[derive(Deserialize)]
pub struct RegisterPayload {
    pub username: String,
    pub email: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct LoginPayload {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct JwtResponse {
    pub token: String,
}

/// Local auth routes
pub fn local_auth_routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register_user))
        .route("/login", post(login_user))
}

/// Register a new user
async fn register_user(
    State(app_state): State<AppState>,
    Json(payload): Json<RegisterPayload>,
) -> Result<Json<&'static str>, String> {
    // Hash password with Argon2
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2.hash_password(payload.password.as_bytes(), &salt)
        .map_err(|_| "Password hashing failed".to_string())?
        .to_string();

    let new_id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO users (id, username, email, identity_hash) VALUES ($1, $2, $3, $4)"
    )
    .bind(new_id)
    .bind(&payload.username)
    .bind(&payload.email)
    .bind(&hash)
    .execute(&app_state.db)
    .await
    .map_err(|e| format!("DB error: {}", e))?;

    Ok(Json("✅ User registered"))
}

/// Login a user and return JWT
async fn login_user(
    State(app_state): State<AppState>,
    Json(payload): Json<LoginPayload>,
) -> Result<Json<JwtResponse>, String> {
    // Lookup user. `role` is authoritative here — it decides whether the
    // minted token can reach AdminUser-gated routes.
    let row = sqlx::query(
        "SELECT id, identity_hash, role FROM users WHERE email = $1"
    )
    .bind(&payload.email)
    .fetch_optional(&app_state.db)
    .await
    .map_err(|e| format!("DB error: {}", e))?;

    let (user_id, stored_hash, role): (Uuid, String, String) = match row {
        Some(r) => (r.get("id"), r.get("identity_hash"), r.get("role")),
        None => return Err("❌ Invalid credentials".into()),
    };

    // Verify password
    let parsed_hash = PasswordHash::new(&stored_hash)
        .map_err(|_| "❌ Invalid stored hash".to_string())?;

    let argon2 = Argon2::default();
    let valid = argon2.verify_password(payload.password.as_bytes(), &parsed_hash).is_ok();

    if !valid {
        return Err("❌ Invalid credentials".into());
    }

    // Expiry: 1 hour
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
        + 3600;

    let claims = Claims {
        sub: user_id.to_string(),
        exp,
        provider: "local".into(),
        role,
    };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(app_state.auth.jwt_secret.as_bytes()),
    ).map_err(|_| "❌ JWT encode failed".to_string())?;

    Ok(Json(JwtResponse { token }))
}

