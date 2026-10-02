# TIDasToken (TIDAT) — what it is for, and where the code actually is

## Why the marketplace needs its own rail

Wayfarer brokers capacity between parties who may have no settlement rail in
common. A farm co-op in one country, a carrier registered in another, and an
orbital platform operator billing in a third currency all have to agree on
what a shipment cost and that it was paid. The usual answer — pick a bank and a
currency and make everyone else convert — fails exactly where this tool is
aimed: at participants who are small, remote, intermittently connected, or
outside the payments infrastructure the big carriers already share.

So **TIDAT is the settlement rail for the marketplace by design**, not a bolted-on
loyalty point. It is the unit a bid is quoted in, the unit a customs
declaration states a value in, and the unit a payout is denominated in.

That is the intent. The rest of this document is about how much of it exists.

---

## The one-sentence status

**Built: verification. Not built: custody of funds.**

Wayfarer never holds, moves, or releases money. It checks that money moved. A
buyer pays out of band — any wallet, any client — and submits the transaction
signature; the system then confirms on-chain that the named payee received at
least the expected amount of a specific token, and only then does an order
reach `settled`.

Everything in the original token design that involves the platform *holding*
value — escrow, staking, treasury, governance weight — is not implemented and
has no code in this repository.

---

## What is built

### The lifecycle of a settlement

```
buyer pays out of band
        │
        ▼
POST /api/fulfillments/:id/settle   { settlement_tx: "<base58 signature>" }
        │
        ├── signature malformed?            → 400, nothing recorded
        ├── no payee or no price on file?   → 412, nothing recorded
        ├── signature already used?         → 409, nothing recorded
        │
        ▼
recorded as settlement_status='pending', order NOT yet settled
        │
        ▼
settlement verifier daemon (polls, backs off, max 12 attempts)
        │
        ├── Verified      → settlement_status='verified', order='settled'
        ├── Rejected      → terminal; the order does not settle
        ├── Unreachable   → retry with backoff, capped at 1 hour
        └── Unverifiable  → recorded as such; never counted as paid
```

The order reaches `settled` **only** behind `Verified`. That is the whole point
of the module: `settled` used to mean "somebody typed a string into the
settlement field".

### Three expectations, all required

A verification compares the chain's answer against three things. If any is
missing the result is `Unverifiable`, never `Verified`:

| Expectation | Where it comes from | Why it is not optional |
|---|---|---|
| **amount** | the accepted bid's price | without it there is nothing to compare |
| **payee** | the shipper's `wallet_address` | without it there is nobody to have been paid |
| **mint** | `TIDAT_MINT` | without it *any* token would do |

The mint is the one people leave out, and it is the one that matters most. The
RPC reports balance changes for every token the payee holds, so with no
expected mint a transfer of 100 units of a worthless SPL token would satisfy a
100-TIDAT expectation.

Amounts are read from `uiTokenAmount.uiAmountString` and compared as decimals,
so the token's decimal places are already applied — a bid of `100` is compared
against 100 TIDAT, not 100 base units.

The credited amount is `post − pre` for the payee's balance in that mint, so a
payee whose token account did not exist before the transfer is handled
correctly, and an unrelated balance the payee already held is not counted.

### Four outcomes, and why `Unverifiable` exists separately

This was a real bug. `Unverifiable` used to be reported as `Verified`, which
meant **any successful transaction settled any order** — and the condition that
triggered it was the common one, because `users.wallet_address` had no write
path at all and was therefore always NULL.

- `Verified` — the chain confirmed the payee received at least the expected
  amount of the expected mint.
- `Rejected` — the chain answered and the answer was wrong (transaction failed
  on-chain, wrong amount, no credit in that mint). Terminal; not retried.
- `Unreachable` — could not reach the chain, or the transaction is not visible
  yet. Retried with exponential backoff, capped at one hour, up to 12 attempts.
- `Unverifiable` — the chain answered but this deployment has nothing to check
  it against. The recorded error names which expectation was missing, so an
  operator can fix the cause rather than guess.

### Offline behaviour

Two deliberate properties, because an outpost can be a long way from a usable
link:

**A settlement is recorded immediately and confirmed later.** Same
store-and-forward shape as DTN and command delivery. The record exists during
a blackout; it just does not claim to be paid.

**Some checks need no network at all.** Address and signature format are
base58 and length-checked locally, which is what stops `"abc"` from settling an
order on a fully disconnected outpost.

### Replay, at the settlement layer

`idx_fulfillments_settlement_tx_unique` makes a transaction signature usable
once. Without it, one payment could settle any number of fulfilments — the same
shape of bug as a replayed DTN envelope, one layer up. Attempting reuse gets a
conflict rather than a second settled order.

### Wallets

- `GET`/`PUT /api/me/wallet` — a participant's own payout address, format
  checked on write.
- `GET /api/me` reports `needs_payout_wallet` for a seller account with none on
  file, so the UI can ask before a settlement fails rather than after.
- `organisations.wallet_address` exists and is set at organisation creation.
  **It is not read by the settlement path** — see [gaps](#known-gaps).

### Where TIDAT appears elsewhere

- `supplier_parts.currency` defaults to `'TIDAT'` — supplier pricing is quoted
  in it.
- Customs declarations state declared value in TIDAT.
- Bids, order price ceilings and rate cards are all denominated in it
  implicitly; there is no multi-currency model.

---

## Configuration

| Variable | Effect if unset | Effect if set |
|---|---|---|
| `SOLANA_RPC_URL` | `NoRpc` mode: settlements are recorded but never confirmed, and nothing is promoted to `settled` | verification runs against that endpoint |
| `TIDAT_MINT` | every verification returns `Unverifiable` | that mint is what counts as payment |
| `SETTLEMENT_VERIFY=off` | — | settlements complete immediately, stamped `skipped`, never `verified` |

`SETTLEMENT_VERIFY=off` is for dev and offline test rigs. It is safe in the
sense that matters: the record says `skipped`, so it can never later be
mistaken for a confirmed payment.

### The state of this deployment, right now

The running stack has **none of the three set**. The verifier logs a warning at
startup and sits in `NoRpc`, so settlements would be recorded and stay
unconfirmed. There is also **no TIDAT mint deployed** — the token does not yet
exist on any chain, test or main. Until it does, the verification path is
correct code with nothing to verify against.

That is worth stating plainly because the code reads as a working payment
system, and it is one — the missing piece is the token itself.

---

## What is not built

Named individually, because a token design is the easiest thing in a project to
describe as finished.

- **No on-chain program.** There are no Solana programs, no Anchor workspace,
  no `anchor-lang` or `solana-program` dependency. The integration is an RPC
  *client* that reads and verifies.
- **No escrow.** Nothing holds the buyer's funds between acceptance and
  delivery. The buyer pays, then proves it. This is the largest functional gap
  and it shapes the trust model below.
- **No payment initiation.** The UI has no wallet connection and cannot
  construct, sign or submit a transfer. A buyer pays with their own wallet and
  pastes the signature.
- **No staking.** `organisations` and `users` have no stake, and nothing
  prioritises sync, delivery or data access by it.
- **No DAO or governance.** No proposals, no votes, no treasury.
- **No gasless relayers.** The buyer pays their own transaction fees.
- **No NFT inventory proofs.** `inventory.token_id`, `assets.nft_token`,
  `packages.nft_token` and `users.nft_token_id` exist as nullable text columns
  and are stored and returned verbatim. Nothing mints them, nothing verifies
  them, and nothing treats their presence as proof of anything.
- **No burn or deflationary mechanics.** No supply logic of any kind.
- **No cross-chain.** `fulfillments.settlement_chain` is written as the literal
  `'solana'`; there is no second implementation behind that column.
- **No offline settlement.** Closing a trade where no chain is reachable is
  [designed](./OfflineSettlement.md) and not built. Today a settlement in a
  blackout is *recorded* and waits — which is useful, but the order does not
  close.

---

## The trust model, stated honestly

With no escrow, what does `settled` actually guarantee?

**It guarantees the payment happened.** A confirmed on-chain transfer of at
least the agreed amount, in the agreed token, to the wallet on file for the
shipper, in a transaction not already used for another settlement. That is a
strong, independently checkable claim, and it is more than most logistics
software can make.

**It does not guarantee the goods arrived.** Delivery is attested separately,
by the custody chain — signed receipts at each handover. The two are recorded
against the same fulfilment but neither is conditional on the other.

**So the residual risk is ordering.** A buyer who pays before delivery is
trusting the carrier; a carrier who delivers before payment is trusting the
buyer. The parties choose which, out of band. Escrow is what would remove that
choice, and escrow is the thing that needs an on-chain program.

Do not describe the current system as trustless. It is *verifiable*, which is a
different and still useful property: nobody has to take anybody's word about
whether a payment occurred.

---

## Known gaps

Smaller than the missing features above, but real, and each one would surprise
somebody:

**The payee is a person, not a company.** The settlement payee is
`users.wallet_address` for the shipper who fulfilled the order.
`organisations.wallet_address` is recorded and never read. For a company
selling capacity this is the wrong shape: payouts should go to the
organisation, with the user as the actor who shipped it. As it stands, a
company's revenue lands in whichever employee's wallet is on their account, and
an employee who leaves takes the payout address with them.

**A rejected settlement does not reopen the order.** `Rejected` is terminal for
the attempt, and the fulfilment keeps the failed transaction recorded. There is
no flow for "that payment was wrong, here is the right one" short of operator
intervention.

**12 attempts is a fixed ceiling.** A chain unreachable for longer than the
backoff schedule allows ends as `Unverifiable` even though the payment may be
perfectly valid. For an outpost dark for a week that is the likely outcome.

**No amount tolerance or overpayment handling.** The check is `actual >=
expected`, so an overpayment verifies and the excess is not tracked anywhere.

---

## Verified against a running stack

Each of these was exercised on a live deployment rather than read off the code.
`SOLANA_RPC_URL` and `TIDAT_MINT` were unset, which is why nothing reaches
`verified` — that is the point of the last row.

| Action | Result |
|---|---|
| `settle` with `"abc"` | **400** — malformed signature, refused with no network involved |
| `settle` before delivery is attested | **409** — settlement follows delivery |
| `settle` with a valid signature, shipper has no wallet | **412** — nothing to verify the payment against |
| `PUT /api/me/wallet` with `"not-a-wallet"` | **400** — base58/length checked on write |
| `settle` once the shipper has a wallet | **200**, recorded `pending`, `amount=2640`, payee stored |
| the order, at that moment | `settling` — **not** `settled`; the chain has confirmed nothing |
| replaying that signature on a different fulfilment | **409** — one payment settles one fulfilment |
| a fresh signature on that fulfilment | **200** |
| the verifier at startup | `WARN SOLANA_RPC_URL unset — settlements will stay unconfirmed` |

The 412 is worth dwelling on: the wallet that mattered was the **shipper's**,
not the caller's. Setting the caller's own payout address changed nothing,
which is the [payee-is-a-person gap](#known-gaps) showing up in practice.

---

## Where the code is

| What | Where |
|---|---|
| Verification, modes, format checks, the verifier daemon | `apps/api/src/services/settlement.rs` |
| The settle endpoint and its preconditions | `apps/api/src/routes/orders.rs` |
| Wallet read/write | `apps/api/src/routes/me.rs` |
| Settlement columns and the tx-uniqueness index | `packages/db/migrations/*settlement*.sql` |
| Unit tests, including the worthless-token case | `settlement.rs` test module |

`apps/api/src/routes/blockchain.rs` is **not** about the token despite the
name — it ingests and lists chain *threat alerts* (`/api/bc/alerts`), which is
a security feed, unrelated to settlement.
