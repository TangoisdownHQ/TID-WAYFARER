use axum::{
    extract::State,
    http::StatusCode,
    routing::get,
    Json, Router,
};
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
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
        .route("/wallet", get(get_wallet).put(put_wallet))
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
