# Damage Evidence — Design & Build Plan

**Status:** implemented (2026-11-15) — steps 1–3 of the build order below. The
signed report, the schema/endpoints, and the settlement + voucher-discharge
suspension are built and tested. Still open: auto-sending the report over DTN
on file (step 4) and surfacing it in the dispute document (step 5).
**Depends on (all built):** node identity / Ed25519 signing (`services::identity`),
custody receipts (`services::custody`), the settlement verifier and voucher
lifecycle (`services::settlement`, `services::voucher`), DTN (`routes::dtn`),
and order-party authorization (`routes::documents::require_order_party`).

---

## The problem

Goods arrive damaged in a place with no reliable link, and the two parties are
different organisations who will not be in contact when it happens. "It arrived
broken" is worth nothing in a dispute a week later on another outpost. What is
needed is evidence that is **authored, timestamped, tamper-evident, and able to
travel over a blackout** — and that *stops the money* until it is resolved.

Everything required already exists in pieces: custody receipts are signed by the
receiving party and carry a `condition` field (`ok|damaged|short|contaminated`),
settlement and voucher discharge are gated state machines, and DTN already
carries signed payloads across partitions. This feature wires them into one
loop: **signed damage report → suspended settlement → resolution**.

## The primitive: a signed damage report

A damage report is a signed statement by a party to the order that a
consignment arrived in a stated condition, binding a hash of the supporting
evidence (photos, a sensor log) rather than the blob itself.

```
damage_report {
  id, order_id, fulfillment_id, lot_id?
  reporter_node_id, reporter_user_id
  condition      // damaged | short | contaminated | spoiled
  severity       // minor | major | total
  quantity_affected?  numeric
  description
  evidence_hash  // sha256 of the evidence file/bundle, computed client-side
  evidence_uri?  // optional pointer (kept out of the DB; the hash is the anchor)
  occurred_at
  reporter_sig   // Ed25519 over the canonical encoding
  status         // open | resolved | withdrawn
  resolution?, resolved_by?, resolved_at?
}
```

**What it proves and what it does not.** The signature proves *who* filed the
report, *when*, and that the referenced evidence has not changed since. It does
**not** prove the photo is of this shipment — that is not cryptographically
knowable, and the doc must say so, the same honesty bar the rest of the codebase
holds. "Authored and tamper-evident" is defensible; "proof of damage" is not.

Canonical encoding follows `custody::canonical_receipt` exactly — length-
prefixed fields, a `damage|v1` domain prefix so a damage signature can never be
reinterpreted as a custody receipt, voucher, or DTN envelope.

## The settlement hook (the point of the feature)

An **open** damage report on an order suspends value transfer:

- `orders::mark_settled` refuses with `409 settlement_suspended` while an open
  report exists on the fulfilment — money does not move onto contested goods.
- `voucher::conditions_met` / `vouchers::discharge_voucher` treat an open report
  as an unmet condition, so an offline voucher cannot be claimed either.
- Resolving the report (`resolved` / `withdrawn`) lifts the suspension. A
  resolution may record a partial credit, which the settling party honours by
  settling the agreed amount — the system records the dispute and its outcome,
  it does not adjudicate it.

This mirrors `compliance_holds`, which already blocks a shipment the same way;
a damage report is the delivery-side analogue of a pre-shipment hold.

## Travelling the blackout

A filed report is sent to the counterparty's outpost as a **sealed, signed DTN
payload** via the existing `routes::dtn` send path — no new transport work. So
the evidence accumulates across a partition and is present on both sides when
the link returns, exactly like a custody receipt.

## Schema

One migration, following existing conventions:

```sql
CREATE TABLE damage_reports (
  id uuid PK, order_id uuid REFERENCES orders, fulfillment_id uuid REFERENCES fulfillments,
  lot_id uuid REFERENCES inventory_lots NULL,
  reporter_node_id uuid NOT NULL, reporter_user_id uuid REFERENCES users NOT NULL,
  condition text CHECK (condition IN ('damaged','short','contaminated','spoiled')),
  severity  text CHECK (severity  IN ('minor','major','total')),
  quantity_affected numeric, description text,
  evidence_hash text NOT NULL, evidence_uri text,
  occurred_at timestamptz NOT NULL DEFAULT now(),
  reporter_sig text NOT NULL,
  status text NOT NULL DEFAULT 'open' CHECK (status IN ('open','resolved','withdrawn')),
  resolution text, resolved_by uuid REFERENCES users, resolved_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now()
);
-- Suspension lookup: is there an open report on this order/fulfilment?
CREATE INDEX idx_damage_open ON damage_reports (fulfillment_id) WHERE status = 'open';
```

## Code changes

- **`services/damage.rs`** (new) — pure: `DamageClaim`, `canonical_damage`,
  `sign_damage`, `verify_damage`, in the style of `services::custody` /
  `services::voucher`. Unit-testable with no DB.
- **`routes/damage.rs`** (new, nested under `/orders`):
  - `POST /orders/:id/damage` — file a report (party-authorized via the existing
    `require_order_party`). Server signs with the outpost identity, stores,
    and enqueues a DTN bundle to the counterparty.
  - `GET /orders/:id/damage` — list reports (party-only).
  - `POST /orders/:id/damage/:rid/resolve` — resolve or withdraw (party/admin),
    recording `resolution`, `resolved_by`, `resolved_at`.
- **`routes/orders.rs::mark_settled`** — refuse with `409 settlement_suspended`
  when an open report exists on the fulfilment.
- **`routes/vouchers.rs::discharge_voucher`** — same check before discharge.
- **`routes/documents.rs::gather`** — surface `damage` alongside `custody` so a
  dispute document includes the signed evidence (optional, nice-to-have).

## Build order

1. `services/damage.rs` — encoding, sign, verify, nonce-free (reports are not
   ordered). Pure, fully testable offline.
2. Schema + `routes/damage.rs` — file / list / resolve, party-authorized.
3. The suspension hooks in `mark_settled` and `discharge_voucher`.
4. DTN send on file, so the evidence reaches the counterparty.
5. (Optional) include damage in the dispute document.

Steps 1–3 are the useful core: a signed report that stops the money. Backend
only, open-source-appropriate (not the private line).

## Tests

- Unit (`services::damage`): sign/verify roundtrip; tampering any field
  (condition, severity, evidence_hash, quantity) invalidates; another key fails.
- Integration: a party can file a report and a stranger cannot (`403`); an open
  report blocks `mark_settled` and `discharge_voucher` (`409`); resolving lifts
  the block; the stored signature verifies against the reporter's key; the
  evidence hash round-trips unchanged.

## Honest limitations

- The signature binds the *evidence file's hash*, not the physical truth of the
  damage. State this in the product copy: tamper-evident and attributable, not
  proof of condition.
- Resolution is recorded, not adjudicated — there is no automatic arbiter, and
  there should not be one.
