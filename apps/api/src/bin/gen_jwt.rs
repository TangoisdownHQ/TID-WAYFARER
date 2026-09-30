//! Mints a JWT compatible with the AuthenticatedUser / AdminUser extractors.
//!
//! Reads:
//!   JWT_SECRET  - required, must match what the API was started with
//!   JWT_SUB     - required, user UUID (the value the API passes through Uuid::parse_str)
//!   JWT_ROLE    - optional, "user" (default) or "admin"
//!   JWT_EXP     - optional, seconds from now (default 3600)
//!   JWT_PROV    - optional, provider label (default "manual")
//!
//! Usage:
//!   JWT_SECRET=... JWT_SUB=<uuid> [JWT_ROLE=admin] cargo run --bin gen_jwt
//!
//! Output: the encoded token on stdout (single line, no trailing newline noise).

use jsonwebtoken::{encode, EncodingKey, Header};
use serde::Serialize;
use std::env;
use std::process::exit;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct Claims {
    sub: String,
    exp: usize,
    provider: String,
    role: String,
}

fn die(msg: &str) -> ! {
    eprintln!("gen_jwt: {}", msg);
    exit(2);
}

fn require_env(key: &str) -> String {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => die(&format!("{} is required", key)),
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn env_secs(key: &str, default: u64) -> u64 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn main() {
    let secret  = require_env("JWT_SECRET");
    let sub     = require_env("JWT_SUB");
    let role    = env_or("JWT_ROLE", "user");
    let exp_in  = env_secs("JWT_EXP", 3600);
    let prov    = env_or("JWT_PROV", "manual");

    let exp = (SystemTime::now().duration_since(UNIX_EPOCH).unwrap() + Duration::from_secs(exp_in))
        .as_secs() as usize;

    let claims = Claims { sub, exp, provider: prov, role };

    let token = encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap_or_else(|e| die(&format!("encode failed: {}", e)));

    print!("{}", token);
}
