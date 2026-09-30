use sqlx::PgPool;
use uuid::Uuid;

use crate::models::inventory::Inventory;
use crate::queries;

/// Add a new inventory item with rules
pub async fn add_inventory_item(
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
) -> Result<Inventory, sqlx::Error> {
    if quantity < 0 {
        return Err(sqlx::Error::Protocol(
            "❌ Cannot insert negative quantity".into(),
        ));
    }

    let item = queries::create_inventory(
        pool,
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
    .await?;

    if item.quantity <= item.threshold {
        eprintln!("⚠️ Low stock alert for item: {}", item.name);
    }

    Ok(item)
}

/// Update an inventory item’s fields
pub async fn update_inventory_item(
    pool: &PgPool,
    id: Uuid,
    name: &str,
    description: Option<&str>,
    quantity: i32,
    category: Option<&str>,
    unit: Option<&str>,
    threshold: Option<i32>,
) -> Result<Inventory, sqlx::Error> {
    if quantity < 0 {
        return Err(sqlx::Error::Protocol(
            "❌ Cannot update to negative quantity".into(),
        ));
    }

    let item = sqlx::query_as!(
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
        RETURNING id, owner_id, name, description, quantity,
                  location, token_id, token_image_url,
                  category, unit, threshold, created_at
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
    .await?;

    if item.quantity <= item.threshold {
        eprintln!("⚠️ Low stock alert for item: {}", item.name);
    }

    Ok(item)
}

/// Update only quantity with rules
pub async fn update_inventory_quantity_with_rules(
    pool: &PgPool,
    id: Uuid,
    new_quantity: i32,
) -> Result<Inventory, sqlx::Error> {
    if new_quantity < 0 {
        return Err(sqlx::Error::Protocol(
            "❌ Cannot set negative quantity".into(),
        ));
    }

    let item = queries::update_inventory_quantity(pool, id, new_quantity).await?;

    if item.quantity <= item.threshold {
        eprintln!("⚠️ Low stock alert for item: {}", item.name);
    }

    Ok(item)
}

/// Get all inventory items
pub async fn get_all_inventory(pool: &PgPool) -> Result<Vec<Inventory>, sqlx::Error> {
    queries::get_inventory(pool).await
}

/// Get single inventory item by ID
pub async fn get_inventory_by_id(
    pool: &PgPool,
    id: Uuid,
) -> Result<Option<Inventory>, sqlx::Error> {
    let item = sqlx::query_as!(
        Inventory,
        r#"
        SELECT id, owner_id, name, description, quantity,
               location, token_id, token_image_url,
               category, unit, threshold, created_at
        FROM inventory
        WHERE id = $1
        "#,
        id
    )
    .fetch_optional(pool)
    .await?;

    Ok(item)
}

/// Delete inventory item
pub async fn delete_inventory_item(
    pool: &PgPool,
    id: Uuid,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query!("DELETE FROM inventory WHERE id = $1", id)
        .execute(pool)
        .await?
        .rows_affected();

    Ok(rows > 0)
}

/// Get low-stock items
pub async fn get_low_stock_items(pool: &PgPool) -> Result<Vec<Inventory>, sqlx::Error> {
    let items = sqlx::query_as!(
        Inventory,
        r#"
        SELECT id, owner_id, name, description, quantity,
               location, token_id, token_image_url,
               category, unit, threshold, created_at
        FROM inventory
        WHERE quantity <= threshold
        ORDER BY quantity ASC
        "#
    )
    .fetch_all(pool)
    .await?;

    Ok(items)
}

/// 🔍 Search inventory by optional name & category
pub async fn search_inventory(
    pool: &PgPool,
    name: Option<String>,
    category: Option<String>,
) -> Result<Vec<Inventory>, sqlx::Error> {
    let mut query = String::from(
        r#"
        SELECT id, owner_id, name, description, quantity,
               location, token_id, token_image_url,
               category, unit, threshold, created_at
        FROM inventory
        WHERE 1=1
        "#
    );

    let mut args: Vec<String> = Vec::new();

    if let Some(n) = name {
        query.push_str(" AND name ILIKE $1");
        args.push(format!("%{}%", n));
    }

    if let Some(c) = category {
        query.push_str(&format!(" AND category = ${}", args.len() + 1));
        args.push(c);
    }

    let mut sql = sqlx::query_as::<_, Inventory>(&query);
    for a in args {
        sql = sql.bind(a);
    }

    Ok(sql.fetch_all(pool).await?)
}

/// 📥 Bulk insert inventory items
pub async fn bulk_import_inventory(
    pool: &PgPool,
    owner_id: Uuid,
    items: Vec<(String, Option<String>, i32, Option<String>, Option<String>, Option<i32>)>,
) -> Result<Vec<Inventory>, sqlx::Error> {
    let mut results = Vec::new();

    for (name, description, quantity, category, unit, threshold) in items {
        let item = add_inventory_item(
            pool,
            owner_id,
            &name,
            description.as_deref(),
            quantity,
            None, // location
            None, // token_id
            None, // token_image_url
            category.as_deref(),
            unit.as_deref(),
            threshold,
        )
        .await?;

        results.push(item);
    }

    Ok(results)
}

