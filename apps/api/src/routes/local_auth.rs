//! Local email/password auth.
//!
//! Both handlers previously returned `Result<_, String>`, and axum renders a
//! bare `String` as **200 OK** — so "invalid credentials" and "duplicate email"
//! came back as successes with the failure written in the body. Anything
//! checking the status code (a client, a probe, a rate limiter) saw every
//! attempt succeed. Errors are now typed with real status codes.

use axum::{
    extract::State,
    http::StatusCode,
    routing::post,
    Json, Router,
};
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use argon2::password_hash::{rand_core::OsRng, PasswordHash, SaltString};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};

use crate::routes::auth_middleware::Claims;
use crate::AppState;

/// Session lifetime. Short because there is no refresh flow yet; a page that
/// polls will simply bounce the operator to sign-in when it lapses.
const TOKEN_TTL_SECS: usize = 3600;

/// Shortest password accepted at signup. Argon2 protects the stored hash; this
/// is only about not accepting something trivially guessable.
const MIN_PASSWORD_LEN: usize = 10;

type ApiError = (StatusCode, String);

#[derive(Deserialize)]
pub struct RegisterPayload {
    pub username: String,
    pub email: String,
    pub password: String,
    /// buyer | seller | both. Absent means buyer.
    ///
    /// Note there is deliberately no `role` field: the security role is always
    /// 'user' at signup and can only be raised by an admin out of band. A
    /// self-service admin flag would make every other guard decorative.
    pub account_type: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginPayload {
    pub email: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct JwtResponse {
    pub token: String,
    /// True when an administrator set this password. The UI sends the operator
    /// straight to a change form; the flag is the only record that the
    /// password was issued rather than chosen, since the temporary one is
    /// shown once and stored only as a hash.
    #[serde(rename = "mustChangePassword")]
    pub must_change_password: bool,
}

/// Signup returns a token as well, so a new account lands signed in rather
/// than being bounced to a login form it just filled in.
#[derive(Serialize)]
pub struct RegisteredResponse {
    pub token: String,
    pub user_id: Uuid,
    pub account_type: String,
    /// True when this account will need a payout wallet before it can be named
    /// as the payee on a settlement. Lets the UI ask now rather than fail later.
    pub needs_payout_wallet: bool,
}

pub fn local_auth_routes() -> Router<AppState> {
    Router::new()
        .route("/register", post(register_user))
        .route("/login", post(login_user))
}

/// Accepted account types. Anything else is rejected rather than silently
/// coerced, so a typo surfaces at signup instead of becoming a wrong profile.
fn normalise_account_type(raw: Option<&str>) -> Result<String, ApiError> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok("buyer".to_string()),
        Some(v) => match v.to_ascii_lowercase().as_str() {
            t @ ("buyer" | "seller" | "both") => Ok(t.to_string()),
            other => Err((
                StatusCode::BAD_REQUEST,
                format!("account_type must be buyer, seller or both (got '{other}')"),
            )),
        },
    }
}

/// Enough of an email check to catch a typo; real validation is delivery.
fn looks_like_email(s: &str) -> bool {
    let s = s.trim();
    match s.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !s.contains(char::is_whitespace)
        }
        None => false,
    }
}

fn mint_token(state: &AppState, user_id: Uuid, role: &str) -> Result<String, ApiError> {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "clock error".to_string()))?
        .as_secs() as usize
        + TOKEN_TTL_SECS;

    let claims = Claims {
        sub: user_id.to_string(),
        exp,
        provider: "local".into(),
        role: role.to_string(),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "could not issue token".to_string()))
}

/// POST /api/local-auth/register — create a buyer and/or seller account.
async fn register_user(
    State(state): State<AppState>,
    Json(payload): Json<RegisterPayload>,
) -> Result<(StatusCode, Json<RegisteredResponse>), ApiError> {
    let account_type = normalise_account_type(payload.account_type.as_deref())?;
    let username = payload.username.trim().to_string();
    // Stored lowercased: login looks up by email, and a case difference must
    // not create a second account for the same person.
    let email = payload.email.trim().to_ascii_lowercase();

    if username.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "username is required".into()));
    }
    if !looks_like_email(&email) {
        return Err((StatusCode::BAD_REQUEST, "that doesn't look like an email address".into()));
    }
    if payload.password.chars().count() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("password must be at least {MIN_PASSWORD_LEN} characters"),
        ));
    }

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(payload.password.as_bytes(), &salt)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "password hashing failed".to_string()))?
        .to_string();

    let new_id = Uuid::new_v4();

    // role is hardcoded 'user' — never taken from the request.
    let inserted = sqlx::query(
        r#"
        INSERT INTO users (id, username, email, identity_hash, role, account_type)
        VALUES ($1, $2, $3, $4, 'user', $5)
        "#,
    )
    .bind(new_id)
    .bind(&username)
    .bind(&email)
    .bind(&hash)
    .bind(&account_type)
    .execute(&state.db)
    .await;

    if let Err(e) = inserted {
        // Unique violation on email: a real conflict, not a server fault, and
        // the caller should be told which it was.
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return Err((
                    StatusCode::CONFLICT,
                    "an account with that email already exists".into(),
                ));
            }
        }
        tracing::error!(error = %e, "signup insert failed");
        return Err((StatusCode::INTERNAL_SERVER_ERROR, "could not create the account".into()));
    }

    let token = mint_token(&state, new_id, "user")?;
    let needs_payout_wallet = matches!(account_type.as_str(), "seller" | "both");

    tracing::info!(user_id = %new_id, account_type = %account_type, "account created");

    Ok((
        StatusCode::CREATED,
        Json(RegisteredResponse {
            token,
            user_id: new_id,
            account_type,
            needs_payout_wallet,
        }),
    ))
}

/// POST /api/local-auth/login
async fn login_user(
    State(state): State<AppState>,
    Json(payload): Json<LoginPayload>,
) -> Result<Json<JwtResponse>, ApiError> {
    let email = payload.email.trim().to_ascii_lowercase();

    // `role` is authoritative here — it decides whether the minted token can
    // reach AdminUser routes.
    let row = sqlx::query(
        "SELECT id, identity_hash, role, active, must_change_password \
         FROM users WHERE lower(email) = $1",
    )
        .bind(&email)
        .fetch_optional(&state.db)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "login lookup failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "login failed".to_string())
        })?;

    // Same response whether the account is missing or the password is wrong,
    // so this endpoint can't be used to enumerate registered emails.
    let invalid = || (StatusCode::UNAUTHORIZED, "invalid credentials".to_string());

    let Some(r) = row else { return Err(invalid()) };
    let user_id: Uuid = r.get("id");
    let stored_hash: String = r.get("identity_hash");
    let role: String = r.get("role");
    let active: bool = r.get("active");
    let must_change_password: bool = r.get("must_change_password");

    let parsed = PasswordHash::new(&stored_hash).map_err(|_| {
        tracing::error!(user_id = %user_id, "stored password hash is unparseable");
        (StatusCode::INTERNAL_SERVER_ERROR, "login failed".to_string())
    })?;

    if Argon2::default()
        .verify_password(payload.password.as_bytes(), &parsed)
        .is_err()
    {
        return Err(invalid());
    }

    // Checked after the password, not before. A deactivated account gets a
    // specific message because that is genuinely useful to the person
    // holding it — but only once they have proved the password, so the
    // endpoint still cannot be used to enumerate who works here.
    if !active {
        tracing::warn!(user_id = %user_id, "sign-in refused: account deactivated");
        return Err((
            StatusCode::FORBIDDEN,
            "this account has been deactivated; ask an administrator to restore it".into(),
        ));
    }

    // Best-effort: a failure here must not cost someone their session. It
    // exists so "is anyone still using this account?" has an answer before
    // somebody deactivates it.
    if let Err(e) = sqlx::query("UPDATE users SET last_login_at = NOW() WHERE id = $1")
        .bind(user_id)
        .execute(&state.db)
        .await
    {
        tracing::warn!(user_id = %user_id, error = %e, "could not record last_login_at");
    }

    Ok(Json(JwtResponse {
        token: mint_token(&state, user_id, &role)?,
        must_change_password,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_type_defaults_to_buyer_and_accepts_the_three_kinds() {
        assert_eq!(normalise_account_type(None).unwrap(), "buyer");
        assert_eq!(normalise_account_type(Some("")).unwrap(), "buyer");
        assert_eq!(normalise_account_type(Some("Seller")).unwrap(), "seller");
        assert_eq!(normalise_account_type(Some(" BOTH ")).unwrap(), "both");
    }

    #[test]
    fn an_unknown_account_type_is_refused_not_coerced() {
        // Silently defaulting would hand someone a buyer profile when they
        // asked to sell, and they'd find out at settlement time.
        let err = normalise_account_type(Some("admin")).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1.contains("buyer, seller or both"));
    }

    #[test]
    fn email_shape_is_checked() {
        assert!(looks_like_email("ops@tidhq.net"));
        assert!(looks_like_email("a.b+c@sub.example.co.uk"));
        assert!(!looks_like_email("ops"));
        assert!(!looks_like_email("ops@localhost"));
        assert!(!looks_like_email("@tidhq.net"));
        assert!(!looks_like_email("ops@.net"));
        assert!(!looks_like_email("ops @tidhq.net"));
        assert!(!looks_like_email(""));
    }
}
