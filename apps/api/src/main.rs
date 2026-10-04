use std::collections::HashMap;
use std::net::SocketAddr;

use tid_wayfarer::AppState;
use tid_wayfarer::routes::{
    me::me_routes,
    inventory::inventory_routes,
    auth::{auth_routes, AuthState},
    auth_middleware::{require_auth, enforce_lockdown},
    commsec::{commsec_routes, init_commsec_state},
    local_auth::local_auth_routes,
    packages::package_routes,
    assets::asset_routes,
    user::user_routes,
    nodes::node_routes,
    nodes_sync::node_sync_routes,
    commands::command_routes,
    blockchain::blockchain_routes,
    dashboard::dashboard_routes,
    assets_fleet::fleet_asset_routes,
    fleet_map::fleet_map_routes,
    map::map_routes,
    bodies::body_routes,
    asset_logistics::{asset_extras_routes, kit_routes},
    orders::{order_routes, fulfillment_routes},
    rules::rules_routes,
    peers::peer_routes,
    ops::ops_routes,
    dtn::dtn_routes,
    fabric::fabric_routes,
    supplylink::supplylink_routes,
    rollup::rollup_routes,
    lots::{lot_routes, inventory_lot_routes},
    capsules::capsule_routes,
    compliance::compliance_routes,
    custody::custody_routes,
    rates::rate_routes,
    movements::movement_routes,
    documents::document_routes,
    search::search_routes,
    orgs::org_routes,
};

use tid_wayfarer::services::identity::load_or_generate_identity;
use tid_wayfarer::services::registration::run_registration;
use tid_wayfarer::services::sync_daemon::start_sync_daemon;
use tid_wayfarer::services::telemetry_processor::run_telemetry_processor;
use tid_wayfarer::services::command_engine::run_command_engine;
use tid_wayfarer::services::dtn::run_dtn_forwarder;
use tid_wayfarer::services::replication::run_replication_daemon;
use tid_wayfarer::services::carrier_tracking::run_carrier_tracker;
use tid_wayfarer::services::login_throttle::run_pruner as run_auth_attempt_pruner;
use tid_wayfarer::services::blockchain_feeder::run_blockchain_feeder;
use tid_wayfarer::services::settlement::run_settlement_verifier;

use tid_wayfarer::routes::request_trace::trace_requests;
use tid_wayfarer::services::metrics;

use axum::{routing::get, Json, Router};
use dotenvy::dotenv;
use serde_json::json;
use sqlx::PgPool;
use tracing::info;
use tracing_subscriber::EnvFilter;

use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

/// Required-at-boot env lookup. Exits the process if missing — only call this
/// during startup, never from a request handler (a probe must not be able to
/// kill the process).
fn get_env_var(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        // init_tracing() runs first in main(), so this reaches the subscriber.
        tracing::error!(env_var = key, "missing required env var; refusing to start");
        std::process::exit(1);
    })
}

/// Non-fatal env lookup for descriptive fields, safe to call from handlers.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Outpost identity: env the Helm chart / operator inject (OUTPOST_NAME,
/// OUTPOST_ROLE, BODY_ID, OUTPOST_REGION, CORE_API_URL) plus this node's
/// live identity. The secret key never leaves the process.
fn outpost_identity(state: &tid_wayfarer::AppState) -> serde_json::Value {
    json!({
        "name": env_or("OUTPOST_NAME", "tid-wayfarer"),
        "role": env_or("OUTPOST_ROLE", "outpost"),
        "bodyId": env_or("BODY_ID", "399").parse::<i64>().unwrap_or(399),
        "region": env_or("OUTPOST_REGION", ""),
        "coreApiUrl": env_or("CORE_API_URL", ""),
        "nodeId": state.identity.node_id,
        "publicKey": state.identity.public_key,
    })
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "service": env_or("OUTPOST_NAME", "tid-wayfarer"),
    }))
}

/// Reports this outpost's identity — what the operator/chart configured it
/// as, plus the node's own ID and public key.
async fn outpost_info(
    axum::extract::State(state): axum::extract::State<tid_wayfarer::AppState>,
) -> Json<serde_json::Value> {
    Json(outpost_identity(&state))
}

async fn root() -> &'static str {
    "tid-wayfarer API is running ✅"
}

/// Prometheus scrape endpoint. Left outside the auth guard so a sidecar or
/// ServiceMonitor can scrape it without holding a fabric credential; it
/// exposes counts only, never payloads or identifiers.
async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<tid_wayfarer::AppState>,
) -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        metrics::render(&state.db).await,
    )
}

/// Structured logging. `RUST_LOG` controls verbosity (default: info for this
/// crate, warn for dependencies); `LOG_FORMAT=json` emits machine-readable
/// lines for a log pipeline instead of the human-readable default.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("tid_wayfarer=info,tower_http=warn,warn"));

    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(true);

    if std::env::var("LOG_FORMAT").unwrap_or_default().eq_ignore_ascii_case("json") {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Seal or unseal the outpost's key bundle, then exit.
///
/// Here rather than in a shell script because `openssl enc` cannot do AEAD
/// ciphers at all — so a script would have had to use CBC plus a hand-rolled
/// HMAC, producing a *second* implementation of a security-critical format
/// beside the one in `services::keyvault`. That is the same mistake as the
/// unauthenticated relay inbox: two implementations means one of them is the
/// weak one. This reuses the tested code, so there is exactly one format.
///
/// Needs no database, which is what lets a restore run against a stopped
/// outpost.
fn run_key_subcommand(cmd: &str) -> std::process::ExitCode {
    use tid_wayfarer::services::keyvault;

    let Ok(passphrase) = std::env::var("BACKUP_PASSPHRASE") else {
        eprintln!("BACKUP_PASSPHRASE is not set");
        return std::process::ExitCode::from(2);
    };

    match cmd {
        "keys-seal" => {
            let name = std::env::var("OUTPOST_NAME").unwrap_or_else(|_| "tid-wayfarer".into());
            match keyvault::bundle_from_disk(&name).and_then(|b| keyvault::seal(&b, &passphrase)) {
                Ok(sealed) => {
                    // stdout is the archive's contents; everything else goes to
                    // stderr so a redirect cannot capture a log line into the
                    // sealed file.
                    println!("{}", serde_json::to_string(&sealed).unwrap());
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("could not seal the key bundle: {e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        "keys-unseal" => {
            let mut raw = String::new();
            if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw) {
                eprintln!("could not read the sealed bundle from stdin: {e}");
                return std::process::ExitCode::FAILURE;
            }
            let sealed: keyvault::SealedBundle = match serde_json::from_str(&raw) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("that is not a sealed bundle: {e}");
                    return std::process::ExitCode::FAILURE;
                }
            };
            match keyvault::open(&sealed, &passphrase).and_then(|b| {
                let node = b.node_id.clone();
                keyvault::write_to_disk(&b).map(|_| node)
            }) {
                Ok(node_id) => {
                    eprintln!("restored identity {node_id}");
                    // The node id on stdout so the caller can check it against
                    // the manifest without parsing a log line.
                    println!("{node_id}");
                    std::process::ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::ExitCode::FAILURE
                }
            }
        }
        _ => unreachable!("caller matched the subcommand"),
    }
}

#[tokio::main]
async fn main() -> Result<(), sqlx::Error> {
    dotenv().ok();

    // Key custody runs before anything else is set up: a restore has to work
    // with no database reachable, which is the state an outpost is in when it
    // most needs restoring.
    if let Some(cmd) = std::env::args().nth(1) {
        if cmd == "keys-seal" || cmd == "keys-unseal" {
            std::process::exit(match run_key_subcommand(&cmd) {
                c if c == std::process::ExitCode::SUCCESS => 0,
                _ => 1,
            });
        }
    }

    init_tracing();

    // ---- DB ----
    let database_url = get_env_var("DATABASE_URL");
    let pool = PgPool::connect(&database_url).await?;

    let clients = HashMap::new();
    let jwt_secret = get_env_var("JWT_SECRET");
    info!("loaded JWT signing secret");

    // ---- Comms ----
    let auth_state = AuthState { clients, jwt_secret };
    let commsec_state = init_commsec_state();

    // ---- Node Identity ----
    let identity = load_or_generate_identity();
    info!(node_id = %identity.node_id, public_key = %identity.public_key, "node identity loaded");

    // ---- Shared State ----
    let state = AppState {
        db: pool.clone(),
        auth: auth_state,
        commsec: commsec_state,
        identity,
    };

    // ---- Background Daemons ----
    tokio::spawn(start_sync_daemon(state.clone()));
    tokio::spawn(run_telemetry_processor(state.clone()));
    tokio::spawn(run_command_engine(state.clone()));
    tokio::spawn(run_dtn_forwarder(state.clone()));
    // Keeps each peer's last reported stock on hand, so a dark site still
    // counts in the rollup instead of silently dropping out of the total.
    tokio::spawn(run_replication_daemon(state.clone()));
    // Polls commercial carriers for live parcels and closes the custody
    // chain when one is reported delivered.
    tokio::spawn(run_carrier_tracker(state.clone()));
    // auth_attempts is append-only on one of the busiest paths; without a
    // pruner it is the table that eventually fills the disk.
    tokio::spawn(run_auth_attempt_pruner(state.clone()));
    tokio::spawn(run_registration(state.clone()));
    tokio::spawn(run_blockchain_feeder(state.clone()));
    tokio::spawn(run_settlement_verifier(state.clone()));

    info!("autonomous ops engine active");

    // ---- CORS ----
    // ALLOWED_ORIGINS: comma-separated origin list. Unset or "*" keeps the
    // permissive dev default.
    let allowed_origins = env_or("ALLOWED_ORIGINS", "*");
    let cors = if allowed_origins.trim() == "*" {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        let origins: Vec<axum::http::HeaderValue> = allowed_origins
            .split(',')
            .filter_map(|o| o.trim().parse().ok())
            .collect();
        info!(origins = origins.len(), "CORS restricted");
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods(Any)
            .allow_headers(Any)
    };

    // ---- Static UI ----
    // The container copies the UI to /app/static, but a local `cargo run` has
    // no such path — which made every page 404 outside Docker. STATIC_DIR
    // overrides; otherwise fall back to the in-repo copy so the console is
    // reachable in development.
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| {
        for candidate in ["/app/static", "shared/static", "../shared/static"] {
            if std::path::Path::new(candidate).is_dir() {
                return candidate.to_string();
            }
        }
        "/app/static".to_string()
    });
    info!(dir = %static_dir, "serving UI at /ui and /static");
    let static_files = ServeDir::new(&static_dir);

    // Built by the shared constructor so the tests exercise the same guard,
    // lockdown layer and nesting the service actually runs.
    let api_routes = tid_wayfarer::app::api_router(state.clone())
        .route("/outpost", axum::routing::get(outpost_info));

    // ---- Final Application ----
    let app = Router::new()
        .nest_service("/static", static_files.clone())
        .nest_service("/ui", static_files)
        .route("/", get(root))
        .route("/healthz", get(health))
        .route("/api/health", get(health))
        .route("/health", get(|| async { "OK" }))
        .route("/metrics", get(metrics_handler))
        .nest("/api", api_routes)
        // Outermost so every request — including 404s and static files — gets
        // a request_id and lands in the counters.
        .layer(axum::middleware::from_fn(trace_requests))
        // Above the tracing layer, so a refused flood is still counted and
        // correlated, and below nothing else — refusing has to happen before
        // authentication and before the body is read, or the request has
        // already cost what the flood was aiming at. /health and /metrics are
        // exempted inside the layer, because a throttled liveness probe makes
        // an orchestrator restart a healthy outpost.
        .layer(axum::middleware::from_fn(
            tid_wayfarer::routes::auth_middleware::enforce_rate_limit,
        ))
        .layer(cors)
        .with_state(state);
        
    // Default 3000 matches docker-compose/Helm; PORT overrides for local
    // multi-node runs.
    let port: u16 = env_or("PORT", "3000").parse().unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(outpost = %get_env_var("OUTPOST_NAME"), %addr, "tid-wayfarer listening");

    // `into_make_service_with_connect_info` is what puts the peer address in
    // extensions. Without it the rate limiter sees no address, falls back to a
    // single shared bucket, and one noisy client throttles everyone.
    axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();

    Ok(())
}

