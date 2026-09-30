use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect},
    routing::get,
    Json, Router,
};
use chrono::Utc;
use jsonwebtoken::{encode, EncodingKey, Header};
use oauth2::{AuthorizationCode, CsrfToken, TokenResponse};
use serde::Deserialize;
use uuid::Uuid;
use std::collections::HashMap;

use crate::{
    AppState,
    routes::auth_middleware::Claims,
};

/// Holds configured OAuth2 clients and JWT secret
#[derive(Clone)]
pub struct AuthState {
    pub clients: HashMap<String, oauth2::basic::BasicClient>,
    pub jwt_secret: String,
}

/// Auth routes (mounted at `/auth`)
pub fn auth_routes() -> Router<AppState> {
    Router::new()
        .route("/login/:provider", get(login_handler))
        .route("/callback/:provider", get(callback_handler))
}

/// Start login flow: redirect to provider's OAuth2 authorize URL
async fn login_handler(
    Path(provider): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(client) = state.auth.clients.get(&provider) else {
        return (StatusCode::BAD_REQUEST, "Unknown provider").into_response();
    };

    let (auth_url, _csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .url();

    Redirect::temporary(auth_url.as_str()).into_response()
}

/// Query params returned from OAuth2 callback
#[derive(Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

/// Minimal GitHub user response
#[derive(Deserialize)]
struct GitHubUser {
    login: String,
    email: Option<String>, // sometimes null
}

/// GitHub email object
#[derive(Deserialize)]
struct GitHubEmail {
    email: String,
    primary: bool,
    verified: bool,
}

/// Handle OAuth2 callback: exchange code for token, ensure user exists, issue JWT
async fn callback_handler(
    Path(provider): Path<String>,
    Query(query): Query<CallbackQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let Some(client) = state.auth.clients.get(&provider) else {
        return (StatusCode::BAD_REQUEST, "Unknown provider").into_response();
    };

    // 🔑 Exchange code for access token
    let token_result = client
        .exchange_code(AuthorizationCode::new(query.code))
        .request_async(oauth2::reqwest::async_http_client)
        .await;

    match token_result {
        Ok(token) => {
            let access_token = token.access_token().secret().to_string();

            // 🔎 Get user info
            let (username, email) = if provider == "github" {
                let client = reqwest::Client::new();

                // Get /user
                let user_resp = client
                    .get("https://api.github.com/user")
                    .bearer_auth(&access_token)
                    .header("User-Agent", "tid-as-one-app")
                    .send()
                    .await;

                let mut login = "github_user".to_string();
                let mut email_val = format!("{}@example.com", provider);

                if let Ok(resp) = user_resp {
                    if let Ok(user) = resp.json::<GitHubUser>().await {
                        login = user.login;
                        if let Some(e) = user.email {
                            email_val = e;
                        } else {
                            // fallback: call /user/emails
                            if let Ok(email_resp) = client
                                .get("https://api.github.com/user/emails")
                                .bearer_auth(&access_token)
                                .header("User-Agent", "tid-as-one-app")
                                .send()
                                .await
                            {
                                if let Ok(emails) = email_resp.json::<Vec<GitHubEmail>>().await {
                                    if let Some(primary) = emails.into_iter().find(|e| e.primary) {
                                        email_val = primary.email;
                                    }
                                }
                            }
                        }
                    }
                }

                (login, email_val)
            } else {
                // other providers stub
                (
                    format!("{}_user", provider),
                    format!("{}@example.com", provider),
                )
            };

            // Generate deterministic UUID for this user
            let provider_user_id = format!("{}-{}", provider, username);
            let user_id = Uuid::new_v5(&Uuid::NAMESPACE_OID, provider_user_id.as_bytes());

            // Ensure user exists in DB. New rows take the schema default
            // role ('user'); an existing row keeps whatever role it was
            // granted — OAuth identity must never re-grant or downgrade it.
            let _ = sqlx::query!(
                r#"
                INSERT INTO users (id, username, email, created_at)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (id) DO NOTHING
                "#,
                user_id,
                username,
                email,
                Utc::now().naive_utc(),
            )
            .execute(&state.db)
            .await;

            // Read the stored role back — it, not the login path, decides
            // whether this token reaches AdminUser routes. Any lookup failure
            // falls back to the least privilege.
            let role: String = sqlx::query_scalar("SELECT role FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_optional(&state.db)
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| "user".to_string());

            // Build JWT claims
            let claims = Claims {
                sub: user_id.to_string(),
                exp: (Utc::now() + chrono::Duration::hours(24)).timestamp() as usize,
                provider: provider.clone(),
                role,
            };

            // Encode JWT
            match encode(
                &Header::default(),
                &claims,
                &EncodingKey::from_secret(state.auth.jwt_secret.as_bytes()),
            ) {
                Ok(jwt) => Json(serde_json::json!({ "jwt": jwt })).into_response(),
                Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "JWT encoding failed").into_response(),
            }
        }
        Err(_) => (StatusCode::BAD_REQUEST, "Token exchange failed").into_response(),
    }
}

