# Cross-organisation federation — the org boundary model

**Status:** proposed for review, not implemented.
**Decision needed from you:** the trust shape (§2) and the crossing rules (§4).

---

## 1. The problem, concretely

Right now the whole fabric is one organisation under one trust root. Three
things follow, and the third is the one you hit today:

- `NODE_SHARED_SECRET` is fabric-wide. Any holder is any node.
- Every outpost's `node_registry` is a flat list of peers it will trust.
- **Each outpost has its own `users` table.** Your `ops@tidhq.net` exists on
  core and nowhere else; outpost B has its own `farm@tidhq.net`. There is no
  way to sign in once, no list of outposts to switch between, and no notion
  that two outposts belong to the same company.

That last part is not a bug — an outpost dark for three weeks cannot
authenticate you against an Earth database, so local credentials are the only
thing that works. But "local credentials" and "a different person at every
site" are not the same thing, and today the system cannot tell them apart.

A marketplace makes it sharper. Shipper, carrier, customs broker and receiver
are *different companies*. A marketplace where every participant shares a root
key is not a marketplace — it is one company pretending to be several.

---

## 2. Three shapes, and which I recommend

### (a) One root, orgs as a column

Add `org_id` to every table, keep the single fabric trust root, filter reads
by tenant.

Cheapest by a wide margin and genuinely fine for one company with many sites.
**Fails as soon as the parties are not the same legal entity**: a shared root
means whoever administers it can mint credentials for every org on it. No
customs broker will accept that, and neither should you.

### (b) Federated roots with explicit pairwise trust — *recommended*

Each organisation holds its own root keypair. An org's outposts carry a
certificate signed by that root. Two orgs that want to trade exchange root
public keys once, out of band, and each signs a **trust grant** naming the
other and what it is trusted for.

- Trust is explicit, bilateral, and revocable by either side alone.
- It verifies **offline**: an outpost holding Org A's root can check any Org A
  outpost's certificate with no network, which is the whole requirement.
- No third party exists to be unreachable, captured, or coerced.
- Cost is O(n²) relationships — but logistics partnerships are few, deliberate
  and contractual. You do not accidentally trade with 500 counterparties.

### (c) A central certificate authority

One authority signs every org root. Simplest to reason about, and the standard
answer on Earth.

Rejected: it reintroduces a party everyone must reach to establish trust. A
Mars outpost cannot consult a CA during conjunction, and a fabric whose trust
model has a single point of failure contradicts the premise of the product.

> **Recommendation: (b).** It is the only one of the three whose trust
> decisions survive a blackout, which is the property the rest of the system
> is built around.

---

## 3. What an organisation is

```
organisations
  id, name, root_public_key, created_at, status

org_members           -- a person's membership, within one org
  org_id, user_id, role          -- owner | admin | operator | viewer

org_outposts          -- which outposts belong to the org
  org_id, node_id, certificate   -- node cert signed by the org root

org_trust             -- what this org has granted another
  org_id, counterparty_org_id, counterparty_root_key,
  scopes[], granted_at, expires_at, revoked_at, grant_signature
```

A node certificate binds `(node_id, node_public_key, org_id, not_after)` and
is signed by the org root. The fabric guard changes from "is this node in my
registry" to "does this node present a certificate signed by a root I trust",
which is what makes a peer verifiable without having met it before.

---

## 4. What crosses the boundary — the decision that matters most

Default is **nothing**. Each scope is granted explicitly.

| Scope | Crosses? | Why |
|---|---|---|
| `marketplace` | opt-in per order | Posting an order to counterparties is the point. The order says what you need, not what you hold. |
| `custody` | yes, when sharing a shipment | Both parties need the same evidence. A chain only one side can see settles nothing. |
| `settlement` | yes | You cannot be paid by someone who cannot see the obligation. |
| `documents` | yes, per consignment | The receiving party needs the bill of lading. |
| `inventory` / `rollup` | **never** | What you hold, where, and how fast you burn it is the most commercially sensitive data in the system. A counterparty learning your oxygen reserve is thin learns exactly when to raise prices. |
| `telemetry` | **never** | Asset positions and condition. |
| `commands` / `actuators` | **never, under any grant** | One org must not be able to LOCKDOWN or ISOLATE another's outpost. Not configurable, not grantable — a hard boundary in code. |
| `rules` / autonomy | **never** | Policy that actuates is the same risk as commands. |

The last three are the ones to be unambiguous about. Everything else is a
business decision; those are a safety decision, and they should be impossible
to grant rather than merely off by default.

---

## 5. Identity — your "can I log into any outpost?" question

Today: no. Each outpost has its own `users` table and you need an account on
each. Under this model:

**Within an org.** The org issues you a signed credential binding
`(user_id, org_id, role, not_after)`. Any outpost carrying the org root can
verify it with no network, so one identity works at every site your org owns —
including one that has been dark for a month. Local password records become a
fallback for bootstrap and break-glass rather than the primary path.

Credential lifetime is the live trade-off. Short-lived means a dark outpost
eventually stops accepting you; long-lived means revocation is slow to bite.
Suggested starting point: 30 days, with revocation lists riding the existing
DTN so they propagate at whatever speed the link allows, and a local
break-glass admin that always works.

**Across orgs.** You are never a *user* of another org's outpost. You are a
counterparty, visible as an org identity on an order, a bid, a custody receipt
or a settlement — and nothing else. There is no cross-org login, deliberately.

---

## 6. What this changes in the existing code

Smaller than it sounds, because the hard part is already done.

- `require_auth` already resolves a `Principal`. It gains an `org_id` and the
  signature branch verifies a certificate chain rather than a registry lookup.
  **Principal propagation was the prerequisite** — without it there would be
  nowhere to put the org.
- `node_registry` keeps working as a cache of peers met; the authority for
  trust moves to `org_trust` + certificates.
- Every query over business data gains an org filter. This is the bulk of the
  mechanical work and the easiest thing to get wrong — one missed filter is a
  data leak across a commercial boundary.
- The rollup already refuses to show what it cannot reach; it additionally
  refuses to cross an org boundary at all.
- `NODE_SHARED_SECRET` is finally removable: certificates replace the one
  credential that cannot distinguish its holders.

## 7. Staging

1. **Organisations exist.** Table, membership, every existing row assigned to
   one default org. No behaviour change, entirely reversible.
2. **Org-scoped reads.** Add the filter everywhere, with tests that a second
   org sees nothing of the first. Still one trust root.
3. **Org roots and node certificates.** The guard verifies certificates;
   `FABRIC_AUTH` gains a `cert` mode so a running fabric rolls over the way it
   did for per-node signing.
4. **Trust grants and scopes.** Marketplace crossing first, since it is the
   one with a customer.
5. **Org-issued user credentials.** Single sign-on across your own outposts —
   the thing you asked for.
6. **Remove the shared secret.**

Steps 1–2 are worth doing regardless of which trust shape you pick, and they
are where the data-leak risk lives, so they deserve the most testing.

---

## 8. Open questions for you

1. **Trust shape** — (b) federated roots, or do you want (a) tenancy-only
   because the near-term customer is one company with many sites?
2. **Can an outpost belong to two orgs?** A shared port facility, a leased
   warehouse. Modelling says no and it keeps everything simple; reality
   occasionally says yes.
3. **Credential lifetime** for the dark-outpost case — is 30 days right for a
   Mars-side worker, given revocation travels by DTN?
4. **Does an org have an on-chain identity?** If TIDAT settlement is per-org
   rather than per-user, `org` wants a wallet and the settlement verifier
   checks the org's address.
