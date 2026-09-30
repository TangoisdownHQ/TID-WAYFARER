//! Per-node Ed25519 request authentication for the outpost fabric.
//!
//! The fabric originally authenticated every node with one shared bearer
//! (`X-Node-Token` vs `NODE_SHARED_SECRET`), which meant any single
//! compromised outpost could impersonate every other node and push commands
//! fleet-wide. This module replaces that with per-node signatures verified
//! against the Ed25519 public key each node already proves possession of at
//! registration (see `routes::nodes::register_node`).
//!
//! A signing node sends:
//!   X-Node-Id:        <uuid of the sending node>
//!   X-Node-Timestamp: <unix seconds>
//!   X-Node-Signature: <base64 Ed25519 signature over the canonical string>
//!
//! The canonical string binds the method, path, timestamp and a hash of the
//! exact body, so a captured signature cannot be replayed against a different
//! route or a mutated payload. The timestamp bounds replay of the *same*
//! request to `MAX_SKEW_SECS`.
//!
//! Rollout is staged via `FABRIC_AUTH` (see [`FabricAuthMode`]) so a running
//! fabric can upgrade node-by-node instead of requiring a flag day.

use sha2::{Digest, Sha256};

use crate::services::identity::{sign_message, NodeIdentity};

pub const HEADER_NODE_ID: &str = "x-node-id";
pub const HEADER_TIMESTAMP: &str = "x-node-timestamp";
pub const HEADER_SIGNATURE: &str = "x-node-signature";

/// Replay window. Matches the registration skew allowance so operators only
/// have one clock-sync requirement to reason about.
pub const MAX_SKEW_SECS: i64 = 300;

/// Largest body we will buffer in order to verify a signature. Signed fabric
/// traffic is telemetry, DTN envelopes and commands — all small. A bigger
/// body is refused rather than buffered, so this can't be used to balloon
/// memory on an unauthenticated path.
pub const MAX_SIGNED_BODY_BYTES: usize = 2 * 1024 * 1024;

/// Which credentials the fabric guard will accept, from `FABRIC_AUTH`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FabricAuthMode {
    /// Ed25519 signatures only. The end state — `NODE_SHARED_SECRET` is dead.
    Signed,
    /// Accept either a signature or the legacy shared token. Default, so an
    /// existing deployment keeps working while nodes are upgraded one by one.
    Both,
    /// Legacy shared token only. Escape hatch for rolling back.
    Legacy,
}

impl FabricAuthMode {
    pub fn from_env() -> Self {
        match std::env::var("FABRIC_AUTH")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "signed" => Self::Signed,
            "legacy" => Self::Legacy,
            _ => Self::Both,
        }
    }

    pub fn allows_signature(self) -> bool {
        matches!(self, Self::Signed | Self::Both)
    }

    pub fn allows_shared_token(self) -> bool {
        matches!(self, Self::Legacy | Self::Both)
    }
}

/// The exact bytes both sides sign. Binding method+path+timestamp+body-hash
/// means a signature is valid for one request shape only: replaying it
/// against another route, or with a mutated body, fails verification.
pub fn canonical_request(method: &str, path: &str, ts: i64, body: &[u8]) -> String {
    let body_hash = hex::encode(Sha256::digest(body));
    format!(
        "fabric|{}|{}|{ts}|{body_hash}",
        method.to_ascii_uppercase(),
        path
    )
}

/// Client side: the three headers an outbound fabric request must carry.
/// Returned as owned pairs so callers can attach them to any HTTP client.
pub fn signed_headers(
    identity: &NodeIdentity,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<(&'static str, String)> {
    let ts = chrono::Utc::now().timestamp();
    let signature = sign_message(identity, canonical_request(method, path, ts, body).as_bytes());

    vec![
        (HEADER_NODE_ID, identity.node_id.clone()),
        (HEADER_TIMESTAMP, ts.to_string()),
        (HEADER_SIGNATURE, signature),
    ]
}

/// Path component of a URL, matching what the receiving server sees as
/// `parts.uri.path()`. An unparseable URL falls back to the raw string, which
/// simply fails verification rather than silently signing the wrong thing.
pub fn url_path(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string())
}

/// Build a signed JSON POST for one fabric peer.
///
/// The payload is serialized once and both signed and sent, so the signature
/// covers exactly the bytes on the wire. `legacy_token` is attached too when
/// non-empty: during a staged rollout the receiving peer may be an older
/// build that only understands `X-Node-Token`, and a receiver that does
/// understand signatures checks those first.
pub fn signed_post_json(
    http: &reqwest::Client,
    identity: &NodeIdentity,
    url: &str,
    payload: &serde_json::Value,
    legacy_token: &str,
) -> reqwest::RequestBuilder {
    let body = serde_json::to_vec(payload).unwrap_or_else(|_| b"{}".to_vec());
    let path = url_path(url);

    let mut request = http
        .post(url)
        .header("content-type", "application/json")
        .body(body.clone());

    for (name, value) in signed_headers(identity, "POST", &path, &body) {
        request = request.header(name, value);
    }
    if !legacy_token.is_empty() {
        request = request.header("X-Node-Token", legacy_token);
    }
    request
}

/// Build a signed GET for one fabric peer.
///
/// A GET has no body, so the canonical string hashes the empty slice — which is
/// what the receiving guard hashes too, since it verifies over whatever bytes
/// arrived. Otherwise identical to [`signed_post_json`], including the legacy
/// token for peers mid-rollout.
pub fn signed_get(
    http: &reqwest::Client,
    identity: &NodeIdentity,
    url: &str,
    legacy_token: &str,
) -> reqwest::RequestBuilder {
    let path = url_path(url);
    let mut request = http.get(url);

    for (name, value) in signed_headers(identity, "GET", &path, b"") {
        request = request.header(name, value);
    }
    if !legacy_token.is_empty() {
        request = request.header("X-Node-Token", legacy_token);
    }
    request
}

/// Why a signed request was refused. Kept separate from the HTTP layer so the
/// reason can be logged precisely while the caller still gets a flat 401.
#[derive(Debug, PartialEq, Eq)]
pub enum SigError {
    MissingHeaders,
    BadNodeId,
    BadTimestamp,
    StaleTimestamp(i64),
    UnknownNode,
    RevokedNode,
    BadSignature,
}

impl std::fmt::Display for SigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingHeaders => write!(f, "incomplete signature headers"),
            Self::BadNodeId => write!(f, "node id is not a uuid"),
            Self::BadTimestamp => write!(f, "timestamp is not an integer"),
            Self::StaleTimestamp(skew) => write!(f, "timestamp outside replay window ({skew}s)"),
            Self::UnknownNode => write!(f, "node not in registry"),
            Self::RevokedNode => write!(f, "node credential has been revoked"),
            Self::BadSignature => write!(f, "signature did not verify"),
        }
    }
}

/// Server side: check the replay window. Split out from the DB lookup so it
/// is unit-testable without a database.
pub fn check_skew(ts: i64, now: i64) -> Result<(), SigError> {
    let skew = (now - ts).abs();
    if skew > MAX_SKEW_SECS {
        return Err(SigError::StaleTimestamp(skew));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::identity::verify_signature;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use uuid::Uuid;

    fn identity() -> NodeIdentity {
        let sk = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(sk.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(sk.to_bytes()),
        }
    }

    /// Pull the signature out of what a client would actually send.
    fn sig_of(headers: &[(&str, String)]) -> String {
        headers
            .iter()
            .find(|(k, _)| *k == HEADER_SIGNATURE)
            .map(|(_, v)| v.clone())
            .unwrap()
    }

    #[test]
    fn signed_request_verifies_against_the_senders_key() {
        let id = identity();
        let body = br#"{"type":"LOCKDOWN"}"#;
        let headers = signed_headers(&id, "POST", "/api/commands/execute", body);

        let ts: i64 = headers
            .iter()
            .find(|(k, _)| *k == HEADER_TIMESTAMP)
            .unwrap()
            .1
            .parse()
            .unwrap();

        let msg = canonical_request("POST", "/api/commands/execute", ts, body);
        assert!(verify_signature(&id.public_key, msg.as_bytes(), &sig_of(&headers)));
    }

    #[test]
    fn signature_is_bound_to_the_path() {
        // A signature captured on a harmless route must not unlock a
        // dangerous one — this is the whole point of binding the path.
        let id = identity();
        let body = b"{}";
        let headers = signed_headers(&id, "POST", "/api/dtn/receive", body);
        let ts: i64 = headers[1].1.parse().unwrap();

        let replayed = canonical_request("POST", "/api/commands/execute", ts, body);
        assert!(!verify_signature(&id.public_key, replayed.as_bytes(), &sig_of(&headers)));
    }

    #[test]
    fn signature_is_bound_to_the_body() {
        let id = identity();
        let headers = signed_headers(&id, "POST", "/api/fleet/telemetry", br#"{"tamper":false}"#);
        let ts: i64 = headers[1].1.parse().unwrap();

        let tampered = canonical_request("POST", "/api/fleet/telemetry", ts, br#"{"tamper":true}"#);
        assert!(!verify_signature(&id.public_key, tampered.as_bytes(), &sig_of(&headers)));
    }

    #[test]
    fn signature_is_bound_to_the_method() {
        let id = identity();
        let headers = signed_headers(&id, "GET", "/api/nodes/list", b"");
        let ts: i64 = headers[1].1.parse().unwrap();

        let swapped = canonical_request("DELETE", "/api/nodes/list", ts, b"");
        assert!(!verify_signature(&id.public_key, swapped.as_bytes(), &sig_of(&headers)));
    }

    #[test]
    fn another_nodes_key_does_not_verify() {
        let id = identity();
        let attacker = identity();
        let headers = signed_headers(&id, "POST", "/api/commands/execute", b"{}");
        let ts: i64 = headers[1].1.parse().unwrap();

        let msg = canonical_request("POST", "/api/commands/execute", ts, b"{}");
        assert!(!verify_signature(&attacker.public_key, msg.as_bytes(), &sig_of(&headers)));
    }

    #[test]
    fn replay_window_bounds_reuse() {
        assert!(check_skew(1_000, 1_000).is_ok());
        assert!(check_skew(1_000, 1_000 + MAX_SKEW_SECS).is_ok());
        // Clock skew in either direction is treated the same.
        assert!(check_skew(1_000, 1_000 - MAX_SKEW_SECS).is_ok());
        assert_eq!(
            check_skew(1_000, 1_000 + MAX_SKEW_SECS + 1),
            Err(SigError::StaleTimestamp(MAX_SKEW_SECS + 1))
        );
    }

    #[test]
    fn mode_parsing_defaults_to_dual_mode() {
        // Unset/garbage must not silently disable either credential — that
        // would either break a live fabric or drop the new check.
        assert_eq!(FabricAuthMode::from_env(), FabricAuthMode::Both);
        assert!(FabricAuthMode::Both.allows_signature());
        assert!(FabricAuthMode::Both.allows_shared_token());
        assert!(FabricAuthMode::Signed.allows_signature());
        assert!(!FabricAuthMode::Signed.allows_shared_token());
        assert!(!FabricAuthMode::Legacy.allows_signature());
        assert!(FabricAuthMode::Legacy.allows_shared_token());
    }
}
