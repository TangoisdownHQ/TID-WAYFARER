//! Remembering failed sign-ins, so a patient guess is stopped too.
//!
//! The token bucket in [`crate::services::ratelimit`] stops a flood. It does
//! not stop patience: six attempts a minute, indefinitely, from a rotating set
//! of addresses, sits inside every rate limit and will eventually find a weak
//! password. Seeing that requires remembering failures — and remembering them
//! across restarts, because an attacker who can make the process restart should
//! not get a fresh allowance.
//!
//! # Checked before the hash, not after
//!
//! The whole point is to refuse *before* spending an Argon2 verify. Doing it
//! afterwards would still let an attacker burn tens of milliseconds of CPU per
//! attempt, which is the denial-of-service half of the problem and the reason
//! an unlimited login endpoint is dangerous even against a strong password.
//!
//! # Two counters, opposite shapes
//!
//! | Attack | Shape | Counter |
//! |---|---|---|
//! | brute force | one account, many guesses | per account, any address |
//! | credential stuffing | many accounts, one guess each | per address, any account |
//!
//! A per-account counter lets somebody lock a known email out deliberately.
//! That cost is real and is why the thresholds differ: the per-address window
//! is tight and short so stuffing dies quickly, and the per-account one is
//! looser and longer so normal mistyping never trips it and a targeted lockout
//! is slow, noisy, and self-limiting. The proper fix is a second factor, which
//! does not exist here yet.

use std::net::IpAddr;

use sqlx::PgPool;

/// Failures from one address before it is refused, and the window.
fn per_ip_limits() -> (i64, i64) {
    (
        env_i64("LOGIN_THROTTLE_IP_FAILURES", 10),
        env_i64("LOGIN_THROTTLE_IP_WINDOW_SECS", 900),
    )
}

/// Failures against one account before it is refused, and the window.
///
/// Deliberately a higher count over a longer window than the per-address rule:
/// this is the counter somebody could abuse to lock a colleague out, so it has
/// to be hard to trip by accident and slow to trip on purpose.
fn per_account_limits() -> (i64, i64) {
    (
        env_i64("LOGIN_THROTTLE_ACCOUNT_FAILURES", 20),
        env_i64("LOGIN_THROTTLE_ACCOUNT_WINDOW_SECS", 3600),
    )
}

fn env_i64(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Why a sign-in was refused before the password was even checked.
pub struct Throttled {
    pub retry_after_secs: i64,
    /// For the log. Never returned to the caller — telling somebody *which*
    /// counter they tripped tells them whether the account exists and how the
    /// defence is shaped.
    pub reason: &'static str,
}

/// Should this attempt be refused outright?
///
/// Fails **open** on a database error. A login endpoint that stops working
/// because the throttle table is unreadable is a self-inflicted outage; the
/// token bucket is still in front of it, so the floor does not disappear.
pub async fn check(db: &PgPool, email: &str, ip: Option<IpAddr>) -> Option<Throttled> {
    let (ip_max, ip_window) = per_ip_limits();
    let (acct_max, acct_window) = per_account_limits();

    if let Some(ip) = ip {
        let failures: i64 = match sqlx::query_scalar(
            "SELECT count(*) FROM auth_attempts \
             WHERE NOT succeeded AND ip = $1 AND at > NOW() - make_interval(secs => $2)",
        )
        .bind(ip.to_string())
        .bind(ip_window as f64)
        .fetch_one(db)
        .await
        {
            Ok(n) => n,
            Err(e) => {
                tracing::error!(error = %e, "login throttle could not read attempts; allowing");
                return None;
            }
        };

        if failures >= ip_max {
            return Some(Throttled { retry_after_secs: ip_window, reason: "address" });
        }
    }

    let failures: i64 = match sqlx::query_scalar(
        "SELECT count(*) FROM auth_attempts \
         WHERE NOT succeeded AND email = $1 AND at > NOW() - make_interval(secs => $2)",
    )
    .bind(email)
    .bind(acct_window as f64)
    .fetch_one(db)
    .await
    {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error = %e, "login throttle could not read attempts; allowing");
            return None;
        }
    };

    if failures >= acct_max {
        return Some(Throttled { retry_after_secs: acct_window, reason: "account" });
    }

    None
}

/// Record the outcome.
///
/// Successes are recorded too, and not only for an audit trail: `users
/// .last_login_at` says when somebody last got in, and this says how many
/// times somebody failed first, which is the difference between a forgotten
/// password and an attempt on the account.
pub async fn record(db: &PgPool, email: &str, ip: Option<IpAddr>, succeeded: bool) {
    if let Err(e) = sqlx::query(
        "INSERT INTO auth_attempts (email, ip, succeeded) VALUES ($1, $2, $3)",
    )
    .bind(email)
    .bind(ip.map(|i| i.to_string()))
    .bind(succeeded)
    .execute(db)
    .await
    {
        // Best effort: failing to record must not fail the sign-in. The bucket
        // is still in front.
        tracing::warn!(error = %e, "could not record a sign-in attempt");
    }
}

/// Clear an account's failures after it signs in successfully.
///
/// Without this, twenty forgotten-password attempts would keep somebody locked
/// out for the rest of the window *after* they remembered it, which is a
/// support call generated by the defence rather than by an attacker.
pub async fn clear(db: &PgPool, email: &str) {
    if let Err(e) = sqlx::query(
        "DELETE FROM auth_attempts WHERE email = $1 AND NOT succeeded",
    )
    .bind(email)
    .execute(db)
    .await
    {
        tracing::warn!(error = %e, "could not clear sign-in failures");
    }
}

/// Drop attempts older than the longest window, on a timer.
///
/// `auth_attempts` is append-only on one of the busiest paths in the
/// application, so without this it is the table that eventually fills the disk.
pub async fn run_pruner(state: crate::AppState) {
    use tokio::time::{sleep, Duration};

    // Nothing to prune on a fresh outpost, and the first sweep should not race
    // the migrations.
    sleep(Duration::from_secs(120)).await;

    loop {
        // Keep well past the longest window so a lockout is never cut short by
        // housekeeping — the whole value of this table is that it remembers.
        let keep_secs = per_account_limits().1.max(per_ip_limits().1) * 4;

        match sqlx::query("DELETE FROM auth_attempts WHERE at < NOW() - make_interval(secs => $1)")
            .bind(keep_secs as f64)
            .execute(&state.db)
            .await
        {
            Ok(r) if r.rows_affected() > 0 => {
                tracing::debug!(rows = r.rows_affected(), "pruned old sign-in attempts")
            }
            Err(e) => tracing::warn!(error = %e, "could not prune sign-in attempts"),
            _ => {}
        }

        sleep(Duration::from_secs(3600)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-account rule must be harder to trip than the per-address one.
    ///
    /// This is the shape that keeps a targeted lockout slow: anyone can fail
    /// against a known email, so that counter has to tolerate more and over a
    /// longer window than the one catching credential stuffing.
    #[test]
    fn the_account_counter_is_looser_than_the_address_counter() {
        for v in [
            "LOGIN_THROTTLE_IP_FAILURES", "LOGIN_THROTTLE_IP_WINDOW_SECS",
            "LOGIN_THROTTLE_ACCOUNT_FAILURES", "LOGIN_THROTTLE_ACCOUNT_WINDOW_SECS",
        ] {
            std::env::remove_var(v);
        }
        let (ip_max, ip_window) = per_ip_limits();
        let (acct_max, acct_window) = per_account_limits();

        assert!(acct_max > ip_max, "an account must tolerate more failures than an address");
        assert!(acct_window > ip_window, "and over a longer window");
    }

    /// Both counters are positive. A zero would mean the first failed attempt
    /// locks the account out forever, which is an outage shaped like a defence.
    #[test]
    fn the_thresholds_are_never_zero() {
        std::env::set_var("LOGIN_THROTTLE_IP_FAILURES", "0");
        std::env::set_var("LOGIN_THROTTLE_ACCOUNT_FAILURES", "-5");
        assert!(per_ip_limits().0 > 0, "a zero threshold would lock out on the first try");
        assert!(per_account_limits().0 > 0);
        std::env::remove_var("LOGIN_THROTTLE_IP_FAILURES");
        std::env::remove_var("LOGIN_THROTTLE_ACCOUNT_FAILURES");
    }
}
