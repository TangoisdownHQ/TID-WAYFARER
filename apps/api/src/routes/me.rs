use axum::{
    extract::State,
    http::StatusCode,
    routing::get,
    Json, Router,
};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AuthenticatedUser, Caller};
use crate::services::org_scope;
use crate::routes::auth_middleware::Claims;
use crate::services::settlement::is_plausible_solana_address;
use crate::AppState;

/// Response returned from `/me`
#[derive(serde::Serialize)]
pub struct MeResponse {
    pub sub: String,
    pub provider: String,
    pub role: String,
    pub exp: usize,
}

/// Build router for `/me`
pub fn me_routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(me_handler))
        .route("/profile", get(get_profile))
        .route("/wallet", get(get_wallet).put(put_wallet))
}

/// The caller's own account, as the UI needs it: who they are, what they came
/// here to do, and whether they can be paid.
#[derive(serde::Serialize)]
pub struct ProfileResponse {
    pub user_id: Uuid,
    pub username: String,
    pub email: String,
    pub role: String,
    /// buyer | seller | both
    pub account_type: String,
    pub wallet_address: Option<String>,
    /// A seller with no payout wallet cannot be named as the payee on a
    /// settlement, so the marketplace refuses to record one. Surfacing it here
    /// lets the UI prompt before that happens rather than after.
    pub needs_payout_wallet: bool,
    /// The organisation this caller is currently acting for. Every read of
    /// business data is filtered by it, so a user who cannot see it cannot
    /// explain why a page is empty.
    pub org_id: Option<Uuid>,
    pub org_name: Option<String>,
    /// How many organisations this person belongs to. More than one means the
    /// resolved org is the earliest they joined, which is deterministic but
    /// worth saying out loud rather than leaving them to guess.
    pub org_count: i64,
}

/// GET /api/me/profile
async fn get_profile(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Caller(principal): Caller,
) -> Result<Json<ProfileResponse>, (StatusCode, String)> {
    let user_id = uid(&claims)?;

    let org_id = org_scope::caller_org(&state, &principal).await;
    let org_name: Option<String> = match org_id {
        Some(id) => sqlx::query_scalar("SELECT name FROM organisations WHERE id = $1")
            .bind(id).fetch_optional(&state.db).await.ok().flatten(),
        None => None,
    };
    let org_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM org_members WHERE user_id = $1")
        .bind(user_id).fetch_one(&state.db).await.unwrap_or(0);

    let row = sqlx::query(
        "SELECT username, email, role, account_type, wallet_address FROM users WHERE id = $1",
    )
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|e| {
        tracing::error!(error = %e, "profile lookup failed");
        (StatusCode::INTERNAL_SERVER_ERROR, "profile lookup failed".to_string())
    })?
    .ok_or((StatusCode::NOT_FOUND, "no such user".to_string()))?;

    let account_type: String = row.get("account_type");
    let wallet_address: Option<String> = row.get("wallet_address");

    Ok(Json(ProfileResponse {
        user_id,
        username: row.get("username"),
        email: row.get("email"),
        role: row.get("role"),
        needs_payout_wallet: matches!(account_type.as_str(), "seller" | "both")
            && wallet_address.is_none(),
        account_type,
        wallet_address,
        org_id,
        org_name,
        org_count,
    }))
}

async fn me_handler(AuthenticatedUser(claims): AuthenticatedUser) -> Json<MeResponse> {
    Json(MeResponse {
        sub: claims.sub,
        provider: claims.provider,
        role: claims.role,
        exp: claims.exp,
    })
}

#[derive(serde::Serialize)]
pub struct WalletResponse {
    /// The payout address, or null if the user has not set one.
    pub wallet_address: Option<String>,
    /// Whether this user can be paid in a way the verifier can confirm. A
    /// settlement to a user with no wallet on file cannot be verified, so the
    /// marketplace refuses to record one.
    pub can_receive_settlement: bool,
}

#[derive(serde::Deserialize)]
pub struct SetWallet {
    pub wallet_address: String,
}

fn uid(claims: &Claims) -> Result<Uuid, (StatusCode, String)> {
    Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "token subject is not a user id".into()))
}

/// GET /api/me/wallet — the caller's own payout address.
async fn get_wallet(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
) -> Result<Json<WalletResponse>, (StatusCode, String)> {
    let user_id = uid(&claims)?;

    let wallet: Option<String> =
        sqlx::query_scalar("SELECT wallet_address FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "wallet lookup failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "wallet lookup failed".to_string())
            })?
            .flatten();

    Ok(Json(WalletResponse {
        can_receive_settlement: wallet.is_some(),
        wallet_address: wallet,
    }))
}

/// PUT /api/me/wallet — set the caller's own payout address.
///
/// A user may only set their own: the id comes from the verified token, never
/// from the body. The address is validated offline (base58, 32 bytes) so a
/// disconnected outpost still rejects a typo rather than storing a payee that
/// can never be credited.
async fn put_wallet(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Json(body): Json<SetWallet>,
) -> Result<Json<WalletResponse>, (StatusCode, String)> {
    let user_id = uid(&claims)?;
    let addr = body.wallet_address.trim().to_string();

    if !is_plausible_solana_address(&addr) {
        return Err((
            StatusCode::BAD_REQUEST,
            "not a valid Solana address (expected 32-44 base58 characters)".to_string(),
        ));
    }

    let updated = sqlx::query("UPDATE users SET wallet_address = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&addr)
        .execute(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "wallet update failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "wallet update failed".to_string())
        })?;

    if updated.rows_affected() == 0 {
        return Err((StatusCode::NOT_FOUND, "no such user".to_string()));
    }

    tracing::info!(user_id = %user_id, "payout wallet set");

    Ok(Json(WalletResponse {
        wallet_address: Some(addr),
        can_receive_settlement: true,
    }))
}
