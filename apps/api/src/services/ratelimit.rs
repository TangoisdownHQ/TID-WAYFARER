//! Rate limiting.
//!
//! There was none. `/api/local-auth/login` would accept unlimited attempts
//! against an Argon2 hash — which is CPU-expensive *by design*, so it was both
//! a credential-stuffing surface and a cheap way to saturate the outpost. A few
//! hundred concurrent login attempts is a denial of service, not just a guess.
//!
//! # Two layers, because they defend different things
//!
//! **This module** is an in-process token bucket keyed by client address. It
//! stops floods: of logins, of writes, of anything. Cheap, no database, no new
//! dependency.
//!
//! **[`crate::services::login_throttle`]** is persistent and keyed by *account*.
//! It stops a slow, patient guess against one password — the attack a bucket
//! that refills every second does not see.
//!
//! # What in-process means, stated plainly
//!
//! Limits are per process. An outpost runs one, so for the deployment this is
//! aimed at they are per outpost. Behind two replicas an attacker gets double
//! the allowance. That is a real limitation and the right trade here: a shared
//! limiter means a round trip per request, on every request, to defend against
//! something that has not happened yet. The persistent login throttle covers
//! the case where being wrong actually costs something.
//!
//! # Trusting a forwarded address is opt-in
//!
//! If `X-Forwarded-For` were honoured by default, anyone could send a different
//! one per request and have unlimited buckets — the limiter would be decorative
//! and would look like it was working. So the socket address is used unless
//! `TRUST_PROXY_HEADERS=true`, which is correct only when something you control
//! sets that header and strips any inbound copy.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// What is being limited. Different classes because the costs differ by orders
/// of magnitude: an Argon2 verify is tens of milliseconds of CPU, a GET is a
/// query.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Class {
    /// Sign-in and sign-up. Tight, because each attempt is expensive to serve
    /// and valuable to an attacker.
    Auth,
    /// Anything that changes state.
    Write,
    /// Reads.
    Read,
    /// Signed peer traffic. Generous: a rollup fans out, a DTN forwarder
    /// retries, and limiting peers the way we limit browsers would break the
    /// fabric this exists to serve.
    Fabric,
}

impl Class {
    /// (requests per minute, burst). Burst is what absorbs a page that fires
    /// six requests on load; the rate is what bounds sustained abuse.
    fn limits(self) -> (f64, f64) {
        let env = |name: &str, default: f64| -> f64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .unwrap_or(default)
        };
        match self {
            // Looser than it first looks, on purpose.
            //
            // The first draft of this was 10/min with a burst of 5, reasoning
            // that no person signs in more than that. But the bucket is keyed
            // by *address*, and an office behind one NAT egress has twenty
            // people arriving at 09:00 — they would throttle each other, and
            // the outage would look like a login bug.
            //
            // The two defences do different jobs and the first draft conflated
            // them. Stopping a *guess* is `login_throttle`: per account,
            // persistent, counted across restarts. Stopping a *flood* is this
            // bucket, and a flood is far more than sixty attempts a minute
            // from one address. So this is set where it does not hurt a shared
            // egress, and the account counter does the security work.
            Class::Auth => (env("RATELIMIT_AUTH_PER_MIN", 60.0), env("RATELIMIT_AUTH_BURST", 20.0)),
            Class::Write => (env("RATELIMIT_WRITE_PER_MIN", 120.0), env("RATELIMIT_WRITE_BURST", 40.0)),
            Class::Read => (env("RATELIMIT_READ_PER_MIN", 600.0), env("RATELIMIT_READ_BURST", 200.0)),
            Class::Fabric => (env("RATELIMIT_FABRIC_PER_MIN", 1200.0), env("RATELIMIT_FABRIC_BURST", 400.0)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Class::Auth => "auth",
            Class::Write => "write",
            Class::Read => "read",
            Class::Fabric => "fabric",
        }
    }
}

/// Who is being limited.
///
/// Fabric traffic is keyed by the node id it *claims* rather than its address,
/// because peers sit behind stable addresses and one busy peer should not
/// exhaust another's allowance. The claim is unverified at this point — the
/// signature has not been checked yet — but forging it only moves an attacker
/// between buckets, which is no better than rotating addresses, and eviction
/// bounds the map either way.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Subject {
    Addr(IpAddr),
    Node(String),
    /// No address available — a unix socket, or a test harness. Shared bucket:
    /// unkeyable traffic should not get an unlimited one.
    Unknown,
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Verdict, with enough to tell the caller when to come back.
pub struct Decision {
    pub allowed: bool,
    /// Seconds until one token is available. Becomes `Retry-After`, so a
    /// well-behaved client backs off by the right amount instead of guessing.
    pub retry_after_secs: u64,
}

struct Limiter {
    buckets: Mutex<HashMap<(Class, Subject), Bucket>>,
    last_evicted: Mutex<Instant>,
}

fn limiter() -> &'static Limiter {
    static L: OnceLock<Limiter> = OnceLock::new();
    L.get_or_init(|| Limiter {
        buckets: Mutex::new(HashMap::new()),
        last_evicted: Mutex::new(Instant::now()),
    })
}

/// How long an idle bucket is kept.
///
/// Eviction is not housekeeping, it is the defence against the limiter itself
/// becoming the vulnerability: an attacker rotating source addresses would
/// otherwise add a map entry per request forever, and the process would run out
/// of memory while dutifully refusing their requests.
const IDLE_EVICT: Duration = Duration::from_secs(600);
const EVICT_EVERY: Duration = Duration::from_secs(60);

/// Take one token, or refuse.
pub fn check(class: Class, subject: Subject) -> Decision {
    let (per_min, burst) = class.limits();
    let refill_per_sec = per_min / 60.0;
    let now = Instant::now();

    maybe_evict(now, burst);

    let mut buckets = match limiter().buckets.lock() {
        Ok(b) => b,
        // A poisoned mutex means another thread panicked while holding it.
        // Failing open is the right call: a limiter that takes the outpost
        // offline because of its own bug is worse than one that stops
        // limiting. Logged loudly so it is not silent.
        Err(_) => {
            tracing::error!("rate limiter lock poisoned; allowing the request");
            return Decision { allowed: true, retry_after_secs: 0 };
        }
    };

    let bucket = buckets
        .entry((class, subject))
        .or_insert(Bucket { tokens: burst, last: now });

    let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
    bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(burst);
    bucket.last = now;

    if bucket.tokens >= 1.0 {
        bucket.tokens -= 1.0;
        Decision { allowed: true, retry_after_secs: 0 }
    } else {
        let deficit = 1.0 - bucket.tokens;
        Decision {
            allowed: false,
            // At least one second: a Retry-After of 0 invites an immediate
            // retry, which is the behaviour being limited.
            retry_after_secs: ((deficit / refill_per_sec).ceil() as u64).max(1),
        }
    }
}

/// Drop buckets nobody has touched, on a timer rather than per request.
fn maybe_evict(now: Instant, burst: f64) {
    let Ok(mut last) = limiter().last_evicted.lock() else { return };
    if now.saturating_duration_since(*last) < EVICT_EVERY {
        return;
    }
    *last = now;
    drop(last);

    if let Ok(mut buckets) = limiter().buckets.lock() {
        let before = buckets.len();
        // Keep anything recently seen, and anything still short of tokens —
        // evicting a throttled bucket would hand its owner a fresh full one,
        // which is exactly the wrong moment to forget about them.
        buckets.retain(|_, b| {
            now.saturating_duration_since(b.last) < IDLE_EVICT || b.tokens < burst
        });
        let dropped = before - buckets.len();
        if dropped > 0 {
            tracing::debug!(dropped, remaining = buckets.len(), "evicted idle rate-limit buckets");
        }
    }
}

/// Whether a forwarded client address may be believed.
pub fn trust_proxy_headers() -> bool {
    matches!(
        std::env::var("TRUST_PROXY_HEADERS").unwrap_or_default().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Work out who to charge for this request.
///
/// `X-Forwarded-For` is only consulted when explicitly trusted. Honouring it by
/// default would let anyone send a fresh value per request and get an unlimited
/// number of buckets — the limiter would appear to work and would do nothing.
pub fn subject_for(
    headers: &axum::http::HeaderMap,
    peer: Option<SocketAddr>,
) -> Subject {
    // Signed peer traffic is keyed by the node it claims to be, so one busy
    // outpost cannot consume another's allowance.
    if let Some(node) = headers
        .get(crate::services::fabric_auth::HEADER_NODE_ID)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 64)
    {
        if headers.contains_key(crate::services::fabric_auth::HEADER_SIGNATURE) {
            return Subject::Node(node.to_string());
        }
    }

    if trust_proxy_headers() {
        if let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            // Leftmost is the original client when the chain is written by
            // proxies you control.
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .and_then(|v| v.parse::<IpAddr>().ok())
        {
            return Subject::Addr(ip);
        }
    }

    peer.map(|p| Subject::Addr(p.ip())).unwrap_or(Subject::Unknown)
}

/// Which class a request falls into, from its path and method.
pub fn classify(method: &axum::http::Method, path: &str, headers: &axum::http::HeaderMap) -> Class {
    // Checked first: a peer's rollup fan-out is a burst of reads that would
    // otherwise look like abuse.
    if headers.contains_key(crate::services::fabric_auth::HEADER_SIGNATURE) {
        return Class::Fabric;
    }

    // Everything that can be used to guess a credential, including the paths
    // that exist to *change* one — a password-change endpoint verifies the old
    // password, so it guesses just as well as a login.
    if path.contains("/local-auth/")
        || path.contains("/auth/")
        || path.ends_with("/password")
        || path.contains("/node-succession/")
    {
        return Class::Auth;
    }

    match *method {
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS => {
            Class::Read
        }
        _ => Class::Write,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, Method};

    fn addr(n: u8) -> Subject {
        Subject::Addr(IpAddr::from([10, 0, 0, n]))
    }

    /// A burst is allowed, then the bucket is empty.
    #[test]
    fn a_burst_is_allowed_and_then_refused() {
        std::env::set_var("RATELIMIT_WRITE_PER_MIN", "60");
        std::env::set_var("RATELIMIT_WRITE_BURST", "3");
        let who = addr(1);

        for i in 0..3 {
            assert!(check(Class::Write, who.clone()).allowed, "request {i} should pass");
        }
        let refused = check(Class::Write, who);
        assert!(!refused.allowed, "the fourth should be refused");
        assert!(refused.retry_after_secs >= 1, "Retry-After must not be zero");
    }

    /// One client's flood does not refuse another's request.
    ///
    /// The failure this guards against is a limiter keyed on something too
    /// coarse, which turns one abuser into an outage for everybody.
    #[test]
    fn one_client_cannot_starve_another() {
        std::env::set_var("RATELIMIT_WRITE_PER_MIN", "60");
        std::env::set_var("RATELIMIT_WRITE_BURST", "2");

        let noisy = addr(2);
        while check(Class::Write, noisy.clone()).allowed {}

        assert!(check(Class::Write, addr(3)).allowed, "a different client was caught in it");
    }

    /// Classes are independent: exhausting writes must not lock someone out of
    /// reading, or a single bad script makes the console unusable.
    #[test]
    fn exhausting_one_class_leaves_the_others_alone() {
        std::env::set_var("RATELIMIT_WRITE_PER_MIN", "60");
        std::env::set_var("RATELIMIT_WRITE_BURST", "1");
        std::env::set_var("RATELIMIT_READ_BURST", "5");

        let who = addr(4);
        while check(Class::Write, who.clone()).allowed {}
        assert!(check(Class::Read, who).allowed);
    }

    /// Tokens come back.
    #[test]
    fn tokens_refill_over_time() {
        // 600/min is 10/sec, so a 50ms sleep returns half a token and 150ms
        // returns more than one.
        std::env::set_var("RATELIMIT_WRITE_PER_MIN", "600");
        std::env::set_var("RATELIMIT_WRITE_BURST", "1");
        let who = addr(5);

        assert!(check(Class::Write, who.clone()).allowed);
        assert!(!check(Class::Write, who.clone()).allowed);
        std::thread::sleep(Duration::from_millis(150));
        assert!(check(Class::Write, who).allowed, "the bucket should have refilled");
    }

    /// Auth is tighter than reading, but not so tight that a shared egress
    /// throttles itself.
    ///
    /// The bucket's job is floods, not guesses — `login_throttle` does guesses,
    /// per account and persistently. An office behind one NAT address has
    /// twenty people signing in at 09:00, and a limit that cannot absorb that
    /// produces an outage which looks like a login bug.
    #[test]
    fn auth_is_tighter_than_reading_but_still_absorbs_an_office() {
        for v in ["RATELIMIT_AUTH_PER_MIN", "RATELIMIT_AUTH_BURST",
                  "RATELIMIT_READ_PER_MIN", "RATELIMIT_READ_BURST"] {
            std::env::remove_var(v);
        }
        let (auth_rate, auth_burst) = Class::Auth.limits();
        assert!(auth_rate < Class::Read.limits().0, "auth should be tighter than reads");
        assert!(auth_burst >= 20.0, "a burst of {auth_burst} cannot absorb a shift change");
    }

    /// A forwarded address is ignored unless it is explicitly trusted.
    ///
    /// This is the one that matters. Honouring the header by default would let
    /// anyone send a fresh value per request, get an unlimited number of
    /// buckets, and leave a limiter that looks like it is working.
    #[test]
    fn a_forwarded_address_is_not_believed_by_default() {
        std::env::remove_var("TRUST_PROXY_HEADERS");
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", HeaderValue::from_static("203.0.113.9"));

        let peer: SocketAddr = "198.51.100.4:40000".parse().unwrap();
        assert_eq!(
            subject_for(&headers, Some(peer)),
            Subject::Addr(IpAddr::from([198, 51, 100, 4])),
            "the spoofable header was used instead of the socket address"
        );

        std::env::set_var("TRUST_PROXY_HEADERS", "true");
        assert_eq!(
            subject_for(&headers, Some(peer)),
            Subject::Addr(IpAddr::from([203, 0, 113, 9])),
        );
        std::env::remove_var("TRUST_PROXY_HEADERS");
    }

    /// With no address at all, traffic shares one bucket rather than escaping.
    #[test]
    fn unkeyable_traffic_does_not_get_a_free_pass() {
        std::env::remove_var("TRUST_PROXY_HEADERS");
        assert_eq!(subject_for(&HeaderMap::new(), None), Subject::Unknown);
    }

    /// Signed peer traffic is keyed per node, so one busy outpost does not
    /// consume another's allowance.
    #[test]
    fn signed_peers_get_their_own_buckets() {
        let mut headers = HeaderMap::new();
        headers.insert(crate::services::fabric_auth::HEADER_NODE_ID,
                       HeaderValue::from_static("peer-a"));
        headers.insert(crate::services::fabric_auth::HEADER_SIGNATURE,
                       HeaderValue::from_static("sig"));
        assert_eq!(subject_for(&headers, None), Subject::Node("peer-a".into()));

        // A node id without a signature is just a claim; it must not win a
        // private bucket.
        let mut unsigned = HeaderMap::new();
        unsigned.insert(crate::services::fabric_auth::HEADER_NODE_ID,
                        HeaderValue::from_static("peer-a"));
        assert_eq!(subject_for(&unsigned, None), Subject::Unknown);
    }

    #[test]
    fn requests_are_classified_by_what_they_cost() {
        let empty = HeaderMap::new();
        assert_eq!(classify(&Method::POST, "/api/local-auth/login", &empty), Class::Auth);
        assert_eq!(classify(&Method::POST, "/api/auth/google", &empty), Class::Auth);
        // Changing a password verifies the old one, so it guesses as well as a
        // login does.
        assert_eq!(classify(&Method::POST, "/api/me/password", &empty), Class::Auth);
        // Succession takes no credential, so it must not be a free-for-all.
        assert_eq!(classify(&Method::POST, "/api/node-succession/abc", &empty), Class::Auth);

        assert_eq!(classify(&Method::GET, "/api/inventory", &empty), Class::Read);
        assert_eq!(classify(&Method::POST, "/api/orders", &empty), Class::Write);
        assert_eq!(classify(&Method::DELETE, "/api/addresses/x", &empty), Class::Write);

        // Signed peer traffic is fabric whatever it is doing, because a rollup
        // fan-out and a DTN retry both look like abuse otherwise.
        let mut peer = HeaderMap::new();
        peer.insert(crate::services::fabric_auth::HEADER_SIGNATURE,
                    HeaderValue::from_static("sig"));
        assert_eq!(classify(&Method::POST, "/api/dtn/receive", &peer), Class::Fabric);
        assert_eq!(classify(&Method::GET, "/api/rollup/local", &peer), Class::Fabric);
    }

    /// A throttled bucket is not evicted.
    ///
    /// Forgetting one would hand its owner a fresh full bucket, which is
    /// precisely the wrong moment to lose track of them.
    #[test]
    fn eviction_does_not_forgive_an_active_offender() {
        std::env::set_var("RATELIMIT_WRITE_PER_MIN", "60");
        std::env::set_var("RATELIMIT_WRITE_BURST", "2");
        let who = addr(9);
        while check(Class::Write, who.clone()).allowed {}

        // Force an eviction sweep.
        if let Ok(mut last) = limiter().last_evicted.lock() {
            *last = Instant::now() - Duration::from_secs(120);
        }
        let d = check(Class::Write, who);
        assert!(!d.allowed, "an offender was handed a fresh bucket by eviction");
    }
}
