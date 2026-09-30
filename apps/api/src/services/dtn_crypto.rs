//! Post-quantum sealing for DTN payloads.
//!
//! DTN messages sit in `dtn_outbox` for as long as the destination is out of
//! contact — potentially days across a comms blackout — and then cross links
//! nobody controls. They were signed but not encrypted, so the payload was
//! readable at rest in the database and on the wire. CommSec's ML-KEM and
//! AES-GCM existed only as standalone REST endpoints; this module is what
//! makes them load-bearing.
//!
//! Scheme (`mlkem1024+aes256gcm`):
//!   1. ML-KEM-1024 encapsulate to the recipient's public key → (kem_ct, ss).
//!      A store-and-forward message can't do an interactive handshake, so KEM
//!      encapsulation carries the key material with the message.
//!   2. HKDF-SHA256(ss, info) → 32-byte AES key. The KEM shared secret is not
//!      used directly as a cipher key.
//!   3. AES-256-GCM over the payload with a random 96-bit nonce, with the
//!      sender and recipient node ids as associated data — so a sealed
//!      payload can't be replayed as though it came from, or was addressed
//!      to, a different node.
//!
//! The envelope is signed separately with Ed25519 over the *ciphertext*, so a
//! recipient authenticates the sender before spending work on decryption.

use aes_gcm::{
    aead::{Aead, Payload},
    Aes256Gcm, KeyInit, Nonce,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use hkdf::Hkdf;
use pqcrypto_mlkem::mlkem1024::{
    decapsulate as pq_decapsulate, encapsulate as pq_encapsulate, Ciphertext, PublicKey, SecretKey,
};
use pqcrypto_traits::kem::{
    Ciphertext as CTTrait, PublicKey as PKTrait, SecretKey as SKTrait, SharedSecret as SSTrait,
};
use rand::RngCore;
use serde_json::Value;

/// Names the exact construction. A future scheme gets a new string rather
/// than silently changing what this one means.
pub const SCHEME: &str = "mlkem1024+aes256gcm";

/// HKDF domain separation — binds derived keys to this protocol and version.
const HKDF_INFO: &[u8] = b"tid-wayfarer/dtn/v1";

#[derive(Debug, PartialEq, Eq)]
pub enum SealError {
    BadRecipientKey,
    Encrypt,
}

#[derive(Debug, PartialEq, Eq)]
pub enum OpenError {
    BadKemCiphertext,
    /// Wrong recipient, tampered ciphertext, or mismatched associated data.
    Decrypt,
    BadPlaintext,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKemCiphertext => write!(f, "malformed KEM ciphertext"),
            Self::Decrypt => write!(f, "AEAD decryption failed"),
            Self::BadPlaintext => write!(f, "decrypted payload was not valid JSON"),
        }
    }
}

/// A sealed payload, base64 for JSON transport.
#[derive(Debug, Clone)]
pub struct Sealed {
    pub scheme: &'static str,
    pub kem_ciphertext: String,
    pub nonce: String,
    pub ciphertext: String,
}

/// Associated data binds the ciphertext to this sender/recipient pair.
fn aad(src_node_id: &str, dest_node_id: &str) -> Vec<u8> {
    format!("{src_node_id}->{dest_node_id}").into_bytes()
}

/// Derive the AEAD key from the KEM shared secret.
fn derive_key(shared_secret: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<sha2::Sha256>::new(None, shared_secret);
    let mut key = [0u8; 32];
    // Only fails on absurd output lengths; 32 bytes is always valid.
    hk.expand(HKDF_INFO, &mut key)
        .expect("HKDF expand of 32 bytes cannot fail");
    key
}

/// Seal a payload for `dest_node_id` using its ML-KEM public key.
pub fn seal(
    recipient_public_key_b64: &str,
    src_node_id: &str,
    dest_node_id: &str,
    payload: &Value,
) -> Result<Sealed, SealError> {
    let pk_bytes = STANDARD
        .decode(recipient_public_key_b64)
        .map_err(|_| SealError::BadRecipientKey)?;
    let pk = PublicKey::from_bytes(&pk_bytes).map_err(|_| SealError::BadRecipientKey)?;

    let (shared_secret, kem_ct) = pq_encapsulate(&pk);
    let key = derive_key(shared_secret.as_bytes());

    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let cipher = Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(&key));
    let plaintext = serde_json::to_vec(payload).map_err(|_| SealError::Encrypt)?;

    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: &plaintext,
                aad: &aad(src_node_id, dest_node_id),
            },
        )
        .map_err(|_| SealError::Encrypt)?;

    Ok(Sealed {
        scheme: SCHEME,
        kem_ciphertext: STANDARD.encode(kem_ct.as_bytes()),
        nonce: STANDARD.encode(nonce_bytes),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

/// Open a sealed payload with this node's ML-KEM secret key.
pub fn open(
    secret_key: &SecretKey,
    kem_ciphertext_b64: &str,
    nonce_b64: &str,
    ciphertext_b64: &str,
    src_node_id: &str,
    dest_node_id: &str,
) -> Result<Value, OpenError> {
    let kem_ct_bytes = STANDARD
        .decode(kem_ciphertext_b64)
        .map_err(|_| OpenError::BadKemCiphertext)?;
    let kem_ct = Ciphertext::from_bytes(&kem_ct_bytes).map_err(|_| OpenError::BadKemCiphertext)?;

    let nonce_bytes = STANDARD.decode(nonce_b64).map_err(|_| OpenError::Decrypt)?;
    if nonce_bytes.len() != 12 {
        return Err(OpenError::Decrypt);
    }
    let ciphertext = STANDARD.decode(ciphertext_b64).map_err(|_| OpenError::Decrypt)?;

    // ML-KEM decapsulation is designed not to fail: a wrong key yields a
    // different shared secret, and the AEAD below is what actually rejects.
    let shared_secret = pq_decapsulate(&kem_ct, secret_key);
    let key = derive_key(shared_secret.as_bytes());

    let cipher = Aes256Gcm::new(aes_gcm::Key::<Aes256Gcm>::from_slice(&key));
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&nonce_bytes),
            Payload {
                msg: &ciphertext,
                aad: &aad(src_node_id, dest_node_id),
            },
        )
        .map_err(|_| OpenError::Decrypt)?;

    serde_json::from_slice(&plaintext).map_err(|_| OpenError::BadPlaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pqcrypto_mlkem::mlkem1024::keypair;
    use serde_json::json;

    fn keys() -> (String, SecretKey) {
        let (pk, sk) = keypair();
        (STANDARD.encode(pk.as_bytes()), sk)
    }

    #[test]
    fn round_trips_a_payload() {
        let (pk, sk) = keys();
        let payload = json!({ "type": "LOCKDOWN", "reason": "tamper" });

        let sealed = seal(&pk, "node-a", "node-b", &payload).unwrap();
        assert_eq!(sealed.scheme, SCHEME);

        let opened = open(&sk, &sealed.kem_ciphertext, &sealed.nonce, &sealed.ciphertext, "node-a", "node-b").unwrap();
        assert_eq!(opened, payload);
    }

    #[test]
    fn ciphertext_does_not_leak_the_plaintext() {
        // The whole point: a payload sitting in dtn_outbox must not be
        // readable by anyone with database access.
        let (pk, _sk) = keys();
        let payload = json!({ "secret": "rover-7 heading 214.5" });
        let sealed = seal(&pk, "a", "b", &payload).unwrap();

        let blob = format!("{}{}{}", sealed.kem_ciphertext, sealed.nonce, sealed.ciphertext);
        assert!(!blob.contains("rover-7"));
        assert!(!blob.contains("secret"));
    }

    #[test]
    fn a_different_recipient_cannot_open_it() {
        let (pk_a, _sk_a) = keys();
        let (_pk_b, sk_b) = keys();

        let sealed = seal(&pk_a, "a", "b", &json!({"x": 1})).unwrap();
        let result = open(&sk_b, &sealed.kem_ciphertext, &sealed.nonce, &sealed.ciphertext, "a", "b");
        assert_eq!(result.unwrap_err(), OpenError::Decrypt);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (pk, sk) = keys();
        let sealed = seal(&pk, "a", "b", &json!({"x": 1})).unwrap();

        // Flip a byte in the ciphertext; GCM's tag must catch it.
        let mut raw = STANDARD.decode(&sealed.ciphertext).unwrap();
        raw[0] ^= 0xff;
        let tampered = STANDARD.encode(raw);

        let result = open(&sk, &sealed.kem_ciphertext, &sealed.nonce, &tampered, "a", "b");
        assert_eq!(result.unwrap_err(), OpenError::Decrypt);
    }

    #[test]
    fn associated_data_binds_sender_and_recipient() {
        // A sealed message replayed as though addressed to a different node
        // must fail, even though the KEM ciphertext still decapsulates.
        let (pk, sk) = keys();
        let sealed = seal(&pk, "node-a", "node-b", &json!({"x": 1})).unwrap();

        let wrong_dest = open(&sk, &sealed.kem_ciphertext, &sealed.nonce, &sealed.ciphertext, "node-a", "node-c");
        assert_eq!(wrong_dest.unwrap_err(), OpenError::Decrypt);

        let wrong_src = open(&sk, &sealed.kem_ciphertext, &sealed.nonce, &sealed.ciphertext, "node-z", "node-b");
        assert_eq!(wrong_src.unwrap_err(), OpenError::Decrypt);
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        // Nonce reuse under the same key is catastrophic for GCM.
        let (pk, _sk) = keys();
        let payload = json!({"x": 1});
        let a = seal(&pk, "a", "b", &payload).unwrap();
        let b = seal(&pk, "a", "b", &payload).unwrap();

        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_ne!(a.kem_ciphertext, b.kem_ciphertext);
    }

    #[test]
    fn rejects_a_malformed_recipient_key() {
        assert_eq!(seal("not-base64!!", "a", "b", &json!({})).unwrap_err(), SealError::BadRecipientKey);
        assert_eq!(seal("QUJD", "a", "b", &json!({})).unwrap_err(), SealError::BadRecipientKey);
    }
}
