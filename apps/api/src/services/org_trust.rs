//! Federated organisation trust: node certificates and trust grants.
//!
//! Pure functions, no database. That is the point — an outpost dark for a
//! month must be able to decide whether to trust a peer with nothing but the
//! certificate the peer presented and the root key it already holds. A trust
//! model that needs a lookup to resolve is a trust model that stops working
//! exactly when it is most needed.
//!
//! Two signed objects:
//!
//!   **Node certificate** — the org root attests "this node, with this key,
//!   is mine, until this date". Replaces "is the node in my registry", which
//!   could only ever recognise peers already met.
//!
//!   **Trust grant** — one org's root attests "I trust the org with this root
//!   key, for these scopes, until this date". Bilateral and revocable by
//!   either side alone; nobody else's agreement is required or sought.
//!
//! Both carry their own domain prefix so a signature over one can never be
//! replayed as the other, and both length-prefix every field so no two
//! different objects can produce identical bytes.

use chrono::{DateTime, Utc};

use crate::services::identity::verify_signature;

const CERT_DOMAIN: &str = "orgcert|v1";
const GRANT_DOMAIN: &str = "orggrant|v1";

/// What one org may be trusted to do with another's data.
///
/// `commands`, `telemetry` and `rules` are deliberately absent and
/// unrepresentable. One org actuating another's outpost — LOCKDOWN,
/// ISOLATE_NETWORK, an autonomy rule that fires a command — is a safety
/// boundary, not a commercial one. Making it an invalid scope rather than a
/// scope that defaults to off means no configuration mistake can grant it.
pub const GRANTABLE_SCOPES: [&str; 4] = ["marketplace", "custody", "settlement", "documents"];

/// Scopes that exist in the system but can never cross an org boundary.
/// Listed so an attempt to grant one can be refused with a reason rather than
/// silently ignored.
pub const NEVER_GRANTABLE: [&str; 5] = ["inventory", "rollup", "commands", "telemetry", "rules"];

pub fn is_grantable(scope: &str) -> bool {
    GRANTABLE_SCOPES.contains(&scope)
}

fn field(out: &mut String, v: &str) {
    out.push_str(&v.len().to_string());
    out.push(':');
    out.push_str(v);
    out.push('|');
}

fn ts(t: Option<DateTime<Utc>>) -> String {
    t.map(|x| x.timestamp().to_string()).unwrap_or_default()
}

/// The bytes an org root signs to certify one of its outposts.
pub fn canonical_certificate(
    node_id: &str,
    node_public_key: &str,
    org_id: &str,
    org_root_key: &str,
    not_after: Option<DateTime<Utc>>,
) -> String {
    let mut s = String::with_capacity(256);
    s.push_str(CERT_DOMAIN);
    s.push('|');
    field(&mut s, node_id);
    field(&mut s, node_public_key);
    field(&mut s, org_id);
    // The root key is inside the signed form as well as being the verifying
    // key. Without it, a certificate issued under one root could be presented
    // as though issued under another that happened to verify it.
    field(&mut s, org_root_key);
    field(&mut s, &ts(not_after));
    s
}

/// The bytes an org root signs to grant trust to another org.
pub fn canonical_grant(
    grantor_org_id: &str,
    grantor_root_key: &str,
    counterparty_root_key: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> String {
    let mut s = String::with_capacity(256);
    s.push_str(GRANT_DOMAIN);
    s.push('|');
    field(&mut s, grantor_org_id);
    field(&mut s, grantor_root_key);
    field(&mut s, counterparty_root_key);
    // Sorted so the same set of scopes always produces the same bytes
    // regardless of the order they were supplied in.
    let mut sorted: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    field(&mut s, &sorted.join(","));
    field(&mut s, &ts(expires_at));
    s
}

/// Why a certificate was not accepted. Separate from the HTTP layer so the
/// reason can be logged precisely while the caller gets a flat refusal.
#[derive(Debug, PartialEq, Eq)]
pub enum CertError {
    BadSignature,
    Expired,
    Revoked,
    UntrustedRoot,
}

impl std::fmt::Display for CertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSignature => write!(f, "certificate signature does not verify"),
            Self::Expired => write!(f, "certificate has expired"),
            Self::Revoked => write!(f, "certificate has been revoked"),
            Self::UntrustedRoot => write!(f, "issuing organisation is not trusted here"),
        }
    }
}

/// Verify a node certificate against the issuing org's root key.
///
/// `now` is a parameter so expiry is testable and so an outpost with a skewed
/// clock fails in a way its operator can reproduce.
#[allow(clippy::too_many_arguments)]
pub fn verify_certificate(
    node_id: &str,
    node_public_key: &str,
    org_id: &str,
    org_root_key: &str,
    not_after: Option<DateTime<Utc>>,
    revoked: bool,
    certificate: &str,
    now: DateTime<Utc>,
) -> Result<(), CertError> {
    if revoked {
        return Err(CertError::Revoked);
    }
    if let Some(exp) = not_after {
        if exp <= now {
            return Err(CertError::Expired);
        }
    }
    let msg = canonical_certificate(node_id, node_public_key, org_id, org_root_key, not_after);
    if verify_signature(org_root_key, msg.as_bytes(), certificate) {
        Ok(())
    } else {
        Err(CertError::BadSignature)
    }
}

/// Verify a trust grant against the granting org's root key.
pub fn verify_grant(
    grantor_org_id: &str,
    grantor_root_key: &str,
    counterparty_root_key: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
    signature: &str,
) -> bool {
    let msg = canonical_grant(
        grantor_org_id,
        grantor_root_key,
        counterparty_root_key,
        scopes,
        expires_at,
    );
    verify_signature(grantor_root_key, msg.as_bytes(), signature)
}

/// Whether a live grant covers a scope.
///
/// Four separate reasons to say no, all collapsed to the same answer on
/// purpose: a caller learning *which* of expiry, revocation or scope failed
/// learns something about an org it has no relationship with.
pub fn grant_permits(
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    wanted: &str,
    now: DateTime<Utc>,
) -> bool {
    if revoked_at.is_some() {
        return false;
    }
    if let Some(exp) = expires_at {
        if exp <= now {
            return false;
        }
    }
    // A scope that can never cross is refused even if somehow recorded, so a
    // bad row in the database cannot become an authorisation.
    if !is_grantable(wanted) {
        return false;
    }
    scopes.iter().any(|s| s == wanted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::identity::{sign_message, NodeIdentity};
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use chrono::Duration;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use uuid::Uuid;

    fn root() -> NodeIdentity {
        let k = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(k.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(k.to_bytes()),
        }
    }
    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn issue(org: &NodeIdentity, org_id: &str, node: &str, node_key: &str,
             exp: Option<DateTime<Utc>>) -> String {
        let msg = canonical_certificate(node, node_key, org_id, &org.public_key, exp);
        sign_message(org, msg.as_bytes())
    }

    #[test]
    fn a_node_certificate_verifies_against_its_own_root() {
        let org = root();
        let org_id = Uuid::new_v4().to_string();
        let exp = Some(now() + Duration::days(30));
        let cert = issue(&org, &org_id, "node-1", "nodekey", exp);

        assert!(verify_certificate("node-1", "nodekey", &org_id, &org.public_key,
                                   exp, false, &cert, now()).is_ok());
    }

    #[test]
    fn another_orgs_root_cannot_vouch_for_this_node() {
        // The whole point of federation: Org B saying "that outpost is mine"
        // means nothing to someone who only trusts Org A.
        let a = root();
        let b = root();
        let org_id = Uuid::new_v4().to_string();
        let cert = issue(&b, &org_id, "node-1", "nodekey", None);

        assert_eq!(
            verify_certificate("node-1", "nodekey", &org_id, &a.public_key, None, false, &cert, now()),
            Err(CertError::BadSignature)
        );
    }

    #[test]
    fn a_certificate_cannot_be_moved_to_a_different_node_or_key() {
        let org = root();
        let org_id = Uuid::new_v4().to_string();
        let cert = issue(&org, &org_id, "node-1", "nodekey", None);

        // Same certificate, different node id.
        assert!(verify_certificate("node-2", "nodekey", &org_id, &org.public_key, None, false, &cert, now()).is_err());
        // Same node, swapped key — the shape of presenting someone else's
        // certificate with your own keypair.
        assert!(verify_certificate("node-1", "otherkey", &org_id, &org.public_key, None, false, &cert, now()).is_err());
    }

    #[test]
    fn expiry_and_revocation_are_checked_before_the_signature() {
        let org = root();
        let org_id = Uuid::new_v4().to_string();
        let past = Some(now() - Duration::days(1));
        let cert = issue(&org, &org_id, "node-1", "nodekey", past);

        assert_eq!(
            verify_certificate("node-1", "nodekey", &org_id, &org.public_key, past, false, &cert, now()),
            Err(CertError::Expired)
        );
        let live = Some(now() + Duration::days(1));
        let cert2 = issue(&org, &org_id, "node-1", "nodekey", live);
        assert_eq!(
            verify_certificate("node-1", "nodekey", &org_id, &org.public_key, live, true, &cert2, now()),
            Err(CertError::Revoked)
        );
    }

    #[test]
    fn a_trust_grant_verifies_and_is_order_independent() {
        let a = root();
        let b = root();
        let org_id = Uuid::new_v4().to_string();
        let scopes = vec!["custody".to_string(), "marketplace".to_string()];
        let msg = canonical_grant(&org_id, &a.public_key, &b.public_key, &scopes, None);
        let sig = sign_message(&a, msg.as_bytes());

        assert!(verify_grant(&org_id, &a.public_key, &b.public_key, &scopes, None, &sig));
        // Supplying the same scopes in a different order must not invalidate
        // it — otherwise a client's map iteration order breaks trust.
        let reordered = vec!["marketplace".to_string(), "custody".to_string()];
        assert!(verify_grant(&org_id, &a.public_key, &b.public_key, &reordered, None, &sig));
    }

    #[test]
    fn a_grant_cannot_be_widened_after_signing() {
        let a = root();
        let b = root();
        let org_id = Uuid::new_v4().to_string();
        let granted = vec!["custody".to_string()];
        let sig = sign_message(&a, canonical_grant(&org_id, &a.public_key, &b.public_key, &granted, None).as_bytes());

        let widened = vec!["custody".to_string(), "settlement".to_string()];
        assert!(!verify_grant(&org_id, &a.public_key, &b.public_key, &widened, None, &sig));
    }

    #[test]
    fn a_grant_cannot_be_redirected_to_a_third_org() {
        let a = root();
        let b = root();
        let c = root();
        let org_id = Uuid::new_v4().to_string();
        let scopes = vec!["marketplace".to_string()];
        let sig = sign_message(&a, canonical_grant(&org_id, &a.public_key, &b.public_key, &scopes, None).as_bytes());

        assert!(!verify_grant(&org_id, &a.public_key, &c.public_key, &scopes, None, &sig));
    }

    #[test]
    fn actuation_can_never_be_granted() {
        // Not "off by default" — unrepresentable. A row in the database
        // claiming otherwise must still not authorise anything.
        for scope in NEVER_GRANTABLE {
            assert!(!is_grantable(scope), "{scope} must not be grantable");
            let recorded = vec![scope.to_string()];
            assert!(
                !grant_permits(&recorded, None, None, scope, now()),
                "{scope} was granted despite being unrepresentable"
            );
        }
        // And the ones that are meant to work, do.
        for scope in GRANTABLE_SCOPES {
            assert!(grant_permits(&[scope.to_string()], None, None, scope, now()));
        }
    }

    #[test]
    fn expiry_and_revocation_withdraw_a_grant() {
        let s = vec!["marketplace".to_string()];
        assert!(grant_permits(&s, Some(now() + Duration::days(1)), None, "marketplace", now()));
        assert!(!grant_permits(&s, Some(now() - Duration::seconds(1)), None, "marketplace", now()));
        assert!(!grant_permits(&s, None, Some(now()), "marketplace", now()));
    }

    #[test]
    fn a_scope_not_granted_is_denied() {
        let s = vec!["marketplace".to_string()];
        assert!(!grant_permits(&s, None, None, "settlement", now()));
        assert!(!grant_permits(&[], None, None, "marketplace", now()));
    }
}
