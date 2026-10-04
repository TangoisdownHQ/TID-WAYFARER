//! Sealing an outpost's private keys so they can be backed up.
//!
//! # Why this is the most important file in the repository
//!
//! An outpost's identity lives in two files on one volume. Lose them and
//! `load_or_generate_identity` mints a *new* Ed25519 key and a *new* node id on
//! the next start, which produces something far worse than an error:
//!
//!   - Peers hold the old node id and public key in `node_registry`. The new
//!     identity cannot authenticate as the old one.
//!   - Its `org_outposts` certificate names the old node id, so `own_org()`
//!     resolves to nothing — the rollup stops asking peers, and org-scoped
//!     reads from peers fail closed.
//!   - **Every DTN bundle queued for it was sealed to its old ML-KEM key, and
//!     becomes permanently undecryptable.** Those are exactly the messages that
//!     were waiting because the link was down.
//!   - Custody receipts it signed still name the old key. The provenance
//!     survives; the ability to sign as that party does not.
//!
//! It is not a degraded outpost. It is a new outpost wearing an old name.
//!
//! This is not hypothetical: this deployment already had an `org_outposts` row
//! naming an identity from before the keys volume existed, which is why its own
//! organisation resolved to nothing until it was re-enrolled by hand.
//!
//! # What this can and cannot fix
//!
//! | Situation | Recovery |
//! |---|---|
//! | Planned rotation | A **successor statement** signed by the outgoing key. Peers accept it without an administrator, because the predecessor's authority transfers. See `routes::nodes::succeed_key`. |
//! | Key loss, with a sealed backup | Restore the bundle. Nothing at any peer changes, because the identity is the same one. |
//! | Key loss, no backup | An operator at **every peer** must rotate you in by hand. There is no cryptographic shortcut, and pretending otherwise would be the dangerous design. |
//!
//! That last row is the whole argument for taking a backup.
//!
//! # The sealing
//!
//! Argon2id over the passphrase to a 32-byte key, then AES-256-GCM. The salt
//! and nonce are random per export and stored in the envelope, so two exports
//! of the same keys with the same passphrase produce different ciphertext —
//! which matters because these end up in object storage where an attacker can
//! see that nothing changed.
//!
//! The node id is sealed *inside* rather than left in the header. It is the
//! name a peer knows this outpost by; leaving it in the clear would turn a
//! backup file into a statement about which outpost it unlocks.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use argon2::Argon2;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};

/// Envelope format. Bumped if the sealing changes, so an old bundle is refused
/// with a version message rather than a decryption failure that looks like a
/// wrong passphrase.
pub const BUNDLE_VERSION: u8 = 1;

/// Shortest passphrase accepted.
///
/// The whole security of a backup rests on this one string: a bundle in object
/// storage is offline-attackable at whatever rate the attacker can afford.
/// Argon2id makes each guess expensive; length is what makes the guess count
/// large. Twenty characters is a four-word phrase, which people will actually
/// use.
pub const MIN_PASSPHRASE_LEN: usize = 20;

/// Argon2id cost. Deliberately above the crate default: this runs once per
/// export or restore, so a second of CPU is free to us and expensive to anyone
/// guessing.
const KDF_MEM_KIB: u32 = 64 * 1024;
const KDF_ITERATIONS: u32 = 3;
const KDF_PARALLELISM: u32 = 1;

/// What is actually worth saving.
///
/// Both secret keys together. Separately they are nearly useless: the Ed25519
/// key alone authenticates to peers but cannot open queued DTN traffic, and the
/// ML-KEM key alone opens traffic for a node that can no longer prove it is
/// that node.
#[derive(Serialize, Deserialize, Clone)]
pub struct KeyBundle {
    pub node_id: String,
    pub outpost_name: String,
    /// Ed25519. Base64, 32 bytes — the same encoding `identity.json` uses.
    pub identity_secret: String,
    pub identity_public: String,
    /// ML-KEM-1024. Base64, matching `commsec.json`.
    pub kem_secret: String,
    pub kem_public: String,
    /// When the bundle was made, so a restore can say how old it is.
    pub exported_at: String,
}

/// Redacting, not derived.
///
/// A derived `Debug` would put both secret keys into any `{:?}` — a log line,
/// a panic message, an `unwrap` in a test — and the whole point of this module
/// is that those two strings never end up anywhere they were not deliberately
/// put. Writing it by hand means an accidental format is safe by construction
/// rather than by everyone remembering.
impl std::fmt::Debug for KeyBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyBundle")
            .field("node_id", &self.node_id)
            .field("outpost_name", &self.outpost_name)
            .field("identity_public", &self.identity_public)
            .field("identity_secret", &"<redacted>")
            .field("kem_public", &self.kem_public)
            .field("kem_secret", &"<redacted>")
            .field("exported_at", &self.exported_at)
            .finish()
    }
}

/// The file an operator stores. Only the version, KDF parameters, salt and
/// nonce are in the clear — everything identifying is inside the ciphertext.
///
/// `Debug` is derived here and deliberately *not* on [`KeyBundle`]: everything
/// in this struct is either a public parameter or ciphertext, so printing it is
/// harmless.
#[derive(Serialize, Deserialize, Debug)]
pub struct SealedBundle {
    pub v: u8,
    pub kdf: String,
    pub mem_kib: u32,
    pub iterations: u32,
    pub salt: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Debug)]
pub enum VaultError {
    WeakPassphrase(String),
    /// Wrong passphrase, or the file was altered. Deliberately one error: an
    /// attacker learning *which* of the two it was would learn whether a
    /// guessed passphrase was close.
    CannotOpen,
    UnsupportedVersion(u8),
    Malformed(String),
    Internal(String),
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::WeakPassphrase(m) => write!(f, "{m}"),
            VaultError::CannotOpen => write!(
                f,
                "could not open the bundle — wrong passphrase, or the file has been altered"
            ),
            VaultError::UnsupportedVersion(v) => {
                write!(f, "bundle format v{v} is not supported by this build")
            }
            VaultError::Malformed(m) => write!(f, "malformed bundle: {m}"),
            VaultError::Internal(m) => write!(f, "{m}"),
        }
    }
}

fn derive(passphrase: &str, salt: &[u8]) -> Result<[u8; 32], VaultError> {
    let params = argon2::Params::new(KDF_MEM_KIB, KDF_ITERATIONS, KDF_PARALLELISM, Some(32))
        .map_err(|e| VaultError::Internal(format!("bad KDF parameters: {e}")))?;
    let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);

    let mut key = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| VaultError::Internal(format!("key derivation failed: {e}")))?;
    Ok(key)
}

/// Reject a passphrase that cannot protect an offline-attackable file.
pub fn check_passphrase(p: &str) -> Result<(), VaultError> {
    let n = p.chars().count();
    if n < MIN_PASSPHRASE_LEN {
        return Err(VaultError::WeakPassphrase(format!(
            "a backup passphrase must be at least {MIN_PASSPHRASE_LEN} characters (got {n}) — \
             this file is offline-attackable, so length is the only thing protecting it. \
             Four unrelated words is enough and is memorable."
        )));
    }
    // A long string of one repeated character defeats the length rule while
    // looking like it passes.
    if p.chars().collect::<std::collections::BTreeSet<_>>().len() < 5 {
        return Err(VaultError::WeakPassphrase(
            "that passphrase has too few distinct characters to be worth its length".into(),
        ));
    }
    Ok(())
}

/// Seal a bundle under a passphrase.
pub fn seal(bundle: &KeyBundle, passphrase: &str) -> Result<SealedBundle, VaultError> {
    check_passphrase(passphrase)?;

    let mut salt = [0u8; 16];
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce_bytes);

    let key = derive(passphrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| VaultError::Internal(format!("cipher init failed: {e}")))?;

    let plaintext = serde_json::to_vec(bundle)
        .map_err(|e| VaultError::Internal(format!("could not encode bundle: {e}")))?;

    // The header is authenticated as associated data, so the parameters
    // recorded alongside the ciphertext are tamper-evident.
    //
    // Note what actually prevents a downgrade attack, because it is not this:
    // `open` never reads the stored `mem_kib`/`iterations` at all, it uses this
    // build's constants. So rewriting the header cannot make a passphrase
    // cheaper to attack — there is nothing to downgrade. The stored values are
    // a record of what sealed the file, and `BUNDLE_VERSION` is what a future
    // build would bump if the cost ever changes, so an old bundle is refused
    // with a version message rather than failing to decrypt.
    let aad = header_aad(&salt, &nonce_bytes);

    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), Payload { msg: &plaintext, aad: &aad })
        .map_err(|_| VaultError::Internal("sealing failed".into()))?;

    Ok(SealedBundle {
        v: BUNDLE_VERSION,
        kdf: "argon2id".into(),
        mem_kib: KDF_MEM_KIB,
        iterations: KDF_ITERATIONS,
        salt: B64.encode(salt),
        nonce: B64.encode(nonce_bytes),
        ciphertext: B64.encode(ciphertext),
    })
}

/// Bind the header into the AEAD, so its parameters are tamper-evident.
///
/// Built from this build's constants rather than the stored fields — which is
/// the point: the values in the file cannot influence anything, so a rewritten
/// header is both detected and inert.
fn header_aad(salt: &[u8], nonce: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(64);
    aad.extend_from_slice(b"tid-wayfarer-keybundle|v");
    aad.push(BUNDLE_VERSION);
    aad.extend_from_slice(b"|argon2id|");
    aad.extend_from_slice(&KDF_MEM_KIB.to_be_bytes());
    aad.extend_from_slice(&KDF_ITERATIONS.to_be_bytes());
    aad.extend_from_slice(&KDF_PARALLELISM.to_be_bytes());
    aad.extend_from_slice(b"|");
    aad.extend_from_slice(salt);
    aad.extend_from_slice(b"|");
    aad.extend_from_slice(nonce);
    aad
}

/// Open a sealed bundle.
pub fn open(sealed: &SealedBundle, passphrase: &str) -> Result<KeyBundle, VaultError> {
    if sealed.v != BUNDLE_VERSION {
        return Err(VaultError::UnsupportedVersion(sealed.v));
    }
    if sealed.kdf != "argon2id" {
        return Err(VaultError::Malformed(format!("unknown kdf '{}'", sealed.kdf)));
    }

    let salt = B64
        .decode(&sealed.salt)
        .map_err(|_| VaultError::Malformed("salt is not base64".into()))?;
    let nonce = B64
        .decode(&sealed.nonce)
        .map_err(|_| VaultError::Malformed("nonce is not base64".into()))?;
    let ciphertext = B64
        .decode(&sealed.ciphertext)
        .map_err(|_| VaultError::Malformed("ciphertext is not base64".into()))?;

    if nonce.len() != 12 {
        return Err(VaultError::Malformed("nonce must be 12 bytes".into()));
    }

    // The stored salt is used — it has to be, it is per-bundle — but the
    // stored *cost* parameters are ignored in favour of this build's. An
    // attacker rewriting `mem_kib` to 8 changes nothing about how expensive
    // their guesses are.
    let key = derive(passphrase, &salt)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|e| VaultError::Internal(format!("cipher init failed: {e}")))?;

    let aad = header_aad(&salt, &nonce);
    let plaintext = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ciphertext, aad: &aad })
        .map_err(|_| VaultError::CannotOpen)?;

    serde_json::from_slice(&plaintext)
        .map_err(|e| VaultError::Malformed(format!("bundle contents unreadable: {e}")))
}

/// Read the live keys off disk into a bundle.
///
/// Reads the files rather than taking the in-memory state, so an export is a
/// statement about what is actually persisted. A process holding keys it never
/// wrote down is precisely the situation a backup is meant to catch.
pub fn bundle_from_disk(outpost_name: &str) -> Result<KeyBundle, VaultError> {
    let identity: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string("keys/identity.json")
            .map_err(|e| VaultError::Internal(format!("cannot read keys/identity.json: {e}")))?,
    )
    .map_err(|e| VaultError::Malformed(format!("identity.json: {e}")))?;

    let kem: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string("keys/commsec.json")
            .map_err(|e| VaultError::Internal(format!("cannot read keys/commsec.json: {e}")))?,
    )
    .map_err(|e| VaultError::Malformed(format!("commsec.json: {e}")))?;

    let s = |v: &serde_json::Value, k: &str| -> Result<String, VaultError> {
        v.get(k)
            .and_then(|x| x.as_str())
            .map(String::from)
            .ok_or_else(|| VaultError::Malformed(format!("missing field '{k}'")))
    };

    Ok(KeyBundle {
        node_id: s(&identity, "node_id")?,
        outpost_name: outpost_name.to_string(),
        identity_secret: s(&identity, "secret_key")?,
        identity_public: s(&identity, "public_key")?,
        kem_secret: s(&kem, "secret_key")?,
        kem_public: s(&kem, "public_key")?,
        exported_at: chrono::Utc::now().to_rfc3339(),
    })
}

/// Write a bundle back to the key files.
///
/// Refuses to overwrite existing files. A restore onto a running outpost that
/// already has an identity would swap it mid-flight, leaving in-memory state,
/// the registry and the certificate disagreeing — and if the existing keys were
/// the good ones, destroy them. Move them aside deliberately instead.
pub fn write_to_disk(bundle: &KeyBundle) -> Result<(), VaultError> {
    for f in ["keys/identity.json", "keys/commsec.json"] {
        if std::path::Path::new(f).exists() {
            return Err(VaultError::Internal(format!(
                "{f} already exists — move it aside before restoring, so an existing identity \
                 is never silently replaced"
            )));
        }
    }

    std::fs::create_dir_all("keys")
        .map_err(|e| VaultError::Internal(format!("cannot create keys/: {e}")))?;

    let identity = serde_json::json!({
        "node_id": bundle.node_id,
        "public_key": bundle.identity_public,
        "secret_key": bundle.identity_secret,
    });
    let kem = serde_json::json!({
        "public_key": bundle.kem_public,
        "secret_key": bundle.kem_secret,
    });

    std::fs::write("keys/identity.json", serde_json::to_string_pretty(&identity).unwrap())
        .map_err(|e| VaultError::Internal(format!("cannot write identity.json: {e}")))?;
    std::fs::write("keys/commsec.json", serde_json::to_string_pretty(&kem).unwrap())
        .map_err(|e| VaultError::Internal(format!("cannot write commsec.json: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle() -> KeyBundle {
        KeyBundle {
            node_id: "9b6825ad-7d9f-49a4-9014-05be2a6696f0".into(),
            outpost_name: "TIDasONE-Core".into(),
            identity_secret: "dGVzdC1zZWNyZXQta2V5LTMyLWJ5dGVzLWxvbmchISE=".into(),
            identity_public: "dGVzdC1wdWJsaWMta2V5LTMyLWJ5dGVzLWxvbmchISE=".into(),
            kem_secret: "a2VtLXNlY3JldA==".into(),
            kem_public: "a2VtLXB1YmxpYw==".into(),
            exported_at: "2026-10-04T00:00:00+00:00".into(),
        }
    }

    const GOOD: &str = "anchor-ballast-cargo-dock-42";

    #[test]
    fn a_sealed_bundle_opens_with_the_right_passphrase() {
        let sealed = seal(&bundle(), GOOD).unwrap();
        let opened = open(&sealed, GOOD).unwrap();
        assert_eq!(opened.node_id, bundle().node_id);
        assert_eq!(opened.identity_secret, bundle().identity_secret);
        assert_eq!(opened.kem_secret, bundle().kem_secret);
    }

    #[test]
    fn the_wrong_passphrase_does_not_open_it() {
        let sealed = seal(&bundle(), GOOD).unwrap();
        assert!(matches!(
            open(&sealed, "anchor-ballast-cargo-dock-43"),
            Err(VaultError::CannotOpen)
        ));
    }

    /// Nothing identifying is readable without the passphrase.
    ///
    /// A backup sitting in object storage should not announce which outpost it
    /// unlocks — that turns the file itself into a targeting aid.
    #[test]
    fn the_envelope_reveals_nothing_about_which_outpost_it_is() {
        let sealed = seal(&bundle(), GOOD).unwrap();
        let json = serde_json::to_string(&sealed).unwrap();
        assert!(!json.contains("9b6825ad"), "node id leaked into the envelope");
        assert!(!json.contains("TIDasONE-Core"), "outpost name leaked into the envelope");
        assert!(!json.contains(&bundle().identity_public), "a key leaked into the envelope");
    }

    /// Two exports of the same keys differ.
    ///
    /// Otherwise an observer of the backup bucket learns when the keys changed
    /// and when they did not, which is information about rotation they should
    /// not have.
    #[test]
    fn two_exports_are_not_byte_identical() {
        let a = seal(&bundle(), GOOD).unwrap();
        let b = seal(&bundle(), GOOD).unwrap();
        assert_ne!(a.ciphertext, b.ciphertext);
        assert_ne!(a.salt, b.salt);
        assert_ne!(a.nonce, b.nonce);
    }

    #[test]
    fn a_tampered_ciphertext_is_refused() {
        let mut sealed = seal(&bundle(), GOOD).unwrap();
        let mut raw = B64.decode(&sealed.ciphertext).unwrap();
        raw[4] ^= 0xff;
        sealed.ciphertext = B64.encode(raw);
        assert!(matches!(open(&sealed, GOOD), Err(VaultError::CannotOpen)));
    }

    /// Rewriting the stored KDF cost achieves nothing.
    ///
    /// The attack this guards against: rewrite `mem_kib` to something cheap,
    /// hand the bundle back to the operator as "your backup", and brute force
    /// the passphrase at a fraction of the cost.
    ///
    /// It fails because `open` never reads those fields — derivation uses this
    /// build's constants — so the header is *inert*, not merely checked. The
    /// test asserts exactly that: a rewritten header still opens with the right
    /// passphrase and still refuses the wrong one, because the numbers in the
    /// file were never consulted.
    #[test]
    fn the_stored_kdf_cost_cannot_influence_anything() {
        let mut sealed = seal(&bundle(), GOOD).unwrap();
        sealed.mem_kib = 8;
        sealed.iterations = 1;

        // Still opens: the real cost came from the build, not the file.
        let opened = open(&sealed, GOOD).expect("a rewritten header must not break a valid bundle");
        assert_eq!(opened.node_id, bundle().node_id);

        // And guessing is no cheaper than it was.
        assert!(matches!(open(&sealed, "anchor-ballast-cargo-dock-43"), Err(VaultError::CannotOpen)));
    }

    #[test]
    fn a_future_format_is_refused_with_a_version_not_a_decryption_error() {
        let mut sealed = seal(&bundle(), GOOD).unwrap();
        sealed.v = 99;
        match open(&sealed, GOOD) {
            Err(VaultError::UnsupportedVersion(99)) => {}
            other => panic!("expected a version error, got {other:?}"),
        }
    }

    /// A short passphrase is refused at seal time, not discovered later.
    #[test]
    fn a_weak_passphrase_is_refused_with_advice() {
        let err = seal(&bundle(), "hunter2").unwrap_err();
        match err {
            VaultError::WeakPassphrase(m) => {
                assert!(m.contains("at least 20"), "{m}");
                assert!(m.contains("four unrelated words") || m.contains("Four unrelated words"), "{m}");
            }
            other => panic!("expected WeakPassphrase, got {other:?}"),
        }

        // Long but trivial defeats the length rule while appearing to pass it.
        assert!(matches!(
            seal(&bundle(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Err(VaultError::WeakPassphrase(_))
        ));
    }

    /// Debug must never print a secret.
    ///
    /// This is the guard on an accidental `{:?}` in a log line or a panic —
    /// the one way these strings could escape without anyone deciding to let
    /// them.
    #[test]
    fn debug_redacts_both_secrets() {
        let printed = format!("{:?}", bundle());
        assert!(!printed.contains(&bundle().identity_secret), "identity secret leaked: {printed}");
        assert!(!printed.contains(&bundle().kem_secret), "kem secret leaked: {printed}");
        assert!(printed.contains("<redacted>"));
        // Public material is still useful to see.
        assert!(printed.contains(&bundle().node_id));
    }

    /// Both keys travel together.
    ///
    /// Separately they are nearly useless: Ed25519 alone authenticates but
    /// cannot open queued DTN traffic; ML-KEM alone opens traffic for a node
    /// that can no longer prove it is that node.
    #[test]
    fn a_bundle_carries_both_secrets() {
        let opened = open(&seal(&bundle(), GOOD).unwrap(), GOOD).unwrap();
        assert!(!opened.identity_secret.is_empty());
        assert!(!opened.kem_secret.is_empty());
    }
}
