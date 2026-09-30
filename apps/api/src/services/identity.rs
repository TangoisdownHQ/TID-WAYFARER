use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NodeIdentity {
    pub node_id: String,
    pub public_key: String,
    pub secret_key: String,
}

const IDENTITY_FILE: &str = "keys/identity.json";

/// Load existing identity or generate a new one
pub fn load_or_generate_identity() -> NodeIdentity {
    let path = Path::new(IDENTITY_FILE);

    if path.exists() {
        let data = fs::read_to_string(path).expect("❌ Failed to read identity file");
        serde_json::from_str(&data).expect("❌ Failed to parse identity file")
    } else {
        let signing_key = SigningKey::generate(&mut OsRng);

        let node_id = Uuid::new_v4().to_string();
        let identity = NodeIdentity {
            node_id,
            public_key: STANDARD.encode(signing_key.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(signing_key.to_bytes()),
        };

        fs::create_dir_all("keys").ok();
        fs::write(path, serde_json::to_string_pretty(&identity).unwrap())
            .expect("❌ Failed to write identity file");

        tracing::info!("🆕 Generated new node identity for this Outpost!");
        identity
    }
}

/// Sign any message using the node's private key
pub fn sign_message(identity: &NodeIdentity, message: &[u8]) -> String {
    let secret_bytes = STANDARD
        .decode(&identity.secret_key)
        .expect("❌ Invalid secret key encoding");
    let secret: [u8; 32] = secret_bytes
        .try_into()
        .expect("❌ Secret key must be 32 bytes");
    let signing_key = SigningKey::from_bytes(&secret);

    let signature: Signature = signing_key.sign(message);
    STANDARD.encode(signature.to_bytes())
}

/// Verify a base64 signature over a message against a base64 Ed25519 public
/// key. Any decode failure means "not verified" — callers treat this as an
/// authentication decision, never an error path.
pub fn verify_signature(public_key_b64: &str, message: &[u8], signature_b64: &str) -> bool {
    let Ok(pk_bytes) = STANDARD.decode(public_key_b64) else {
        return false;
    };
    let Ok(pk_arr) = <[u8; 32]>::try_from(pk_bytes.as_slice()) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(&pk_arr) else {
        return false;
    };

    let Ok(sig_bytes) = STANDARD.decode(signature_b64) else {
        return false;
    };
    let Ok(sig_arr) = <[u8; 64]>::try_from(sig_bytes.as_slice()) else {
        return false;
    };
    let signature = Signature::from_bytes(&sig_arr);

    verifying_key.verify(message, &signature).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> NodeIdentity {
        let signing_key = SigningKey::generate(&mut OsRng);
        NodeIdentity {
            node_id: Uuid::new_v4().to_string(),
            public_key: STANDARD.encode(signing_key.verifying_key().to_bytes()),
            secret_key: STANDARD.encode(signing_key.to_bytes()),
        }
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let id = test_identity();
        let sig = sign_message(&id, b"hello mars");
        assert!(verify_signature(&id.public_key, b"hello mars", &sig));
    }

    #[test]
    fn rejects_tampered_message() {
        let id = test_identity();
        let sig = sign_message(&id, b"hello mars");
        assert!(!verify_signature(&id.public_key, b"hello venus", &sig));
    }

    #[test]
    fn rejects_wrong_key() {
        let id = test_identity();
        let other = test_identity();
        let sig = sign_message(&id, b"hello mars");
        assert!(!verify_signature(&other.public_key, b"hello mars", &sig));
    }

    #[test]
    fn rejects_garbage_inputs() {
        let id = test_identity();
        assert!(!verify_signature("not-base64!!", b"m", "also-not-base64!!"));
        assert!(!verify_signature(&id.public_key, b"m", "AAAA"));
    }
}
