//! Settlement verification for SupplyLink fulfillments.
//!
//! A settlement used to be a string: `mark_settled` accepted any non-empty
//! `settlement_tx` and flipped the order to 'settled'. This module makes
//! 'settled' mean "someone was actually paid".
//!
//! Two properties shape the design:
//!
//! 1. **It must work offline.** An outpost can be a long way from a usable
//!    link, and a settlement recorded during a blackout still has to be
//!    recorded. So the tx is stored immediately with status 'settling', and a
//!    daemon promotes it once confirmed — the same store-and-forward pattern
//!    as DTN and command delivery.
//! 2. **Some checks need no network at all.** A malformed signature can be
//!    rejected on the spot, which is what stops `"abc"` from settling an
//!    order even on a fully disconnected outpost.

use sqlx::Row;
use tokio::time::{sleep, Duration};
use uuid::Uuid;

use crate::AppState;

/// Poll interval when there is nothing pending.
const IDLE_SECS: u64 = 15;
/// Cap on the retry backoff for an unreachable RPC.
const MAX_BACKOFF_SECS: f64 = 3600.0;
/// Give up re-checking after this many attempts and mark it unverifiable.
const MAX_ATTEMPTS: i32 = 12;

/// Whether an on-chain check is even possible in this deployment.
pub enum VerifyMode {
    /// Verify against a Solana JSON-RPC endpoint.
    Rpc(String),
    /// Explicitly disabled (`SETTLEMENT_VERIFY=off`) — dev and offline test
    /// rigs. Settlements are marked 'skipped', never silently "verified".
    Disabled,
    /// No RPC configured. Settlements stay recorded but unconfirmed rather
    /// than being waved through.
    NoRpc,
}

pub fn verify_mode() -> VerifyMode {
    if std::env::var("SETTLEMENT_VERIFY")
        .unwrap_or_default()
        .eq_ignore_ascii_case("off")
    {
        return VerifyMode::Disabled;
    }
    match std::env::var("SOLANA_RPC_URL") {
        Ok(url) if !url.trim().is_empty() => VerifyMode::Rpc(url),
        _ => VerifyMode::NoRpc,
    }
}

/// The SPL mint that counts as payment (`TIDAT_MINT`).
///
/// Without it, "credited 100" is meaningless: the RPC reports balances for
/// every token the payee holds, so a transfer of 100 units of any worthless
/// token would satisfy a 100-TIDAT expectation. An unset mint therefore makes
/// a settlement *unverifiable*, not verified.
pub fn expected_mint() -> Option<String> {
    std::env::var("TIDAT_MINT")
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
}

/// Offline-checkable validity of a Solana account address: base58, 32 bytes
/// once decoded (43–44 base58 characters). Used to reject a malformed wallet
/// before it can be stored as a payee.
pub fn is_plausible_solana_address(addr: &str) -> bool {
    const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let s = addr.trim();
    if !(32..=44).contains(&s.len()) {
        return false;
    }
    s.bytes().all(|c| B58.contains(&c))
}

/// Offline-checkable validity of a Solana transaction signature: base58, and
/// 64 bytes once decoded (which is 87–88 base58 characters).
///
/// This is the check that makes a disconnected outpost able to reject
/// nonsense — no RPC required.
pub fn is_plausible_solana_signature(sig: &str) -> bool {
    const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let s = sig.trim();
    if !(80..=90).contains(&s.len()) {
        return false;
    }
    s.bytes().all(|c| B58.contains(&c))
}

/// Outcome of one verification attempt.
#[derive(Debug, PartialEq)]
enum Attempt {
    /// Confirmed and correct.
    Verified,
    /// Confirmed and wrong — a terminal failure, not worth retrying.
    Rejected(String),
    /// Could not reach the chain; retry later.
    Unreachable(String),
    /// The chain answered, but this deployment has nothing to check the answer
    /// against — no payee on file, no expected amount, or no configured mint.
    ///
    /// Distinct from `Rejected` (which means "we checked and it was wrong")
    /// and from `Verified`, which this used to be reported as. Collapsing it
    /// into `Verified` meant any successful transaction settled any order.
    Unverifiable(String),
}

pub async fn run_settlement_verifier(state: AppState) {
    let mode = verify_mode();
    match &mode {
        VerifyMode::Rpc(url) => tracing::info!(rpc = %url, "settlement verifier active"),
        VerifyMode::Disabled => {
            tracing::warn!("SETTLEMENT_VERIFY=off — settlements will be recorded but never confirmed");
            return;
        }
        VerifyMode::NoRpc => {
            tracing::warn!("SOLANA_RPC_URL unset — settlements will stay unconfirmed");
            return;
        }
    }
    let VerifyMode::Rpc(rpc_url) = mode else { return };

    let http = reqwest::Client::new();

    loop {
        let rows = match sqlx::query(
            r#"
            SELECT f.id, f.settlement_tx, f.settlement_amount, f.settlement_payee,
                   f.settlement_attempts
            FROM fulfillments f
            WHERE f.settlement_status = 'pending'
              AND f.settlement_next_try_at <= NOW()
            ORDER BY f.settlement_next_try_at
            LIMIT 20
            "#,
        )
        .fetch_all(&state.db)
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "settlement queue fetch failed");
                sleep(Duration::from_secs(IDLE_SECS)).await;
                continue;
            }
        };

        if rows.is_empty() {
            sleep(Duration::from_secs(IDLE_SECS)).await;
            continue;
        }

        for r in rows {
            let id: Uuid = r.get("id");
            let tx: String = r.get::<Option<String>, _>("settlement_tx").unwrap_or_default();
            let amount: Option<rust_decimal::Decimal> = r.get("settlement_amount");
            let payee: Option<String> = r.get("settlement_payee");
            let attempts: i32 = r.get("settlement_attempts");

            let outcome = verify_transaction(&http, &rpc_url, &tx, amount, payee.as_deref()).await;
            apply_outcome(&state, id, outcome, attempts).await;
        }
    }
}

/// Ask the chain about one transaction.
async fn verify_transaction(
    http: &reqwest::Client,
    rpc_url: &str,
    signature: &str,
    expected_amount: Option<rust_decimal::Decimal>,
    expected_payee: Option<&str>,
) -> Attempt {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getTransaction",
        "params": [signature, { "encoding": "jsonParsed", "maxSupportedTransactionVersion": 0 }],
    });

    let resp = match http.post(rpc_url).json(&body).timeout(Duration::from_secs(20)).send().await {
        Ok(r) => r,
        Err(e) => return Attempt::Unreachable(format!("rpc unreachable: {e}")),
    };
    if !resp.status().is_success() {
        return Attempt::Unreachable(format!("rpc http {}", resp.status()));
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(j) => j,
        Err(e) => return Attempt::Unreachable(format!("rpc bad json: {e}")),
    };

    // A null result means "not found (yet)" — on Solana that is genuinely
    // ambiguous between unconfirmed and nonexistent, so it is retryable
    // rather than terminal. MAX_ATTEMPTS bounds how long we care.
    let Some(result) = json.get("result").filter(|v| !v.is_null()) else {
        return Attempt::Unreachable("transaction not found yet".into());
    };

    // A transaction that landed but failed did not pay anyone.
    if let Some(err) = result.pointer("/meta/err").filter(|v| !v.is_null()) {
        return Attempt::Rejected(format!("transaction failed on-chain: {err}"));
    }

    // Confirm the payee actually received the expected amount of the expected
    // token. All three expectations are required: without a payee there is
    // nobody to have been paid, without an amount nothing to compare, and
    // without a mint any token would do. A missing expectation makes the
    // settlement unverifiable — it must not pass as verified.
    let (Some(expected), Some(payee), Some(mint)) =
        (expected_amount, expected_payee, expected_mint())
    else {
        return Attempt::Unverifiable(missing_expectations(
            expected_amount,
            expected_payee,
            expected_mint().as_deref(),
        ));
    };

    match credited_amount(result, payee, &mint) {
        Some(actual) if actual >= expected => Attempt::Verified,
        Some(actual) => Attempt::Rejected(format!(
            "payee {payee} credited {actual} of mint {mint}, expected at least {expected}"
        )),
        None => Attempt::Rejected(format!(
            "no increase in mint {mint} for payee {payee}"
        )),
    }
}

/// Spell out which expectations were missing, so the recorded error tells an
/// operator what to fix rather than just that something was absent.
fn missing_expectations(
    amount: Option<rust_decimal::Decimal>,
    payee: Option<&str>,
    mint: Option<&str>,
) -> String {
    let mut missing = Vec::new();
    if amount.is_none() {
        missing.push("expected amount (no accepted bid price)");
    }
    if payee.is_none() {
        missing.push("payee wallet (shipper has no wallet_address on file)");
    }
    if mint.is_none() {
        missing.push("token mint (TIDAT_MINT unset)");
    }
    format!(
        "transaction exists and succeeded, but cannot confirm payment: missing {}",
        missing.join("; ")
    )
}

/// Net amount of `mint` credited to `payee` by this transaction, from the
/// pre/post token balances the RPC returns.
///
/// Matching on owner *and* mint matters twice: it stops a transfer of some
/// other token from counting as payment, and it picks the right account when a
/// payee holds several token accounts.
fn credited_amount(
    result: &serde_json::Value,
    payee: &str,
    mint: &str,
) -> Option<rust_decimal::Decimal> {
    use rust_decimal::Decimal;
    use std::str::FromStr;

    let read = |key: &str| -> Option<Decimal> {
        result
            .pointer(&format!("/meta/{key}"))?
            .as_array()?
            .iter()
            .find(|b| {
                b.get("owner").and_then(|o| o.as_str()) == Some(payee)
                    && b.get("mint").and_then(|m| m.as_str()) == Some(mint)
            })
            .and_then(|b| b.pointer("/uiTokenAmount/uiAmountString"))
            .and_then(|v| v.as_str())
            .and_then(|s| Decimal::from_str(s).ok())
    };

    let post = read("postTokenBalances")?;
    let pre = read("preTokenBalances").unwrap_or_default();
    Some(post - pre)
}

/// Record the result of one attempt, with backoff on transient failures.
async fn apply_outcome(state: &AppState, id: Uuid, outcome: Attempt, attempts: i32) {
    match outcome {
        Attempt::Verified => {
            let _ = sqlx::query(
                r#"
                UPDATE fulfillments
                SET settlement_status='verified', settlement_verified_at=NOW(),
                    status='settled', settled_at=COALESCE(settled_at, NOW()),
                    settlement_error=NULL, updated_at=NOW()
                WHERE id=$1
                "#,
            )
            .bind(id)
            .execute(&state.db)
            .await;

            // The order only reaches 'settled' behind a confirmed payment.
            let _ = sqlx::query(
                "UPDATE orders SET status='settled', updated_at=NOW() WHERE id=(SELECT order_id FROM fulfillments WHERE id=$1)",
            )
            .bind(id)
            .execute(&state.db)
            .await;

            tracing::info!(fulfillment_id = %id, "settlement verified on-chain");
        }

        Attempt::Rejected(reason) => {
            // Terminal: the chain answered and the answer was wrong. Leaving
            // the order in 'settling' with a recorded reason is deliberate —
            // this is a dispute for a human, not something to retry.
            let _ = sqlx::query(
                r#"
                UPDATE fulfillments
                SET settlement_status='rejected', settlement_error=$2,
                    settlement_attempts=settlement_attempts+1, updated_at=NOW()
                WHERE id=$1
                "#,
            )
            .bind(id)
            .bind(&reason)
            .execute(&state.db)
            .await;

            tracing::error!(fulfillment_id = %id, reason = %reason, "settlement REJECTED");
        }

        Attempt::Unverifiable(reason) => {
            // Terminal, but not a dispute: the payment may well be fine, we
            // just cannot prove it. The order stays short of 'settled' because
            // 'settled' is supposed to mean someone was demonstrably paid.
            // Fixing the cause (set the wallet, set TIDAT_MINT) and re-posting
            // the settlement is the recovery path.
            let _ = sqlx::query(
                r#"
                UPDATE fulfillments
                SET settlement_status='unverifiable', settlement_error=$2,
                    settlement_attempts=settlement_attempts+1, updated_at=NOW()
                WHERE id=$1
                "#,
            )
            .bind(id)
            .bind(&reason)
            .execute(&state.db)
            .await;

            tracing::warn!(fulfillment_id = %id, reason = %reason, "settlement UNVERIFIABLE");
        }

        Attempt::Unreachable(reason) => {
            let next = attempts + 1;
            if next >= MAX_ATTEMPTS {
                let _ = sqlx::query(
                    r#"
                    UPDATE fulfillments
                    SET settlement_status='unverifiable', settlement_error=$2,
                        settlement_attempts=$3, updated_at=NOW()
                    WHERE id=$1
                    "#,
                )
                .bind(id)
                .bind(&reason)
                .bind(next)
                .execute(&state.db)
                .await;
                tracing::warn!(fulfillment_id = %id, attempts = next, "settlement unverifiable; giving up");
            } else {
                let delay = (2_f64).powi(next.min(12)).min(MAX_BACKOFF_SECS);
                let _ = sqlx::query(
                    r#"
                    UPDATE fulfillments
                    SET settlement_attempts=$3, settlement_error=$2,
                        settlement_next_try_at = NOW() + make_interval(secs => $4),
                        updated_at=NOW()
                    WHERE id=$1
                    "#,
                )
                .bind(id)
                .bind(&reason)
                .bind(next)
                .bind(delay)
                .execute(&state.db)
                .await;
                tracing::debug!(fulfillment_id = %id, delay_secs = delay, reason = %reason, "settlement retry scheduled");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use serde_json::json;

    #[test]
    fn rejects_obvious_nonsense_without_a_network() {
        // The original bug: "abc" settled an order. This must fail on a
        // fully disconnected outpost.
        assert!(!is_plausible_solana_signature("abc"));
        assert!(!is_plausible_solana_signature(""));
        assert!(!is_plausible_solana_signature("   "));
        assert!(!is_plausible_solana_signature("placeholder_signature"));
    }

    #[test]
    fn rejects_wrong_length_and_non_base58() {
        assert!(!is_plausible_solana_signature(&"1".repeat(50)));
        assert!(!is_plausible_solana_signature(&"1".repeat(120)));
        // 0, O, I and l are not in the base58 alphabet.
        assert!(!is_plausible_solana_signature(&"0".repeat(88)));
        assert!(!is_plausible_solana_signature(&"O".repeat(88)));
    }

    #[test]
    fn accepts_a_well_formed_signature() {
        let sig = "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
        assert!(is_plausible_solana_signature(sig));
    }

    const MINT: &str = "TIDATmint11111111111111111111111111111111111";
    const OTHER_MINT: &str = "Wrongmint1111111111111111111111111111111111";

    /// A balance entry as the RPC reports it.
    fn bal(owner: &str, mint: &str, amount: &str) -> serde_json::Value {
        json!({ "owner": owner, "mint": mint, "uiTokenAmount": { "uiAmountString": amount } })
    }

    #[test]
    fn credits_are_computed_as_post_minus_pre() {
        let result = json!({
            "meta": {
                "preTokenBalances":  [bal("PAYEE", MINT, "10")],
                "postTokenBalances": [bal("PAYEE", MINT, "35")]
            }
        });
        assert_eq!(credited_amount(&result, "PAYEE", MINT), Some(Decimal::from(25)));
    }

    #[test]
    fn a_payee_with_no_balance_change_is_not_credited() {
        let result = json!({
            "meta": {
                "preTokenBalances":  [bal("OTHER", MINT, "1")],
                "postTokenBalances": [bal("OTHER", MINT, "9")]
            }
        });
        assert_eq!(credited_amount(&result, "PAYEE", MINT), None);
    }

    #[test]
    fn a_first_time_payee_counts_from_zero() {
        // No pre-balance entry at all: the account was created by this tx.
        let result = json!({
            "meta": {
                "preTokenBalances":  [],
                "postTokenBalances": [bal("PAYEE", MINT, "42")]
            }
        });
        assert_eq!(credited_amount(&result, "PAYEE", MINT), Some(Decimal::from(42)));
    }

    #[test]
    fn payment_in_the_wrong_token_does_not_count() {
        // The original bug: matching on owner alone meant 25 units of any
        // worthless SPL token satisfied a 25-TIDAT expectation.
        let result = json!({
            "meta": {
                "preTokenBalances":  [bal("PAYEE", OTHER_MINT, "0")],
                "postTokenBalances": [bal("PAYEE", OTHER_MINT, "25")]
            }
        });
        assert_eq!(credited_amount(&result, "PAYEE", MINT), None);
    }

    #[test]
    fn the_right_mint_is_picked_when_a_payee_holds_several() {
        // Matching on owner alone would take whichever account came first.
        let result = json!({
            "meta": {
                "preTokenBalances":  [bal("PAYEE", OTHER_MINT, "0"), bal("PAYEE", MINT, "5")],
                "postTokenBalances": [bal("PAYEE", OTHER_MINT, "900"), bal("PAYEE", MINT, "12")]
            }
        });
        assert_eq!(credited_amount(&result, "PAYEE", MINT), Some(Decimal::from(7)));
    }

    #[test]
    fn missing_expectations_are_named_individually() {
        // The recorded error has to tell an operator what to go and fix.
        let all_absent = missing_expectations(None, None, None);
        assert!(all_absent.contains("expected amount"));
        assert!(all_absent.contains("wallet_address"));
        assert!(all_absent.contains("TIDAT_MINT"));

        let only_wallet = missing_expectations(Some(Decimal::from(5)), None, Some(MINT));
        assert!(only_wallet.contains("wallet_address"));
        assert!(!only_wallet.contains("TIDAT_MINT"));
        assert!(!only_wallet.contains("expected amount"));
    }

    #[test]
    fn unverifiable_is_not_verified() {
        // These were the same value before the fix, which is precisely how any
        // successful transaction came to settle any order.
        assert_ne!(Attempt::Unverifiable(String::new()), Attempt::Verified);
    }

    #[test]
    fn wallet_addresses_are_checked_offline() {
        assert!(is_plausible_solana_address("9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM"));
        assert!(!is_plausible_solana_address(""));
        assert!(!is_plausible_solana_address("not a wallet"));
        // 0, O, I and l are not in the base58 alphabet.
        assert!(!is_plausible_solana_address(&"0".repeat(44)));
        assert!(!is_plausible_solana_address(&"1".repeat(60)));
    }
}
