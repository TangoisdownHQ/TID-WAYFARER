use axum::{
    routing::{get, post, put, delete},
    Router, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use chrono::NaiveDateTime;

use crate::{
    AppState,
    routes::auth_middleware::{AuthenticatedUser, AdminUser},
};

#[derive(Serialize, Deserialize, sqlx::FromRow)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub email: String,
    pub created_at: NaiveDateTime,
}

#[derive(Deserialize)]
pub struct NewUser {
    pub username: String,
    pub email: String,
}

/// Mount user routes
pub fn user_routes() -> Router<AppState> {
    Router::new()
        .route("/users", get(get_users).post(create_user))
        .route("/users/:id", get(get_user).put(update_user).delete(delete_user))
}

/// List all users
pub async fn get_users(State(state): State<AppState>) -> Json<Vec<User>> {
    let users = sqlx::query_as::<_, User>(
        "SELECT * FROM users ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Json(users)
}

/// Get single user by ID
pub async fn get_user(
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<User>, StatusCode> {
    let user = sqlx::query_as::<_, User>(
        "SELECT * FROM users WHERE id = $1"
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match user {
        Some(u) => Ok(Json(u)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Create new user (requires authentication)
pub async fn create_user(
    AuthenticatedUser(_user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewUser>,
) -> Result<Json<User>, StatusCode> {
    let user = sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (id, username, email)
        VALUES ($1, $2, $3)
        RETURNING id, username, email, created_at
        "#,
        Uuid::new_v4(),
        payload.username,
        payload.email,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(user))
}

/// Update existing user (requires authentication)
pub async fn update_user(
    AuthenticatedUser(_user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewUser>,
) -> Result<Json<User>, StatusCode> {
    let user = sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET username = $2, email = $3
        WHERE id = $1
        RETURNING id, username, email, created_at
        "#,
        id,
        payload.username,
        payload.email,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match user {
        Some(u) => Ok(Json(u)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Delete user (admin only 🚨)
pub async fn delete_user(
    AdminUser(_admin): AdminUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let rows_affected = sqlx::query!("DELETE FROM users WHERE id = $1", id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .rows_affected();

    if rows_affected == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("User deleted"))
    }
}

