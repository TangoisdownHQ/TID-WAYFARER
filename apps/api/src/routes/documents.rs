//! Shipping documents.
//!
//! Unglamorous and completely non-negotiable: physical freight does not move
//! without paper. A packing list, a bill of lading, a customs declaration and
//! a commercial invoice are what a dock, a carrier and a border actually
//! operate on.
//!
//! Every field is generated from data already held — the order, its manifest,
//! the lots, the custody chain, the certificates — and nothing is invented. A
//! document that quietly fabricates a value an inspector then relies on is
//! worse than no document, so a missing field is rendered as a visible blank
//! with a note, and the response separately lists what could not be filled.
//!
//! Output is structured JSON plus plain text. Deliberately not PDF: a PDF
//! generator is a dependency an offline outpost has to carry, and the text
//! form prints, faxes, pastes into a radio message and survives a format
//! nobody planned for. The JSON is there for anyone who wants to render it
//! properly.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::routes::auth_middleware::AuthenticatedUser;
use crate::AppState;

type ApiError = (StatusCode, String);

fn server_err(what: &'static str) -> impl Fn(sqlx::Error) -> ApiError {
    move |e| {
        tracing::error!(error = %e, "{what}");
        (StatusCode::INTERNAL_SERVER_ERROR, what.to_string())
    }
}

pub fn document_routes() -> Router<AppState> {
    Router::new()
        .route("/packing-list/:order_id", get(packing_list))
        .route("/bill-of-lading/:order_id", get(bill_of_lading))
        .route("/customs-declaration/:order_id", get(customs_declaration))
        .route("/invoice/:order_id", get(commercial_invoice))
}

#[derive(Deserialize)]
pub struct Format {
    /// `text` renders the printable form; anything else returns JSON.
    pub format: Option<String>,
}

/// Collected facts about one consignment, with gaps recorded rather than
/// papered over.
struct Consignment {
    order_id: Uuid,
    description: String,
    quantity: Decimal,
    unit: String,
    mass_kg: Option<Decimal>,
    volume_m3: Option<Decimal>,
    delivery_address: Option<String>,
    delivery_body_id: Option<i32>,
    needed_by: Option<DateTime<Utc>>,
    status: String,
    requester: Option<String>,
    shipper: Option<String>,
    price: Option<Decimal>,
    capsule: Option<String>,
    departs_at: Option<DateTime<Utc>>,
    lots: Vec<Value>,
    custody: Vec<Value>,
    certificates: Vec<Value>,
    /// Fields a document needs that the data could not supply.
    missing: Vec<String>,
}

async fn gather(state: &AppState, order_id: Uuid) -> Result<Consignment, ApiError> {
    let o = sqlx::query(
        r#"
        SELECT o.id, o.description, o.quantity, o.unit, o.mass_kg, o.volume_m3,
               o.delivery_address, o.delivery_body_id, o.needed_by, o.status,
               ru.username AS requester,
               su.username AS shipper,
               b.price
        FROM orders o
        LEFT JOIN users ru ON ru.id = o.requester_id
        LEFT JOIN fulfillments f ON f.order_id = o.id
        LEFT JOIN users su ON su.id = f.shipper_id
        LEFT JOIN bids b ON b.id = o.accepted_bid_id
        WHERE o.id = $1
        LIMIT 1
        "#,
    )
    .bind(order_id)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("order lookup failed"))?
    .ok_or((StatusCode::NOT_FOUND, "no such order".to_string()))?;

    let capsule = sqlx::query(
        r#"
        SELECT c.name, c.departs_at
        FROM capsule_manifest m JOIN capsules c ON c.id = m.capsule_id
        WHERE m.order_id = $1 LIMIT 1
        "#,
    )
    .bind(order_id)
    .fetch_optional(&state.db)
    .await
    .map_err(server_err("capsule lookup failed"))?;

    // Lots actually committed to this order, via the custody chain.
    let lots = sqlx::query(
        r#"
        SELECT DISTINCT l.lot_code, l.serial, l.quantity, l.expires_at, l.supplier, i.name
        FROM custody_receipts r
        JOIN inventory_lots l ON l.id = r.lot_id
        JOIN inventory i ON i.id = l.inventory_id
        WHERE r.order_id = $1
        "#,
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("lot lookup failed"))?;

    let custody = sqlx::query(
        r#"
        SELECT seq, event, from_label, to_label, condition, occurred_at, verified
        FROM custody_receipts WHERE order_id = $1 ORDER BY seq
        "#,
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("custody lookup failed"))?;

    let certs = sqlx::query(
        r#"
        SELECT c.kind, c.authority, c.identifier, c.expires_at
        FROM certifications c
        LEFT JOIN inventory i ON i.id = c.inventory_id
        WHERE c.status = 'valid'
          AND (c.expires_at IS NULL OR c.expires_at > NOW())
          AND (i.name = (SELECT description FROM orders WHERE id = $1)
            OR i.name = (SELECT target_meta->>'resource' FROM orders WHERE id = $1))
        "#,
    )
    .bind(order_id)
    .fetch_all(&state.db)
    .await
    .map_err(server_err("certificate lookup failed"))?;

    let mass: Option<Decimal> = o.get("mass_kg");
    let volume: Option<Decimal> = o.get("volume_m3");
    let shipper: Option<String> = o.get("shipper");
    let address: Option<String> = o.get("delivery_address");
    let price: Option<Decimal> = o.get("price");

    // Named explicitly so a document can show what it could not establish.
    let mut missing = Vec::new();
    if mass.is_none() { missing.push("gross mass".into()); }
    if volume.is_none() { missing.push("volume".into()); }
    if shipper.is_none() { missing.push("carrier (no accepted bid)".into()); }
    if address.is_none() { missing.push("delivery address".into()); }
    if price.is_none() { missing.push("declared value (no accepted bid)".into()); }
    if custody.is_empty() { missing.push("custody chain".into()); }

    Ok(Consignment {
        order_id,
        description: o.get("description"),
        quantity: o.get("quantity"),
        unit: o.get("unit"),
        mass_kg: mass,
        volume_m3: volume,
        delivery_address: address,
        delivery_body_id: o.get("delivery_body_id"),
        needed_by: o.get("needed_by"),
        status: o.get("status"),
        requester: o.get("requester"),
        shipper,
        price,
        capsule: capsule.as_ref().map(|c| c.get("name")),
        departs_at: capsule.as_ref().and_then(|c| c.get("departs_at")),
        lots: lots.iter().map(|l| json!({
            "item": l.get::<String, _>("name"),
            "lotCode": l.get::<Option<String>, _>("lot_code"),
            "serial": l.get::<Option<String>, _>("serial"),
            "quantity": l.get::<Decimal, _>("quantity").to_string(),
            "expiresAt": l.get::<Option<DateTime<Utc>>, _>("expires_at"),
            "supplier": l.get::<Option<String>, _>("supplier"),
        })).collect(),
        custody: custody.iter().map(|c| json!({
            "seq": c.get::<i32, _>("seq"),
            "event": c.get::<String, _>("event"),
            "from": c.get::<Option<String>, _>("from_label"),
            "to": c.get::<Option<String>, _>("to_label"),
            "condition": c.get::<String, _>("condition"),
            "at": c.get::<DateTime<Utc>, _>("occurred_at"),
            "verified": c.get::<bool, _>("verified"),
        })).collect(),
        certificates: certs.iter().map(|c| json!({
            "kind": c.get::<String, _>("kind"),
            "authority": c.get::<Option<String>, _>("authority"),
            "identifier": c.get::<Option<String>, _>("identifier"),
            "expiresAt": c.get::<Option<DateTime<Utc>>, _>("expires_at"),
        })).collect(),
        missing,
    })
}

/// A value, or a visible blank. Never a plausible-looking guess.
fn or_blank(v: Option<&str>) -> String {
    v.filter(|s| !s.is_empty()).unwrap_or("———————").to_string()
}

fn d(v: Option<Decimal>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "———————".into())
}

fn short(id: Uuid) -> String {
    id.to_string()[..8].to_uppercase()
}

fn header_block(title: &str, c: &Consignment) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", title.to_uppercase()));
    s.push_str(&format!("{}\n", "=".repeat(title.len().max(40))));
    s.push_str(&format!("Reference        TIDW-{}\n", short(c.order_id)));
    s.push_str(&format!("Issued           {}\n", Utc::now().format("%Y-%m-%d %H:%M UTC")));
    s.push_str(&format!("Order status     {}\n", c.status));
    s
}

fn footer_block(c: &Consignment) -> String {
    if c.missing.is_empty() {
        return String::new();
    }
    // Printed on the document itself, not merely returned in the envelope: the
    // person holding the paper is the one who needs to know it is incomplete.
    format!(
        "\n{}\nINCOMPLETE — the following could not be established from the record:\n{}\n",
        "-".repeat(60),
        c.missing.iter().map(|m| format!("  · {m}")).collect::<Vec<_>>().join("\n")
    )
}

async fn render(
    state: &AppState,
    order_id: Uuid,
    fmt: Option<String>,
    kind: &'static str,
    build_json: impl Fn(&Consignment) -> Value,
    build_text: impl Fn(&Consignment) -> String,
) -> Result<axum::response::Response, ApiError> {
    let c = gather(state, order_id).await?;

    if fmt.as_deref() == Some("text") {
        let body = build_text(&c);
        return Ok((
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("inline; filename=\"{kind}-TIDW-{}.txt\"", short(order_id)),
                ),
            ],
            body,
        )
            .into_response());
    }

    let mut v = build_json(&c);
    if let Value::Object(ref mut m) = v {
        m.insert("documentType".into(), json!(kind));
        m.insert("reference".into(), json!(format!("TIDW-{}", short(order_id))));
        m.insert("issuedAt".into(), json!(Utc::now()));
        m.insert("complete".into(), json!(c.missing.is_empty()));
        m.insert("missing".into(), json!(c.missing));
    }
    Ok(Json(v).into_response())
}

// ---------------------------------------------------------------------------

/// What is in the box, lot by lot.
async fn packing_list(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    Query(f): Query<Format>,
) -> Result<axum::response::Response, ApiError> {
    render(&state, order_id, f.format, "packing-list",
        |c| json!({
            "order": { "id": c.order_id, "description": c.description,
                       "quantity": c.quantity.to_string(), "unit": c.unit },
            "consignee": c.requester,
            "shipper": c.shipper,
            "destination": c.delivery_address,
            "grossMassKg": c.mass_kg.map(|m| m.to_string()),
            "volumeM3": c.volume_m3.map(|v| v.to_string()),
            "lots": c.lots,
        }),
        |c| {
            let mut s = header_block("Packing list", c);
            s.push_str(&format!("\nShipper          {}\n", or_blank(c.shipper.as_deref())));
            s.push_str(&format!("Consignee        {}\n", or_blank(c.requester.as_deref())));
            s.push_str(&format!("Destination      {}\n", or_blank(c.delivery_address.as_deref())));
            s.push_str(&format!("\nCONTENTS\n{}\n", "-".repeat(60)));
            s.push_str(&format!("{} — {} {}\n", c.description, c.quantity, c.unit));
            s.push_str(&format!("Gross mass       {} kg\n", d(c.mass_kg)));
            s.push_str(&format!("Volume           {} m3\n", d(c.volume_m3)));
            if c.lots.is_empty() {
                s.push_str("\nNo lot or serial detail recorded for this consignment.\n");
            } else {
                s.push_str(&format!("\nLOTS AND SERIALS\n{}\n", "-".repeat(60)));
                for l in &c.lots {
                    s.push_str(&format!(
                        "  {:<24} lot {:<14} serial {:<14} qty {}\n",
                        l["item"].as_str().unwrap_or(""),
                        l["lotCode"].as_str().unwrap_or("—"),
                        l["serial"].as_str().unwrap_or("—"),
                        l["quantity"].as_str().unwrap_or("—"),
                    ));
                }
            }
            s.push_str(&footer_block(c));
            s
        },
    ).await
}

/// The carriage contract: who moves it, from where, to where, and in what
/// condition it was received at each leg.
async fn bill_of_lading(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    Query(f): Query<Format>,
) -> Result<axum::response::Response, ApiError> {
    render(&state, order_id, f.format, "bill-of-lading",
        |c| json!({
            "shipper": c.shipper, "consignee": c.requester,
            "carrier": c.capsule, "departsAt": c.departs_at,
            "destination": c.delivery_address, "destinationBodyId": c.delivery_body_id,
            "goods": { "description": c.description, "quantity": c.quantity.to_string(),
                       "unit": c.unit, "grossMassKg": c.mass_kg.map(|m| m.to_string()) },
            "custodyChain": c.custody,
        }),
        |c| {
            let mut s = header_block("Bill of lading", c);
            s.push_str(&format!("\nShipper          {}\n", or_blank(c.shipper.as_deref())));
            s.push_str(&format!("Consignee        {}\n", or_blank(c.requester.as_deref())));
            s.push_str(&format!("Carrier / hull   {}\n", or_blank(c.capsule.as_deref())));
            s.push_str(&format!("Departs          {}\n",
                c.departs_at.map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or("———————".into())));
            s.push_str(&format!("Destination      {}\n", or_blank(c.delivery_address.as_deref())));
            s.push_str(&format!("\nGOODS\n{}\n", "-".repeat(60)));
            s.push_str(&format!("{} — {} {}   gross {} kg\n", c.description, c.quantity, c.unit, d(c.mass_kg)));
            s.push_str(&format!("\nCUSTODY\n{}\n", "-".repeat(60)));
            if c.custody.is_empty() {
                s.push_str("  No handoffs recorded.\n");
            } else {
                for leg in &c.custody {
                    s.push_str(&format!(
                        "  {} {:<10} {:<16} -> {:<16} {:<12} {}\n",
                        leg["seq"], leg["event"].as_str().unwrap_or(""),
                        leg["from"].as_str().unwrap_or("—"), leg["to"].as_str().unwrap_or("—"),
                        leg["condition"].as_str().unwrap_or(""),
                        // Whether the leg is attested matters on the paper, not
                        // only in the API: an unsigned handoff is a weaker claim
                        // and the person signing for it should see that.
                        if leg["verified"] == json!(true) { "signed" } else { "UNSIGNED" },
                    ));
                }
            }
            s.push_str(&footer_block(c));
            s
        },
    ).await
}

/// What a border is told: contents, value, origin, and the licences relied on.
async fn customs_declaration(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    Query(f): Query<Format>,
) -> Result<axum::response::Response, ApiError> {
    render(&state, order_id, f.format, "customs-declaration",
        |c| json!({
            "exporter": c.shipper, "importer": c.requester,
            "destinationBodyId": c.delivery_body_id, "destination": c.delivery_address,
            "goods": { "description": c.description, "quantity": c.quantity.to_string(),
                       "unit": c.unit, "grossMassKg": c.mass_kg.map(|m| m.to_string()) },
            "declaredValue": c.price.map(|p| p.to_string()),
            "currency": "TIDAT",
            "certificates": c.certificates,
        }),
        |c| {
            let mut s = header_block("Customs declaration", c);
            s.push_str(&format!("\nExporter         {}\n", or_blank(c.shipper.as_deref())));
            s.push_str(&format!("Importer         {}\n", or_blank(c.requester.as_deref())));
            s.push_str(&format!("Destination      {} (body {})\n",
                or_blank(c.delivery_address.as_deref()),
                c.delivery_body_id.map(|b| b.to_string()).unwrap_or("—".into())));
            s.push_str(&format!("\nGOODS DECLARED\n{}\n", "-".repeat(60)));
            s.push_str(&format!("{} — {} {}\n", c.description, c.quantity, c.unit));
            s.push_str(&format!("Gross mass       {} kg\n", d(c.mass_kg)));
            s.push_str(&format!("Declared value   {} TIDAT\n", d(c.price)));
            s.push_str(&format!("\nLICENCES AND CERTIFICATES RELIED ON\n{}\n", "-".repeat(60)));
            if c.certificates.is_empty() {
                s.push_str("  None on file for these goods.\n");
            } else {
                for cert in &c.certificates {
                    s.push_str(&format!("  {:<12} {:<28} {}\n",
                        cert["kind"].as_str().unwrap_or(""),
                        cert["authority"].as_str().unwrap_or("—"),
                        cert["identifier"].as_str().unwrap_or("—")));
                }
            }
            s.push_str("\nI declare the above to be a true account of the consignment.\n");
            s.push_str("\nSignature  ______________________    Date  ____________\n");
            s.push_str(&footer_block(c));
            s
        },
    ).await
}

/// What is owed, and for what.
async fn commercial_invoice(
    State(state): State<AppState>,
    _user: AuthenticatedUser,
    Path(order_id): Path<Uuid>,
    Query(f): Query<Format>,
) -> Result<axum::response::Response, ApiError> {
    render(&state, order_id, f.format, "invoice",
        |c| json!({
            "from": c.shipper, "to": c.requester,
            "lineItems": [{ "description": c.description,
                            "quantity": c.quantity.to_string(), "unit": c.unit,
                            "amount": c.price.map(|p| p.to_string()) }],
            "total": c.price.map(|p| p.to_string()), "currency": "TIDAT",
            "dueBy": c.needed_by,
        }),
        |c| {
            let mut s = header_block("Commercial invoice", c);
            s.push_str(&format!("\nFrom             {}\n", or_blank(c.shipper.as_deref())));
            s.push_str(&format!("To               {}\n", or_blank(c.requester.as_deref())));
            s.push_str(&format!("\n{:<42}{:>8}{:>14}\n", "DESCRIPTION", "QTY", "AMOUNT"));
            s.push_str(&format!("{}\n", "-".repeat(64)));
            s.push_str(&format!("{:<42}{:>8}{:>14}\n",
                c.description.chars().take(42).collect::<String>(),
                c.quantity.to_string(), d(c.price)));
            s.push_str(&format!("{}\n", "-".repeat(64)));
            s.push_str(&format!("{:<50}{:>14}\n", "TOTAL (TIDAT)", d(c.price)));
            s.push_str(&footer_block(c));
            s
        },
    ).await
}
