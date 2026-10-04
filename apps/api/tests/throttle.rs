//! Rate limiting and the persistent login throttle, with the limits set tight
//! on purpose.
//!
//! Every other suite raises them — the limiter is per process and tests in one
//! binary share a bucket, so left at the defaults a suite that signs in a few
//! times throttles itself and the failure looks like a bug in whatever it was
//! actually testing. This file lowers them deliberately instead.
//!
//! The property that matters most is the ordering: a throttled sign-in must be
//! refused **before** the Argon2 verify. Doing it afterwards would still let an
//! attacker burn tens of milliseconds of CPU per guess, which is the
//! denial-of-service half of the problem and the reason an unlimited login
//! endpoint is dangerous even against a strong password.

mod common;

use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// Tighten the limits for this test, and keep them tight for assertions.
fn tight_auth(per_min: &str, burst: &str) {
    std::env::set_var("RATELIMIT_AUTH_PER_MIN", per_min);
    std::env::set_var("RATELIMIT_AUTH_BURST", burst);
}

fn tight_login_throttle(ip_failures: &str, account_failures: &str) {
    std::env::set_var("LOGIN_THROTTLE_IP_FAILURES", ip_failures);
    std::env::set_var("LOGIN_THROTTLE_ACCOUNT_FAILURES", account_failures);
}

/// Create a real account through the API, so the password hash is a real one.
async fn account(h: &common::Harness) -> (String, String) {
    let email = format!("{}@test.invalid", Uuid::new_v4());
    let password = "deck-plate-manifest-77";
    let (status, body) = h
        .call(
            "POST",
            "/api/local-auth/register",
            None,
            Some(json!({ "username": "throttle-subject", "email": email, "password": password })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
    (email, password.to_string())
}

/// A flood of sign-ins is refused, with a Retry-After.
///
/// `/api/local-auth/login` previously accepted unlimited attempts against an
/// Argon2 hash — expensive by design, so it was both a credential-stuffing
/// surface and a cheap way to saturate the outpost.
#[tokio::test]
async fn a_flood_of_sign_ins_is_refused() {
    let h = require_db!();
    tight_auth("60", "4");
    tight_login_throttle("100000", "100000"); // isolate the bucket from the counter

    let (email, password) = account(&h).await;
    let body = json!({ "email": email, "password": password });

    let mut refused = 0;
    let mut saw_429 = false;
    for _ in 0..12 {
        let (status, _) = h.call("POST", "/api/local-auth/login", None, Some(body.clone())).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            refused += 1;
        }
    }

    assert!(saw_429, "twelve sign-ins with a burst of four were all allowed");
    assert!(refused >= 4, "only {refused} of twelve were refused");
}

/// Reads are not caught by the auth limit.
///
/// Classes are independent so a burst of sign-ins cannot make the console
/// unusable — a limiter that turns one bad script into an outage is worse than
/// none.
#[tokio::test]
async fn exhausting_the_auth_limit_leaves_reads_working() {
    let h = require_db!();
    tight_auth("60", "2");
    std::env::set_var("RATELIMIT_READ_PER_MIN", "100000");
    std::env::set_var("RATELIMIT_READ_BURST", "100000");

    let (_id, token) = h.user("admin").await;

    // Drain the auth bucket.
    for _ in 0..8 {
        h.call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": "nobody@test.invalid", "password": "x" })),
        )
        .await;
    }

    let (status, _) = h.get("/api/people", &token).await;
    assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "a read was caught by the auth limit");
}

/// Health is never throttled.
///
/// A liveness probe that gets a 429 makes an orchestrator restart a perfectly
/// healthy outpost — the defence becoming the outage.
#[tokio::test]
async fn health_is_exempt() {
    let h = require_db!();
    std::env::set_var("RATELIMIT_READ_PER_MIN", "60");
    std::env::set_var("RATELIMIT_READ_BURST", "1");

    for i in 0..20 {
        let (status, _) = h.call("GET", "/api/health", None, None).await;
        assert_ne!(status, StatusCode::TOO_MANY_REQUESTS, "health throttled on attempt {i}");
    }
    std::env::set_var("RATELIMIT_READ_PER_MIN", "100000");
    std::env::set_var("RATELIMIT_READ_BURST", "100000");
}

/// Repeated failures lock an account out, and the lockout survives a restart.
///
/// This is the counter the token bucket cannot see: six attempts a minute,
/// indefinitely, sits inside every rate limit and will eventually find a weak
/// password. Persistence matters because an attacker able to make the process
/// restart should not get a fresh allowance — so the test asserts against a
/// *new* harness, which is a new in-memory limiter reading the same table.
#[tokio::test]
async fn repeated_failures_lock_the_account_and_survive_a_restart() {
    let h = require_db!();
    tight_auth("100000", "100000"); // the bucket must not be what refuses
    tight_login_throttle("100000", "3");

    let (email, password) = account(&h).await;

    for i in 0..3 {
        let (status, _) = h
            .call(
                "POST",
                "/api/local-auth/login",
                None,
                Some(json!({ "email": email, "password": "wrong-every-time" })),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {i} should be a plain refusal");
    }

    // The fourth is refused before the password is even looked at — so even the
    // *correct* password is turned away.
    let (status, body) = h
        .call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": email, "password": password })),
        )
        .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body:?}");

    // The refusal must not say which counter tripped: that would reveal
    // whether the account exists and how the defence is shaped.
    let msg = body.as_str().unwrap_or_default();
    assert!(msg.contains("too many sign-in attempts"), "{msg}");
    assert!(!msg.contains("account") && !msg.contains("address"), "the counter leaked: {msg}");

    // A fresh harness is a fresh in-process limiter over the same table. The
    // lockout has to still be there.
    let restarted = require_db!();
    // `harness()` raises the limits for every other suite, so re-tighten them
    // after building it — otherwise this would assert against the generous
    // defaults and pass for the wrong reason.
    tight_login_throttle("100000", "3");
    let (status, _) = restarted
        .call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": email, "password": password })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "the lockout was forgotten when the process restarted"
    );
}

/// A successful sign-in clears the account's failures.
///
/// Without this, three forgotten-password attempts would keep somebody locked
/// out for the rest of the window *after* they remembered it — a support call
/// generated by the defence rather than by an attacker.
#[tokio::test]
async fn a_successful_sign_in_clears_the_failures() {
    let h = require_db!();
    tight_auth("100000", "100000");
    tight_login_throttle("100000", "4");

    let (email, password) = account(&h).await;

    // Three near-misses, one short of the lockout.
    for _ in 0..3 {
        h.call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": email, "password": "nearly" })),
        )
        .await;
    }

    let (status, _) = h
        .call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": email, "password": password })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "the right password should still work");

    // The slate is clean, so the next three mistakes do not lock them out.
    for i in 0..3 {
        let (status, _) = h
            .call(
                "POST",
                "/api/local-auth/login",
                None,
                Some(json!({ "email": email, "password": "wrong" })),
            )
            .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "failure {i} after a success should not be throttled — the count did not reset"
        );
    }
}

/// Attempts against an unknown account are counted too.
///
/// If they were not, an attacker could tell registered addresses from
/// unregistered ones by watching which ones start getting throttled — turning
/// the defence into an enumeration oracle.
#[tokio::test]
async fn attempts_against_an_unknown_account_are_counted() {
    let h = require_db!();
    tight_auth("100000", "100000");
    tight_login_throttle("100000", "2");

    let nobody = format!("{}@test.invalid", Uuid::new_v4());

    for _ in 0..2 {
        let (status, _) = h
            .call(
                "POST",
                "/api/local-auth/login",
                None,
                Some(json!({ "email": nobody, "password": "x" })),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let (status, _) = h
        .call(
            "POST",
            "/api/local-auth/login",
            None,
            Some(json!({ "email": nobody, "password": "x" })),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "an unknown account was not throttled, which distinguishes it from a real one"
    );

    // And the attempts are on record, which is what makes that true.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM auth_attempts WHERE email = $1")
        .bind(&nobody)
        .fetch_one(&h.db)
        .await
        .unwrap();
    assert!(n >= 2, "attempts against an unknown account were not recorded");
}

/// Changing a password is throttled like a sign-in.
///
/// It verifies the current password, so it guesses just as well as a login
/// does — and would otherwise be the unthrottled way in.
#[tokio::test]
async fn changing_a_password_is_throttled_too() {
    let h = require_db!();
    tight_auth("60", "3");

    let (_id, token) = h.user("user").await;

    let mut saw_429 = false;
    for _ in 0..10 {
        let (status, _) = h
            .post(
                "/api/me/password",
                &token,
                json!({ "current_password": "guess", "new_password": "something-long-enough" }),
            )
            .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            break;
        }
    }
    assert!(saw_429, "the password-change path is not rate limited");
}
