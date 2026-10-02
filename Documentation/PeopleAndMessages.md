# People and Messages

Two features that arrived together because they need each other: an outpost has
to be able to *name* the people working on it before those people can usefully
talk to each other.

---

## Part 1 — People

### What was wrong before

An account could only come into existence by signing itself up. That is the
wrong shape for anyone who would actually run this. A depot lead adds three
people and says which of them may approve a shipment; they do not ask the
warehouse to self-register and then try to work out afterwards who the new
accounts belong to.

### Two different kinds of role

This trips people up, so it is worth being explicit. There are **two** role
systems and they are not the same thing.

| | `users.role` | `org_members.role` |
|---|---|---|
| Scope | The whole outpost | One organisation on it |
| Values | `user`, `admin` | `owner`, `admin`, `operator`, `viewer` |
| Who sets it | Out of band, by whoever runs the outpost | An org owner or admin, in Settings |
| Decides | Whether a token reaches outpost-administration routes | What you can do with your company's data |

**Settings only ever writes the second one.** An org administrator runs a
company *on* the outpost, which is a different job from administering the
outpost itself. Conflating them would let any customer's admin reach every
other customer's data — so `users.role` is hardcoded to `user` on every account
created here, and there is no field to override it.

Org roles:

- **owner** — may administer people. Cannot be deactivated, and cannot have
  their role changed by an admin. On an outpost that may be out of contact for
  days, there has to be one account that cannot be locked out.
- **admin** — may administer people.
- **operator** — may act on inventory, shipments and custody.
- **viewer** — may read.

### Adding someone

`Settings → Add someone`, or `POST /api/people`.

A **one-time password is generated and shown exactly once.** There is no reset
email, and that is deliberate rather than unfinished: an outpost may have no
route to a mail server for days, so a flow that depends on delivered mail is a
flow that fails exactly when it is needed. The administrator reads the password
out; the recipient changes it on first sign-in.

The generated password is four words and a number — `lumen-girder-nadir-pallet-43`.
Words rather than symbols because it has to survive being read aloud across a
loading bay, and a password that gets written on a label to be legible is worse
than a longer one that does not.

`must_change_password` is set with it. It is the only record that the password
was *issued* rather than *chosen*, since the password itself is stored only as
an Argon2 hash. The flag shows as a banner on Settings and comes back in the
login response as `mustChangePassword`.

The account and its org membership are written **in one transaction**. Split,
a failure between them would leave a user belonging to no organisation — and
because every read filters on organisation, that account would sign in
successfully, see nothing, and appear in no org's member list for an
administrator to find and fix.

### Deactivation, never deletion

`PATCH /api/people/:id/active`.

A person who signed a custody receipt or released a compliance hold is
*referenced by those records*, and the records are the point. A deleted row
would leave a shipment attested by nobody. So an account is deactivated:
sign-in is refused with a specific message, and everything they ever signed
still names them. Restoring is the same call with `active: true`.

The deactivation check runs **after** the password is verified, not before.
That way someone holding a deactivated account gets a useful message, but the
endpoint still cannot be used to work out who has an account here.

Two refusals are built in: you cannot deactivate yourself (there may be nobody
to ring), and you cannot deactivate an owner (transfer ownership first). The UI
does not render a button that always refuses — offering one teaches people to
ignore errors.

### Changing your own password

`POST /api/me/password`, with the current password required even though the
caller already holds a valid token. A token lifted from a shared browser should
not be enough to take an account over permanently.

### Endpoints

| Route | Who | What |
|---|---|---|
| `GET /api/people` | any member | Everyone in **your** org. Not an outpost-wide directory. |
| `POST /api/people` | owner / admin | Create an account + membership. Returns the one-time password. |
| `PATCH /api/people/:id` | owner / admin | Name, org role, marketplace type. |
| `POST /api/people/:id/password` | owner / admin | Issue a new one-time password. |
| `PATCH /api/people/:id/active` | owner / admin | Deactivate or restore. |
| `POST /api/me/password` | anyone | Change your own. |

Every one of these resolves the caller's organisation through
`services::org_scope`, which **fails closed**: a caller whose org cannot be
established sees nothing rather than everything. The target id always comes
from the caller, so each write confirms the target is in the caller's org
first — without that check, a bare `UPDATE users` has nothing in its `WHERE`
clause about who was allowed to ask.

---

## Part 2 — Messages

### The access rule

> You may read a thread and post to it **if and only if** there is a row for
> you in `chat_participants`.

That is the whole check, and it is deliberately not derived from anything else.

The tempting alternative — *"anyone in the buying organisation may read the
buyer's threads"* — is wrong in a way that only surfaces later. Org membership
changes. Someone who joins next month would inherit a conversation about a
shipment that closed last month, including whatever was said about price.
Membership of a **thread** is a fact with a date on it; membership of an **org**
is not.

So a colleague of a participant is still not a participant. There is a test for
exactly that.

### Three kinds of conversation

| Kind | Who | Authorised by |
|---|---|---|
| `org` | everyone currently in one organisation | being in the org |
| `direct` | named people, all in one organisation | all of them being colleagues |
| `deal` | the two sides of a trade | **the order or bid itself** |

`deal` is the one that crosses a company boundary, and it is the reason the
table exists.

### Why a cross-org thread needs an anchor

Buyers and sellers have to be able to talk — about a substitution, a delivery
window, a damaged pallet. But [the org-boundary model](./OrgBoundaries.md) holds
that `marketplace` is the one scope an organisation may grant another, and
`inventory` is never grantable.

A free-for-all messaging directory would quietly undo that. Hand every
participant a searchable list of every other organisation's staff and you have
leaked the org chart — which is competitive information on a marketplace where
the same companies bid against each other.

So a `deal` thread is authorised by the transaction that justifies it, and its
participants are derived from the two sides of that transaction at the moment
it is opened:

- **anchored to a bid** — the person who placed it, and the person who raised
  the order it is against. This is the pre-award channel: you can negotiate
  before anything is accepted.
- **anchored to an order** — the requester, plus whoever placed the accepted
  bid if one has been.

**No order, no channel.** Knowing an order id is not enough — the caller has to
be a party to it, or they get a `403`. And a `direct` thread naming someone from
another organisation is refused with a message pointing at the legitimate
route, because otherwise `direct` would become the unauthorised cross-org
channel that anchoring `deal` threads exists to prevent.

### Opening one

From the **Supply** page, on the bid you care about: a *Message* button beside
*Accept*. That placement is the point — the channel lives next to the deal it
is about, not in a contact list.

Opening is idempotent for `org` and `deal` threads. The realistic client
behaviour is a button labelled "message the seller", and a second channel would
split the negotiation so each side could quote from a different one. A unique
index enforces one thread per order and one per bid; a repeat open returns
`200` with `existed: true` instead of `201`.

### Reading and unread counts

A thread the caller is not in answers **404, not 403**. A 403 confirms the
thread exists, which on a marketplace tells one bidder that another
conversation is happening about the order they are bidding on.

Unread is derived from `chat_participants.last_read_at` — per participant,
rather than a read receipt per message, because the question is "what have I
not seen", not "who saw this". Your own messages never count against you, and
posting marks the thread read (otherwise your own message would come back as
unread on the next poll).

`GET /api/chat/unread` exists on its own so the nav badge does not have to
fetch every thread to add up a number. The nav shows it on every page, because
a question from the other side of a deal is time-sensitive and nobody is going
to sit on one tab waiting for it. It fails silently — a chat endpoint that is
down must not put an error on every page in the application.

### Endpoints

| Route | What |
|---|---|
| `GET /api/chat` | Your conversations, most recent first, with unread counts and a preview. |
| `GET /api/chat/unread` | Just the badge number. |
| `POST /api/chat` | Open a conversation (`kind`: `org` / `direct` / `deal`), optionally with an opening message. |
| `GET /api/chat/:id?after=<id>` | A thread and a page of messages. `after` makes polling incremental. |
| `POST /api/chat/:id/messages` | Post. Max 4000 characters — attach a document for anything longer. |
| `POST /api/chat/:id/read` | Clear the badge for this thread. |

### Crossing a disconnected link

`chat_messages.via_node_id` records that a message arrived over DTN from
another outpost, and the UI marks it *relayed*. Worth showing: it explains a
delay that would otherwise look like someone ignoring you.

The column and the thread-touch trigger are in place so that the DTN receive
path can insert messages without the two write paths drifting — but **routing
chat over DTN is not yet wired**. Today a conversation lives on one outpost.
Two people signed in to the same outpost can talk; two people on different
outposts cannot yet.

---

## Tests

```bash
TEST_DATABASE_URL=postgres://… SQLX_OFFLINE=true cargo test --test chat_people
```

Eleven cases, mostly about what is refused: an operator cannot create accounts;
an admin cannot touch another org's people; a created account gets a working
one-time password *and* a membership; a deactivated account cannot sign in and
can be restored; an admin cannot deactivate themselves; a colleague of a
participant still cannot read the thread; a stranger cannot open a thread on
someone else's order; opening a deal thread twice returns the same thread; a
direct thread cannot cross an org boundary; unread counts the other person's
messages only; changing your password requires the old one.
