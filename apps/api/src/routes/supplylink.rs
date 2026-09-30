use axum::{
    routing::{get, post, put, delete},
    Router, Json,
    extract::{Path, State},
    http::StatusCode,
};
use uuid::Uuid;
use chrono::Utc;

use crate::{
    AppState,
    routes::auth_middleware::{AuthenticatedUser, AdminUser},
};
use core_db::models::{Assignment, NewAssignment};

/// Mount all supplylink (assignment) routes
pub fn supplylink_routes() -> Router<AppState> {
    Router::new()
        .route("/assignments", post(create_assignment).get(list_user_assignments))
        .route("/assignments/all", get(list_all_assignments)) // 🚨 admin-only
        .route(
            "/assignments/:id",
            get(get_assignment).put(update_assignment).delete(delete_assignment),
        )
        .route("/assignments/:id/complete", put(complete_assignment))
}

/// Create a new assignment
pub async fn create_assignment(
    AuthenticatedUser(_user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewAssignment>,
) -> Result<Json<Assignment>, StatusCode> {
    let assignment = sqlx::query_as!(
        Assignment,
        r#"
        INSERT INTO assignments 
        (id, asset_id, package_id, inventory_id, schedule_start, schedule_end, recurrence_rule, eta, status, created_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'scheduled', $9)
        RETURNING id, asset_id, package_id, inventory_id, schedule_start, schedule_end, recurrence_rule, eta, completed_at, status, created_at
        "#,
        Uuid::new_v4(),
        payload.asset_id,
        payload.package_id,
        payload.inventory_id,
        payload.schedule_start,
        payload.schedule_end,
        payload.recurrence_rule,
        payload.eta,
        Utc::now().naive_utc(),
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assignment))
}

/// List assignments for the authenticated user
pub async fn list_user_assignments(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Assignment>>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let assignments = sqlx::query_as!(
        Assignment,
        r#"
        SELECT a.*
        FROM assignments a
        JOIN assets s ON a.asset_id = s.id
        WHERE s.owner_id = $1
        ORDER BY a.created_at DESC
        "#,
        user_id
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assignments))
}

/// 🚨 List ALL assignments (admin only)
pub async fn list_all_assignments(
    AdminUser(_admin): AdminUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Assignment>>, StatusCode> {
    let assignments = sqlx::query_as::<_, Assignment>(
        "SELECT * FROM assignments ORDER BY created_at DESC"
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(assignments))
}

/// Get a single assignment (owner only)
pub async fn get_assignment(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Assignment>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let assignment = sqlx::query_as!(
        Assignment,
        r#"
        SELECT a.*
        FROM assignments a
        JOIN assets s ON a.asset_id = s.id
        WHERE a.id = $1 AND s.owner_id = $2
        "#,
        id,
        user_id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match assignment {
        Some(a) => Ok(Json(a)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Update an assignment (owner only)
pub async fn update_assignment(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewAssignment>, // reuse same struct for now
) -> Result<Json<Assignment>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let assignment = sqlx::query_as!(
        Assignment,
        r#"
        UPDATE assignments a
        SET asset_id = $3,
            package_id = $4,
            inventory_id = $5,
            schedule_start = $6,
            schedule_end = $7,
            recurrence_rule = $8,
            eta = $9
        FROM assets s
        WHERE a.id = $1 AND a.asset_id = s.id AND s.owner_id = $2
        RETURNING a.id, a.asset_id, a.package_id, a.inventory_id, a.schedule_start, a.schedule_end,
                  a.recurrence_rule, a.eta, a.completed_at, a.status, a.created_at
        "#,
        id,
        user_id,
        payload.asset_id,
        payload.package_id,
        payload.inventory_id,
        payload.schedule_start,
        payload.schedule_end,
        payload.recurrence_rule,
        payload.eta
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match assignment {
        Some(a) => Ok(Json(a)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Mark assignment as complete (owner only)
pub async fn complete_assignment(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Assignment>, StatusCode> {
    let user_id = Uuid::parse_str(&user.sub).map_err(|_| StatusCode::UNAUTHORIZED)?;

    let assignment = sqlx::query_as!(
        Assignment,
        r#"
        UPDATE assignments a
        SET status = 'completed', completed_at = $3
        FROM assets s
        WHERE a.id = $1 AND a.asset_id = s.id AND s.owner_id = $2
        RETURNING a.id, a.asset_id, a.package_id, a.inventory_id, a.schedule_start, a.schedule_end,
                  a.recurrence_rule, a.eta, a.completed_at, a.status, a.created_at
        "#,
        id,
        user_id,
        Utc::now().naive_utc(),
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match assignment {
        Some(a) => Ok(Json(a)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Delete an assignment (admin only 🚨)
pub async fn delete_assignment(
    AdminUser(_admin): AdminUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let rows = sqlx::query!("DELETE FROM assignments WHERE id = $1", id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .rows_affected();

    if rows == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("Assignment deleted"))
    }
}

