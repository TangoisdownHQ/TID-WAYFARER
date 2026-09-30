use sqlx::PgPool;
use uuid::Uuid;
use chrono::NaiveDateTime;

use crate::models::packages::Package;

/// Insert a new package (supports NFT metadata)
pub async fn add_package(
    pool: &PgPool,
    owner_id: Uuid,
    inventory_item_id: Option<Uuid>,
    status: &str,
    description: Option<&str>,
    location: Option<&str>,
    nft_token: Option<&str>,
    nft_image_url: Option<&str>,
    eta: Option<NaiveDateTime>,
) -> Result<Package, sqlx::Error> {
    let package = sqlx::query_as!(
        Package,
        r#"
        INSERT INTO packages (
            id, owner_id, inventory_item_id, status, description,
            location, nft_token, nft_image_url, eta
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        RETURNING id, owner_id, inventory_item_id, status, description,
                  location, nft_token, nft_image_url, eta,
                  created_at, completed_at
        "#,
        Uuid::new_v4(),
        owner_id,
        inventory_item_id,
        status,
        description,
        location,
        nft_token,
        nft_image_url,
        eta,
    )
    .fetch_one(pool)
    .await?;

    Ok(package)
}

/// Get all packages
pub async fn get_all_packages(pool: &PgPool) -> Result<Vec<Package>, sqlx::Error> {
    let packages = sqlx::query_as!(
        Package,
        r#"
        SELECT id, owner_id, inventory_item_id, status, description,
               location, nft_token, nft_image_url, eta,
               created_at, completed_at
        FROM packages
        ORDER BY created_at DESC
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(packages)
}

/// Get package by ID
pub async fn get_package_by_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Package>, sqlx::Error> {
    let package = sqlx::query_as!(
        Package,
        r#"
        SELECT id, owner_id, inventory_item_id, status, description,
               location, nft_token, nft_image_url, eta,
               created_at, completed_at
        FROM packages
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(package)
}

/// Update a package (supports NFT metadata)
pub async fn update_package(
    pool: &PgPool,
    id: Uuid,
    status: Option<&str>,
    description: Option<&str>,
    location: Option<&str>,
    eta: Option<NaiveDateTime>,
    nft_token: Option<&str>,
    nft_image_url: Option<&str>,
) -> Result<Option<Package>, sqlx::Error> {
    let package = sqlx::query_as!(
        Package,
        r#"
        UPDATE packages
        SET status       = COALESCE($2, status),
            description  = COALESCE($3, description),
            location     = COALESCE($4, location),
            eta          = COALESCE($5, eta),
            nft_token    = COALESCE($6, nft_token),
            nft_image_url= COALESCE($7, nft_image_url)
        WHERE id = $1
        RETURNING id, owner_id, inventory_item_id, status, description,
                  location, nft_token, nft_image_url, eta,
                  created_at, completed_at
        "#,
        id,
        status,
        description,
        location,
        eta,
        nft_token,
        nft_image_url
    )
    .fetch_optional(pool)
    .await?;

    Ok(package)
}

/// Delete package by ID
pub async fn delete_package(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query!(
        "DELETE FROM packages WHERE id = $1",
        id
    )
    .execute(pool)
    .await?
    .rows_affected();

    Ok(rows > 0)
}

/// Mark a package as delivered (sets completed_at = NOW())
pub async fn mark_package_delivered(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Package>, sqlx::Error> {
    let package = sqlx::query_as!(
        Package,
        r#"
        UPDATE packages
        SET status = 'delivered',
            completed_at = NOW()
        WHERE id = $1
        RETURNING id, owner_id, inventory_item_id, status, description,
                  location, nft_token, nft_image_url, eta,
                  created_at, completed_at
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(package)
}

