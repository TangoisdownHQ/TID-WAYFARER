# Building out TIDAT — what to do, in what order

Companion to [Token.md](./Token.md), which says what exists. This says how to
get from there to a working settlement rail, and — just as important — which
items on the "not built" list are worth building at all.

The sequence matters more than the list. One step is nearly free and unblocks
everything else; one is weeks of work and needs a decision from you before a
line of it is written; and several are conventional token features that would
add surface area without making a single shipment arrive.

| # | Step | Effort | Blocks what follows? |
|---|---|---|---|
| 0 | Mint TIDAT on devnet | **hours** | yes — everything |
| 1 | Pay the organisation, not the employee | half a day | no, but cheap and wrong today |
| 2 | Payment initiation via Solana Pay | days | no |
| 3 | Escrow (Anchor program) | **weeks** | needs a decision from you first |
| 4 | Fee sponsorship | days | no |
| — | Staking, DAO, burn, NFT proofs, cross-chain | — | **don't** — see the end |

---

## Phase 0 — Mint TIDAT on devnet

**This is the highest-value thing you can do and it takes an afternoon.** The
verification path is finished and tested; it has nothing to verify against.
Mint a devnet token and the whole settlement flow goes live end-to-end.

```bash
solana config set --url devnet
solana-keygen new -o treasury.json          # keep this somewhere real
solana airdrop 2

# 6 decimals, matching the SPL convention USDC uses. Your bid prices are
# plain numbers (2640), and the verifier reads uiAmountString, so decimals
# are applied for you — 2640 compares against 2640 TIDAT, not base units.
spl-token create-token --decimals 6 --fee-payer treasury.json

spl-token create-account <MINT>
spl-token mint <MINT> 1000000
```

Then, on the outpost:

```bash
SOLANA_RPC_URL=https://api.devnet.solana.com
TIDAT_MINT=<MINT>
```

Restart and the verifier leaves `NoRpc`. Transfer some TIDAT to a shipper's
wallet, settle against that signature, and watch the order go `settling` →
`settled`. That is the first time the system will have confirmed a real
payment.

### Two decisions that are hard to walk back

**Freeze authority must be null.** `spl-token create-token` sets none by
default — keep it that way, and do not add one later. A freeze authority means
the issuer can freeze a carrier's payout account. No serious counterparty
accepts payment in a token whose issuer can do that, and discovering the
authority exists later is worse than it never existing.

```bash
spl-token authorize <MINT> freeze --disable    # if one ever got set
```

**Mint authority needs a plan before mainnet.** An unlimited-mint settlement
token is not a settlement token — whoever holds the key can pay for shipments
with nothing. Either revoke it after the initial supply, or move it to a
multisig. Devnet is the place to practise this, not the place to skip it.

```bash
spl-token authorize <MINT> mint --disable      # irreversible
```

### What you will immediately learn

Settle one order and the gaps surface in the right order. Expect to hit
Phase 1 within the hour: the payee is the shipper's personal wallet.

---

## Phase 1 — Pay the organisation, not the employee

No chain work. Half a day. It is a bug, not a feature.

`organisations.wallet_address` is recorded and never read. The settlement payee
is `users.wallet_address` for whichever person shipped the order. So a
company's revenue lands in an employee's wallet, and an employee who leaves
takes the payout address with them.

**The fix:** resolve the payee as `org wallet → else user wallet → else
refuse`, in `apps/api/src/routes/orders.rs` where the expectation is built.
Keep the user fallback, because a sole trader is a real participant and has no
organisation wallet to use.

**Watch for:** the `412` precondition must still fire when *neither* exists,
and the integration test for it should assert the precedence, not just that
something was found. Add it to `apps/api/tests/` alongside the settlement
cases.

---

## Phase 2 — Payment initiation, without a build step

Right now a human copies an 88-character base58 signature into a form. That is
the single worst piece of UX in the application, and it is also a data-entry
error waiting to happen.

The conventional answer is `@solana/wallet-adapter` plus
`@solana/web3.js` — roughly a megabyte of JavaScript and a bundler. Your
frontend is deliberately vanilla ES modules with **no build step**, and the
product's whole stance is offline-first. Importing that stack would be the
largest architectural concession in the codebase, for one screen.

**Use Solana Pay transfer requests instead.** They are a URL:

```
solana:<payee>
  ?amount=2640
  &spl-token=<TIDAT_MINT>
  &reference=<unique pubkey>
  &label=TID%20Wayfarer
  &message=Order%20a1b2c3%20%E2%80%94%202%20pallets%20to%20Jezero
```

Render that as a QR code. The operator scans it with Phantom or Solflare on
their phone, confirms, and pays. No JavaScript dependency, no bundler, any
wallet, and the realistic device in a loading bay is a phone rather than the
depot terminal.

**The `reference` is the part that earns its place.** It is a unique public key
included as a read-only account in the transfer, which means the server can
*find the payment itself*:

```
getSignaturesForAddress(<reference>)  →  the signature
```

So nobody transcribes anything. The operator pays; the next verifier pass
discovers the transaction and settles the order.

### What to build

1. **`settlement_reference` column** on `fulfillments`, plus a generated
   keypair's public key per settlement intent. Unique — it is how a payment is
   matched, so two intents sharing one reference would match each other's
   payments.
2. **`GET /api/fulfillments/:id/payment-request`** returning the URL, the
   amount, the mint and the reference. Org-scoped, and only for a party to the
   order.
3. **QR rendering.** You already *read* QR codes (`makeScannable`); generating
   them is a small self-contained routine — draw to a canvas, no library, and
   certainly nothing from a CDN given the CSP on this frontend.
4. **Reference resolution in `settlement.rs`** — a second lookup path beside
   the signature one. `getSignaturesForAddress`, take the confirmed one, then
   run the existing `verify_transaction` against it unchanged. The three
   expectations and the four outcomes all still apply; only the way the
   signature is discovered changes.
5. **Keep the paste-a-signature path.** It is the fallback when the reference
   lookup cannot reach the chain, and it is how an operator records a payment
   made out of band.

### The offline caveat, stated plainly

Both paths need chain access *eventually*, and the QR path needs the payer to
have connectivity at the moment of paying. On a dark outpost neither works.
That is not a gap in this phase — it is what [Offline
Settlement](./OfflineSettlement.md) exists for, and it is a different project.

---

## Phase 3 — Escrow

This is the real work and the only item that needs a Solana program. Weeks, not
days. **Do not start it until you have answered the question below**, because
the answer changes the program's shape.

### Why it matters

Today `settled` means "the payment happened". It says nothing about whether the
goods arrived — delivery is attested separately by the custody chain, and
neither is conditional on the other. So somebody extends credit: the buyer who
pays before delivery, or the carrier who delivers before payment. They choose,
out of band, every single time.

Escrow removes that choice. Funds lock on acceptance; the carrier can verify
they exist before loading; release follows delivery.

### The decision you have to make: who authorises release?

| Option | How it works | Trade-off |
|---|---|---|
| **A. Buyer releases** | Funds lock; buyer signs a release on delivery; deadline refunds the buyer if they never do | Simplest, no oracle, no new trusted party. A buyer can still stall — but the carrier *knows the money is there*, which is most of the value. |
| **B. Outpost releases** | The outpost's key signs a release when the custody chain shows delivery | Automatic, and it makes the outpost a custodian of other people's money. On a federated fabric where outposts are run by different companies, this is a large new trust assumption. |
| **C. 2-of-3 multisig** | Buyer + carrier normally; an arbiter breaks ties; deadline refunds | Correct, and the most work. Needs a dispute flow and someone willing to arbitrate. |

**Recommendation: build A, design for C.** A is a complete improvement on
today with no new trusted party, and it is a fraction of the work. Leave the
release authority as a program parameter so C is a later change rather than a
rewrite. Avoid B — the moment an outpost can move customer funds, running one
becomes a regulated activity and a liability, and the federation stops being
a federation of equals.

### Program sketch (Anchor)

```
initialize(order_id, amount, mint, payee, release_authority, deadline)
fund()            // buyer transfers TIDAT into the escrow ATA
release()         // release_authority signs → payee
refund()          // after deadline, anyone can trigger → buyer
```

Escrow PDA seeded on `order_id` so there is exactly one per order and the
address is derivable without storing it. Nothing in the program should know
about shipments — keep it a dumb vault with an authority and a clock, and let
Wayfarer hold the logistics meaning.

### What changes in Wayfarer

- A new workspace member — `packages/contracts/` or similar. This is the first
  Anchor code in the repo, so it brings `anchor-lang`, a Solana toolchain in
  CI, and a localnet test harness. Budget for that, not just the program.
- `fulfillments` gains escrow state: PDA, funded amount, release/refund tx.
- The verifier gains a second thing to watch: not "was this transfer made" but
  "is this escrow funded / released".
- `services/settlement.rs` stays almost intact. Verifying a release is still
  "payee received at least amount of mint in a confirmed transaction" — the
  three expectations do not change.

### Before you write any of it

A program holding customer funds is a different legal animal from a system that
verifies payments. In most jurisdictions escrow of third-party money is a
regulated activity, and a token used to settle B2B payments raises
money-transmission questions on its own. I can't advise you on that, and you
should not take this document as having cleared it — but it is far cheaper to
ask before building escrow than after. Phases 0–2 do not hold anyone's money
and are a much lighter question.

---

## Phase 4 — Fee sponsorship

Not "gasless" in the token-marketing sense. The concrete problem: a depot lead
paying in TIDAT needs SOL for the transaction fee, and acquiring SOL is a
second onboarding problem in a second asset for every participant you want on
the marketplace.

**What it is:** a fee-payer service. The outpost holds a small SOL balance,
builds the transfer with itself as fee payer, partially signs, and hands the
transaction to the payer to add their signature. Octane is the reference
implementation of the pattern.

**Where it fits:** after Phase 2, because it sponsors the transaction Phase 2
constructs. Before Phase 3 if onboarding friction is what is actually blocking
adoption — that is a product question, not a technical one.

**What to guard:** a fee payer is a faucet. Rate-limit per account, only
sponsor transfers of your own mint to a payee that is a known participant, and
cap the daily spend. An unguarded sponsor is drained within a day of anyone
noticing.

---

## What I would not build

Each of these is on the "not built" list. Not everything on that list deserves
to come off it.

**Staking.** Prioritising sync, delivery or data access by stake makes the
network worse at its job: the outpost that most needs its resupply order seen
is the one with the least capital. The fabric already has a better
prioritisation signal — what is actually running out — and it is in the
forecast module.

**DAO governance.** Nothing in the current product has a decision that a vote
would improve. Voting on parameters nobody has yet found a reason to change is
machinery for its own sake. If a genuine multi-party decision appears — fee
splits on shared capsules, say, or who arbitrates escrow disputes — build
governance *for that decision*, then.

**Burn / deflationary mechanics.** A settlement rail wants a boring, stable
unit. Supply mechanics that make the unit appreciate make it a worse medium of
exchange: nobody wants to pay a carrier in something they expect to be worth
more next month, and the carrier does not want to quote in it. These two goals
genuinely conflict, and the marketplace is the one you are building.

**NFT inventory proofs.** The catalogue and the custody chain already answer
"what is this part" and "who held it when" — queryably, org-scoped, revocable,
and free. An NFT per lot adds rent and a mint fee per unit and answers neither
question better. The columns (`inventory.token_id`, `assets.nft_token`,
`packages.nft_token`, `users.nft_token_id`) are stored and returned and
otherwise unused. I would either drop them or comment them as inert, because
an unused column in a schema reads as a feature.

**Cross-chain.** `fulfillments.settlement_chain` already exists and is written
as the literal `'solana'`. The column is the right amount of preparation. Add
a second implementation when a customer tells you which chain they need, not
before — each one is a full verification path with its own failure modes.

None of this is an argument against TIDAT. It is an argument that the token's
value here is being *the rail* — the thing a bid is quoted in and a payout
lands in, between parties with no other rail in common — and that every one of
the features above makes the rail worse at being a rail.

---

## Where this joins the offline work

[OfflineSettlement.md](./OfflineSettlement.md) names two prerequisites that
blocked a correct implementation. **Both are now done:**

- Authenticated-principal propagation — `Principal` is inserted into request
  extensions and every handler can ask who called it.
- DTN replay protection — the envelope binds a message id, a lifetime and the
  recipient under the signature; reception is idempotent.

So its build order is open. Note that its steps 1–3 (vouchers, commit/present,
custody-receipt discharge) **need no chain at all** and make the marketplace
usable offline before any of Phase 3 exists. If your users are more often dark
than they are unfunded, that work is worth more than escrow.

---

## The shortest useful path

If you do one thing: **Phase 0.** Mint on devnet, set two environment
variables, settle one real order. It costs an afternoon, it converts a
well-tested code path into a working payment system, and everything you learn
from it will reorder the rest of this document.

If you do three: Phase 0, Phase 1, Phase 2. That is a marketplace where a
buyer scans a code, pays from their phone, and the order settles itself —
without a bundler, a browser wallet, or an on-chain program.
