//
// ─── IMPORTS ────────────────────────────────────────────────────────────────
//

use sqlx::PgPool;
use uuid::Uuid;
use chrono::NaiveDateTime;

use crate::models::{
    users::User,
    inventory::Inventory,
    packages::Package,
    assets::Asset,
    assignments::Assignment,
    node::Node,
};

//
// ─── USERS ───────────────────────────────────────────────────────────────
//

// Create
pub async fn create_user(
    pool: &PgPool,
    username: &str,
    email: &str,
    role: Option<&str>,   // ✅ fixed: optional role
) -> sqlx::Result<User> {
    sqlx::query_as!(
        User,
        r#"
        INSERT INTO users (id, username, email, role, nft_token_id, nft_image_url, identity_hash)
        VALUES ($1, $2, $3, COALESCE($4, 'user'), $5, $6, $7)
        RETURNING id, username, email, role, nft_token_id, nft_image_url, identity_hash, created_at
        "#,
        Uuid::new_v4(),
        username,
        email,
        role,
        None::<&str>,
        None::<&str>,
        None::<&str>
    )
    .fetch_one(pool)
    .await
}

// Read all
pub async fn get_users(pool: &PgPool) -> sqlx::Result<Vec<User>> {
    sqlx::query_as!(
        User,
        r#"
        SELECT id, username, email, role, nft_token_id, nft_image_url, identity_hash, created_at
        FROM users
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
}

// Update
pub async fn update_user_email(pool: &PgPool, user_id: Uuid, new_email: &str) -> sqlx::Result<User> {
    sqlx::query_as!(
        User,
        r#"
        UPDATE users
        SET email = $2
        WHERE id = $1
        RETURNING id, username, email, role, nft_token_id, nft_image_url, identity_hash, created_at
        "#,
        user_id,
        new_email
    )
    .fetch_one(pool)
    .await
}

// Delete
pub async fn delete_user(pool: &PgPool, user_id: Uuid) -> sqlx::Result<u64> {
    let rows = sqlx::query!("DELETE FROM users WHERE id = $1", user_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows)
}

//
// ─── INVENTORY ────────────────────────────────────────────────────────────────
//

pub async fn create_inventory(
    pool: &PgPool,
    owner_id: Uuid,
    name: &str,
    description: Option<&str>,
    quantity: i32,
    location: Option<&str>,
    token_id: Option<&str>,
    token_image_url: Option<&str>,
    category: Option<&str>,
    unit: Option<&str>,
    threshold: Option<i32>,
) -> sqlx::Result<Inventory> {
    sqlx::query_as!(
        Inventory,
        r#"
        INSERT INTO inventory (
            id, owner_id, name, description, quantity, location,
            token_id, token_image_url, category, unit, threshold, created_at
        )
        VALUES (
            $1, $2, $3, $4, $5, $6,
            $7, $8,
            COALESCE($9, 'general'),
            COALESCE($10, 'units'),
            COALESCE($11, 0),
            NOW()
        )
        RETURNING
            id, owner_id, name, description, quantity, location,
            token_id, token_image_url, category, unit, threshold, created_at
        "#,
        Uuid::new_v4(),
        owner_id,
        name,
        description,
        quantity,
        location,
        token_id,
        token_image_url,
        category,
        unit,
        threshold,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_inventory(pool: &PgPool) -> sqlx::Result<Vec<Inventory>> {
    sqlx::query_as!(
        Inventory,
        r#"
        SELECT id, owner_id, name, description, quantity, location,
               token_id, token_image_url, category, unit, threshold, created_at
        FROM inventory
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn update_inventory(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    description: Option<&str>,
    quantity: i32,
    category: Option<&str>,
    unit: Option<&str>,
    threshold: Option<i32>,
) -> sqlx::Result<Inventory> {
    sqlx::query_as!(
        Inventory,
        r#"
        UPDATE inventory
        SET name = $2,
            description = $3,
            quantity = $4,
            category = COALESCE($5, category),
            unit = COALESCE($6, unit),
            threshold = COALESCE($7, threshold)
        WHERE id = $1
        RETURNING id, owner_id, name, description, quantity, location,
                  token_id, token_image_url, category, unit, threshold, created_at
        "#,
        id,
        name,
        description,
        quantity,
        category,
        unit,
        threshold,
    )
    .fetch_one(pool)
    .await
}

pub async fn update_inventory_quantity(
    pool: &PgPool,
    inventory_id: Uuid,
    new_quantity: i32,
) -> sqlx::Result<Inventory> {
    sqlx::query_as!(
        Inventory,
        r#"
        UPDATE inventory
        SET quantity = $2
        WHERE id = $1
        RETURNING id, owner_id, name, description, quantity, location,
                  token_id, token_image_url, category, unit, threshold, created_at
        "#,
        inventory_id,
        new_quantity
    )
    .fetch_one(pool)
    .await
}

pub async fn delete_inventory(pool: &PgPool, inventory_id: Uuid) -> sqlx::Result<u64> {
    let rows = sqlx::query!("DELETE FROM inventory WHERE id = $1", inventory_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows)
}

//
// ─── PACKAGES ────────────────────────────────────────────────────────────────
//

pub async fn create_package(
    pool: &PgPool,
    owner_id: Uuid,
    inventory_item_id: Option<Uuid>,
    status: Option<&str>,
    location: Option<&str>,
    nft_token: Option<&str>,
    nft_image_url: Option<&str>,
    description: Option<&str>,
    eta: Option<NaiveDateTime>,
) -> sqlx::Result<Package> {
    sqlx::query_as!(
        Package,
        r#"
        INSERT INTO packages (
            id, owner_id, inventory_item_id, status, location,
            nft_token, nft_image_url, description, eta
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING
            id, owner_id, inventory_item_id, description, status, location,
            eta, completed_at, nft_token, nft_image_url, created_at
        "#,
        Uuid::new_v4(),
        owner_id,
        inventory_item_id,
        status,
        location,
        nft_token,
        nft_image_url,
        description,
        eta,
    )
    .fetch_one(pool)
    .await
}

pub async fn get_packages(pool: &PgPool) -> sqlx::Result<Vec<Package>> {
    sqlx::query_as!(
        Package,
        r#"
        SELECT id, owner_id, inventory_item_id, description, status, location,
               eta, completed_at, nft_token, nft_image_url, created_at
        FROM packages
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_package(pool: &PgPool, package_id: Uuid) -> sqlx::Result<Package> {
    sqlx::query_as!(
        Package,
        r#"
        SELECT id, owner_id, inventory_item_id, description, status, location,
               eta, completed_at, nft_token, nft_image_url, created_at
        FROM packages
        WHERE id = $1
        "#,
        package_id
    )
    .fetch_one(pool)
    .await
}

pub async fn update_package_status(
    pool: &PgPool,
    package_id: Uuid,
    new_status: &str,
) -> sqlx::Result<Package> {
    sqlx::query_as!(
        Package,
        r#"
        UPDATE packages
        SET status = $2
        WHERE id = $1
        RETURNING id, owner_id, inventory_item_id, description, status, location,
                  eta, completed_at, nft_token, nft_image_url, created_at
        "#,
        package_id,
        new_status
    )
    .fetch_one(pool)
    .await
}

pub async fn delete_package(pool: &PgPool, package_id: Uuid) -> sqlx::Result<u64> {
    let rows = sqlx::query!("DELETE FROM packages WHERE id = $1", package_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows)
}

//
// ─── ASSETS ────────────────────────────────────────────────────────────────
//

pub async fn create_asset(
    pool: &PgPool,
    owner_id: Uuid,
    name: &str,
    description: Option<&str>,
    location: &str,
    status: Option<&str>,
    nft_token: Option<&str>,
    nft_image_url: Option<&str>,
) -> sqlx::Result<Asset> {
    sqlx::query_as!(
        Asset,
        r#"
        INSERT INTO assets (
            id, owner_id, name, description, location, status, nft_token, nft_image_url, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, $5, COALESCE($6, 'in_transit'), $7, $8, NOW(), NOW())
        RETURNING id, owner_id, name, description, location, status, nft_token, nft_image_url, created_at, updated_at
        "#,
        Uuid::new_v4(),
        owner_id,
        name,
        description,
        location,
        status,
        nft_token,
        nft_image_url
    )
    .fetch_one(pool)
    .await
}

pub async fn get_assets(pool: &PgPool) -> sqlx::Result<Vec<Asset>> {
    sqlx::query_as!(
        Asset,
        r#"
        SELECT id, owner_id, name, description, location, status,
               nft_token, nft_image_url, created_at, updated_at
        FROM assets
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn get_asset(pool: &PgPool, asset_id: Uuid) -> sqlx::Result<Asset> {
    sqlx::query_as!(
        Asset,
        r#"
        SELECT id, owner_id, name, description, location, status,
               nft_token, nft_image_url, created_at, updated_at
        FROM assets
        WHERE id = $1
        "#,
        asset_id
    )
    .fetch_one(pool)
    .await
}

pub async fn update_asset_status(
    pool: &PgPool,
    asset_id: Uuid,
    new_status: &str,
) -> sqlx::Result<Asset> {
    sqlx::query_as!(
        Asset,
        r#"
        UPDATE assets
        SET status = $2, updated_at = NOW()
        WHERE id = $1
        RETURNING id, owner_id, name, description, location, status,
                  nft_token, nft_image_url, created_at, updated_at
        "#,
        asset_id,
        new_status
    )
    .fetch_one(pool)
    .await
}

pub async fn update_asset_location(
    pool: &PgPool,
    asset_id: Uuid,
    new_location: &str,
) -> sqlx::Result<Asset> {
    sqlx::query_as!(
        Asset,
        r#"
        UPDATE assets
        SET location = $2, updated_at = NOW()
        WHERE id = $1
        RETURNING id, owner_id, name, description, location, status,
                  nft_token, nft_image_url, created_at, updated_at
        "#,
        asset_id,
        new_location
    )
    .fetch_one(pool)
    .await
}

pub async fn delete_asset(pool: &PgPool, asset_id: Uuid) -> sqlx::Result<u64> {
    let rows = sqlx::query!("DELETE FROM assets WHERE id = $1", asset_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows)
}


//
// ─── ASSIGNMENTS ────────────────────────────────────────────────────────────────
//

pub async fn create_assignment(
    pool: &PgPool,
    asset_id: Uuid,
    package_id: Option<Uuid>,
    inventory_id: Option<Uuid>,
    schedule_start: Option<NaiveDateTime>,
    schedule_end: Option<NaiveDateTime>,
    recurrence_rule: Option<&str>,
    eta: Option<NaiveDateTime>,
    completed_at: Option<NaiveDateTime>,
) -> sqlx::Result<Assignment> {
    sqlx::query_as!(
        Assignment,
        r#"
        INSERT INTO assignments (
            id, asset_id, package_id, inventory_id,
            schedule_start, schedule_end, recurrence_rule, eta, completed_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, asset_id, package_id, inventory_id, schedule_start,
                  schedule_end, recurrence_rule, eta, completed_at, status, created_at
        "#,
        Uuid::new_v4(),
        asset_id,
        package_id,
        inventory_id,
        schedule_start,
        schedule_end,
        recurrence_rule,
        eta,
        completed_at
    )
    .fetch_one(pool)
    .await
}

pub async fn get_assignments(pool: &PgPool) -> sqlx::Result<Vec<Assignment>> {
    sqlx::query_as!(
        Assignment,
        r#"
        SELECT id, asset_id, package_id, inventory_id, schedule_start,
               schedule_end, recurrence_rule, eta, completed_at, status, created_at
        FROM assignments
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await
}

pub async fn update_assignment_status(
    pool: &PgPool,
    assignment_id: Uuid,
    new_status: &str,
) -> sqlx::Result<Assignment> {
    sqlx::query_as!(
        Assignment,
        r#"
        UPDATE assignments
        SET status = $2
        WHERE id = $1
        RETURNING id, asset_id, package_id, inventory_id, schedule_start,
                  schedule_end, recurrence_rule, eta, completed_at, status, created_at
        "#,
        assignment_id,
        new_status
    )
    .fetch_one(pool)
    .await
}

pub async fn delete_assignment(pool: &PgPool, assignment_id: Uuid) -> sqlx::Result<u64> {
    let rows = sqlx::query!("DELETE FROM assignments WHERE id = $1", assignment_id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows)
}

//
// ─── NODES ────────────────────────────────────────────────────────────────
//

// Create
pub async fn register_node(
    pool: &PgPool,
    name: &str,
    api_endpoint: &str,
    public_key: &str,
    location: Option<&str>,
) -> sqlx::Result<Node> {
    sqlx::query_as!(
        Node,
        r#"
        INSERT INTO node_registry (node_id, name, api_endpoint, public_key, location)
        VALUES ($1, $2, $3, $4, $5)
        RETURNING node_id, name, api_endpoint, public_key, location, last_seen, status
        "#,
        Uuid::new_v4(),
        name,
        api_endpoint,
        public_key,
        location
    )
    .fetch_one(pool)
    .await
}

// Read all
pub async fn get_nodes(pool: &PgPool) -> sqlx::Result<Vec<Node>> {
    sqlx::query_as!(
        Node,
        r#"
        SELECT node_id, name, api_endpoint, public_key, location, last_seen, status
        FROM node_registry
        ORDER BY last_seen DESC
        "#
    )
    .fetch_all(pool)
    .await
}

// Update status / heartbeat
pub async fn update_node_heartbeat(pool: &PgPool, node_id: Uuid) -> sqlx::Result<Node> {
    sqlx::query_as!(
        Node,
        r#"
        UPDATE node_registry
        SET last_seen = NOW()
        WHERE node_id = $1
        RETURNING node_id, name, api_endpoint, public_key, location, last_seen, status
        "#,
        node_id
    )
    .fetch_one(pool)
    .await
}

