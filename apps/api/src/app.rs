//! Router construction, shared by the binary and the integration tests.
//!
//! This lived inline in `main.rs`, which meant a test could exercise handlers
//! individually but never the thing that actually runs: the guard, the
//! lockdown layer and the nesting that decides what is reachable and under
//! what authority. Several of the defects found in this codebase — a route
//! group declared but never mounted, a page hitting an endpoint that did not
//! exist, middleware seeing a rewritten path — were only visible from the
//! assembled application.

use axum::{routing::get, Router};

use crate::routes::{
    asset_logistics::{asset_extras_routes, kit_routes},
    assets::asset_routes,
    assets_fleet::fleet_asset_routes,
    auth::auth_routes,
    auth_middleware::{enforce_lockdown, require_auth},
    blockchain::blockchain_routes,
    bodies::body_routes,
    capsules::capsule_routes,
    catalogue::catalogue_routes,
    commands::command_routes,
    commsec::commsec_routes,
    compliance::compliance_routes,
    custody::custody_routes,
    dashboard::dashboard_routes,
    documents::document_routes,
    dtn::dtn_routes,
    fabric::fabric_routes,
    fleet_map::fleet_map_routes,
    inventory::inventory_routes,
    local_auth::local_auth_routes,
    lots::{inventory_lot_routes, lot_routes},
    map::map_routes,
    me::me_routes,
    movements::movement_routes,
    nodes::node_routes,
    nodes_sync::node_sync_routes,
    ops::ops_routes,
    orders::{fulfillment_routes, order_routes},
    orgs::org_routes,
    packages::package_routes,
    peers::peer_routes,
    rates::rate_routes,
    request_trace::trace_requests,
    rollup::rollup_routes,
    rules::rules_routes,
    search::search_routes,
    supplylink::supplylink_routes,
    user::user_routes,
};
use crate::AppState;

/// Everything under `/api`, with the auth guard and lockdown layer applied
/// exactly as the running service applies them.
pub fn api_router(state: AppState) -> Router<AppState> {
    let guard = axum::middleware::from_fn_with_state(state.clone(), require_auth);

    // Open: how a caller obtains a token, plus outpost identity for probes.
    let open_routes = Router::new()
        .nest("/auth", auth_routes())
        .nest("/local-auth", local_auth_routes());

    // Guarded at the router level. Groups whose handlers already demand
    // AuthenticatedUser/AdminUser keep enforcing that on top.
    let guarded_routes = Router::new()
        .nest("/commsec", commsec_routes())
        .nest("/inventory", inventory_routes().merge(inventory_lot_routes()))
        .nest("/me", me_routes())
        .nest("/packages", package_routes())
        .nest("/assets", asset_routes().merge(asset_extras_routes()))
        .nest("/kits", kit_routes())
        .nest("/orders", order_routes())
        .nest("/fulfillments", fulfillment_routes())
        .nest("/supplylink", supplylink_routes())
        .nest("/rollup", rollup_routes())
        .nest("/lots", lot_routes())
        .nest("/capsules", capsule_routes())
        .nest("/compliance", compliance_routes())
        .nest("/custody", custody_routes())
        .nest("/rates", rate_routes())
        .nest("/movements", movement_routes())
        .nest("/documents", document_routes())
        .nest("/search", search_routes())
        .nest("/orgs", org_routes())
        .nest("/catalogue", catalogue_routes())
        .nest("/fleet", fleet_asset_routes())
        .nest("/users", user_routes())
        .nest("/nodes", node_routes())
        .nest("/nodes-sync", node_sync_routes())
        .nest("/commands", command_routes())
        .nest("/bc", blockchain_routes())
        .nest("/dashboard", dashboard_routes())
        .nest("/rules", rules_routes())
        .nest("/peers", peer_routes())
        .nest("/ops", ops_routes())
        .nest("/dtn", dtn_routes())
        .nest("/fabric", fabric_routes())
        .nest("/map/fleet", fleet_map_routes())
        .nest("/map", map_routes())
        .nest("/bodies", body_routes())
        // Lockdown sits inside the auth guard: only authenticated callers get
        // far enough to be told the outpost is locked.
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), enforce_lockdown))
        .route_layer(guard);

    open_routes.merge(guarded_routes)
}

/// The application as the tests exercise it: every API route, the guard, and
/// the request-id layer. The binary adds static files, CORS and `/metrics` on
/// top — none of which change an authorisation decision.
pub fn test_router(state: AppState) -> Router {
    Router::new()
        .nest("/api", api_router(state.clone()))
        .route("/api/health", get(|| async { "OK" }))
        .layer(axum::middleware::from_fn(trace_requests))
        .with_state(state)
}
