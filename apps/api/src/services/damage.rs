//! Signed damage reports — tamper-evident evidence that a consignment arrived
//! in a stated condition, used to suspend settlement on contested goods.
//!
//! Pure: canonical encoding, signing, verification. No database, so a report
//! can be minted and checked on an outpost that has been dark for a month, and
//! it travels over DTN exactly like a custody receipt.
//!
//! What a signature proves: *who* filed the report, *when*, and that the
//! referenced evidence file (bound by its hash) has not changed since. What it
//! does NOT prove: that the photo is of this shipment, or that the damage is
//! real. That is not cryptographically knowable — "authored and tamper-evident"
//! is the honest claim, not "proof of damage".

use serde::Serialize;
use uuid::Uuid;

use crate::services::identity::{sign_message, verify_signature, NodeIdentity};

/// Domain prefix. Distinct from `custody|v1`, `ctv|v1`, `fabric|` and the DTN
/// domains, so a damage signature can never be reinterpreted as another shape.
const DOMAIN: &str = "damage|v1";

/// The recognised conditions and severities. Kept here so the route validates
/// against the same list the signature commits to.
pub const CONDITIONS: [&str; 4] = ["damaged", "short", "contaminated", "spoiled"];
pub const SEVERITIES: [&str; 3] = ["minor", "major", "total"];

/// The fields a damage-report signature commits to. Borrowed like
/// `custody::ReceiptClaim`: the row owns the strings, this names what is signed.
/// `status`/`resolution` are deliberately absent — they change after filing and
/// are not part of the original attestation.
#[derive(Debug, Clone, Serialize)]
pub struct DamageClaim<'a> {
    pub id: Uuid,
    pub order_id: Uuid,
    pub fulfillment_id: Uuid,
    pub lot_id: Option<Uuid>,
    pub reporter_node_id: Uuid,
    pub reporter_user_id: Uuid,
    pub condition: &'a str,
    pub severity: &'a str,
    /// NUMERIC rendered to the exact string both sides agree on.
    pub quantity_affected: Option<&'a str>,
    pub description: &'a str,
    /// sha256 (or similar) of the evidence file/bundle, computed by the client.
    pub evidence_hash: &'a str,
    /// RFC3339 — the exact string both sides must agree on.
    pub occurred_at: &'a str,
}

/// Length-prefix a field so boundaries are unambiguous — same encoding as
/// `custody::canonical_receipt`, deliberately, so there is one convention.
fn field(out: &mut String, value: &str) {
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
    out.push('|');
}

/// The exact bytes a reporter signs for one damage report.
pub fn canonical_damage(c: &DamageClaim<'_>) -> String {
    let mut s = String::with_capacity(320);
    s.push_str(DOMAIN);
    s.push('|');
    field(&mut s, &c.id.to_string());
    field(&mut s, &c.order_id.to_string());
    field(&mut s, &c.fulfillment_id.to_string());
    field(&mut s, &c.lot_id.map(|v| v.to_string()).unwrap_or_default());
    field(&mut s, &c.reporter_node_id.to_string());
    field(&mut s, &c.reporter_user_id.to_string());
    field(&mut s, c.condition);
    field(&mut s, c.severity);
    field(&mut s, c.quantity_affected.unwrap_or(""));
    field(&mut s, c.description);
    field(&mut s, c.evidence_hash);
    field(&mut s, c.occurred_at);
    s
}

/// Sign a damage report with the reporting outpost's identity key.
pub fn sign_damage(reporter: &NodeIdentity, c: &DamageClaim<'_>) -> String {
    sign_message(reporter, canonical_damage(c).as_bytes())
}

/// Verify a damage report against the reporting node's registered public key.
pub fn verify_damage(c: &DamageClaim<'_>, signature: &str, reporter_public_key: &str) -> bool {
    verify_signature(reporter_public_key, canonical_damage(c).as_bytes(), signature)
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

    fn reporter() -> NodeIdentity {
        let k = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(k.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(k.to_bytes()),
        }
    }

    fn claim<'a>() -> DamageClaim<'a> {
        DamageClaim {
            id: uid(1),
            order_id: uid(2),
            fulfillment_id: uid(3),
            lot_id: Some(uid(4)),
            reporter_node_id: uid(5),
            reporter_user_id: uid(6),
            condition: "damaged",
            severity: "major",
            quantity_affected: Some("12"),
            description: "crushed carton, 12 units unusable",
            evidence_hash: "sha256:abc123",
            occurred_at: "2026-11-15T00:00:00Z",
        }
    }

    #[test]
    fn a_report_verifies_against_the_reporters_key() {
        let r = reporter();
        let c = claim();
        let sig = sign_damage(&r, &c);
        assert!(verify_damage(&c, &sig, &r.public_key));
    }

    #[test]
    fn changing_any_signed_field_invalidates_the_report() {
        let r = reporter();
        let c = claim();
        let sig = sign_damage(&r, &c);

        // Condition is what a party would most want to soften after the fact.
        let mut softened = c.clone();
        softened.condition = "short";
        assert!(!verify_damage(&softened, &sig, &r.public_key));

        let mut inflated = c.clone();
        inflated.severity = "total";
        assert!(!verify_damage(&inflated, &sig, &r.public_key));

        let mut reqty = c.clone();
        reqty.quantity_affected = Some("1");
        assert!(!verify_damage(&reqty, &sig, &r.public_key));

        // Swapping the evidence after signing is the attack this defends against.
        let mut swapped = c.clone();
        swapped.evidence_hash = "sha256:different";
        assert!(!verify_damage(&swapped, &sig, &r.public_key));
    }

    #[test]
    fn another_nodes_key_does_not_verify() {
        let r = reporter();
        let impostor = reporter();
        let c = claim();
        let sig = sign_damage(&impostor, &c);
        assert!(!verify_damage(&c, &sig, &r.public_key));
    }

    #[test]
    fn length_prefixing_stops_field_boundaries_from_sliding() {
        let mut a = claim();
        a.condition = "damaged";
        a.severity = "major";
        let mut b = claim();
        b.condition = "damagedm";
        b.severity = "ajor";
        assert_ne!(canonical_damage(&a), canonical_damage(&b));
    }
}
