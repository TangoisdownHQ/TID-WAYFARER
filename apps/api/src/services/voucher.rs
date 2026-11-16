//! Conditional Transfer Vouchers — the offline-settlement primitive.
//!
//! A CTV is a signed promise to pay that needs no network to create or to
//! validate. A trade closes locally by exchanging one; the chain becomes an
//! eventual anchor, not the transaction medium — the same store-and-forward
//! shape as DTN, the command queue and settlement verification.
//!
//! This module is the pure core: canonical encoding, signing, verification,
//! nonce rules and condition evaluation. No database and no network, so a CTV
//! can be minted and checked on an outpost that has been dark for a month —
//! everything needed to validate one travels inside it. The DB tables, the
//! commit/present endpoints and the anchoring daemon build on top of this, the
//! same way `routes::custody` builds on `services::custody`.
//!
//! The delivery condition is discharged by a custody receipt chain, which
//! already exists as `services::custody` — a CTV does not re-implement it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::services::identity::{sign_message, verify_signature, NodeIdentity};

/// Domain prefix. Distinct from `fabric|` (request signing), `dtn-v2|*`
/// (envelopes) and `custody|v1` (receipts), so a signature over a voucher can
/// never be reinterpreted as any of those — the discipline used everywhere in
/// the fabric.
const DOMAIN: &str = "ctv|v1";

/// A condition that must hold before a voucher can be claimed.
///
/// Ordered as given; both the order and the values are covered by the
/// signature, so a payee cannot drop or weaken a condition after the fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Condition {
    /// Discharged by a complete, continuous custody chain whose final receipt
    /// is signed by the recipient named in the order (see `services::custody`).
    DeliveryReceipt,
    /// Unix seconds. The voucher is only claimable at or before this instant.
    NotAfter(i64),
}

impl Condition {
    /// Stable textual form for the canonical encoding. Must never change for an
    /// existing variant, or already-signed vouchers stop verifying.
    fn encode(&self) -> String {
        match self {
            Condition::DeliveryReceipt => "delivery_receipt".to_string(),
            Condition::NotAfter(ts) => format!("not_after:{ts}"),
        }
    }
}

/// The fields a voucher signature commits to.
///
/// Borrowed like `custody::ReceiptClaim`: the row and the wire payload own the
/// strings, this just names what gets signed. Order is fixed and every field is
/// length-prefixed, so no two different vouchers can share canonical bytes.
#[derive(Debug, Clone, Serialize)]
pub struct VoucherClaim<'a> {
    pub id: Uuid,
    pub order_id: Uuid,
    pub fulfillment_id: Option<Uuid>,
    /// The payer is a node *and* a user: who signed, and on whose behalf.
    pub payer_node_id: Uuid,
    pub payer_user_id: Uuid,
    /// Where payment must land. Must exist at commit time (checked by the
    /// caller, which can see the order); here it is simply part of the promise.
    pub payee_wallet: &'a str,
    /// NUMERIC rendered to the exact string both sides agree on, like
    /// `custody::ReceiptClaim::quantity` — avoids a float round-trip changing
    /// the signed bytes.
    pub amount: &'a str,
    /// Which token counts as payment.
    pub mint: &'a str,
    /// The on-chain escrow this voucher draws against, if any. `None` is an
    /// IOU — a debt settled on reconnect, worth exactly the counterparty.
    pub escrow_ref: Option<&'a str>,
    pub conditions: &'a [Condition],
    /// Monotonic per `(payer, escrow_ref)`. A gap or duplicate is detectable
    /// the moment two vouchers meet, which is what makes over-commitment
    /// provable after the fact even though it cannot be prevented offline.
    pub nonce: u64,
    /// The settlement horizon, RFC3339 seconds — the string both sides agree
    /// on. Set from the route's realistic contact schedule, not a constant.
    pub anchor_by: &'a str,
}

/// Length-prefix a field so boundaries are unambiguous. Same encoding as
/// `custody::canonical_receipt`, deliberately, so there is one convention.
fn field(out: &mut String, value: &str) {
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
    out.push('|');
}

/// The exact bytes a payer signs for one voucher.
pub fn canonical_voucher(c: &VoucherClaim<'_>) -> String {
    let conditions = c
        .conditions
        .iter()
        .map(Condition::encode)
        .collect::<Vec<_>>()
        .join(",");

    let mut s = String::with_capacity(320);
    s.push_str(DOMAIN);
    s.push('|');
    field(&mut s, &c.id.to_string());
    field(&mut s, &c.order_id.to_string());
    field(&mut s, &c.fulfillment_id.map(|v| v.to_string()).unwrap_or_default());
    field(&mut s, &c.payer_node_id.to_string());
    field(&mut s, &c.payer_user_id.to_string());
    field(&mut s, c.payee_wallet);
    field(&mut s, c.amount);
    field(&mut s, c.mint);
    field(&mut s, c.escrow_ref.unwrap_or(""));
    field(&mut s, &conditions);
    field(&mut s, &c.nonce.to_string());
    field(&mut s, c.anchor_by);
    s
}

/// Sign a voucher with the payer's existing node identity key.
pub fn sign_voucher(payer: &NodeIdentity, claim: &VoucherClaim<'_>) -> String {
    sign_message(payer, canonical_voucher(claim).as_bytes())
}

/// Verify a voucher against the payer node's registered public key.
pub fn verify_voucher(claim: &VoucherClaim<'_>, signature: &str, payer_public_key: &str) -> bool {
    verify_signature(payer_public_key, canonical_voucher(claim).as_bytes(), signature)
}

/// Whether `candidate` is a fresh nonce given the highest already seen for this
/// `(payer, escrow_ref)`.
///
/// Strictly increasing: a value at or below the high-water mark is a replay or
/// a reused slot and is refused. Gaps above it are allowed here — a gap is not
/// a forgery, and the design wants gaps *detectable* (surfaced when two
/// vouchers meet), not silently rejected at mint time.
pub fn nonce_is_fresh(highest_seen: Option<u64>, candidate: u64) -> bool {
    match highest_seen {
        None => true,
        Some(high) => candidate > high,
    }
}

/// The next nonce a payer should use for a `(payer, escrow_ref)` series.
pub fn next_nonce(highest_seen: Option<u64>) -> u64 {
    highest_seen.map(|h| h + 1).unwrap_or(0)
}

/// Whether every condition holds, so the voucher may be claimed.
///
/// `now` is unix seconds; `delivery_discharged` is the caller's verdict on the
/// custody chain (complete, continuous, final receipt signed by the order's
/// recipient) — this module does not re-check the chain, `services::custody`
/// does.
pub fn conditions_met(conditions: &[Condition], now: i64, delivery_discharged: bool) -> bool {
    conditions.iter().all(|c| match c {
        Condition::DeliveryReceipt => delivery_discharged,
        Condition::NotAfter(ts) => now <= *ts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn uid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn payer() -> NodeIdentity {
        let k = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(k.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(k.to_bytes()),
        }
    }

    fn claim<'a>(nonce: u64, conditions: &'a [Condition]) -> VoucherClaim<'a> {
        VoucherClaim {
            id: uid(1),
            order_id: uid(2),
            fulfillment_id: Some(uid(3)),
            payer_node_id: uid(4),
            payer_user_id: uid(5),
            payee_wallet: "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM",
            amount: "100",
            mint: "TIDATmint11111111111111111111111111111111111",
            escrow_ref: Some("escrow-1"),
            conditions,
            nonce,
            anchor_by: "2026-12-31T00:00:00Z",
        }
    }

    #[test]
    fn a_voucher_verifies_against_the_payers_key() {
        let p = payer();
        let conds = [Condition::DeliveryReceipt, Condition::NotAfter(1_900_000_000)];
        let c = claim(0, &conds);
        let sig = sign_voucher(&p, &c);
        assert!(verify_voucher(&c, &sig, &p.public_key));
    }

    #[test]
    fn changing_any_signed_field_invalidates_the_voucher() {
        let p = payer();
        let conds = [Condition::DeliveryReceipt];
        let c = claim(0, &conds);
        let sig = sign_voucher(&p, &c);

        // Amount is the field a payee would most want to inflate after signing.
        let mut more = c.clone();
        more.amount = "1000";
        assert!(!verify_voucher(&more, &sig, &p.public_key));

        let mut elsewhere = c.clone();
        elsewhere.payee_wallet = "Attacker1111111111111111111111111111111111";
        assert!(!verify_voucher(&elsewhere, &sig, &p.public_key));

        let mut replayed = c.clone();
        replayed.nonce = 1;
        assert!(!verify_voucher(&replayed, &sig, &p.public_key));

        // Dropping a condition must not keep the signature valid.
        let weaker: &[Condition] = &[];
        let mut unconditional = c.clone();
        unconditional.conditions = weaker;
        assert!(!verify_voucher(&unconditional, &sig, &p.public_key));
    }

    #[test]
    fn another_nodes_key_does_not_verify() {
        let p = payer();
        let impostor = payer();
        let conds = [Condition::DeliveryReceipt];
        let c = claim(0, &conds);
        let sig = sign_voucher(&impostor, &c);
        assert!(!verify_voucher(&c, &sig, &p.public_key));
    }

    #[test]
    fn length_prefixing_stops_field_boundaries_from_sliding() {
        // Without length prefixes, (amount "10", mint "0X…") and
        // (amount "100", mint "X…") could produce identical bytes.
        let conds = [Condition::DeliveryReceipt];
        let mut a = claim(0, &conds);
        a.amount = "10";
        a.mint = "0Xmint";
        let mut b = claim(0, &conds);
        b.amount = "100";
        b.mint = "Xmint";
        assert_ne!(canonical_voucher(&a), canonical_voucher(&b));
    }

    #[test]
    fn an_iou_and_an_escrowed_voucher_sign_differently() {
        let conds = [Condition::DeliveryReceipt];
        let mut escrowed = claim(0, &conds);
        escrowed.escrow_ref = Some("escrow-1");
        let mut iou = claim(0, &conds);
        iou.escrow_ref = None;
        assert_ne!(canonical_voucher(&escrowed), canonical_voucher(&iou));
    }

    #[test]
    fn nonces_are_strictly_increasing() {
        assert!(nonce_is_fresh(None, 0));
        assert!(nonce_is_fresh(Some(4), 5));
        // A reused or lower slot is a replay.
        assert!(!nonce_is_fresh(Some(5), 5));
        assert!(!nonce_is_fresh(Some(5), 3));
        // A gap is allowed at mint — it is detectable later, not a forgery now.
        assert!(nonce_is_fresh(Some(5), 9));

        assert_eq!(next_nonce(None), 0);
        assert_eq!(next_nonce(Some(7)), 8);
    }

    #[test]
    fn conditions_gate_the_claim() {
        let conds = [Condition::DeliveryReceipt, Condition::NotAfter(1_000)];
        // Delivered and within the horizon: claimable.
        assert!(conditions_met(&conds, 999, true));
        // Delivered but past the horizon: not claimable.
        assert!(!conditions_met(&conds, 1_001, true));
        // Within the horizon but not delivered: not claimable.
        assert!(!conditions_met(&conds, 999, false));
        // No conditions is vacuously met (a bare IOU with no delivery gate).
        assert!(conditions_met(&[], 0, false));
    }
}
