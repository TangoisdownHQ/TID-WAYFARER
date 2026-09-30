use axum::{
    routing::post,
    Json as AxumJson, Router,
    response::IntoResponse,
    http::StatusCode,
    extract::State,
};
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use pqcrypto_mlkem::mlkem1024::{
    keypair as kem_keypair, encapsulate as pq_encapsulate, decapsulate as pq_decapsulate,
    PublicKey, SecretKey, Ciphertext, SharedSecret,
};
use pqcrypto_traits::kem::{
    PublicKey as PKTrait, SecretKey as SKTrait, Ciphertext as CTTrait, SharedSecret as SSTrait,
};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};

use crate::AppState;

/// Persistent PQ keypair state
#[derive(Clone)]
pub struct CommSecState {
    pub pk: PublicKey,
    pub sk: SecretKey,
}

/// Where the ML-KEM keypair is persisted, next to the Ed25519 identity.
const KEM_KEY_FILE: &str = "keys/commsec.json";

#[derive(Serialize, Deserialize)]
struct StoredKemKeypair {
    public_key: String,
    secret_key: String,
}

/// Load the PQ keypair, generating and persisting one on first boot.
///
/// This *must* survive restarts: peers encrypt DTN payloads to this node's
/// public key, and a keypair regenerated on every boot would make every
/// message queued for this outpost permanently undecryptable — exactly the
/// traffic a store-and-forward fabric is built to hold onto during a blackout.
pub fn init_commsec_state() -> CommSecState {
    let path = Path::new(KEM_KEY_FILE);

    if path.exists() {
        match fs::read_to_string(path)
            .ok()
            .and_then(|d| serde_json::from_str::<StoredKemKeypair>(&d).ok())
            .and_then(|stored| {
                let pk = general_purpose::STANDARD.decode(&stored.public_key).ok()?;
                let sk = general_purpose::STANDARD.decode(&stored.secret_key).ok()?;
                Some((PublicKey::from_bytes(&pk).ok()?, SecretKey::from_bytes(&sk).ok()?))
            }) {
            Some((pk, sk)) => {
                tracing::info!("loaded persisted ML-KEM keypair");
                return CommSecState { pk, sk };
            }
            // Refuse to silently mint a new key over an unreadable one: that
            // would look like success while orphaning queued ciphertext.
            None => {
                tracing::error!(
                    file = KEM_KEY_FILE,
                    "ML-KEM key file exists but could not be parsed; refusing to overwrite it"
                );
                std::process::exit(1);
            }
        }
    }

    let (pk, sk) = kem_keypair();
    let stored = StoredKemKeypair {
        public_key: general_purpose::STANDARD.encode(pk.as_bytes()),
        secret_key: general_purpose::STANDARD.encode(sk.as_bytes()),
    };

    fs::create_dir_all("keys").ok();
    if let Err(e) = fs::write(path, serde_json::to_string_pretty(&stored).unwrap_or_default()) {
        tracing::error!(error = %e, "could not persist ML-KEM keypair; peers will lose the ability to reach this node after a restart");
    } else {
        tracing::info!("generated and persisted a new ML-KEM keypair");
    }

    CommSecState { pk, sk }
}

/// All commsec routes
// Nested under /api/commsec in main.rs.
pub fn commsec_routes() -> Router<AppState> {
    Router::new()
        .route("/keypair", post(get_keypair))
        .route("/encapsulate", post(encapsulate))
        .route("/decapsulate", post(decapsulate))
        .route("/aead/encrypt", post(aead_encrypt))
        .route("/aead/decrypt", post(aead_decrypt))
}

#[derive(Serialize)]
struct KeypairResponse {
    public_key: String,
    algorithm: &'static str,
}

/// Returns this outpost's ML-KEM **public** key.
///
/// This used to return the secret key alongside it, which handed the node's
/// PQ private key to any caller holding a JWT or the fabric token — and that
/// key now decrypts every DTN payload addressed to this outpost. The secret
/// never leaves the process; peers only ever need the public half to
/// encapsulate to it.
async fn get_keypair(State(state): State<AppState>) -> impl IntoResponse {
    AxumJson(KeypairResponse {
        public_key: general_purpose::STANDARD.encode(state.commsec.pk.as_bytes()),
        algorithm: "ML-KEM-1024",
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct EncapsulateRequest {
    pub public_key: String,
}

#[derive(Serialize)]
pub struct EncapsulateResponse {
    pub ciphertext: String,
    pub shared_secret: String,
}

pub async fn encapsulate(AxumJson(req): AxumJson<EncapsulateRequest>) -> impl IntoResponse {
    let pk_bytes = match general_purpose::STANDARD.decode(&req.public_key) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid base64").into_response(),
    };

    let pk = match PublicKey::from_bytes(&pk_bytes) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid public key").into_response(),
    };

    let (ss, ct): (SharedSecret, Ciphertext) = pq_encapsulate(&pk);

    let ct_b64 = general_purpose::STANDARD.encode(ct.as_bytes());
    let ss_b64 = general_purpose::STANDARD.encode(ss.as_bytes());

    AxumJson(EncapsulateResponse {
        ciphertext: ct_b64,
        shared_secret: ss_b64,
    })
    .into_response()
}

#[derive(Deserialize)]
pub struct DecapsulateRequest {
    pub secret_key: String,
    pub ciphertext: String,
}

#[derive(Serialize)]
pub struct DecapsulateResponse {
    pub shared_secret: String,
}

pub async fn decapsulate(AxumJson(req): AxumJson<DecapsulateRequest>) -> impl IntoResponse {
    let sk_bytes = match general_purpose::STANDARD.decode(&req.secret_key) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid base64").into_response(),
    };
    let ct_bytes = match general_purpose::STANDARD.decode(&req.ciphertext) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid base64").into_response(),
    };

    let sk = match SecretKey::from_bytes(&sk_bytes) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid secret key").into_response(),
    };
    let ct = match Ciphertext::from_bytes(&ct_bytes) {
        Ok(c) => c,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid ciphertext").into_response(),
    };

    let ss = pq_decapsulate(&ct, &sk);
    let ss_b64 = general_purpose::STANDARD.encode(ss.as_bytes());

    AxumJson(DecapsulateResponse { shared_secret: ss_b64 }).into_response()
}

#[derive(Deserialize)]
pub struct AeadEncryptRequest {
    pub key: String,
    pub nonce: String,
    pub plaintext: String,
    pub associated_data: Option<String>,
}

#[derive(Serialize)]
pub struct AeadEncryptResponse {
    pub ciphertext: String,
}

pub async fn aead_encrypt(AxumJson(req): AxumJson<AeadEncryptRequest>) -> impl IntoResponse {
    let key_bytes = match general_purpose::STANDARD.decode(&req.key) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid key base64").into_response(),
    };
    let nonce_bytes = match general_purpose::STANDARD.decode(&req.nonce) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid nonce base64").into_response(),
    };
    if nonce_bytes.len() != 12 {
        return (StatusCode::BAD_REQUEST, "nonce must be 12 bytes").into_response();
    }

    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let payload = if let Some(ad) = &req.associated_data {
        Payload { msg: req.plaintext.as_bytes(), aad: ad.as_bytes() }
    } else {
        Payload { msg: req.plaintext.as_bytes(), aad: &[] }
    };

    match cipher.encrypt(nonce, payload) {
        Ok(ct) => {
            let ct_b64 = general_purpose::STANDARD.encode(ct);
            AxumJson(AeadEncryptResponse { ciphertext: ct_b64 }).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "encryption failed").into_response(),
    }
}

#[derive(Deserialize)]
pub struct AeadDecryptRequest {
    pub key: String,
    pub nonce: String,
    pub ciphertext: String,
    pub associated_data: Option<String>,
}

#[derive(Serialize)]
pub struct AeadDecryptResponse {
    pub plaintext: String,
}

pub async fn aead_decrypt(AxumJson(req): AxumJson<AeadDecryptRequest>) -> impl IntoResponse {
    let key_bytes = match general_purpose::STANDARD.decode(&req.key) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid key base64").into_response(),
    };
    let nonce_bytes = match general_purpose::STANDARD.decode(&req.nonce) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid nonce base64").into_response(),
    };
    if nonce_bytes.len() != 12 {
        return (StatusCode::BAD_REQUEST, "nonce must be 12 bytes").into_response();
    }

    let ct_bytes = match general_purpose::STANDARD.decode(&req.ciphertext) {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid ciphertext base64").into_response(),
    };

    let key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_bytes);
    let cipher = Aes256Gcm::new(key);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let payload = if let Some(ad) = &req.associated_data {
        Payload { msg: &ct_bytes, aad: ad.as_bytes() }
    } else {
        Payload { msg: &ct_bytes, aad: &[] }
    };

    match cipher.decrypt(nonce, payload) {
        Ok(pt) => {
            let pt_str = match String::from_utf8(pt) {
                Ok(s) => s,
                Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "invalid utf-8").into_response(),
            };
            AxumJson(AeadDecryptResponse { plaintext: pt_str }).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "decryption failed").into_response(),
    }
}

