# Key Custody and Backup

How an outpost survives losing its disk, and how it rotates a key without an
administrator at every peer.

---

## Why this is the highest-stakes part of the system

An outpost's identity is two files on one volume: `keys/identity.json` (Ed25519)
and `keys/commsec.json` (ML-KEM-1024). Lose them and the next start does not
fail — it **mints a new identity and a new node id** and carries on, which is
far worse:

- Peers hold the old node id and public key in `node_registry`. The new identity
  cannot authenticate as the old one.
- Its `org_outposts` certificate names the old node id, so `own_org()` resolves
  to nothing. The rollup stops asking peers, and org-scoped reads from peers
  fail closed.
- **Every DTN bundle queued for it was sealed to the old ML-KEM key and becomes
  permanently unreadable.** Those are precisely the messages that were waiting
  because the link was down.
- Custody receipts it signed still name the old key. The provenance survives;
  the ability to sign as that party does not.

It is not a degraded outpost. It is a new outpost wearing an old name.

**This is not hypothetical.** This deployment already had an `org_outposts` row
naming an identity from before the keys volume existed, and the symptom was a
rollup that silently stopped asking peers. It took re-enrolling the outpost by
hand to fix.

---

## What covers what

| Situation | Recovery | Needs |
|---|---|---|
| Planned rotation | Succession: the outgoing key signs for its successor | nothing but the old key |
| Key loss, backup in hand | Restore the archive | the archive and its passphrase |
| Key loss, no backup | An operator at **every peer** rotates you in by hand | administrative access you probably do not have |
| Lost the backup passphrase | The archive is scrap | — |

The third row is the whole argument for the second. There is no cryptographic
shortcut and pretending otherwise would be the dangerous design.

---

## Backup

```bash
scripts/backup_outpost.sh --out /srv/backups
```

One archive containing the database dump **and** the sealed keys, with a
`.sha256` beside it.

**Together is the point.** The script refuses to produce a half-backup, and
reads the keys *before* dumping the database, so a missing key file stops it
rather than leaving a directory of database-only archives that look like
backups and are not:

- a database without keys restores an outpost that cannot authenticate to any
  peer and cannot open its own queued DTN traffic;
- keys without a database restore an identity with nothing behind it.

The key bundle is sealed with Argon2id → AES-256-GCM. The passphrase must be at
least 20 characters, because the file is offline-attackable once it leaves the
machine and length is the only thing protecting it. Four unrelated words is
enough and is memorable.

### Where the sealing happens, and why not in the shell

`openssl enc` cannot do AEAD ciphers at all — it refuses `aes-256-gcm`
outright. Doing this in shell would have meant CBC plus a hand-rolled HMAC: a
second implementation of a security-critical format beside the Rust one. Two
implementations of a path like this means one of them is the weak one, which is
exactly how the relay ended up with an unauthenticated DTN inbox.

So the script shells into the binary (`/app/api keys-seal`), and
`services::keyvault` is the only thing that writes the format. The subcommand
needs no database, which is what lets a restore run against a stopped outpost.

### What the envelope does not reveal

Only the version, KDF parameters, salt and nonce are in the clear. The node id,
the outpost name and both keys are inside the ciphertext — a backup in object
storage should not announce which outpost it unlocks. Two exports of the same
keys also differ byte-for-byte, so an observer of the bucket cannot tell when
the keys changed and when they did not.

The stored KDF parameters are a record, not an instruction: `open` uses the
build's own constants, so rewriting `mem_kib` to something cheap and handing
the file back as "your backup" achieves nothing.

### Automating it

```cron
17 3 * * *  cd /srv/wayfarer && BACKUP_PASSPHRASE_FILE=/etc/wayfarer/backup.pass \
            BACKUP_PASSPHRASE="$(cat /etc/wayfarer/backup.pass)" \
            scripts/backup_outpost.sh --out /srv/backups --keep 14 >> /var/log/wayfarer-backup.log 2>&1
```

Then get the archives **off the machine**. A backup on the disk you are
protecting against is not a backup.

### Is there a recent one?

```
GET /api/ops/keys/status
```

```jsonc
{
  "standing": "ok",            // "never" | "ok" | "stale" (>30 days)
  "lastKeyBackupAgeDays": 0,
  "keysOnDisk": true,
  "whatLossMeans": "A key backup exists. Restoring it keeps this outpost's identity…"
}
```

Three states rather than a boolean, because "never" and "too long ago" need
different responses and only one of them is urgent today. This route exists
because a backup habit that depends on a runbook nobody opens does not survive
a busy month; one an operator can *see* the age of does.

---

## Restore

Against a **stopped** outpost:

```bash
docker compose stop tid-wayfarer-core
scripts/restore_outpost.sh /srv/backups/TIDasONE-Core-20261004T073730Z.tar.gz
docker compose up -d tid-wayfarer-core
```

The script refuses to run while the process is up. Restoring underneath a live
outpost would swap its identity mid-flight, leaving in-memory keys, the peer
registry and the organisation certificate disagreeing — and if the keys on the
volume were the good ones, destroy them.

Order of operations, chosen deliberately:

1. **Verify the checksum.** A truncated archive that restores "successfully" is
   worse than one that fails, because the operator believes they recovered.
2. **Open the key bundle**, before touching the database. A wrong passphrase
   discovered *after* the restore would leave a database with no matching
   identity — the exact half-state this is meant to prevent.
3. **Write the keys**, through a throwaway container on the volume.
   `write_to_disk` refuses to overwrite existing key files, so an identity
   already there is never silently replaced.
4. **Restore the database**, then record the restore itself in `backups`, so
   the next operator can see this database is a recovery rather than a
   continuous history.

Afterwards, check the log says the identity was **loaded**, not generated:

```bash
docker compose logs tid-wayfarer-core | grep -iE "identity|ML-KEM"
# node identity loaded node_id=80cfecaf-… public_key=BXF4TPZ…
# loaded persisted ML-KEM keypair
```

`Generated new node identity` means the keys did not land. Stop and investigate
— the outpost has just become a different one.

### The drill, actually run

Performed on this deployment on 2026-10-04, not merely described:

```
destroyed:  keys volume + database
restored:   from the archive
result:     node id  80cfecaf-4feb-4387-a049-00c3b5bcbb17  (unchanged)
            ed25519  BXF4TPZlS3zEzUNdcVGShjcX…             (unchanged)
            log      "loaded persisted ML-KEM keypair"     (not generated)
            data     7 orders, 3 addresses, 1 shipment, 4 custody receipts
            fabric   peer online, rollup complete=true, 2 live sources
```

That last line is the one that matters: `complete: true` with two live sources
means `own_org()` resolved, so the organisation certificate still matched the
restored identity. Run this drill on your own deployment before you need it.

---

## Rotation

### The problem with the admin path

`POST /api/nodes/:id/rotate-key` needs an admin JWT **on the peer**. On a fabric
of one organisation that is fine. On a federated fabric it means rotating your
own key requires administrative access to other companies' outposts — which you
do not have, so in practice nobody rotates.

### Succession

```
POST /api/node-succession/:id        (no credential)
{
  "new_public_key": "<base64>",
  "ts": 1760000000,
  "predecessor_signature": "<base64>",   // the OUTGOING key signs
  "successor_signature":   "<base64>",   // the INCOMING key signs
  "reason": "annual rotation"
}
```

Signed over length-prefixed canonical bytes:

```
"node-succession|v1" ++ len(node_id)||node_id ++ len(old_pub)||old_pub
                     ++ len(new_pub)||new_pub ++ len(ts)||ts
```

**Unauthenticated, on purpose, and on its own path.** The authority is the
predecessor signature in the body — a node mid-rotation may be unable to present
either key as a header. Nothing new is trusted: the outgoing key was already
authoritative for this node, and an attacker who could forge its signature could
already sign requests as it. It gets a distinct prefix rather than hiding among
the admin-gated `/api/nodes` routes, because an endpoint that accepts no
credential should be visible as one in the route table.

Both signatures are required. Without the successor's, a typo in
`new_public_key` would hand the node's identity to a key nobody holds, and it
could never sign its way back.

A **revoked** node cannot succeed itself. Revocation is an operator saying "this
node is not to be trusted"; letting its key nominate a successor would let it
undo that, and make revocation decorative.

### The order, which is easy to get backwards

> **Tell every peer first. Install the new key last.**

A rotation cannot be atomic across a fabric. A peer that has accepted the
succession expects the new key while you are still signing with the old one — so
peers accept the **predecessor** for `KEY_ROTATION_GRACE_SECS` (default 24
hours, chosen because a peer may be unreachable for most of a day).

The opposite order has no fix at all: a peer that has not been told cannot
verify a key it has never seen, and no grace window can invent one.

Every acceptance of a superseded key is logged at `warn`, because a peer still
signing with a retired key past the window is a misconfiguration somebody needs
to see rather than a condition to absorb quietly.

### Compromise removes the window

If the reason mentions a compromise — `compromised`, `leaked`, `stolen`,
`theft`, `exposed`, `breach` — `grace` is recorded false and the old key stops
working **immediately**. A grace window on a stolen key is a grace window for
whoever stole it.

Matched loosely on purpose: being wrong costs a slightly more disruptive
rotation, where the other direction leaves a stolen key working for a day.

---

## The organisation root key

Worth stating plainly because it is worse than the node key and nothing here
fixes it.

`organisations.root_public_key` is the key that signs node certificates and
trust grants. The **secret** half is never held by the outpost — which is
correct, it should live offline — but that also means there is no export path
for it here, and **losing it means you can never issue another node certificate
or trust grant for that organisation again**. Existing ones keep verifying;
nothing new can ever be signed.

Treat it like a certificate authority root, because that is what it is: offline,
on two separate media, in two separate places, with the passphrase recorded
somewhere other than beside the key. A proper threshold scheme for it is unbuilt
and is the obvious next piece of this work.

---

## Configuration

| Variable | Default | What it does |
|---|---|---|
| `BACKUP_PASSPHRASE` | — | Seals and opens the key bundle. 20 characters minimum. |
| `KEY_ROTATION_GRACE_SECS` | `86400` | How long a peer accepts a superseded key after a succession. `0` disables it, which makes multi-peer rotation unworkable. |

## Endpoints and commands

| What | Where |
|---|---|
| Export a sealed bundle | `POST /api/ops/keys/export` (admin) |
| Backup standing | `GET /api/ops/keys/status` (admin) |
| Succeed a node's key | `POST /api/node-succession/:id` (no credential; signature is the authority) |
| Admin rotation | `POST /api/nodes/:id/rotate-key` (admin) |
| Key history | `GET /api/nodes/:id/key-history` (admin) |
| Full backup | `scripts/backup_outpost.sh` |
| Full restore | `scripts/restore_outpost.sh` |
| Seal / unseal only | `/app/api keys-seal`, `/app/api keys-unseal` |

Restore is deliberately **not** an API call. Swapping an outpost's identity at
runtime would leave its in-memory state, the registry and its certificate
disagreeing, and a restore belongs where the operator can see what they are
overwriting.

## Tests

```bash
SQLX_OFFLINE=true cargo test --lib keyvault          # 10 — sealing
TEST_DATABASE_URL=… cargo test --test key_custody    # 10 — succession and rollover
```

The sealing tests cover the properties that are easy to lose: that the envelope
reveals nothing identifying, that two exports differ, that a tampered ciphertext
is refused, that the stored KDF cost cannot influence anything, and that `Debug`
on a key bundle redacts both secrets — which is the guard against a stray `{:?}`
in a log line being the way they escape.
