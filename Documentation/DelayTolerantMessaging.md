# Delay-Tolerant Messaging: the envelope, and why it is shaped like this

How one outpost hands a message to another when the link between them may be
down for days — and what stops that message being used twice.

This document is both an explanation and a specification. If you are
implementing a peer, the [wire format](#the-wire-format) is normative; the rest
is the reasoning, which matters because several of the fields look redundant
until you know what their absence cost.

---

## The problem store-and-forward creates

A normal API call fails when the far end is unreachable, and the caller retries
or gives up. An outpost on Mars does not get that choice: the link is *expected*
to be down, so a message is written to a queue, and something delivers it
whenever contact resumes. `POST /api/dtn/send` enqueues; a forwarder daemon
drains the queue with exponential backoff.

That design has a consequence people usually discover in production. **The
forwarder cannot tell a lost message from a lost acknowledgement.** It retries
on any non-2xx *and* on a dropped connection, so a bundle that arrived
perfectly well — but whose response never made it home — is sent again.

So duplicates are not an attack. They are ordinary operation. Any receiver that
cannot recognise a message it has already seen will process some messages
twice, and on a bad link, many times. For a chat message that means a
duplicate. For a custody transfer or an inventory movement it means doing the
thing twice.

Which is also, exactly, the shape of a replay attack. The honest case and the
hostile case are indistinguishable at the door — so they get the same defence
and the same answer.

---

## What the envelope used to be

```
signed bytes = "dtn|" + src_node_id + "|" + payload_json
```

That proves two things: *who wrote this*, and *that the body has not been
altered*. Both worth having. But it says nothing about **which** message this
is, **when** it was written, **how long** it remains valid, or **who** it was
addressed to. Four absences, four consequences:

| Missing | What it allowed |
|---|---|
| message id | The same envelope verified every time it was posted. No bound on how many times. |
| timestamp | No notion of a message being old. |
| lifetime | No point at which a captured envelope stopped working. Forever is a long time. |
| destination | An envelope addressed to outpost A verified at outpost B. A capture could be *re-aimed*. |

There was a fifth problem, separate from the signature. An envelope that failed
verification — or carried no signature at all — was **stored anyway**, with
`verified = false`, and answered `200 OK`. The reasoning at the time was
store-and-forward-first: keep it, let consumers filter. But `/api/dtn/receive`
sat behind the general auth guard, which accepts a *user* token, so any
signed-in account could put rows in the inbox; and "consumers filter on
`verified`" is a convention each consumer has to remember, which means it holds
until the first one that forgets.

---

## The wire format

Version 2. Both envelope shapes carry the same five header fields.

```jsonc
{
  "v": 2,
  "msg_id":       "<uuid>",        // chosen by the sender, unique per sender
  "src_node_id":  "<uuid>",        // who wrote it
  "dest_node_id": "<uuid>",        // who it is for
  "sent_at":      "<rfc3339>",     // when it was written
  "expires_at":   "<rfc3339>",     // after this it is dead
  "signature":    "<base64>",      // Ed25519 over the canonical bytes below

  // --- plaintext body (peer has published no ML-KEM key) ---
  "payload": { }

  // --- or sealed body ---
  "scheme":         "mlkem1024-aes256gcm",
  "kem_ciphertext": "<base64>",
  "nonce":          "<base64>",
  "ciphertext":     "<base64>"
}
```

### Canonical signing bytes

Fields are **length-prefixed**, not delimited:

```
canonical(domain, fields) =
    domain
    ++ for each field: be_u32(field.len) ++ field

plaintext:  canonical("dtn-v2|plain",  [msg_id, src, dest, sent_at, expires_at, payload_json])
sealed:     canonical("dtn-v2|sealed", [msg_id, src, dest, sent_at, expires_at,
                                        kem_ciphertext, nonce, ciphertext])
```

Two details that are not decoration:

**The domain differs between shapes.** If plaintext and sealed envelopes could
produce identical signed bytes, a signature lifted from a sealed envelope would
authenticate a plaintext one whose body the attacker chose.

**The length prefixes replace a `|` separator.** With one fixed-width field
either side of a separator, `a|b` was unambiguous. With six variable-length
fields it is not: `dest="a", sent_at="b|c"` and `dest="a|b", sent_at="c"` would
sign identical bytes, so one signature would cover two different routings.
Length-prefixing makes field boundaries unforgeable.

A sealed body additionally binds sender and recipient as AEAD associated data,
so the recipient is committed twice over — once by the signature, once by the
decryption. Plaintext bodies previously had neither.

---

## What a receiver does, in order

Each step is cheaper than the one after it, so an envelope that will be refused
is refused before anything expensive happens to it. The replay check in
particular sits *before* decryption: a replayed bundle costs one indexed
lookup, not an ML-KEM decapsulation.

1. **Is the caller a peer?** `Principal::Node` is required — a named peer whose
   key is on file. A user token is refused (`403 not_a_peer`), and so is the
   fabric's shared secret: every holder of it is indistinguishable, and the
   next step needs to know *whose* key to verify against.

2. **Does the transport identity match the claimed author?** There is no
   relaying in this implementation — the forwarder posts straight to the
   destination — so a mismatch is a forgery, not a hop (`403 src_mismatch`).
   When multi-hop relaying lands, this check moves to the outer hop and the
   envelope signature plus the dedupe below carry the inner guarantee.

3. **Is it v2?** v1 is refused (`400 unsupported_envelope`) rather than
   accepted unverified. Accepting both shapes would make every guarantee here
   optional for anyone claiming to be an older build.

4. **Is it for us?** `dest_node_id` must be this node (`409 misdirected`).

5. **Do the clocks make sense?**
   - `sent_at` more than 300 s in the future → `400 future_dated`. A bundle may
     take days to arrive, but it cannot have been *written* tomorrow.
   - `expires_at <= sent_at` → `400 expired_on_arrival`.
   - `expires_at - sent_at` over the cap → `400 lifetime_too_long`. Refused
     rather than trimmed; see [the cap](#the-lifetime-cap).
   - past `expires_at` → **`200 {"status":"expired"}`**. Deliberately a
     success; see [why 2xx](#why-a-refusal-is-sometimes-a-2xx).

6. **Is the signature good?** Verified against `node_registry.public_key` for
   `src_node_id`. A bad signature or an unknown sender is refused
   (`401 bad_signature` / `401 unknown_sender`) and **not stored**.

7. **Have we seen it before?** `INSERT INTO dtn_seen (src_node_id, msg_id, …)
   ON CONFLICT DO NOTHING`. If it inserted nothing, this is a repeat →
   **`200 {"status":"duplicate"}`**, and no second inbox row.

8. **Open and file it.** Steps 7 and 8 are one transaction. Split, a crash
   between them would leave the message remembered but never delivered — and
   the sender's retransmit, the one mechanism that could have recovered it,
   would then be absorbed as a duplicate.

A decryption failure at step 8 is recorded rather than fatal: the envelope was
authentic, so an operator needs to see that a peer is sealing to a stale key.
The payload slot carries the diagnosis instead of the message.

### Why a refusal is sometimes a 2xx

`duplicate` and `expired` both answer `200`, which looks wrong and is not.

The forwarder retries on **any non-2xx**. A `409 Conflict` on a duplicate would
leave the bundle in the sender's outbox, to be redelivered on the backoff
schedule until it expired — turning the defence into a traffic generator. The
same goes for a dead bundle: it can never be accepted, so the only useful reply
is one that makes the sender stop.

Everything that indicates a *fixable fault* — bad signature, wrong version,
wrong address — answers 4xx, because there a retry is harmless and the failure
should be loud.

### The lifetime cap

The sender chooses the lifetime and signs it, so it cannot be trimmed in
flight. But a message id has to be remembered for the whole lifetime, or the
replay window reopens — which makes an uncapped lifetime an uncapped table.

So it is capped on arrival: `DTN_MAX_LIFETIME_SECS`, default **7 days**
(generous for a relay chain; interplanetary one-way light time is minutes, not
days). Outgoing bundles default to `DTN_DEFAULT_LIFETIME_SECS`, 24 hours, and a
caller can ask for less with `ttl_secs`.

A bundle over the cap is **refused, not trimmed**. Trimming would mean
forgetting the id while the sender still believed the bundle live, which is
precisely the gap a replay needs.

The forwarder prunes `dtn_seen` on each bundle's own `expires_at` — not on an
age, and not on a fixed retention, because either of those could drop a row
while the message was still deliverable.

---

## Where refusals go

A refused envelope is no longer stored in the inbox, which is right but would
otherwise make the entire class invisible: the sender's forwarder reports only
a failed attempt, and the receiver's operator sees nothing at all.

So every refusal is counted in `dtn_rejected` with its reason, readable at
`GET /api/dtn/rejected`. No payload is kept — a refused envelope is
unauthenticated input, and storing its body invites something downstream to
read it.

This is also the one place where "a peer has not been upgraded"
(`unsupported_envelope`), "a clock has drifted" (`future_dated`), and
"something is replaying our traffic" are distinguishable from one another.

---

## What this does not solve

- **Relaying.** Step 2 requires the transport identity to equal the envelope
  author, so a bundle cannot currently traverse an intermediate outpost. True
  multi-hop DTN needs a hop-by-hop layer around the end-to-end envelope.
- **A compromised sender.** Replay of *another* node's traffic is stopped; a
  node whose key is stolen can write genuinely new, genuinely signed messages.
  That is what revocation (`node_registry.revoked`) is for.
- **Ordering.** Bundles may arrive out of order and nothing reassembles them.
  Consumers that care must carry their own sequence.
- **Payload-level idempotency.** Exactly-once *delivery* is now guaranteed;
  whether acting on a message twice is safe is still the consumer's problem if
  the same instruction is sent under two different message ids.

---

## Configuration

| Variable | Default | What it does |
|---|---|---|
| `DTN_MAX_LIFETIME_SECS` | `604800` (7 d) | Longest bundle lifetime accepted, and therefore how long a message id is remembered. |
| `DTN_DEFAULT_LIFETIME_SECS` | `86400` (24 h) | Lifetime stamped on outgoing bundles when the caller names none. |

## Endpoints

| Route | Who | What |
|---|---|---|
| `POST /api/dtn/send` | operator | Queue a payload for a destination node. |
| `POST /api/dtn/receive` | **peer only** | Accept an envelope. |
| `GET /api/dtn/outbox` | operator | What is queued, with attempts and expiry. |
| `GET /api/dtn/inbox` | operator | What arrived. Every row here is verified. |
| `GET /api/dtn/rejected` | operator | What was turned away, and why. |

## Tests

`apps/api/tests/dtn_replay.rs` forges envelopes deliberately — signing with a
peer key the harness controls, posting with real fabric transport headers —
because a replay is indistinguishable from a retransmit at the HTTP layer and
only a genuinely signed request proves the server tells them apart.

```bash
TEST_DATABASE_URL=postgres://… SQLX_OFFLINE=true cargo test --test dtn_replay
```

Eleven cases: delivered-twice-stored-once, duplicate-answered-2xx,
re-aimed-elsewhere, tampered-payload, extended-lifetime, expired-absorbed,
absurd-lifetime, v1-refused, user-token-refused, peer-presenting-another's-
envelope, and refusals-recorded-with-a-reason.
