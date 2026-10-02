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

#[tokio::main]
async fn main() -> Result<(), sqlx::Error> {
    dotenv().ok();
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
        .layer(cors)
        .with_state(state);
        
    // Default 3000 matches docker-compose/Helm; PORT overrides for local
    // multi-node runs.
    let port: u16 = env_or("PORT", "3000").parse().unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    info!(outpost = %get_env_var("OUTPOST_NAME"), %addr, "tid-wayfarer listening");

    axum::serve(tokio::net::TcpListener::bind(addr).await?, app).await.unwrap();

    Ok(())
}

