//! Canonical encoding and verification for custody receipts.
//!
//! Pure functions, no database — so the chain can be checked on an outpost
//! that has been dark for a month and cannot ask anyone whether a signature
//! was good. Everything needed to verify a receipt travels inside it.
//!
//! The signer is the **receiving** party. The receiver is the one making a
//! claim about the world — "I have it now, and it arrived in this condition" —
//! so the receiver is who must attest. A sender-signed receipt proves only
//! that the sender says they sent it, which is exactly what is in dispute.

use serde::Serialize;
use uuid::Uuid;

use crate::services::identity::verify_signature;

/// Domain prefix. Distinct from `fabric|` (request signing) and `dtn|` /
/// `dtn-sealed|` (envelopes) so a signature over one shape can never be
/// reinterpreted as another — the same discipline used everywhere else in the
/// fabric.
const DOMAIN: &str = "custody|v1";

/// The fields a signature commits to.
///
/// Order is fixed and every field is length-prefixed, so no combination of
/// values can be rearranged into a different receipt with the same bytes.
/// Concatenating unprefixed strings would let ("ab","c") and ("a","bc")
/// collide, and a custody chain is precisely where someone would try.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptClaim<'a> {
    pub order_id: Option<Uuid>,
    pub fulfillment_id: Option<Uuid>,
    pub seq: i32,
    pub from_node_id: Option<Uuid>,
    pub to_node_id: Uuid,
    pub event: &'a str,
    pub item_hash: Option<&'a str>,
    pub quantity: Option<&'a str>,
    pub unit: Option<&'a str>,
    pub condition: &'a str,
    /// RFC3339, seconds precision — the same string both sides must agree on.
    pub occurred_at: &'a str,
}

/// Length-prefix a field so boundaries are unambiguous.
fn field(out: &mut String, value: &str) {
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
    out.push('|');
}

fn opt(v: Option<&str>) -> &str {
    v.unwrap_or("")
}

/// The exact bytes a receiving node signs.
pub fn canonical_receipt(c: &ReceiptClaim<'_>) -> String {
    let mut s = String::with_capacity(256);
    s.push_str(DOMAIN);
    s.push('|');
    field(&mut s, &c.order_id.map(|v| v.to_string()).unwrap_or_default());
    field(&mut s, &c.fulfillment_id.map(|v| v.to_string()).unwrap_or_default());
    field(&mut s, &c.seq.to_string());
    field(&mut s, &c.from_node_id.map(|v| v.to_string()).unwrap_or_default());
    field(&mut s, &c.to_node_id.to_string());
    field(&mut s, c.event);
    field(&mut s, opt(c.item_hash));
    field(&mut s, opt(c.quantity));
    field(&mut s, opt(c.unit));
    field(&mut s, c.condition);
    field(&mut s, c.occurred_at);
    s
}

/// Verify a receipt against the receiving node's registered public key.
pub fn verify_receipt(claim: &ReceiptClaim<'_>, signature: &str, receiver_public_key: &str) -> bool {
    verify_signature(receiver_public_key, canonical_receipt(claim).as_bytes(), signature)
}

/// A break in a custody chain.
#[derive(Debug, PartialEq, Eq)]
pub enum ChainFault {
    /// The chain does not start at 0 — the first handoff is missing.
    MissingStart(i32),
    /// A leg is absent between two recorded ones.
    Gap { after: i32, before: i32 },
    /// Two receipts claim the same position.
    Duplicate(i32),
    /// Custody left a party that never received it.
    Discontinuous { seq: i32, expected_from: Uuid, actual_from: Option<Uuid> },
}

/// One leg, reduced to what continuity checking needs.
pub struct Leg {
    pub seq: i32,
    pub from_node_id: Option<Uuid>,
    pub to_node_id: Uuid,
}

/// Check that a chain is whole and continuous.
///
/// Two separate properties, both of which matter in a dispute:
///   - *complete*: no gaps and no duplicates, so nothing happened off-record
///   - *continuous*: each leg departs from whoever last held the goods, so
///     custody never teleports between parties
///
/// `legs` need not be sorted.
pub fn check_chain(legs: &mut [Leg]) -> Vec<ChainFault> {
    let mut faults = Vec::new();
    if legs.is_empty() {
        return faults;
    }
    legs.sort_by_key(|l| l.seq);

    if legs[0].seq != 0 {
        faults.push(ChainFault::MissingStart(legs[0].seq));
    }

    for pair in legs.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        if b.seq == a.seq {
            faults.push(ChainFault::Duplicate(b.seq));
            continue;
        }
        if b.seq > a.seq + 1 {
            faults.push(ChainFault::Gap { after: a.seq, before: b.seq });
        }
        // Whoever received leg N must be the one handing over at leg N+1.
        if b.from_node_id != Some(a.to_node_id) {
            faults.push(ChainFault::Discontinuous {
                seq: b.seq,
                expected_from: a.to_node_id,
                actual_from: b.from_node_id,
            });
        }
    }
    faults
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::identity::{sign_message, NodeIdentity};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;

    fn uid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn test_identity() -> NodeIdentity {
        let k = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(k.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(k.to_bytes()),
        }
    }

    fn claim<'a>(seq: i32, from: Option<Uuid>, to: Uuid) -> ReceiptClaim<'a> {
        ReceiptClaim {
            order_id: Some(uid(1)),
            fulfillment_id: None,
            seq,
            from_node_id: from,
            to_node_id: to,
            event: "transfer",
            item_hash: Some("sha256:abc"),
            quantity: Some("40"),
            unit: Some("kg"),
            condition: "ok",
            occurred_at: "2026-09-30T12:00:00Z",
        }
    }

    fn leg(seq: i32, from: Option<u128>, to: u128) -> Leg {
        Leg { seq, from_node_id: from.map(uid), to_node_id: uid(to) }
    }

    #[test]
    fn a_receipt_verifies_against_the_receivers_key() {
        let id = test_identity();
        let c = claim(0, None, uid(7));
        let sig = sign_message(&id, canonical_receipt(&c).as_bytes());
        assert!(verify_receipt(&c, &sig, &id.public_key));
    }

    #[test]
    fn changing_any_signed_field_invalidates_the_receipt() {
        let id = test_identity();
        let c = claim(0, None, uid(7));
        let sig = sign_message(&id, canonical_receipt(&c).as_bytes());

        // Condition is the field someone would most want to alter after the
        // fact: signing for damaged goods and later claiming they were fine.
        let mut tampered = c.clone();
        tampered.condition = "damaged";
        assert!(!verify_receipt(&tampered, &sig, &id.public_key));

        let mut moved = c.clone();
        moved.seq = 1;
        assert!(!verify_receipt(&moved, &sig, &id.public_key));

        let mut requantified = c.clone();
        requantified.quantity = Some("4");
        assert!(!verify_receipt(&requantified, &sig, &id.public_key));
    }

    #[test]
    fn another_nodes_key_does_not_verify() {
        let receiver = test_identity();
        let impostor = test_identity();
        let c = claim(0, None, uid(7));
        let sig = sign_message(&impostor, canonical_receipt(&c).as_bytes());
        assert!(!verify_receipt(&c, &sig, &receiver.public_key));
    }

    #[test]
    fn length_prefixing_stops_field_boundaries_from_sliding() {
        // Without length prefixes, ("ab","c") and ("a","bc") would produce the
        // same bytes and one receipt's signature would validate the other.
        let mut a = claim(0, None, uid(7));
        a.unit = Some("kg");
        a.item_hash = Some("XY");
        let mut b = claim(0, None, uid(7));
        b.unit = Some("k");
        b.item_hash = Some("gXY");
        assert_ne!(canonical_receipt(&a), canonical_receipt(&b));
    }

    #[test]
    fn a_whole_continuous_chain_has_no_faults() {
        let mut legs = vec![leg(0, None, 1), leg(1, Some(1), 2), leg(2, Some(2), 3)];
        assert!(check_chain(&mut legs).is_empty());
    }

    #[test]
    fn a_missing_leg_is_detected() {
        let mut legs = vec![leg(0, None, 1), leg(2, Some(2), 3)];
        let faults = check_chain(&mut legs);
        assert!(faults.contains(&ChainFault::Gap { after: 0, before: 2 }));
    }

    #[test]
    fn custody_cannot_teleport_between_parties() {
        // Leg 1 departs from node 9, but node 2 is who actually received leg 0.
        // This is the shape of a substituted carrier, and it is invisible to a
        // gap check alone.
        let mut legs = vec![leg(0, None, 2), leg(1, Some(9), 3)];
        let faults = check_chain(&mut legs);
        assert!(faults.iter().any(|f| matches!(
            f,
            ChainFault::Discontinuous { seq: 1, .. }
        )));
    }

    #[test]
    fn a_chain_that_does_not_start_at_zero_is_incomplete() {
        let mut legs = vec![leg(1, Some(1), 2)];
        assert!(check_chain(&mut legs).contains(&ChainFault::MissingStart(1)));
    }

    #[test]
    fn two_receipts_for_one_position_is_a_fault() {
        let mut legs = vec![leg(0, None, 1), leg(1, Some(1), 2), leg(1, Some(1), 5)];
        assert!(check_chain(&mut legs).contains(&ChainFault::Duplicate(1)));
    }

    #[test]
    fn an_empty_chain_is_not_a_fault() {
        // Nothing has shipped yet. That is a state, not a broken chain.
        assert!(check_chain(&mut []).is_empty());
    }
}
