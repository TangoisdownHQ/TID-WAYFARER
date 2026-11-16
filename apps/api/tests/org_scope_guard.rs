//! A tripwire against cross-tenant reads.
//!
//! Two leaks of the same shape have now been found by hand: the lots module
//! ignored the caller's organisation (Oct 2026), and the documents module
//! served any order's papers to any account (fixed alongside this test). Both
//! were a handler reading one tenant's private holdings with no check that the
//! caller was entitled to them.
//!
//! This test does not prove scoping is *correct* — the runtime boundary tests
//! do that. It forces a *conscious* review: if a route module touches a
//! tenant-holdings table and references no access-control marker at all, it
//! fails here, so the omission is caught at the point it is introduced rather
//! than by the next audit. It is deliberately coarse and deliberately source-
//! level; a passing module still owes a runtime boundary test.

use std::fs;
use std::path::Path;

/// Tables that hold one tenant's private data. The marketplace tables
/// (orders/bids/fulfillments) are intentionally cross-org and are not listed —
/// a document or a bid legitimately spans two organisations.
const TENANT_TABLES: &[&str] = &["inventory_lots"];

/// Evidence that a module enforces access control of some kind: organisation
/// scoping, order-party authorization, or owner scoping. Presence is necessary,
/// not sufficient — hence "review", not "proof".
const SCOPE_MARKERS: &[&str] =
    &["caller_org", "own_org", "org_scope", "require_order_party", "owner_id"];

#[test]
fn every_module_touching_tenant_holdings_enforces_some_scope() {
    let routes = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes");
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&routes).expect("read routes dir") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let src = fs::read_to_string(&path).expect("read source");
        let touches_holdings = TENANT_TABLES.iter().any(|t| src.contains(t));
        if !touches_holdings {
            continue;
        }
        let has_scope = SCOPE_MARKERS.iter().any(|m| src.contains(m));
        if !has_scope {
            offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    assert!(
        offenders.is_empty(),
        "route modules query tenant-holdings tables {TENANT_TABLES:?} but reference no \
         access-control marker {SCOPE_MARKERS:?} — review each for a cross-tenant leak \
         before shipping: {offenders:?}"
    );
}
