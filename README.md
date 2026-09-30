<p align="center">
  <img src="docs/banner.svg" alt="TID Wayfarer — a logistics ecosystem for operations spread across places that can't rely on a network. One view of every resource across a farm, a port, Earth HQ, the Moon and Mars." width="100%">
</p>

# TID Wayfarer 🚀

[![Rust](https://img.shields.io/badge/Rust-stable-orange?logo=rust)](https://www.rust-lang.org/)
[![Build Status](https://img.shields.io/github/actions/workflow/status/TangoisdownHQ/TID-WAYFARER/rust.yml?branch=main)](https://github.com/TangoisdownHQ/TID-WAYFARER/actions)
[![License](https://img.shields.io/github/license/TangoisdownHQ/TID-WAYFARER)](./LICENSE)
[![Contributions Welcome](https://img.shields.io/badge/contributions-welcome-brightgreen.svg)](./Documentation/CONTRIBUTING.md)
![CyberSpaceOps](https://img.shields.io/badge/Secured_by-Tangoisdown_Systems-1b5e20?logo=shield&logoColor=white)
![TangoisdownHQ](https://img.shields.io/badge/TangoisdownHQ-Cyber_Intelligence-002b36?logo=linux&logoColor=white)

**TID Wayfarer is a logistics ecosystem for operations spread across places that
can't rely on a network.**

Know what resources you have, everywhere you have them. Reorder what's running
low without paperwork, in either direction — Earth to orbit, orbit to Earth,
warehouse to farm. Share capsule space with other shippers instead of paying
for a whole one. Keep doing all of it while the link is down.

A farm co-op tracking seed and fertilizer across six fields has the same
problem as a lunar outpost tracking oxygen and spares: inventory in many
places, replenishment that takes time, partners who need to see the same
picture, and connectivity that can't be assumed. TID Wayfarer treats those as
one problem.

> 📖 Platform vision, token model, and module deep-dive:
> [Documentation/Description.md](./Documentation/Description.md)

---

## Why it's built this way

Every deployment is an **Outpost** — a sovereign node (Earth HQ, a farm office,
a Moon base, Mars-Jezero, a ship in transit) running the identical core, holding
its own Ed25519 identity and its own database.

That means an outpost keeps working when it's cut off. It doesn't degrade to
read-only, it doesn't queue up at a central server, and it doesn't pretend the
data it's showing you is fresh when it isn't. Three design rules follow from it,
and they show up everywhere in the code:

1. **Store and forward, never block.** Messages, commands, and settlements are
   all recorded locally the moment they're made, then converge when a route
   exists. Same shape in all three.
2. **State carries its age.** A reading without a timestamp is misleading, so
   time-since-contact is a first-class field, not a footnote. The operator
   console leads with it.
3. **Never claim what wasn't checked.** "Delivered" and "settled" are different
   words for a reason, and so are "unsupported" and "failed". A settlement that
   couldn't be confirmed is recorded as unconfirmed — not waved through.

---

## 🧩 Modules

### 📦 SupplyLink — Inventory, Orders & the Capsule Marketplace
**Status: Core implemented ✅** (capsule sharing + escrow pending)

The logistics layer. Inventory, packages, kits, and an asset registry, plus a
working marketplace:

- **Post an order** — "100 kg of seed, delivered to Jezero by April" — with
  quantity, units (kg/L/m³/each), a delivery body (NAIF id) and coordinates, a
  deadline, and a price ceiling.
- **Receive competing bids** with price and estimated transit days.
- **Accept one**, then follow ship → deliver → settle, with settlement state
  spelled out rather than assumed.
- Assignment workflow (`/api/supplylink`) for delivery tracking.
- Operator UI at `/ui/market.html`.

Pricing is denominated in **TIDasToken** — because a marketplace spanning a
comms blackout needs value transfer that can be committed locally and settled
later, which is something conventional payment rails cannot do.

Planned: **shared capsules** (multiple orders consolidated into one shipment
with a manifest and split settlement), escrow contracts, IPFS-backed inventory
proofs.

### 🛰️ AstroNet — Resource Registry & Cross-Domain Coordination
**Status: Tracking + resource rollup implemented ✅** (replication + ledger pending)

Where everything is, what condition it's in, and what it holds:

- **One view of every resource, everywhere** — `GET /api/rollup/inventory`
  fans signed reads out to every registered outpost, merges them, and reports
  fabric-wide totals with a per-location breakdown. Operator UI at
  `/ui/resources.html`.

  Two things it refuses to do, both deliberate. It will not present a total as
  a count when a site is out of contact: the response and every affected line
  item carry `complete: false`, because an operator reordering against a
  number that silently omitted three sites is the failure this whole product
  exists to prevent. And it will not report only a sum — 500 L spread over six
  locations is not 500 L you can use anywhere, so the breakdown travels with
  the total. An item can be healthy fabric-wide and still be out at the site
  that needs it, which the reorder flag tracks per location.
- Celestial bodies registry (`/api/bodies`) — assets live on Earth, Moon, Mars,
  ISS, or in transit between them.
- Fleet asset tracking with live telemetry ingestion, anomaly scoring, and
  automatic response (anomaly → diagnostic snapshot, tamper → lockdown,
  malware → network isolation).
- Geo-dashboard endpoints (`/api/map`, `/api/map/fleet`) including per-asset
  movement trails as GeoJSON.
- Peer node registry and sync daemon (`/api/nodes`, `/api/nodes-sync`).

Planned: **replication**, so a dark site's last-known stock still counts with
an age attached rather than dropping out of the total; automatic reorder when
stock crosses its threshold; IPFS event ledger; DAO voting.

### 🔐 CommSec — Secure Communications
**Status: Implemented ✅**

- Post-quantum **ML-KEM-1024** for key agreement, AES-256-GCM for payloads
  (`apps/api/src/routes/commsec.rs`). `/keypair` returns the **public** key
  only — the secret never leaves the process.
- **Sealed DTN** — payloads are encrypted for the recipient, not merely signed:
  ML-KEM-1024 encapsulation → HKDF-SHA256 → AES-256-GCM, with sender and
  recipient node ids as associated data so a sealed payload can't be replayed
  as though it came from, or was addressed to, a different node. The envelope
  is Ed25519-signed over the *ciphertext*, so a peer authenticates the sender
  before spending work on decryption.

  Payloads are opaque both in transit and **at rest** in `dtn_outbox` — which
  matters because a message can sit queued for days across a blackout. That
  makes it an at-rest problem, not just a transit one.
- **Delay-Tolerant Networking** — `POST /api/dtn/send` queues per destination;
  the forwarder retries with exponential backoff until the outpost comes back
  into contact, landing payloads in the peer's inbox.
- Each outpost's ML-KEM keypair persists (`keys/commsec.json`) and is exchanged
  during node registration alongside the Ed25519 identity. A peer on an older
  build with no KEM key on file still receives plaintext, flagged via
  `dtn_outbox.encrypted`, so a partially-upgraded fabric keeps working.
- End-to-end test harness: `scripts/test_commsec.sh`.

### 💠 TIDasToken (TIDAT)
**Status: Settlement verification implemented 🧱** (on-chain escrow pending)

The unit of account for the marketplace — payments, staking, governance, and
rewards. Developed in the sibling `TIDasTOKEN/` project; Solana primary.

In-core today: a settlement verifier daemon confirms a recorded payment
actually credited the payee the expected amount of the expected token before an
order reaches `settled`, and `blockchain_feeder` normalizes chain events into
threat/alert records (`/api/bc`).

Planned: **offline settlement** — locally-committed conditional transfers that
anchor to chain on a settlement horizon. A Mars outpost cannot participate in
Solana consensus (light delay is minutes; slot time is milliseconds), and a
ship with no uplink for three days has the same problem for a different
reason, so the chain has to be an eventual anchor rather than the transaction
medium. Design: [Documentation/OfflineSettlement.md](./Documentation/OfflineSettlement.md).

---

## 🏗 Architecture

One Rust/Axum binary (`tid-wayfarer`) per outpost, backed by PostgreSQL:

- **API layer** — 20+ route groups under `/api` (inventory, assets, fleet,
  orders, fulfillments, supplylink, users, nodes, peers, commands, rules, ops,
  dashboard, map, bodies, dtn, fabric…).
- **Background daemons** — peer sync, telemetry processor, command engine, DTN
  forwarder, self-registration, blockchain feeder, settlement verifier. All
  fail soft: a missing table or unreachable peer never kills the API, and
  everything that crosses the network retries with exponential backoff.
- **Autonomy** — telemetry is evaluated against DB-driven policies
  (`/api/rules`) with per-asset cooldowns; firings queue commands and are
  auditable at `/api/ops/events`.
- **Command actuators** — a pushed command *does* something. The receiver
  dispatches to a handler registry (`LOCKDOWN`, `UNLOCK`, `ISOLATE_NETWORK`,
  `REJOIN_NETWORK`, `DIAGNOSTIC_SNAPSHOT`) whose effects persist in
  `outpost_state`, and reports the outcome, which core records as the ack. An
  unimplemented command answers `unsupported` rather than silently claiming
  success. A locked-down outpost refuses mutating requests with `423` while
  still serving reads and `/commands/execute`, so `UNLOCK` can reach it.
- **Signed telemetry** — a node with a registered `hmac_secret` must send a
  valid `X-Telemetry-HMAC` over the body; unsigned telemetry from that node is
  rejected before it can drive autonomy. Rotate via
  `POST /api/nodes/:id/telemetry-secret` (admin).
- **Per-node fabric auth** — peers authenticate with an Ed25519 signature over
  `fabric|METHOD|path|timestamp|sha256(body)`. Binding method, path and body
  means a captured signature can't be replayed onto another route or a mutated
  payload; the timestamp bounds reuse to 5 minutes. `FABRIC_AUTH` selects
  `both` / `signed` / `legacy` so a running fabric can be rolled over
  node-by-node.
- **Key rotation & revocation** — `POST /api/nodes/:id/revoke` makes the guard
  refuse a node's signatures immediately; `rotate-key` swaps its Ed25519 key,
  requiring a signature from the **new** key so a rotation can't strand a node
  with a key nobody holds. Rotation clears a revocation — that's the recovery
  path — and every change lands in `node_key_history` with the admin who made
  it.
- **Observability** — structured `tracing` logs (`RUST_LOG`, `LOG_FORMAT=json`).
  Every request carries a `request_id`; every telemetry row mints a `trace_id`
  stamped onto each ops event and queued command it causes, travelling inside
  the pushed command so the executing outpost logs the same id. One
  `WHERE trace_id = …` returns the whole causal chain: telemetry → rule firing
  → command → execution. `GET /metrics` exposes Prometheus counters and gauges,
  outside the auth guard so a scraper needs no fabric credential.
- **Operator console** — `/ui/console.html` answers "what needs me right now?"
  A time-since-contact rail sits above everything and states plainly whether
  the readings below can be trusted. Beyond ok/warn/bad the palette carries a
  fourth state — *unknown because out of contact* — which is the characteristic
  condition here and is not a fault. A "needs a decision" list surfaces only
  genuinely actionable states, and a trace box expands any `trace_id` into its
  causal chain. Self-contained, no CDN.
- **Kubernetes-native** — Helm chart (`deploy/helm/tid-wayfarer`) and a Go
  operator with an `Outpost` CRD (`deploy/operator`) declare outposts as
  cluster resources.

```
TID-WAYFARER/
├── apps/api/                # tid-wayfarer core (Rust, Axum, sqlx)
│   └── src/
│       ├── main.rs          # entrypoint: router + daemons
│       ├── routes/          # API route groups
│       └── services/        # daemons, identity, autonomy, crypto, settlement
├── packages/db/migrations/  # PostgreSQL schema migrations
├── shared/static/           # operator console + marketplace UI
├── deploy/
│   ├── helm/tid-wayfarer/   # Helm chart
│   └── operator/            # Go operator + Outpost CRD
├── scripts/                 # wait-for-db.sh, test_commsec.sh, helpers
├── liboqs/ liboqs-python/   # post-quantum crypto (submodules)
└── docker-compose.yml       # local multi-outpost stack
```

---

## 🛠 Build & Run

Requirements: latest stable [Rust](https://www.rust-lang.org/tools/install),
`cargo`, PostgreSQL (or Docker), `jq`, `curl`.

```bash
cp .env.example .env   # DATABASE_URL, JWT_SECRET, OUTPOST_NAME, …
SQLX_OFFLINE=true cargo run --bin tid-wayfarer   # port 3000
```

Local multi-outpost stack (core + outpost B + mesh relay + Postgres):

```bash
docker compose up -d
```

Kubernetes:

```bash
helm install tid-wayfarer deploy/helm/tid-wayfarer -f deploy/helm/tid-wayfarer/values-core-hq.yaml
# or operator-managed outposts:
kubectl apply -k deploy/operator/config/default
kubectl apply -f deploy/operator/config/samples/
```

Quick checks:

```bash
curl localhost:3000/api/health    # {"status":"ok","service":"..."}
curl localhost:3000/api/outpost   # identity (name, role, bodyId, region)
open  localhost:3000/ui/console.html
```

---

## 🔬 Testing

```bash
SQLX_OFFLINE=true cargo test --lib   # unit tests
./scripts/test_commsec.sh            # PQC end-to-end
```

`test_commsec.sh` validates KEM keypair generation, encapsulation/decapsulation
(shared-secret agreement), AEAD round-trips with and without Associated Data,
and rejection of tampered AD.

---

## 📍 Roadmap

Shipped:

- [x] CommSec PQC KEM + AEAD API
- [x] Post-quantum sealed DTN payloads (ML-KEM-1024 + AES-256-GCM)
- [x] Node identity (Ed25519) + peer sync daemon + self-registration
- [x] Per-node Ed25519 request signing, key rotation, revocation with audit history
- [x] Fleet telemetry pipeline with autonomous response
- [x] Command actuators + delivery acks (LOCKDOWN actually locks down)
- [x] Rules engine: DB-driven autonomy policies with per-asset cooldowns
- [x] SupplyLink core: inventory, orders, bids, fulfillment, assignments
- [x] Settlement verification (payee + amount + mint confirmed before `settled`)
- [x] Operator console + marketplace UI
- [x] Auth on all machine-facing routes; role-based access from `users.role`
- [x] Structured logging & metrics (tracing, request/trace correlation, `/metrics`)
- [x] Helm chart + Kubernetes operator (`Outpost` CRD)
- [x] Cross-location resource rollup (`/api/rollup/inventory`) — read-only,
      provenance-carrying, honest about dark sites

Next — the logistics vision, in dependency order:

- [ ] **Replicated resource view** — the rollup above answers "what do we have,
      everywhere" by fanning out live reads, which is honest but only works for
      sites currently in contact. Replication would let a dark site's
      last-known stock still count, with an age attached.
- [ ] **Automatic replenishment** — low stock is *detected* per-outpost
      (`GET /api/inventory/low-stock`, and the rollup flags it fabric-wide) but
      nothing acts on it. Crossing a threshold should draft an order rather
      than wait for someone to notice.
- [ ] **Shared capsules** — a manifest consolidating several orders into one
      shipment, with proportional settlement and per-item custody. The single
      most-requested marketplace mechanic and not yet modelled.
- [ ] **Offline settlement** — locally-committed conditional transfers with a
      settlement horizon, so a trade can close where no chain is reachable.
      ([design](./Documentation/OfflineSettlement.md))
- [ ] **Partition reconciliation** — what happens when two disconnected
      outposts both allocate the last unit. Today there is no cross-outpost
      state replication, so the conflict can't even be detected.
- [ ] **Cross-organization federation** — the fabric currently assumes one org
      under one trust root; a real marketplace has independent parties.
- [ ] Rules engine v2: compound conditions (AND/OR), rate-of-change triggers
- [ ] SupplyLink escrow contracts + IPFS inventory proofs
- [ ] AstroNet event ledger + DAO voting

---

## 📜 License

MIT or Apache-2.0 (to be decided).

## 🌍 Community

Looking for early adopters, testers, and collaborators:

- Developers interested in logistics, distributed systems, post-quantum
  security, or space systems.
- Operators with distributed inventory and unreliable connectivity — farms,
  ships, remote sites, research stations.
- Companies moving payloads between Earth and orbit who want capsule space
  brokered rather than bought whole.
