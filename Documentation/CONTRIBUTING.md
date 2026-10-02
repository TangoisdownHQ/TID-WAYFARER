# Contributing

## Getting it running

```bash
git clone https://github.com/TangoisdownHQ/TID-WAYFARER.git
cd TID-WAYFARER
cp .env.example .env            # then edit it; see below
docker compose up -d
```

The UI is on <http://localhost:4000/ui/console.html>. The database is on
**5433**, not 5432 — the default is left free because a lot of people already
have something on it.

`docker compose` applies the migration set on every start, which is why
migrations must be re-runnable (see below). Two outposts come up, each with its
own database and its own persisted identity keys, so the fabric behaves like a
fabric rather than like one process talking to itself.

### Building outside Docker

```bash
SQLX_OFFLINE=true cargo build --workspace
```

**`SQLX_OFFLINE=true` is not optional.** `sqlx::query!` verifies every query
against a live database *at compile time*. Offline mode compiles against the
committed query cache in `.sqlx/` instead — which is also how the Docker image
builds, and how CI builds. Omit it with a `DATABASE_URL` pointing at an
unmigrated database and you get dozens of errors that read like unrelated Rust
problems (`no method named 'rows_affected' found for type '_'` and friends).
CI failed this way for five pushes before anyone looked past the error text.

If you add a `sqlx::query!` macro call, the cache has to be regenerated
(`cargo sqlx prepare`) against a migrated database. The simpler option, and
what most of the newer modules do, is to use the non-macro `sqlx::query` /
`query_scalar` / `query_as`, which are checked at runtime and need no cache
entry.

## Tests

```bash
SQLX_OFFLINE=true cargo test --lib            # unit tests, no database

export TEST_DATABASE_URL=postgres://postgres:…@localhost:5433/tidasone
SQLX_OFFLINE=true cargo test --test boundaries    # organisation isolation
SQLX_OFFLINE=true cargo test --test dtn_replay    # DTN replay protection
SQLX_OFFLINE=true cargo test --test chat_people   # accounts and conversations
SQLX_OFFLINE=true cargo test --test replication   # dark sites still counting
```

**Point `TEST_DATABASE_URL` at a throwaway database, never at your dev one.**
The suites insert fixtures and do not clean up after themselves — they are
written to be isolated by using fresh uuids, not by rolling back. Run them
against your working database and it fills with fixture organisations, users
and registry peers; the node registry in particular will show dozens of dark
peers in the fabric bar and slow every rollup down while they time out.

```bash
docker exec tidasone-db-v2 psql -U postgres -c 'CREATE DATABASE wf_itest'
for f in packages/db/migrations/*.sql; do
  docker exec -i tidasone-db-v2 psql -U postgres -d wf_itest -q -v ON_ERROR_STOP=1 < "$f"
done
export TEST_DATABASE_URL=postgres://postgres:…@localhost:5433/wf_itest
```

Without `TEST_DATABASE_URL` the integration suites **skip rather than fail**.
That is deliberate: a test that goes red for want of infrastructure trains
people to ignore red.

The integration suites run against a real Postgres because the properties they
assert cannot be reached from a unit test — whether a guard actually refuses a
request, whether a query actually filters by organisation, whether a hold
actually blocks a shipment. Those are facts about the assembled application and
its schema, not about a function. They share one database and run in parallel,
so fixtures must not collide: use fresh uuids, and never a fixed string in a
column with a unique constraint.

## Things that have bitten us

These are not style preferences. Each one is a bug that shipped.

**Migrations must be re-runnable.** They are applied on every container start,
so a migration that only works once breaks every deploy after the first — and
that is invisible to a single pass on an empty database. Apply the whole set
two or three times to one database before you push. CI does this now, and it
caught `ALTER TABLE … ADD CONSTRAINT` the first time it ran: Postgres has no
`ADD CONSTRAINT IF NOT EXISTS`, so guard it on `pg_constraint` or
drop-then-add. A grep for `IF NOT EXISTS` will not find this class of problem;
running the migrations will.

**Authorisation reads from the database, never from the request.** The caller's
organisation comes from `services::org_scope::caller_org`, which **fails
closed**: a caller whose org cannot be established sees nothing, rather than
seeing everything. That direction matters more than it sounds — the convenient
failure mode turns every bug in that function into a silent disclosure of every
customer's holdings; the safe one turns the same bug into an empty page, which
someone reports. Roles come from the membership row, not the token, so a
demoted user loses access on their next call rather than at the end of their
session.

**Prefer 404 to 403 for something you are not party to.** A 403 confirms the
resource exists. On a marketplace that is itself information — it tells one
bidder that a conversation is happening about the order they are bidding on.

**A canonical encoding needs a domain prefix and length-prefixed fields.** Any
signed structure — DTN envelopes, custody receipts, org certificates, trust
grants, fabric requests — follows this. Without the domain, a signature over
one shape can be reinterpreted as another. Without length prefixes, field
boundaries can be moved: with a `|` separator, `dest="a", sent="b|c"` and
`dest="a|b", sent="c"` sign identical bytes, so one signature covers two
different meanings.

**Writes that belong together go in one transaction.** A user without an org
membership would sign in successfully, see nothing, and appear in no member
list for an administrator to find. A remembered-but-unstored DTN message would
be unrecoverable, because the sender's retransmit would be absorbed as a
duplicate.

**One implementation of a security-critical path, not two.** The Go relay had
a second DTN receive endpoint with no authentication at all, next to a Rust one
that verifies signatures and refuses replays. Hardening one of two just picks
which one gets used. If a path must exist in two places, the second one has to
be a client of the first, not a reimplementation of it.

**On a store-and-forward link, a 2xx is sometimes the correct refusal.** The
DTN forwarder retries on any non-2xx, so answering `409` to a duplicate or an
expired bundle turns a defence into a traffic generator. Reserve 4xx for
*fixable* faults, where a retry is harmless and the failure should be loud.

## Code conventions

Match the surrounding code. Beyond that:

- **Comments explain why, not what.** Where a non-obvious choice was made, say
  what the obvious alternative was and what it cost. Several of the comments in
  this codebase are the only record of a bug that is now impossible.
- **Name tests after the property, not the function.** `a_colleague_of_a_participant_still_cannot_read_the_thread`
  tells a reader what breaks if it fails. `test_chat_access` does not.
- **Say what was dropped.** If something caps coverage — a `LIMIT`, a sampled
  sweep, a skipped retry — log it. Silent truncation reads as "covered
  everything" when it did not.
- Frontend is vanilla ES modules with no build step. Shared helpers live in
  `shared/static/js/wayfarer.js`; a new page calls `page({…})` and gets the
  nav, the fabric bar, search and sign-in for free.
- Four-state colour vocabulary for status: live / lagging / dark / **unknown**.
  "Unknown" is a real state and must not be rendered as "fine".

## Commits

Sign them (`git commit -S`). Write the message as prose explaining what changed
and why; if a bug is being fixed, say what the wrong behaviour actually was,
because that is the part a future reader needs.

## Security

Found something exploitable? Please report it privately to
<contact@tidhq.net> rather than opening an issue.
