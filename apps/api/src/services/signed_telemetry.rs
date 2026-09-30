use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Verify an HMAC-SHA256 hex digest over the raw body using the per-node
/// secret. The digest is decoded to bytes and checked with `verify_slice`,
/// which compares in constant time — a hex string compare would leak the
/// correct prefix length through timing. Any malformed input (bad hex, bad
/// key) fails closed rather than panicking: this runs on an unauthenticated
/// request path.
pub fn verify_hmac(body: &[u8], provided_hex: &str, secret: &str) -> bool {
    type H = Hmac<Sha256>;

    let Ok(provided) = hex::decode(provided_hex.trim()) else {
        return false;
    };
    let Ok(mut mac) = H::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&provided).is_ok()
}

#[cfg(test)]
mod tests {
    use super::verify_hmac;

    const SECRET: &str = "rotated_secret";
    const BODY: &[u8] = br#"{"asset_id":"rover-7","battery":41.5}"#;

    /// Reference digest for BODY under SECRET, produced by the same primitive.
    fn digest() -> String {
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
        mac.update(BODY);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn accepts_a_correct_digest_in_either_case() {
        let d = digest();
        assert!(verify_hmac(BODY, &d, SECRET));
        assert!(verify_hmac(BODY, &d.to_uppercase(), SECRET));
    }

    #[test]
    fn rejects_wrong_secret_body_and_digest() {
        assert!(!verify_hmac(BODY, &digest(), "other_secret"));
        assert!(!verify_hmac(b"tampered", &digest(), SECRET));
        assert!(!verify_hmac(BODY, &"0".repeat(64), SECRET));
    }

    #[test]
    fn fails_closed_on_malformed_input() {
        assert!(!verify_hmac(BODY, "not-hex", SECRET));
        assert!(!verify_hmac(BODY, "", SECRET));
        // Right bytes, truncated — verify_slice must reject a short digest.
        let d = digest();
        assert!(!verify_hmac(BODY, &d[..32], SECRET));
    }
}

