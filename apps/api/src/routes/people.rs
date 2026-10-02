//! Administering the people in an organisation.
//!
//! Before this, an account could only come into existence by signing itself
//! up. That is the wrong shape for anyone who would actually run this: a depot
//! lead adds three people and says which of them may approve a shipment. They
//! do not ask the warehouse to self-register and then try to work out
//! afterwards who the new accounts belong to.
//!
//! # What an administrator here can and cannot do
//!
//! They can create an account, set its organisation role, reset its password,
//! and deactivate it — but only inside **their own** organisation, resolved
//! through [`org_scope`] rather than taken from the request. The security role
//! (`users.role`, which decides whether a token reaches admin routes) is
//! deliberately *not* settable here: an org administrator runs a company on
//! the outpost, which is a different thing from administering the outpost
//! itself, and conflating them would let any customer's admin reach every
//! other customer's data.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use argon2::password_hash::{rand_core::OsRng, SaltString};
use argon2::{Argon2, PasswordHasher};
use rand::Rng;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::{AuthenticatedUser, Caller};
use crate::services::org_scope::caller_org;
use crate::AppState;

type ApiError = (StatusCode, String);

/// Roles a person can hold *within an organisation*. Distinct from
/// `users.role`, which is the outpost-level security role.
///
///   owner    may administer people, and cannot be removed by an admin
///   admin    may administer people
///   operator may act on inventory, shipments and custody
///   viewer   may read
const ORG_ROLES: [&str; 4] = ["owner", "admin", "operator", "viewer"];

/// Which org roles may administer other people.
fn may_administer(role: &str) -> bool {
    matches!(role, "owner" | "admin")
}

pub fn people_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_people).post(create_person))
        .route("/roles", get(|| async { Json(json!(ORG_ROLES)) }))
        .route("/:id", patch(update_person))
        .route("/:id/password", post(reset_password))
        .route("/:id/active", patch(set_active))
}

/// The caller's org plus their role in it, or a refusal.
///
/// Both halves come from the database, never from the request. The org comes
/// from [`caller_org`], which fails closed; the role comes from the membership
/// row, so someone who was demoted loses the ability on their next call rather
/// than at the end of their session.
async fn admin_context(state: &AppState, caller: &Caller) -> Result<(Uuid, Uuid, String), ApiError> {
    let Caller(principal) = caller;
    let user_id = match principal {
        crate::routes::auth_middleware::Principal::User(claims) => Uuid::parse_str(&claims.sub)
            .map_err(|_| (StatusCode::UNAUTHORIZED, "malformed subject".to_string()))?,
        other => {
            return Err((
                StatusCode::FORBIDDEN,
                format!("{} cannot administer people", other.describe()),
            ))
        }
    };

    let org = caller_org(state, principal)
        .await
        .ok_or((StatusCode::FORBIDDEN, "no organisation for this caller".to_string()))?;

    let role: String = sqlx::query_scalar(
        "SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .unwrap_or_default();

    Ok((org, user_id, role))
}

/// As above, but refuses anyone who may not administer people.
async fn require_admin(state: &AppState, caller: &Caller) -> Result<(Uuid, Uuid), ApiError> {
    let (org, me, role) = admin_context(state, caller).await?;
    if !may_administer(&role) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("your role in this organisation is '{role}'; only an owner or admin may manage people"),
        ));
    }
    Ok((org, me))
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %e, "people route failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "request failed".to_string())
}

/// GET /api/people — everyone in the caller's organisation.
///
/// Readable by any member, not just an admin: knowing who your colleagues are
/// is needed to start a conversation or hand over a shipment. Scoped to the
/// caller's own org, so it is not a directory of everyone on the outpost.
async fn list_people(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<Value>, ApiError> {
    let (org, me, my_role) = admin_context(&state, &caller).await?;

    let rows = sqlx::query(
        r#"
        SELECT u.id, u.username, u.full_name, u.email, u.account_type,
               u.active, u.must_change_password, u.last_login_at, u.created_at,
               m.role AS org_role, m.added_at
        FROM org_members m
        JOIN users u ON u.id = m.user_id
        WHERE m.org_id = $1
        ORDER BY u.active DESC, m.added_at ASC
        "#,
    )
    .bind(org)
    .fetch_all(&state.db)
    .await
    .map_err(internal)?;

    let people: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<Uuid, _>("id"),
                "username": r.get::<String, _>("username"),
                "fullName": r.get::<Option<String>, _>("full_name"),
                "email": r.get::<String, _>("email"),
                "orgRole": r.get::<String, _>("org_role"),
                "accountType": r.get::<Option<String>, _>("account_type"),
                "active": r.get::<bool, _>("active"),
                // Shown so an admin can tell "has not signed in yet" from
                // "has not signed in for a year" before deactivating someone.
                "mustChangePassword": r.get::<bool, _>("must_change_password"),
                "lastLoginAt": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("last_login_at")
                    .map(|d| d.to_rfc3339()),
                "isSelf": r.get::<Uuid, _>("id") == me,
            })
        })
        .collect();

    Ok(Json(json!({
        "orgId": org,
        "myRole": my_role,
        "canManage": may_administer(&my_role),
        "count": people.len(),
        "people": people,
    })))
}

#[derive(Deserialize)]
struct NewPerson {
    email: String,
    username: Option<String>,
    full_name: Option<String>,
    /// One of [`ORG_ROLES`]. Defaults to operator.
    org_role: Option<String>,
    /// buyer | seller | both. Defaults to buyer.
    account_type: Option<String>,
    /// Optional. Omit and one is generated — which is the better path, since a
    /// password typed into a form by an administrator tends to be reused.
    password: Option<String>,
}

/// A readable one-time password.
///
/// Four words from a small list beats a random string of symbols here: it has
/// to survive being read aloud across a loading bay, and a password that gets
/// written on a label to be legible is worse than a longer one that does not.
/// It is temporary in any case — `must_change_password` is set with it.
fn temporary_password() -> String {
    const WORDS: [&str; 32] = [
        "anchor", "ballast", "cargo", "dock", "ember", "fathom", "girder", "harbour",
        "ingot", "jetty", "keel", "lantern", "manifest", "nadir", "orbit", "pallet",
        "quarry", "rigging", "sextant", "tether", "umber", "vessel", "winch", "yardarm",
        "zenith", "basalt", "cinder", "drift", "kelvin", "lumen", "parsec", "quartz",
    ];
    let mut rng = rand::thread_rng();
    let mut out = Vec::with_capacity(4);
    for _ in 0..4 {
        out.push(WORDS[rng.gen_range(0..WORDS.len())]);
    }
    // A digit pair so it also satisfies the kind of policy that insists.
    format!("{}-{:02}", out.join("-"), rng.gen_range(10..100))
}

fn hash_password(plain: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "password hashing failed".to_string()))?
        .to_string())
}

fn check_role(raw: Option<&str>) -> Result<String, ApiError> {
    let role = raw.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("operator");
    if ORG_ROLES.contains(&role) {
        Ok(role.to_string())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            format!("org_role must be one of {}", ORG_ROLES.join(", ")),
        ))
    }
}

/// POST /api/people — create an account inside the caller's organisation.
///
/// The account and the membership are written in one transaction. Split, a
/// failure between them would leave a user who belongs to no organisation —
/// and since every read filters on organisation, that account would sign in
/// successfully and then see nothing, with no way for an admin to find it in
/// any org's member list to fix it.
async fn create_person(
    State(state): State<AppState>,
    caller: Caller,
    Json(body): Json<NewPerson>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let (org, me) = require_admin(&state, &caller).await?;

    let email = body.email.trim().to_ascii_lowercase();
    if email.is_empty() || !email.contains('@') || !email.contains('.') {
        return Err((StatusCode::BAD_REQUEST, "a valid email address is required".into()));
    }
    let org_role = check_role(body.org_role.as_deref())?;
    let account_type = match body.account_type.as_deref().map(str::trim) {
        None | Some("") => "buyer".to_string(),
        Some(t) if matches!(t, "buyer" | "seller" | "both") => t.to_string(),
        Some(other) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("account_type must be buyer, seller or both (got '{other}')"),
            ))
        }
    };

    let username = body
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .unwrap_or_else(|| email.split('@').next().unwrap_or("user").to_string());

    // An administrator-supplied password is still held to the signup minimum;
    // a generated one is well past it.
    let (plain, generated) = match body.password.as_deref() {
        Some(p) if p.chars().count() >= 10 => (p.to_string(), false),
        Some(_) => return Err((StatusCode::BAD_REQUEST, "password must be at least 10 characters".into())),
        None => (temporary_password(), true),
    };
    let hash = hash_password(&plain)?;

    let new_id = Uuid::new_v4();
    let mut tx = state.db.begin().await.map_err(internal)?;

    // `role` is hardcoded 'user'. An org admin administers a company, not the
    // outpost — see the module note.
    let inserted = sqlx::query(
        r#"
        INSERT INTO users (id, username, full_name, email, identity_hash, role,
                           account_type, must_change_password, created_by)
        VALUES ($1, $2, $3, $4, $5, 'user', $6, $7, $8)
        "#,
    )
    .bind(new_id)
    .bind(&username)
    .bind(body.full_name.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    .bind(&email)
    .bind(&hash)
    .bind(&account_type)
    // Anyone the administrator issued a password to must change it; someone
    // who supplied their own chosen password need not.
    .bind(generated)
    .bind(me)
    .execute(&mut *tx)
    .await;

    if let Err(e) = inserted {
        if let sqlx::Error::Database(db) = &e {
            if db.code().as_deref() == Some("23505") {
                return Err((
                    StatusCode::CONFLICT,
                    "an account with that email already exists on this outpost".into(),
                ));
            }
        }
        return Err(internal(e));
    }

    sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1, $2, $3)")
        .bind(org)
        .bind(new_id)
        .bind(&org_role)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;

    tx.commit().await.map_err(internal)?;

    tracing::info!(user_id = %new_id, org = %org, by = %me, role = %org_role, "account created by administrator");

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": new_id,
            "email": email,
            "username": username,
            "orgRole": org_role,
            "accountType": account_type,
            // Returned once and never again — it is stored only as a hash.
            // The UI has to put it in front of the administrator now.
            "temporaryPassword": if generated { Some(plain) } else { None },
            "mustChangePassword": generated,
        })),
    ))
}

#[derive(Deserialize)]
struct PersonUpdate {
    full_name: Option<String>,
    org_role: Option<String>,
    account_type: Option<String>,
}

/// Confirm the target is in the caller's org before touching them.
///
/// Without this, an id from another organisation would be edited happily: the
/// WHERE clause on a bare `UPDATE users` has nothing in it about who is
/// allowed to ask.
async fn target_in_org(state: &AppState, org: Uuid, target: Uuid) -> Result<String, ApiError> {
    sqlx::query_scalar::<_, String>(
        "SELECT role FROM org_members WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org)
    .bind(target)
    .fetch_optional(&state.db)
    .await
    .map_err(internal)?
    .ok_or((StatusCode::NOT_FOUND, "no such person in your organisation".to_string()))
}

/// PATCH /api/people/:id
async fn update_person(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<PersonUpdate>,
) -> Result<Json<Value>, ApiError> {
    let (org, me) = require_admin(&state, &caller).await?;
    let current_role = target_in_org(&state, org, id).await?;

    // An owner is the account that cannot be locked out of its own
    // organisation. Letting an admin demote one would make ownership
    // meaningless, and on an outpost with one owner it is also how an org
    // ends up with nobody who can administer it.
    if current_role == "owner" && body.org_role.is_some() && me != id {
        return Err((
            StatusCode::FORBIDDEN,
            "an owner's role can only be changed by that owner".into(),
        ));
    }

    if let Some(name) = &body.full_name {
        sqlx::query("UPDATE users SET full_name = $2 WHERE id = $1")
            .bind(id)
            .bind(name.trim())
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }

    if let Some(t) = &body.account_type {
        if !matches!(t.as_str(), "buyer" | "seller" | "both") {
            return Err((StatusCode::BAD_REQUEST, "account_type must be buyer, seller or both".into()));
        }
        sqlx::query("UPDATE users SET account_type = $2 WHERE id = $1")
            .bind(id)
            .bind(t)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }

    if let Some(r) = &body.org_role {
        let role = check_role(Some(r))?;
        sqlx::query("UPDATE org_members SET role = $3 WHERE org_id = $1 AND user_id = $2")
            .bind(org)
            .bind(id)
            .bind(&role)
            .execute(&state.db)
            .await
            .map_err(internal)?;
    }

    Ok(Json(json!({ "id": id, "updated": true })))
}

/// POST /api/people/:id/password — issue a new temporary password.
///
/// There is no "send a reset link": that needs mail, and an outpost may have
/// no route to a mail server for days. An administrator reads the new password
/// to the person instead, which is also the realistic flow on a site where
/// everyone is in the same building.
async fn reset_password(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let (org, me) = require_admin(&state, &caller).await?;
    target_in_org(&state, org, id).await?;

    let plain = temporary_password();
    let hash = hash_password(&plain)?;

    sqlx::query("UPDATE users SET identity_hash = $2, must_change_password = TRUE WHERE id = $1")
        .bind(id)
        .bind(&hash)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    tracing::warn!(user_id = %id, by = %me, "password reset by administrator");

    Ok(Json(json!({ "id": id, "temporaryPassword": plain, "mustChangePassword": true })))
}

#[derive(Deserialize)]
struct ActiveFlag {
    active: bool,
}

/// PATCH /api/people/:id/active — deactivate or restore an account.
///
/// Deactivation, never deletion. A person who signed a custody receipt or
/// released a compliance hold is referenced by those records, and the records
/// are the point: a deleted row would leave a shipment attested by nobody.
async fn set_active(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<ActiveFlag>,
) -> Result<Json<Value>, ApiError> {
    let (org, me) = require_admin(&state, &caller).await?;
    let target_role = target_in_org(&state, org, id).await?;

    // Locking yourself out is a support call, and on a disconnected outpost
    // there may be nobody to call.
    if id == me && !body.active {
        return Err((
            StatusCode::BAD_REQUEST,
            "you cannot deactivate your own account".into(),
        ));
    }

    if target_role == "owner" && !body.active {
        return Err((
            StatusCode::FORBIDDEN,
            "an owner cannot be deactivated; transfer ownership first".into(),
        ));
    }

    sqlx::query("UPDATE users SET active = $2 WHERE id = $1")
        .bind(id)
        .bind(body.active)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    tracing::warn!(user_id = %id, by = %me, active = body.active, "account activation changed");
    Ok(Json(json!({ "id": id, "active": body.active })))
}

/// POST /api/me/password — change your own password.
///
/// Lives here rather than in `me` because it shares the hashing and the
/// `must_change_password` flag with the administrator paths, and a second copy
/// of either would be one to forget.
pub fn own_password_route() -> Router<AppState> {
    Router::new().route("/password", post(change_own_password))
}

#[derive(Deserialize)]
struct PasswordChange {
    current_password: String,
    new_password: String,
}

async fn change_own_password(
    State(state): State<AppState>,
    AuthenticatedUser(claims): AuthenticatedUser,
    Json(body): Json<PasswordChange>,
) -> Result<Json<Value>, ApiError> {
    use argon2::{PasswordHash, PasswordVerifier};

    let me = Uuid::parse_str(&claims.sub)
        .map_err(|_| (StatusCode::UNAUTHORIZED, "malformed subject".to_string()))?;

    if body.new_password.chars().count() < 10 {
        return Err((StatusCode::BAD_REQUEST, "password must be at least 10 characters".into()));
    }

    let stored: String = sqlx::query_scalar("SELECT identity_hash FROM users WHERE id = $1")
        .bind(me)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?
        .ok_or((StatusCode::NOT_FOUND, "no such account".to_string()))?;

    // The current password is required even though the caller already holds a
    // valid token. A token lifted from a shared browser should not be enough
    // to take the account over permanently.
    let parsed = PasswordHash::new(&stored).map_err(|_| internal("unparseable stored hash"))?;
    if Argon2::default()
        .verify_password(body.current_password.as_bytes(), &parsed)
        .is_err()
    {
        return Err((StatusCode::UNAUTHORIZED, "current password is incorrect".into()));
    }

    let hash = hash_password(&body.new_password)?;
    sqlx::query("UPDATE users SET identity_hash = $2, must_change_password = FALSE WHERE id = $1")
        .bind(me)
        .bind(&hash)
        .execute(&state.db)
        .await
        .map_err(internal)?;

    tracing::info!(user_id = %me, "password changed by its owner");
    Ok(Json(json!({ "changed": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_owners_and_admins_administer_people() {
        assert!(may_administer("owner"));
        assert!(may_administer("admin"));
        assert!(!may_administer("operator"));
        assert!(!may_administer("viewer"));
        // An unknown or absent role must not administer anything. This is the
        // value `admin_context` produces when there is no membership row, so
        // it is the one that matters most.
        assert!(!may_administer(""));
        assert!(!may_administer("superuser"));
    }

    #[test]
    fn an_unknown_org_role_is_refused_not_defaulted() {
        assert_eq!(check_role(None).unwrap(), "operator");
        assert_eq!(check_role(Some(" admin ")).unwrap(), "admin");
        // Coercing this to 'viewer' would quietly give someone less access
        // than intended; coercing to 'admin' would give them more.
        assert!(check_role(Some("root")).is_err());
    }

    /// A generated password has to be long enough to be worth generating and
    /// short enough to read aloud.
    #[test]
    fn a_temporary_password_is_long_and_distinct() {
        let a = temporary_password();
        let b = temporary_password();
        assert!(a.chars().count() >= 20, "{a}");
        assert_ne!(a, b, "two generated passwords must not match");
    }
}
