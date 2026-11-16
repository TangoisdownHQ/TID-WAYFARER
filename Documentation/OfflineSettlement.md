# Offline Settlement — Design

**Status:** steps 1–3 implemented (IOU mode); steps 4–6 pending (escrow, console,
shared capsules).
**Depends on:** the settlement verifier (implemented), node identity / Ed25519
signing (implemented), DTN (implemented).

**What is built (2026-11-15):** the CTV primitive (`services::voucher` — canonical
`ctv|v1` encoding, sign/verify, monotonic nonces, condition evaluation; pure,
unit-tested); the `transfer_vouchers`/`escrow_accounts` schema and
`fulfillments.ctv_id` (migration `20261115_02`); and `routes::vouchers` —
commit (IOU only, payer from the verified principal, payee resolved org→user,
signed with the outpost identity), present, list, and discharge (delivery judged
from the order's `services::custody` receipt chain). Both prerequisites below are
already satisfied in the current codebase. Not yet built: on-chain escrow backing
and the anchoring daemon (step 4), horizons/expiry/dispute console surfacing
(step 5), and shared-capsule manifests (step 6).

---

## The problem

TID Wayfarer's marketplace prices trades in TIDasToken and confirms them
against Solana. That works for an outpost with a link. It cannot work for the
case the product is built around.

Two independent reasons, and the second is the one that matters more:

**1. Distance.** Solana's slot time is ~400 ms and a recent blockhash stays
valid for roughly 150 slots — about a minute. One-way light delay to Mars runs
from ~3 to ~22 minutes depending on orbital geometry. A Mars outpost cannot
obtain a blockhash, sign against it, and have the transaction arrive while it
is still valid. This is not a latency budget to optimise; it is the speed of
light. The Moon (~1.3 s each way) is fine. Anything beyond it is not.

**2. Blackouts.** Far more common and not distance-related at all. A ship with
no VSAT for three days. A farm whose uplink is down. A rover over the horizon.
An outpost in LOCKDOWN with its network isolated by its own autonomy rules.
Each of these has *zero* chain access at the moment a trade needs to close,
and each of them is a Tuesday, not an edge case.

The current design handles both the same way: record the settlement, retry for
12 attempts, then give up and mark it `unverifiable`. For a warehouse in
Nevada that's correct. For the cases above it means the marketplace works
everywhere except the places that define the product.

### What we are actually trying to buy

A trade must be able to **close locally and settle later** — the same
store-and-forward shape already used three times in this codebase (DTN
outbox, command queue, settlement verification). The chain becomes an
*eventual anchor*, not the transaction medium.

Conventional rails cannot do this: correspondent banking requires online
authorization. This is the strongest argument for the token, and it's worth
stating plainly — TIDAT isn't here for ecosystem flavour, it's here because
locally-enforceable value transfer is the only kind that survives a partition.

---

## The primitive: Conditional Transfer Voucher

A **CTV** is a signed promise to pay, enforceable on presentation, that needs
no network to create or to validate.

```
ctv {
  id:            uuid
  order_id:      uuid
  fulfillment_id: uuid
  payer:         node_id + user_id
  payee_wallet:  solana address       // must exist at commit time
  amount:        numeric
  mint:          solana address       // which token counts as payment
  escrow_ref:    Option<String>       // on-chain escrow this draws against
  conditions:    [ DeliveryReceipt, NotAfter(timestamp) ]
  nonce:         u64                  // monotonic per (payer, escrow_ref)
  anchor_by:     timestamp            // the settlement horizon
  payer_sig:     ed25519              // over the canonical encoding
}
```

Signed with the payer's existing Ed25519 identity key — the same trust root as
fabric auth and sealed DTN, so there is no new key management. The payee holds
it. Validation is offline: check the signature, check the nonce hasn't been
seen, check the conditions.

Canonical encoding follows the existing `fabric_auth::canonical_request`
convention (field-ordered, length-prefixed), with its own domain prefix
(`ctv|v1|…`) so a CTV signature can never be reinterpreted as a fabric request
signature or a DTN envelope.

### Custody receipts

A CTV's delivery condition is discharged by a chain of signed handoffs, not by
someone clicking a button:

```
custody_receipt {
  ctv_id, seq, from_node, to_node,
  item_hash,                  // what was handed over
  at, signature               // signed by the *receiving* party
}
```

Each leg — shipper → launch provider → orbital transfer → surface outpost —
adds a receipt. The final receipt, signed by the recipient named in the order,
discharges the condition. The chain travels over DTN, so it accumulates even
across blackouts, and it is exactly the evidence a dispute needs.

This is also the mechanism **shared capsules** need: one capsule carries N
orders, each with its own CTV and its own per-item receipt chain, so custody
and settlement split proportionally without the parties having to trust the
capsule operator's accounting.

---

## Lifecycle

| State | Meaning | Needs network? |
|---|---|---|
| `committed` | CTV signed and exchanged. The trade is locally final. | No |
| `discharged` | Conditions met; receipt chain complete. Payee may claim. | No |
| `anchored` | Claimed and confirmed on chain. | Yes |
| `disputed` | Conditions contested, or the anchor contradicted the voucher. | — |
| `expired` | `anchor_by` passed without anchoring. Actionable. | — |

The important state is `committed`. It is **not** a pending failure — it is a
legitimate resting state, potentially for weeks. The console must render it in
the violet *unknown-because-out-of-contact* palette, not the warning palette.
Only `expired` and `disputed` belong in "needs a decision."

### Settlement horizons

`anchor_by` is set from the route's realistic contact schedule, not a global
constant:

| Route | Horizon |
|---|---|
| Terrestrial, connected | minutes |
| Ship / farm / remote site | days |
| LEO / lunar | days |
| Mars surface | weeks to months (conjunction-dependent) |

A horizon that fits the route is what makes "unconfirmed" informative. A
global 12-attempt retry cannot distinguish "the RPC is down" from "Mars is
behind the Sun."

---

## Backing the promise

A CTV is only worth what stands behind it. Three options, and the choice is
the real decision in this design:

**(a) Pre-funded on-chain escrow — recommended default.**
The payer locks funds on chain *while connected*; CTVs draw against that
balance while dark. Funding is periodic and online; trading is continuous and
offline. That asymmetry is exactly the shape of the problem, which is why this
is the default. Requires an Anchor program (escrow account, claim instruction
verifying the payer's Ed25519 signature over the CTV).

**(b) Post-hoc settlement — an IOU.**
No escrow; the CTV is a debt settled on reconnect. Zero chain dependency and
trivial to build, but it is only as good as the counterparty. Appropriate for
low-value trades inside one organisation, or where reputation or off-platform
collateral already exists. Worth supporting explicitly rather than pretending
everything is escrowed.

**(c) Hash-chain / state channel.**
Payer pre-commits a hash chain; each CTV reveals the next preimage, and the
chain enforces ordering. Non-custodial and offline-verifiable, but needs a
channel per counterparty and ties up funds per channel. Right answer for two
outposts trading constantly; wrong for an open marketplace.

Build (a), support (b) behind a per-deployment policy flag, keep (c) for a
high-volume bilateral route later.

---

## The honest limitation: offline double-spend

With escrow, a payer who is dark can sign two CTVs that together exceed one
escrow balance. This is the double-spend problem and **it cannot be eliminated
offline.** Any design claiming otherwise is wrong. It can only be bounded:

- **Local balance accounting.** The payer's own core deducts committed CTVs
  from the escrow's known balance and refuses to sign beyond it. Defeats
  accident, not malice.
- **Monotonic nonces per escrow.** A gap or a duplicate is detectable the
  moment two vouchers meet, which makes over-commitment *provable after the
  fact* even though it isn't preventable.
- **Escrow caps sized to the horizon.** Exposure is bounded by how much can be
  locked, so a long horizon should carry a smaller cap.
- **First-claim-wins at the anchor, losers → `disputed`.** With the signed
  voucher and nonce as evidence, and the over-committing payer identifiable.
- **Reputation as the real deterrent.** A provable over-commit is attributable
  to a node identity that can be revoked — the existing
  `POST /api/nodes/:id/revoke` path.

State this limit in the product documentation. "Bounded and attributable" is a
defensible claim; "prevented" is not.

---

## Schema

New tables, following existing conventions:

```sql
transfer_vouchers   -- the CTVs: all fields above, plus state and anchor_tx
custody_receipts    -- (ctv_id, seq) primary key; the handoff chain
escrow_accounts     -- on-chain escrow refs, last known balance, last sync
```

On `fulfillments`: add `ctv_id UUID REFERENCES transfer_vouchers(id)`.
`settlement_status` gains `committed` and `discharged` ahead of the existing
`pending` → `verified` path, which becomes the `anchored` check.

Indexes mirror the settlement verifier's: a partial index on vouchers
`WHERE state = 'discharged'` for the anchoring daemon's poll, and one on
`anchor_by WHERE state IN ('committed','discharged')` for horizon expiry.

---

## Code changes

Deliberately small, because the machinery mostly exists:

- **`services/voucher.rs`** (new) — canonical encoding, sign, verify, nonce
  checks, condition evaluation. Pure functions, unit-testable with no DB, in
  the style of `fabric_auth` and `dtn_crypto`.
- **`services/settlement.rs`** — gains an anchoring path: poll `discharged`
  vouchers, submit claims, reconcile. The existing
  `verify_transaction` / `credited_amount` / backoff machinery is reused as-is
  for the `anchored` confirmation; `Attempt` gains no new variants.
- **`routes/vouchers.rs`** (new) — commit a CTV, present one, submit a custody
  receipt, list vouchers by order. All behind the existing guard.
- **DTN** — vouchers and receipts are just sealed DTN payloads. No transport
  work.
- **Console** — `committed` renders violet; `expired` and `disputed` join
  "needs a decision."

### Prerequisite

Two findings from the September 2026 review blocked a correct implementation.
**Both have since landed**, which is why step 2 could take the payer from the
verified principal:

1. ✅ **Authenticated-principal propagation.** The guard now inserts a
   `Principal` (node and/or user) into request extensions, read back via the
   `Caller` / `AuthenticatedUser` extractors. `routes::vouchers::commit_voucher`
   takes the payer from there, never from the body.
2. ✅ **DTN replay protection.** Landed as migration `20261002_04_dtn_replay`
   (`dtn_seen` keyed on `(src_node_id, msg_id)`). Vouchers also carry their own
   monotonic nonces regardless.

---

## Build order

1. ✅ `services/voucher.rs` — encoding, signing, verification, nonce rules.
   Pure, fully testable offline.
2. ✅ Schema + `routes/vouchers.rs` — commit, present and list, IOU mode only
   (no escrow). End-to-end offline trade, settled by trust.
3. ✅ Delivery-condition discharge from the custody receipt chain
   (`POST /vouchers/:id/discharge`, reusing `services::custody`). Receipts
   already travel over DTN as sealed payloads; no transport work was needed.
4. Anchor program on Solana (escrow + claim) and the anchoring daemon.
5. Horizons, expiry, dispute surfacing in the console.
6. Shared-capsule manifests on top of per-item receipt chains.

Steps 1–3 are worth doing on their own: they make the marketplace usable
offline before any on-chain work exists, and step 2 is a complete, honest
product for a single organisation.
