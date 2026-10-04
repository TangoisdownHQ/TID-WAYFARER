//! Backing up an outpost's identity, and knowing whether you have.
//!
//! # Export is here; restore is not
//!
//! Export is an API call so an operator can take a backup without shell access
//! on the host — which on a Kubernetes outpost is the normal case.
//!
//! Restore is deliberately **not**. Swapping an outpost's identity while it is
//! running would leave the in-memory keys, the peer registry and the
//! organisation certificate disagreeing with each other, and if the keys
//! already on disk were the good ones it would destroy them. Restore is
//! `scripts/restore_outpost.sh`, run against a stopped outpost, where the
//! operator can see what they are overwriting.
//!
//! # What export does not include
//!
//! The database. Keys and data are backed up together by
//! `scripts/backup_outpost.sh`, because either alone restores something broken:
//! a database without its keys cannot authenticate to any peer or open its own
//! queued DTN traffic, and keys without a database are an identity with nothing
//! behind it.

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::routes::auth_middleware::AdminUser;
use crate::services::keyvault::{self, VaultError};
use crate::AppState;

type ApiError = (StatusCode, String);

pub fn key_routes() -> Router<AppState> {
    Router::new()
        .route("/export", post(export_keys))
        .route("/status", get(custody_status))
}

fn vault_status(e: &VaultError) -> StatusCode {
    match e {
        VaultError::WeakPassphrase(_) | VaultError::Malformed(_) => StatusCode::BAD_REQUEST,
        VaultError::UnsupportedVersion(_) => StatusCode::CONFLICT,
        VaultError::CannotOpen => StatusCode::UNAUTHORIZED,
        VaultError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Deserialize)]
struct ExportRequest {
    passphrase: String,
    /// Where the operator intends to keep it. Recorded so a restore does not
    /// begin by guessing, and so an auditor can see a backup went somewhere
    /// other than the machine it was taken from.
    destination: Option<String>,
}

/// POST /api/ops/keys/export
///
/// Returns the sealed bundle. Admin-gated, and the passphrase does the real
/// work: the file is offline-attackable once it leaves here, so an admin token
/// alone must not be enough to make a usable copy of this outpost's identity.
async fn export_keys(
    State(state): State<AppState>,
    admin: AdminUser,
    Json(body): Json<ExportRequest>,
) -> Result<Json<Value>, ApiError> {
    let outpost = std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into());

    let bundle = keyvault::bundle_from_disk(&outpost).map_err(|e| {
        tracing::error!(error = %e, "key export failed to read the key files");
        (vault_status(&e), e.to_string())
    })?;

    // Sanity check before sealing: an export of keys that do not match the
    // running identity would restore an outpost the fabric does not recognise,
    // and it would look like a successful backup until the day it was needed.
    if bundle.node_id != state.identity.node_id {
        tracing::error!(
            on_disk = %bundle.node_id, running = %state.identity.node_id,
            "refusing to export: the key file does not match the running identity"
        );
        return Err((
            StatusCode::CONFLICT,
            format!(
                "keys/identity.json holds node {} but this process is running as {} — \
                 exporting would back up an identity the fabric does not know. Resolve which \
                 is correct before taking a backup.",
                bundle.node_id, state.identity.node_id
            ),
        ));
    }

    let sealed = keyvault::seal(&bundle, &body.passphrase).map_err(|e| (vault_status(&e), e.to_string()))?;

    // Recorded as a keys-only backup. A full one comes from the script, which
    // takes the database alongside.
    let _ = sqlx::query(
        "INSERT INTO backups (node_id, outpost_name, kind, location, includes_keys, note) \
         VALUES ($1::uuid, $2, 'keys', $3, TRUE, $4)",
    )
    .bind(&bundle.node_id)
    .bind(&outpost)
    .bind(body.destination.as_deref().unwrap_or("downloaded by an administrator"))
    .bind(format!("exported by {}", admin.0.sub))
    .execute(&state.db)
    .await;

    tracing::warn!(
        by = %admin.0.sub,
        "outpost key bundle exported — a copy of this outpost's identity now exists elsewhere"
    );

    Ok(Json(json!({
        "bundle": sealed,
        "nodeId": bundle.node_id,
        "outpostName": bundle.outpost_name,
        "filename": format!("{}-keys-{}.json", outpost, chrono::Utc::now().format("%Y%m%dT%H%M%SZ")),
        "note": "Store this somewhere the outpost is not. Losing both the keys and the backup \
                 is unrecoverable: a new identity cannot authenticate to peers, and every DTN \
                 bundle sealed to the old key becomes permanently unreadable.",
        "restoreWith": "scripts/restore_outpost.sh (against a stopped outpost)",
    })))
}

/// GET /api/ops/keys/status — can this outpost survive losing its volume?
///
/// Its own route because the answer is the single most useful fact about an
/// outpost's resilience, and a dashboard that shows it is how a backup habit
/// survives contact with a busy week.
async fn custody_status(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        "SELECT kind, taken_at, location, includes_keys FROM backups \
         WHERE includes_keys ORDER BY taken_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let latest_full = sqlx::query(
        "SELECT taken_at, location FROM backups WHERE kind = 'full' ORDER BY taken_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let keys_on_disk = std::path::Path::new("keys/identity.json").exists()
        && std::path::Path::new("keys/commsec.json").exists();

    let age_days = row
        .as_ref()
        .map(|r| (chrono::Utc::now() - r.get::<chrono::DateTime<chrono::Utc>, _>("taken_at")).num_days());

    // Three states rather than a boolean, because "never" and "too long ago"
    // need different responses and only one of them is urgent today.
    let standing = match age_days {
        None => "never",
        Some(d) if d <= 30 => "ok",
        Some(_) => "stale",
    };

    Ok(Json(json!({
        "nodeId": state.identity.node_id,
        "keysOnDisk": keys_on_disk,
        "standing": standing,
        "lastKeyBackupAt": row.as_ref()
            .map(|r| r.get::<chrono::DateTime<chrono::Utc>, _>("taken_at").to_rfc3339()),
        "lastKeyBackupAgeDays": age_days,
        "lastKeyBackupLocation": row.as_ref().and_then(|r| r.get::<Option<String>, _>("location")),
        "lastFullBackupAt": latest_full.as_ref()
            .map(|r| r.get::<chrono::DateTime<chrono::Utc>, _>("taken_at").to_rfc3339()),
        "lastFullBackupLocation": latest_full.as_ref()
            .and_then(|r| r.get::<Option<String>, _>("location")),
        "whatLossMeans": if standing == "never" {
            "No key backup has ever been recorded. If this outpost's volume is lost it becomes \
             a different outpost: peers will not recognise it, its organisation certificate \
             names an identity it no longer has, and every DTN bundle queued for it becomes \
             permanently unreadable."
        } else {
            "A key backup exists. Restoring it keeps this outpost's identity, so nothing at any \
             peer has to change."
        },
    })))
}
