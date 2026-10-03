//! Reaching commercial carriers — rating, buying a label, and tracking.
//!
//! # Why an aggregator, and why `manual` is the default
//!
//! UPS, USPS, FedEx and DHL are four APIs, four auth schemes and four sets of
//! quirks for one feature. A first implementation goes through an aggregator
//! (EasyPost here) so that one integration reaches all of them; direct
//! integrations are worth it later, per carrier, where negotiated rates
//! justify the work.
//!
//! But rating and buying a label **need the network**, and this is a tool whose
//! premise is that the network may be gone for days. So the provider that
//! always works is `Manual`: an operator rates and buys on the carrier's own
//! site, or at a counter, and records the tracking number. That is not a
//! degraded mode bolted on afterwards — it is the default, and every other
//! provider is an optimisation over it.
//!
//! A consequence worth stating: a shipment recorded manually is
//! indistinguishable, downstream, from one bought through an API. The custody
//! chain, the compliance check and the cost accounting do not care which
//! happened.
//!
//! # No trait object
//!
//! Dispatch is an enum rather than a `dyn CarrierProvider`, because there are
//! two implementations and adding `async-trait` to the workspace to express
//! two match arms is a poor trade.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How we reach a carrier.
#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    /// No API. The operator supplies the tracking number; nothing is fetched.
    /// Works with no link, which is why it is the fallback for everything.
    Manual,
    /// EasyPost, reaching UPS/USPS/FedEx/DHL through one integration.
    EasyPost(String),
}

impl Provider {
    /// Resolve a provider name against the environment.
    ///
    /// An unknown name, or a configured provider with no key, falls back to
    /// `Manual` rather than erroring. A missing API key should not stop an
    /// operator recording a parcel they have already handed over — the parcel
    /// exists either way, and refusing to record it loses the custody link.
    pub fn resolve(name: &str) -> Self {
        match name {
            "easypost" => match std::env::var("CARRIER_EASYPOST_KEY") {
                Ok(k) if !k.trim().is_empty() => Provider::EasyPost(k),
                _ => {
                    tracing::warn!(
                        "carrier provider 'easypost' requested but CARRIER_EASYPOST_KEY is unset; \
                         falling back to manual"
                    );
                    Provider::Manual
                }
            },
            _ => Provider::Manual,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Provider::Manual => "manual",
            Provider::EasyPost(_) => "easypost",
        }
    }

    /// Whether this provider can fetch anything. Lets a handler tell an
    /// operator "type the tracking number" rather than failing a request.
    pub fn is_online(&self) -> bool {
        !matches!(self, Provider::Manual)
    }
}

/// A postal or coordinate destination, as the provider needs it.
#[derive(Debug, Clone, Serialize)]
pub struct Address {
    pub name: Option<String>,
    pub company: Option<String>,
    pub line1: Option<String>,
    pub line2: Option<String>,
    pub city: Option<String>,
    pub region: Option<String>,
    pub postcode: Option<String>,
    pub country: Option<String>,
    pub phone: Option<String>,
    pub residential: Option<bool>,
}

/// What is being shipped, physically.
#[derive(Debug, Clone, Serialize)]
pub struct Parcel {
    pub weight_kg: Decimal,
    pub length_cm: Option<Decimal>,
    pub width_cm: Option<Decimal>,
    pub height_cm: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RateRequest {
    pub from: Address,
    pub to: Address,
    pub parcel: Parcel,
}

/// One option a carrier offered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateQuote {
    pub carrier: String,
    pub service: String,
    pub amount: Decimal,
    pub currency: String,
    pub transit_days: Option<Decimal>,
    /// The provider's handle for this quote, needed to buy it.
    pub provider_rate_id: Option<String>,
    /// Set when the carrier billed a dimensional weight above the actual one —
    /// surfacing it stops "why is this twice the quote" being a mystery.
    pub billable_weight_kg: Option<Decimal>,
}

#[derive(Debug, Clone)]
pub struct PurchasedLabel {
    pub tracking_number: String,
    pub tracking_url: Option<String>,
    pub label_url: Option<String>,
    pub label_format: Option<String>,
    pub cost_amount: Option<Decimal>,
    pub cost_currency: Option<String>,
    pub provider_shipment_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TrackingEvent {
    pub status: String,
    pub detail: Option<String>,
    pub location: Option<String>,
    pub occurred_at: chrono::DateTime<chrono::Utc>,
    /// The carrier's own id for this event. Polling is at-least-once, so
    /// without it a re-poll duplicates the whole history — the same
    /// idempotency problem as DTN, one layer up.
    pub source_ref: Option<String>,
}

#[derive(Debug)]
pub enum CarrierError {
    /// This provider cannot do that — `Manual` cannot rate or fetch.
    NotSupported(&'static str),
    /// The provider was reachable and refused.
    Refused(String),
    /// Could not reach the provider. Retryable.
    Unreachable(String),
}

impl std::fmt::Display for CarrierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CarrierError::NotSupported(w) => write!(f, "not supported by this provider: {w}"),
            CarrierError::Refused(m) => write!(f, "carrier refused: {m}"),
            CarrierError::Unreachable(m) => write!(f, "carrier unreachable: {m}"),
        }
    }
}

fn timeout() -> Duration {
    Duration::from_secs(
        std::env::var("CARRIER_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|s| *s > 0)
            .unwrap_or(20),
    )
}

impl Provider {
    /// Ask for rates. `Manual` cannot, and says so rather than returning an
    /// empty list — "no rates available" and "this provider does not rate"
    /// would otherwise look identical to a caller.
    pub async fn rate(&self, req: &RateRequest) -> Result<Vec<RateQuote>, CarrierError> {
        match self {
            Provider::Manual => Err(CarrierError::NotSupported(
                "rating — get the price from the carrier and record the shipment manually",
            )),
            Provider::EasyPost(key) => easypost_rate(key, req).await,
        }
    }

    pub async fn buy(&self, rate_id: &str) -> Result<PurchasedLabel, CarrierError> {
        match self {
            Provider::Manual => Err(CarrierError::NotSupported(
                "buying a label — buy it from the carrier and record the tracking number",
            )),
            Provider::EasyPost(key) => easypost_buy(key, rate_id).await,
        }
    }

    pub async fn track(
        &self,
        carrier: &str,
        tracking_number: &str,
    ) -> Result<Vec<TrackingEvent>, CarrierError> {
        match self {
            Provider::Manual => Err(CarrierError::NotSupported(
                "tracking — update the shipment by hand, or paste the carrier's status",
            )),
            Provider::EasyPost(key) => easypost_track(key, carrier, tracking_number).await,
        }
    }
}

// ---------------------------------------------------------------------------
// EasyPost
// ---------------------------------------------------------------------------
//
// Deliberately thin. The provider's job is to turn its own shapes into the
// four types above and nothing else — no retry policy, no caching, no
// business rules. Those belong to the caller, which can see the whole
// shipment; this can only see one HTTP call.

const EASYPOST_BASE: &str = "https://api.easypost.com/v2";

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// EasyPost works in whole ounces/inches. Converting here rather than at the
/// call site keeps the rest of the application metric, which is what the
/// catalogue and the rate cards already are.
fn kg_to_oz(kg: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    kg.to_f64().unwrap_or(0.0) * 35.27396
}

fn cm_to_in(cm: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    cm.to_f64().unwrap_or(0.0) / 2.54
}

fn ep_address(a: &Address) -> serde_json::Value {
    serde_json::json!({
        "name": a.name,
        "company": a.company,
        "street1": a.line1,
        "street2": a.line2,
        "city": a.city,
        "state": a.region,
        "zip": a.postcode,
        "country": a.country,
        "phone": a.phone,
        "residential": a.residential,
    })
}

async fn easypost_rate(key: &str, req: &RateRequest) -> Result<Vec<RateQuote>, CarrierError> {
    let body = serde_json::json!({
        "shipment": {
            "from_address": ep_address(&req.from),
            "to_address": ep_address(&req.to),
            "parcel": {
                "weight": kg_to_oz(req.parcel.weight_kg),
                "length": req.parcel.length_cm.map(cm_to_in),
                "width":  req.parcel.width_cm.map(cm_to_in),
                "height": req.parcel.height_cm.map(cm_to_in),
            }
        }
    });

    let resp = client()
        .post(format!("{EASYPOST_BASE}/shipments"))
        .basic_auth(key, Some(""))
        .json(&body)
        .timeout(timeout())
        .send()
        .await
        .map_err(|e| CarrierError::Unreachable(e.to_string()))?;

    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CarrierError::Unreachable(format!("unreadable response: {e}")))?;

    if !status.is_success() {
        return Err(CarrierError::Refused(describe_ep_error(&json, status)));
    }

    let rates = json
        .get("rates")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();

    // An empty rate list is a refusal with a reason, not a success with no
    // options: usually an unvalidatable address or a parcel no service
    // accepts. Reporting it as "no rates" sends an operator looking in the
    // wrong place.
    if rates.is_empty() {
        return Err(CarrierError::Refused(
            "no services available for this address and parcel — check the address and weight"
                .into(),
        ));
    }

    Ok(rates
        .iter()
        .filter_map(|r| {
            Some(RateQuote {
                carrier: r.get("carrier")?.as_str()?.to_lowercase(),
                service: r.get("service")?.as_str()?.to_string(),
                amount: r.get("rate")?.as_str()?.parse().ok()?,
                currency: r
                    .get("currency")
                    .and_then(|c| c.as_str())
                    .unwrap_or("USD")
                    .to_string(),
                transit_days: r
                    .get("delivery_days")
                    .and_then(|d| d.as_i64())
                    .map(Decimal::from),
                provider_rate_id: r.get("id").and_then(|i| i.as_str()).map(String::from),
                billable_weight_kg: None,
            })
        })
        .collect())
}

async fn easypost_buy(key: &str, rate_id: &str) -> Result<PurchasedLabel, CarrierError> {
    // EasyPost buys a rate through its shipment, so the rate id carries the
    // shipment it belongs to. Asking for the rate first means a stale rate id
    // fails here, with a reason, rather than buying something unexpected.
    let rate = client()
        .get(format!("{EASYPOST_BASE}/rates/{rate_id}"))
        .basic_auth(key, Some(""))
        .timeout(timeout())
        .send()
        .await
        .map_err(|e| CarrierError::Unreachable(e.to_string()))?
        .json::<serde_json::Value>()
        .await
        .map_err(|e| CarrierError::Unreachable(format!("unreadable rate: {e}")))?;

    let shipment_id = rate
        .get("shipment_id")
        .and_then(|s| s.as_str())
        .ok_or_else(|| CarrierError::Refused("rate is no longer available".into()))?;

    let resp = client()
        .post(format!("{EASYPOST_BASE}/shipments/{shipment_id}/buy"))
        .basic_auth(key, Some(""))
        .json(&serde_json::json!({ "rate": { "id": rate_id } }))
        .timeout(timeout())
        .send()
        .await
        .map_err(|e| CarrierError::Unreachable(e.to_string()))?;

    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CarrierError::Unreachable(format!("unreadable response: {e}")))?;

    if !status.is_success() {
        return Err(CarrierError::Refused(describe_ep_error(&json, status)));
    }

    let tracking_number = json
        .get("tracking_code")
        .and_then(|t| t.as_str())
        .ok_or_else(|| {
            // Bought and untrackable is the worst outcome: money spent, and no
            // evidence to hang a custody handover on. Treated as a refusal so
            // the caller does not record a purchased shipment.
            CarrierError::Refused("label bought but the carrier returned no tracking code".into())
        })?
        .to_string();

    Ok(PurchasedLabel {
        tracking_number,
        tracking_url: json.get("tracker").and_then(|t| t.get("public_url"))
            .and_then(|u| u.as_str()).map(String::from),
        label_url: json.pointer("/postage_label/label_url").and_then(|u| u.as_str()).map(String::from),
        label_format: json.pointer("/postage_label/label_file_type")
            .and_then(|f| f.as_str())
            .map(|f| f.rsplit('/').next().unwrap_or(f).to_string()),
        cost_amount: json
            .get("selected_rate")
            .and_then(|r| r.get("rate"))
            .and_then(|r| r.as_str())
            .and_then(|s| s.parse().ok()),
        cost_currency: json
            .get("selected_rate")
            .and_then(|r| r.get("currency"))
            .and_then(|c| c.as_str())
            .map(String::from),
        provider_shipment_id: json.get("id").and_then(|i| i.as_str()).map(String::from),
    })
}

async fn easypost_track(
    key: &str,
    carrier: &str,
    tracking_number: &str,
) -> Result<Vec<TrackingEvent>, CarrierError> {
    let resp = client()
        .post(format!("{EASYPOST_BASE}/trackers"))
        .basic_auth(key, Some(""))
        .json(&serde_json::json!({
            "tracker": { "tracking_code": tracking_number, "carrier": carrier }
        }))
        .timeout(timeout())
        .send()
        .await
        .map_err(|e| CarrierError::Unreachable(e.to_string()))?;

    let status = resp.status();
    let json: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CarrierError::Unreachable(format!("unreadable response: {e}")))?;

    if !status.is_success() {
        return Err(CarrierError::Refused(describe_ep_error(&json, status)));
    }

    Ok(json
        .get("tracking_details")
        .and_then(|d| d.as_array())
        .map(|events| {
            events
                .iter()
                .filter_map(|e| {
                    let occurred = e.get("datetime").and_then(|d| d.as_str())?;
                    Some(TrackingEvent {
                        status: e.get("status").and_then(|s| s.as_str()).unwrap_or("unknown").to_string(),
                        detail: e.get("message").and_then(|m| m.as_str()).map(String::from),
                        location: e.get("tracking_location").map(describe_location),
                        occurred_at: chrono::DateTime::parse_from_rfc3339(occurred)
                            .ok()?
                            .with_timezone(&chrono::Utc),
                        // EasyPost does not give events ids, so the timestamp
                        // and status together are the dedupe key. Two distinct
                        // events at the same second with the same status are
                        // indistinguishable — acceptable, because they are
                        // also indistinguishable to an operator.
                        source_ref: Some(format!("{occurred}|{}",
                            e.get("status").and_then(|s| s.as_str()).unwrap_or(""))),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn describe_location(loc: &serde_json::Value) -> String {
    let part = |k: &str| loc.get(k).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    [part("city"), part("state"), part("country")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
}

/// Turn a provider error body into something an operator can act on.
///
/// Carrier APIs put the useful part in nested per-field errors, and reporting
/// only the top-level message produces "Unable to buy shipment" with no cause.
fn describe_ep_error(json: &serde_json::Value, status: reqwest::StatusCode) -> String {
    let top = json
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .map(String::from);

    let fields: Vec<String> = json
        .pointer("/error/errors")
        .and_then(|e| e.as_array())
        .map(|errs| {
            errs.iter()
                .filter_map(|e| {
                    let msg = e.get("message").and_then(|m| m.as_str())?;
                    match e.get("field").and_then(|f| f.as_str()) {
                        Some(f) => Some(format!("{f}: {msg}")),
                        None => Some(msg.to_string()),
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    match (top, fields.is_empty()) {
        (Some(t), true) => t,
        (Some(t), false) => format!("{t} ({})", fields.join("; ")),
        (None, false) => fields.join("; "),
        (None, true) => format!("http {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unconfigured provider degrades to manual rather than failing.
    ///
    /// A missing API key must not stop an operator recording a parcel they
    /// have already handed over — the parcel exists either way, and refusing
    /// to record it loses the custody link, which is the thing of lasting
    /// value here.
    #[test]
    fn an_unconfigured_provider_falls_back_to_manual() {
        std::env::remove_var("CARRIER_EASYPOST_KEY");
        assert_eq!(Provider::resolve("easypost"), Provider::Manual);
        assert_eq!(Provider::resolve("nonsense"), Provider::Manual);
        assert_eq!(Provider::resolve("manual"), Provider::Manual);
    }

    #[test]
    fn a_configured_provider_is_online_and_manual_is_not() {
        assert!(!Provider::Manual.is_online());
        assert!(Provider::EasyPost("k".into()).is_online());
        assert_eq!(Provider::EasyPost("k".into()).name(), "easypost");
    }

    /// Manual refuses with a reason that tells an operator what to do instead,
    /// rather than returning an empty list that reads as "no services".
    #[tokio::test]
    async fn manual_refuses_rating_but_says_what_to_do() {
        let req = RateRequest {
            from: addr(),
            to: addr(),
            parcel: Parcel { weight_kg: Decimal::from(1), length_cm: None, width_cm: None, height_cm: None },
        };
        match Provider::Manual.rate(&req).await {
            Err(CarrierError::NotSupported(msg)) => assert!(msg.contains("record the shipment")),
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    fn addr() -> Address {
        Address {
            name: None, company: None, line1: Some("1 Dock Rd".into()), line2: None,
            city: Some("Chicago".into()), region: Some("IL".into()),
            postcode: Some("60601".into()), country: Some("US".into()),
            phone: None, residential: Some(false),
        }
    }

    /// Unit conversion happens at the provider boundary, so the rest of the
    /// application stays metric like the catalogue and the rate cards.
    #[test]
    fn weights_and_lengths_convert_at_the_boundary() {
        assert!((kg_to_oz(Decimal::from(1)) - 35.27396).abs() < 0.001);
        assert!((cm_to_in(Decimal::from(254)) - 100.0).abs() < 0.001);
    }

    /// A provider error must name the field that was wrong.
    ///
    /// "Unable to buy shipment" with no cause is the difference between an
    /// operator fixing a postcode in ten seconds and filing a support ticket.
    #[test]
    fn a_provider_error_names_the_offending_field() {
        let body = serde_json::json!({
            "error": {
                "message": "Unable to buy shipment",
                "errors": [
                    { "field": "to_address.zip", "message": "is invalid" },
                    { "field": "parcel.weight", "message": "must be greater than 0" }
                ]
            }
        });
        let msg = describe_ep_error(&body, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(msg.contains("to_address.zip: is invalid"), "{msg}");
        assert!(msg.contains("parcel.weight"), "{msg}");
        assert!(msg.contains("Unable to buy shipment"), "{msg}");
    }

    /// An error body with nothing useful in it still produces something.
    #[test]
    fn an_opaque_provider_error_still_reports_the_status() {
        let msg = describe_ep_error(&serde_json::json!({}), reqwest::StatusCode::BAD_GATEWAY);
        assert!(msg.contains("502"), "{msg}");
    }

    #[test]
    fn a_tracking_location_reads_as_a_place() {
        let loc = serde_json::json!({ "city": "Memphis", "state": "TN", "country": "US" });
        assert_eq!(describe_location(&loc), "Memphis, TN, US");
        // Partial locations are common and must not render stray commas.
        let partial = serde_json::json!({ "city": "", "state": "TN", "country": "US" });
        assert_eq!(describe_location(&partial), "TN, US");
    }
}
