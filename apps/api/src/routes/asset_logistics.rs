//! 📦 Asset logistics — BOM (parts), tags, kits.
//!
//! Three primitives that turn a flat asset list into a real logistics graph:
//! - **BOM**     : recursive parent → child tree (recall, provenance, MRO)
//! - **Tags**    : flat, free-form labels for cross-cutting groups
//! - **Kits**    : named bundles of assets that travel/store as one unit

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::{routes::auth_middleware::AuthenticatedUser, AppState};

// ---------------------------------------------------------------------------
// Routers
// ---------------------------------------------------------------------------

/// Routes mounted under `/api/assets` alongside the existing asset router.
/// Order matters: the literal `/parts/tree` segment is declared BEFORE
/// `/parts/:child_id` so axum prefers the exact match.
pub fn asset_extras_routes() -> Router<AppState> {
    Router::new()
        // BOM
        .route("/assets/:id/parts/tree",      get(get_bom_tree))
        .route("/assets/:id/parts/:child_id", delete(detach_part))
        .route("/assets/:id/parts",           get(list_parts).post(attach_part))
        .route("/assets/:id/used-in",         get(get_used_in))
        // Tags
        .route("/assets/:id/tags",            get(list_asset_tags).post(add_tag))
        .route("/assets/:id/tags/:tag",       delete(remove_tag))
        .route("/assets/by-tag/:tag",         get(assets_by_tag))
}

/// Routes mounted under `/api/kits`.
pub fn kit_routes() -> Router<AppState> {
    Router::new()
        .route("/",                       get(list_kits).post(create_kit))
        .route("/:id",                    get(get_kit).put(update_kit).delete(delete_kit))
        .route("/:id/items",              post(add_kit_item))
        .route("/:id/items/:asset_id",    delete(remove_kit_item))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_uid(s: &str) -> Result<Uuid, StatusCode> {
    Uuid::parse_str(s).map_err(|_| StatusCode::UNAUTHORIZED)
}

async fn require_owner_asset(
    pool: &PgPool,
    asset_id: Uuid,
    user_id: Uuid,
) -> Result<(), StatusCode> {
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_id FROM assets WHERE id = $1")
            .bind(asset_id)
            .fetch_optional(pool)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match owner {
        Some(o) if o == user_id => Ok(()),
        Some(_)                 => Err(StatusCode::FORBIDDEN),
        None                    => Err(StatusCode::NOT_FOUND),
    }
}

// ===========================================================================
// 🧩 BOM (asset_parts)
// ===========================================================================

#[derive(Deserialize)]
pub struct AttachPartRequest {
    pub child_id:    Uuid,
    pub qty:         Option<f64>,
    pub position:    Option<String>,
    pub criticality: Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct PartRow {
    pub parent_id:           Uuid,
    pub child_id:            Uuid,
    pub child_name:          String,
    pub child_part_number:   Option<String>,
    pub child_serial_number: Option<String>,
    pub child_lot_number:    Option<String>,
    pub qty:                 f64,
    pub position:            String,
    pub criticality:         Option<String>,
    pub installed_at:        Option<NaiveDateTime>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct BomNode {
    pub depth:               i32,
    pub parent_id:           Uuid,
    pub child_id:            Uuid,
    pub child_name:          String,
    pub child_part_number:   Option<String>,
    pub child_serial_number: Option<String>,
    pub child_lot_number:    Option<String>,
    pub qty:                 f64,
    pub position:            String,
    pub criticality:         Option<String>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AncestorRow {
    pub id:            Uuid,
    pub name:          String,
    pub part_number:   Option<String>,
    pub serial_number: Option<String>,
    pub lot_number:    Option<String>,
}

/// Attach a child asset as a part of the parent. Upserts on
/// (parent_id, child_id, position) so re-attaching updates qty.
pub async fn attach_part(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<AttachPartRequest>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, id, user_id).await?;

    if payload.child_id == id {
        return Err(StatusCode::BAD_REQUEST); // an asset can't be its own part
    }

    sqlx::query(
        r#"
        INSERT INTO asset_parts (parent_id, child_id, qty, position, criticality)
        VALUES ($1, $2, COALESCE($3, 1), COALESCE($4, ''), $5)
        ON CONFLICT (parent_id, child_id, position) DO UPDATE
          SET qty         = EXCLUDED.qty,
              criticality = EXCLUDED.criticality,
              removed_at  = NULL
        "#,
    )
    .bind(id)
    .bind(payload.child_id)
    .bind(payload.qty)
    .bind(payload.position)
    .bind(&payload.criticality)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::BAD_REQUEST)?;

    Ok(Json("part attached"))
}

/// Detach all instances of `child_id` from `parent_id` (across all positions).
pub async fn detach_part(
    AuthenticatedUser(user): AuthenticatedUser,
    Path((parent_id, child_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, parent_id, user_id).await?;

    let res = sqlx::query("DELETE FROM asset_parts WHERE parent_id = $1 AND child_id = $2")
        .bind(parent_id)
        .bind(child_id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if res.rows_affected() == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("part detached"))
    }
}

/// Direct children only (single-level BOM).
pub async fn list_parts(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<PartRow>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, id, user_id).await?;

    let rows = sqlx::query_as::<_, PartRow>(
        r#"
        SELECT
            ap.parent_id,
            ap.child_id,
            a.name          AS child_name,
            a.part_number   AS child_part_number,
            a.serial_number AS child_serial_number,
            a.lot_number    AS child_lot_number,
            ap.qty::float8  AS qty,
            ap.position,
            ap.criticality,
            ap.installed_at
        FROM asset_parts ap
        JOIN assets a ON a.id = ap.child_id
        WHERE ap.parent_id = $1
          AND ap.removed_at IS NULL
        ORDER BY ap.position
        "#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows))
}

/// Recursive descendants — full BOM tree as a flat list with `depth`.
/// Depth capped at 10 to avoid runaway cycles.
pub async fn get_bom_tree(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<BomNode>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, id, user_id).await?;

    let rows = sqlx::query_as::<_, BomNode>(
        r#"
        WITH RECURSIVE bom AS (
          SELECT parent_id, child_id, qty, position, criticality, 0::int AS depth
          FROM asset_parts
          WHERE parent_id = $1 AND removed_at IS NULL
          UNION ALL
          SELECT ap.parent_id, ap.child_id, ap.qty, ap.position, ap.criticality, b.depth + 1
          FROM asset_parts ap
          JOIN bom b ON ap.parent_id = b.child_id
          WHERE ap.removed_at IS NULL AND b.depth < 10
        )
        SELECT
            b.depth,
            b.parent_id,
            b.child_id,
            a.name          AS child_name,
            a.part_number   AS child_part_number,
            a.serial_number AS child_serial_number,
            a.lot_number    AS child_lot_number,
            b.qty::float8   AS qty,
            b.position,
            b.criticality
        FROM bom b
        JOIN assets a ON a.id = b.child_id
        ORDER BY b.depth, b.position
        "#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows))
}

/// Recursive ancestors — every assembly that contains this asset (directly
/// or transitively). Scoped to assemblies owned by the caller.
pub async fn get_used_in(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<AncestorRow>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let rows = sqlx::query_as::<_, AncestorRow>(
        r#"
        WITH RECURSIVE ancestors AS (
          SELECT parent_id, child_id, 0::int AS depth
          FROM asset_parts
          WHERE child_id = $1 AND removed_at IS NULL
          UNION ALL
          SELECT ap.parent_id, ap.child_id, anc.depth + 1
          FROM asset_parts ap
          JOIN ancestors anc ON ap.child_id = anc.parent_id
          WHERE ap.removed_at IS NULL AND anc.depth < 10
        )
        SELECT DISTINCT
            a.id,
            a.name,
            a.part_number,
            a.serial_number,
            a.lot_number
        FROM ancestors anc
        JOIN assets a ON a.id = anc.parent_id
        WHERE a.owner_id = $2
        ORDER BY a.name
        "#,
    )
    .bind(id)
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows))
}

// ===========================================================================
// 🏷  Tags (asset_tags)
// ===========================================================================

#[derive(Deserialize)]
pub struct AddTagRequest {
    pub tag: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct TaggedAssetRow {
    pub id:            Uuid,
    pub name:          String,
    pub part_number:   Option<String>,
    pub serial_number: Option<String>,
    pub lifecycle:     Option<String>,
    pub body_id:       Option<i32>,
}

pub async fn add_tag(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<AddTagRequest>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, id, user_id).await?;

    let tag = payload.tag.trim().to_string();
    if tag.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    sqlx::query("INSERT INTO asset_tags (asset_id, tag) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(&tag)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json("tag added"))
}

pub async fn remove_tag(
    AuthenticatedUser(user): AuthenticatedUser,
    Path((id, tag)): Path<(Uuid, String)>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, id, user_id).await?;

    let res = sqlx::query("DELETE FROM asset_tags WHERE asset_id = $1 AND tag = $2")
        .bind(id)
        .bind(&tag)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if res.rows_affected() == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("tag removed"))
    }
}

pub async fn list_asset_tags(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<Vec<String>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;
    require_owner_asset(&state.db, id, user_id).await?;

    let tags = sqlx::query_scalar::<_, String>(
        "SELECT tag FROM asset_tags WHERE asset_id = $1 ORDER BY tag",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(tags))
}

pub async fn assets_by_tag(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(tag): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Vec<TaggedAssetRow>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let rows = sqlx::query_as::<_, TaggedAssetRow>(
        r#"
        SELECT a.id, a.name, a.part_number, a.serial_number, a.lifecycle, a.body_id
        FROM asset_tags t
        JOIN assets a ON a.id = t.asset_id
        WHERE t.tag = $1 AND a.owner_id = $2
        ORDER BY a.name
        "#,
    )
    .bind(&tag)
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(rows))
}

// ===========================================================================
// 🧰 Kits (asset_kits + asset_kit_items)
// ===========================================================================

#[derive(Serialize, Deserialize, sqlx::FromRow, Clone)]
pub struct Kit {
    pub id:          Uuid,
    pub name:        String,
    pub description: Option<String>,
    pub owner_id:    Option<Uuid>,
    pub meta:        Option<serde_json::Value>,
    pub created_at:  NaiveDateTime,
}

#[derive(Deserialize)]
pub struct NewKit {
    pub name:        String,
    pub description: Option<String>,
    pub meta:        Option<serde_json::Value>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct KitItem {
    pub asset_id:      Uuid,
    pub name:          String,
    pub part_number:   Option<String>,
    pub serial_number: Option<String>,
    pub qty:           f64,
}

#[derive(Serialize)]
pub struct KitWithItems {
    pub kit:   Kit,
    pub items: Vec<KitItem>,
}

#[derive(Deserialize)]
pub struct AddKitItemRequest {
    pub asset_id: Uuid,
    pub qty:      Option<f64>,
}

pub async fn list_kits(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Kit>>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let kits = sqlx::query_as::<_, Kit>(
        "SELECT * FROM asset_kits WHERE owner_id = $1 ORDER BY created_at DESC",
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(kits))
}

pub async fn create_kit(
    AuthenticatedUser(user): AuthenticatedUser,
    State(state): State<AppState>,
    Json(payload): Json<NewKit>,
) -> Result<Json<Kit>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let kit = sqlx::query_as::<_, Kit>(
        r#"
        INSERT INTO asset_kits (id, name, description, owner_id, meta)
        VALUES (gen_random_uuid(), $1, $2, $3, COALESCE($4, '{}'::jsonb))
        RETURNING *
        "#,
    )
    .bind(&payload.name)
    .bind(&payload.description)
    .bind(user_id)
    .bind(&payload.meta)
    .fetch_one(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(kit))
}

pub async fn get_kit(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<KitWithItems>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let kit = sqlx::query_as::<_, Kit>("SELECT * FROM asset_kits WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if kit.owner_id != Some(user_id) {
        return Err(StatusCode::FORBIDDEN);
    }

    let items = sqlx::query_as::<_, KitItem>(
        r#"
        SELECT ki.asset_id, a.name, a.part_number, a.serial_number, ki.qty::float8 AS qty
        FROM asset_kit_items ki
        JOIN assets a ON a.id = ki.asset_id
        WHERE ki.kit_id = $1
        ORDER BY a.name
        "#,
    )
    .bind(id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    Ok(Json(KitWithItems { kit, items }))
}

pub async fn update_kit(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<NewKit>,
) -> Result<Json<Kit>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let kit = sqlx::query_as::<_, Kit>(
        r#"
        UPDATE asset_kits
        SET name        = $2,
            description = $3,
            meta        = COALESCE($4, meta)
        WHERE id = $1 AND owner_id = $5
        RETURNING *
        "#,
    )
    .bind(id)
    .bind(&payload.name)
    .bind(&payload.description)
    .bind(&payload.meta)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match kit {
        Some(k) => Ok(Json(k)),
        None    => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn delete_kit(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    let res = sqlx::query("DELETE FROM asset_kits WHERE id = $1 AND owner_id = $2")
        .bind(id)
        .bind(user_id)
        .execute(&state.db)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if res.rows_affected() == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("kit deleted"))
    }
}

pub async fn add_kit_item(
    AuthenticatedUser(user): AuthenticatedUser,
    Path(kit_id): Path<Uuid>,
    State(state): State<AppState>,
    Json(payload): Json<AddKitItemRequest>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    // verify kit ownership (owner_id is nullable on the kits table)
    let owner: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT owner_id FROM asset_kits WHERE id = $1")
            .bind(kit_id)
            .fetch_optional(&state.db)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    match owner {
        Some(Some(o)) if o == user_id => {}
        Some(_)                       => return Err(StatusCode::FORBIDDEN),
        None                          => return Err(StatusCode::NOT_FOUND),
    }

    sqlx::query(
        r#"
        INSERT INTO asset_kit_items (kit_id, asset_id, qty)
        VALUES ($1, $2, COALESCE($3, 1))
        ON CONFLICT (kit_id, asset_id) DO UPDATE SET qty = EXCLUDED.qty
        "#,
    )
    .bind(kit_id)
    .bind(payload.asset_id)
    .bind(payload.qty)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::BAD_REQUEST)?;

    Ok(Json("item added"))
}

pub async fn remove_kit_item(
    AuthenticatedUser(user): AuthenticatedUser,
    Path((kit_id, asset_id)): Path<(Uuid, Uuid)>,
    State(state): State<AppState>,
) -> Result<Json<&'static str>, StatusCode> {
    let user_id = parse_uid(&user.sub)?;

    // owner check done via correlated subquery so we don't need a second round-trip
    let res = sqlx::query(
        r#"
        DELETE FROM asset_kit_items
        WHERE kit_id = $1 AND asset_id = $2
          AND kit_id IN (SELECT id FROM asset_kits WHERE owner_id = $3)
        "#,
    )
    .bind(kit_id)
    .bind(asset_id)
    .bind(user_id)
    .execute(&state.db)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if res.rows_affected() == 0 {
        Err(StatusCode::NOT_FOUND)
    } else {
        Ok(Json("item removed"))
    }
}
