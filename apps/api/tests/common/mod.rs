//! Shared harness for the integration tests.
//!
//! These run against a real Postgres because the properties worth asserting
//! here cannot be reached from a unit test: whether a guard actually refuses a
//! request, whether a query actually filters by organisation, whether a hold
//! actually blocks a shipment. Each of those is a fact about the assembled
//! application and its schema, not about a function.
//!
//! Set `TEST_DATABASE_URL` to run them. Without it they skip rather than fail,
//! so `cargo test` stays useful on a machine with no database — a test that
//! fails for want of infrastructure trains people to ignore red.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use tid_wayfarer::routes::auth::AuthState;
use tid_wayfarer::routes::commsec::init_commsec_state;
use tid_wayfarer::services::identity::NodeIdentity;
use tid_wayfarer::AppState;

pub const JWT_SECRET: &str = "integration-test-secret";

/// A live application plus the pool behind it.
pub struct Harness {
    pub app: axum::Router,
    pub db: PgPool,
    /// This outpost's own node id. The DTN tests need it to address an
    /// envelope here, since an envelope addressed anywhere else is refused.
    pub node_id: String,
}

/// Connect, or tell the caller to skip.
pub async fn harness() -> Option<Harness> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let db = match PgPool::connect(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("TEST_DATABASE_URL set but unreachable: {e}");
            return None;
        }
    };

    // Each run works in its own schema-less island by using fresh ids
    // everywhere; the fixtures below never reuse a name.
    std::env::set_var("JWT_SECRET", JWT_SECRET);
    std::env::set_var("SETTLEMENT_VERIFY", "off");
    // Per-node signatures must be accepted, or no test can present itself as
    // a peer and the DTN cases would all pass by being refused for the wrong
    // reason.
    std::env::set_var("FABRIC_AUTH", "both");
    std::env::set_var("NODE_SHARED_SECRET", "integration-test-fabric-secret");

    let identity = NodeIdentity {
        node_id: Uuid::new_v4().to_string(),
        public_key: "dGVzdC1wdWJsaWMta2V5LTMyLWJ5dGVzLWxvbmchISE=".into(),
        secret_key: "dGVzdC1zZWNyZXQta2V5LTMyLWJ5dGVzLWxvbmchISE=".into(),
    };

    let state = AppState {
        db: db.clone(),
        auth: AuthState { clients: Default::default(), jwt_secret: JWT_SECRET.into() },
        commsec: init_commsec_state(),
        identity,
    };

    let node_id = state.identity.node_id.clone();
    Some(Harness { app: tid_wayfarer::app::test_router(state), db, node_id })
}

/// Skip the body of a test when there is no database, with a visible note.
#[macro_export]
macro_rules! require_db {
    () => {
        match common::harness().await {
            Some(h) => h,
            None => {
                eprintln!("skipping: set TEST_DATABASE_URL to run integration tests");
                return;
            }
        }
    };
}

impl Harness {
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let req = if let Some(b) = body {
            req.header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap()
        } else {
            req.body(Body::empty()).unwrap()
        };

        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::String(
            String::from_utf8_lossy(&bytes).to_string(),
        ));
        (status, json)
    }

    pub async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.call("GET", path, Some(token), None).await
    }
    pub async fn post(&self, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
        self.call("POST", path, Some(token), Some(body)).await
    }

    /// Create a user directly and return (id, token). Going through the signup
    /// endpoint would work, but these tests are about authorisation, not about
    /// re-testing signup on every case.
    pub async fn user(&self, role: &str) -> (Uuid, String) {
        let id = Uuid::new_v4();
        let email = format!("{id}@test.invalid");
        sqlx::query(
            "INSERT INTO users (id, username, email, role, identity_hash) VALUES ($1,$2,$3,$4,'x')",
        )
        .bind(id)
        .bind(format!("u{}", &id.to_string()[..8]))
        .bind(&email)
        .bind(role)
        .execute(&self.db)
        .await
        .expect("could not create test user");

        (id, mint(id, role))
    }

    /// An organisation with a usable (if unused) root key.
    pub async fn org(&self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO organisations (id, name, slug, root_public_key) VALUES ($1,$2,$3,$4)",
        )
        .bind(id)
        .bind(name)
        .bind(format!("{name}-{}", &id.to_string()[..8]))
        // Distinct per org; these tests exercise scoping, not signatures.
        .bind(base64_key(id))
        .execute(&self.db)
        .await
        .expect("could not create test org");
        id
    }

    pub async fn join(&self, org: Uuid, user: Uuid, role: &str) {
        sqlx::query("INSERT INTO org_members (org_id, user_id, role) VALUES ($1,$2,$3)")
            .bind(org)
            .bind(user)
            .bind(role)
            .execute(&self.db)
            .await
            .expect("could not add member");
    }

    /// An inventory row owned by an org.
    pub async fn inventory(&self, org: Uuid, owner: Uuid, name: &str, qty: i32) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO inventory (id, owner_id, org_id, name, quantity, location, category, unit, threshold)
             VALUES ($1,$2,$3,$4,$5,'bay','general','each',1)",
        )
        .bind(id).bind(owner).bind(org).bind(name).bind(qty)
        .execute(&self.db)
        .await
        .expect("could not create inventory");
        id
    }
}

fn base64_key(seed: Uuid) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(seed.as_bytes())
}

/// Mint a token the running guard will accept.
pub fn mint(user: Uuid, role: &str) -> String {
    use jsonwebtoken::{encode, EncodingKey, Header};
    #[derive(serde::Serialize)]
    struct Claims {
        sub: String,
        exp: usize,
        provider: String,
        role: String,
    }
    let exp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as usize
        + 3600;
    encode(
        &Header::default(),
        &Claims { sub: user.to_string(), exp, provider: "test".into(), role: role.into() },
        &EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .unwrap()
}
