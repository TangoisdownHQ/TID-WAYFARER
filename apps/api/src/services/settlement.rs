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

/// Which settlement rail a fulfilment was paid through.
///
/// A static enum, not a `dyn` trait, for the same reason `carriers::Provider`
/// is: a small closed set of rails dispatched by `match`, where adding
/// `async-trait` to express a few arms would be a poor trade. The always-works
/// rail is `FiatInvoice` — it needs no chain and no network to record, which is
/// what lets an organisation that settles by invoice use the marketplace at
/// all. The on-chain rails are an optimisation over it, not a prerequisite.
///
/// TIDAT is a first-class rail here, not a special case: these rails sit
/// *beside* it and do not replace it. The default rail stays TIDAT unless
/// `SETTLEMENT_DEFAULT_RAIL` says otherwise, so a caller that sends only a
/// signature settles exactly as it did before rails existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettlementRail {
    /// Out-of-band fiat — an invoice paid by bank transfer, or a payment driven
    /// through an ERP. Recorded immediately and confirmed by the counterparty
    /// (or an ERP webhook); never touches a chain.
    FiatInvoice,
    /// TIDasToken on Solana. The native rail. Verified on-chain against
    /// `TIDAT_MINT`.
    Tidat,
    /// USDC on Solana. A regulated stablecoin, verified by exactly the same
    /// on-chain path as TIDAT, against the mint in `USDC_MINT`.
    Usdc,
    // A proprietary, out-of-tree rail plugs in here behind a cargo feature,
    // delegating verification to a private crate that is never committed to the
    // public repository. Left as a documented extension point so adding it
    // later touches only this enum and its `match` arms:
    //   #[cfg(feature = "proprietary-rail")]
    //   Proprietary(proprietary_rail::Handle),
}

impl SettlementRail {
    /// Resolve a rail name. An unknown name returns `None` so the caller can
    /// refuse: unlike a carrier (where an unconfigured provider sensibly falls
    /// back to manual), the rail *is* the payment, and silently settling on the
    /// wrong one would be a lie rather than a graceful degradation. Legacy
    /// 'solana' resolves to TIDAT.
    pub fn resolve(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "fiat" | "invoice" | "fiat_invoice" => Some(Self::FiatInvoice),
            "tidat" | "solana" => Some(Self::Tidat),
            "usdc" => Some(Self::Usdc),
            _ => None,
        }
    }

    /// The rail a settlement defaults to when the caller names none. TIDAT
    /// unless the deployment overrides it, so a farm- or carrier-only
    /// deployment can make fiat the default without changing any client.
    pub fn default_rail() -> Self {
        std::env::var("SETTLEMENT_DEFAULT_RAIL")
            .ok()
            .and_then(|n| Self::resolve(&n))
            .unwrap_or(Self::Tidat)
    }

    /// The string stored in `fulfillments.settlement_chain`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::FiatInvoice => "fiat",
            Self::Tidat => "tidat",
            Self::Usdc => "usdc",
        }
    }

    /// Whether settlement on this rail is confirmed by asking a chain. Fiat is
    /// confirmed by attestation instead, so the on-chain verifier skips it.
    pub fn is_onchain(&self) -> bool {
        matches!(self, Self::Tidat | Self::Usdc)
    }

    /// The SPL mint that counts as payment on this rail.
    ///
    /// Without it, "credited 100" is meaningless: the RPC reports balances for
    /// every token the payee holds, so a transfer of 100 units of any worthless
    /// token would satisfy a 100-unit expectation. An unset mint therefore
    /// makes an on-chain settlement *unverifiable*, not verified. `None` for
    /// fiat, which has no mint.
    pub fn expected_mint(&self) -> Option<String> {
        let var = match self {
            Self::Tidat => "TIDAT_MINT",
            Self::Usdc => "USDC_MINT",
            Self::FiatInvoice => return None,
        };
        std::env::var(var)
            .ok()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty())
    }

    /// Whether `reference` is a well-formed payment reference for this rail,
    /// checkable with no network. On-chain rails need a plausible Solana
    /// signature; fiat needs a non-empty invoice reference an operator can look
    /// up in their own books.
    pub fn validate_reference(&self, reference: &str) -> bool {
        match self {
            Self::Tidat | Self::Usdc => is_plausible_solana_signature(reference),
            Self::FiatInvoice => !reference.trim().is_empty(),
        }
    }
}

/// Resolve the settlement payee: the shipper's organisation wallet if it has
/// one, else the shipper's own wallet, else nobody.
///
/// Organisation first, because the revenue is the organisation's; the user
/// fallback keeps a sole trader — who has no organisation wallet — a valid
/// payee. Without this, a company's payments landed in whichever employee
/// shipped the order, and an employee who left took the payout address with
/// them.
pub fn resolve_payee(org_wallet: Option<String>, user_wallet: Option<String>) -> Option<String> {
    org_wallet
        .filter(|w| !w.trim().is_empty())
        .or_else(|| user_wallet.filter(|w| !w.trim().is_empty()))
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
                   f.settlement_attempts, f.settlement_chain
            FROM fulfillments f
            WHERE f.settlement_status = 'pending'
              AND f.settlement_next_try_at <= NOW()
              AND f.settlement_chain IN ('tidat', 'usdc', 'solana')
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
            let chain: String = r.get::<Option<String>, _>("settlement_chain").unwrap_or_default();

            // The query only returns on-chain rails, so an unrecognised value
            // is a data error rather than a fiat row. Default to TIDAT's mint so
            // it surfaces as unverifiable rather than silently disappearing.
            let rail = SettlementRail::resolve(&chain).unwrap_or(SettlementRail::Tidat);
            let mint = rail.expected_mint();

            let outcome =
                verify_transaction(&http, &rpc_url, &tx, amount, payee.as_deref(), mint).await;
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
    expected_mint: Option<String>,
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
        (expected_amount, expected_payee, expected_mint.clone())
    else {
        return Attempt::Unverifiable(missing_expectations(
            expected_amount,
            expected_payee,
            expected_mint.as_deref(),
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
        missing.push("token mint (set TIDAT_MINT or USDC_MINT for the rail)");
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
            // Terminal for *this* attempt, and a dispute record — but the order
            // must not be stranded in 'settling' with no way forward. Reopen it
            // to 'delivered' so either party can post a corrected settlement
            // (the "that payment was wrong, here is the right one" path the
            // design previously lacked), while the rejected tx and its reason
            // stay recorded on the fulfilment as the audit trail.
            let _ = sqlx::query(
                r#"
                UPDATE fulfillments
                SET settlement_status='rejected', settlement_error=$2,
                    settlement_attempts=settlement_attempts+1,
                    status=CASE WHEN status='settling' THEN 'delivered' ELSE status END,
                    updated_at=NOW()
                WHERE id=$1
                "#,
            )
            .bind(id)
            .bind(&reason)
            .execute(&state.db)
            .await;

            let _ = sqlx::query(
                "UPDATE orders SET status='delivered', updated_at=NOW() \
                 WHERE id=(SELECT order_id FROM fulfillments WHERE id=$1) \
                   AND status='settling'",
            )
            .bind(id)
            .execute(&state.db)
            .await;

            tracing::error!(
                fulfillment_id = %id,
                reason = %reason,
                "settlement REJECTED; order reopened for a corrected settlement"
            );
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

    // --- Settlement rails ---

    #[test]
    fn rail_names_resolve_case_insensitively_and_legacy_solana_is_tidat() {
        assert_eq!(SettlementRail::resolve("fiat"), Some(SettlementRail::FiatInvoice));
        assert_eq!(SettlementRail::resolve("invoice"), Some(SettlementRail::FiatInvoice));
        assert_eq!(SettlementRail::resolve("TIDAT"), Some(SettlementRail::Tidat));
        assert_eq!(SettlementRail::resolve(" usdc "), Some(SettlementRail::Usdc));
        // Rows written before rails existed carry 'solana' and must keep working.
        assert_eq!(SettlementRail::resolve("solana"), Some(SettlementRail::Tidat));
    }

    #[test]
    fn an_unknown_rail_is_refused_not_defaulted() {
        // Unlike a carrier, the rail *is* the payment: settling on the wrong one
        // would be a lie, so an unknown name resolves to nothing.
        assert_eq!(SettlementRail::resolve("dogecoin"), None);
        assert_eq!(SettlementRail::resolve(""), None);
    }

    #[test]
    fn only_the_chain_rails_are_on_chain() {
        assert!(SettlementRail::Tidat.is_onchain());
        assert!(SettlementRail::Usdc.is_onchain());
        assert!(!SettlementRail::FiatInvoice.is_onchain());
    }

    #[test]
    fn references_are_validated_per_rail_without_a_network() {
        let sig = "5VERv8NMvzbJMEkV8xnrLkEaWRtSz9CosKDYjCJjBRnbJLgp8uirBgmQpjKhoR4tjF3ZpRzrFmBV6UjKdiSZkQUW";
        // On-chain rails need a plausible Solana signature.
        assert!(SettlementRail::Tidat.validate_reference(sig));
        assert!(SettlementRail::Usdc.validate_reference(sig));
        assert!(!SettlementRail::Tidat.validate_reference("abc"));
        // Fiat needs only a non-empty invoice reference — and must NOT demand a
        // Solana signature, which is the whole point of the rail.
        assert!(SettlementRail::FiatInvoice.validate_reference("INV-2026-0042"));
        assert!(!SettlementRail::FiatInvoice.validate_reference("   "));
    }

    #[test]
    fn each_on_chain_rail_reads_its_own_mint_and_fiat_has_none() {
        std::env::set_var("TIDAT_MINT", "TIDATmint11111111111111111111111111111111111");
        std::env::set_var("USDC_MINT", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(
            SettlementRail::Tidat.expected_mint().as_deref(),
            Some("TIDATmint11111111111111111111111111111111111")
        );
        assert_eq!(
            SettlementRail::Usdc.expected_mint().as_deref(),
            Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v")
        );
        // Fiat never consults a mint even when the env vars are set.
        assert_eq!(SettlementRail::FiatInvoice.expected_mint(), None);
        std::env::remove_var("TIDAT_MINT");
        std::env::remove_var("USDC_MINT");
    }

    #[test]
    fn the_default_rail_is_tidat_unless_overridden() {
        std::env::remove_var("SETTLEMENT_DEFAULT_RAIL");
        assert_eq!(SettlementRail::default_rail(), SettlementRail::Tidat);
        std::env::set_var("SETTLEMENT_DEFAULT_RAIL", "fiat");
        assert_eq!(SettlementRail::default_rail(), SettlementRail::FiatInvoice);
        // A nonsense override falls back to TIDAT rather than erroring at boot.
        std::env::set_var("SETTLEMENT_DEFAULT_RAIL", "nonsense");
        assert_eq!(SettlementRail::default_rail(), SettlementRail::Tidat);
        std::env::remove_var("SETTLEMENT_DEFAULT_RAIL");
    }

    // --- Payee resolution (the org-wallet bug) ---

    #[test]
    fn payee_prefers_the_org_wallet_then_falls_back_to_the_user() {
        let org = || Some("ORGwa11et1111111111111111111111111111111111".to_string());
        let usr = || Some("USERwa11et111111111111111111111111111111111".to_string());
        // Organisation wallet wins: a company's revenue is the company's.
        assert_eq!(resolve_payee(org(), usr()).as_deref(), Some("ORGwa11et1111111111111111111111111111111111"));
        // A sole trader with no org wallet still settles to their own wallet.
        assert_eq!(resolve_payee(None, usr()).as_deref(), Some("USERwa11et111111111111111111111111111111111"));
        // A blank org wallet is not a payee — fall through to the user.
        assert_eq!(resolve_payee(Some("   ".into()), usr()).as_deref(), Some("USERwa11et111111111111111111111111111111111"));
        // Nothing on file: nobody to pay, so the caller must refuse.
        assert_eq!(resolve_payee(None, None), None);
        assert_eq!(resolve_payee(Some("".into()), Some("".into())), None);
    }
}
