#!/usr/bin/env bash
# Back up one outpost: its database and its keys, together, in one archive.
#
# Together is the point. Either alone restores something broken:
#
#   - A database without the keys restores an outpost that cannot authenticate
#     to any peer, whose organisation certificate names an identity it no longer
#     has, and which cannot open a single DTN bundle queued for it.
#   - Keys without a database restore an identity with nothing behind it.
#
# So this script refuses to produce a half-backup. If it cannot read the keys it
# stops, rather than writing a database dump that looks like a backup and is not.
#
# Usage:
#   scripts/backup_outpost.sh [--out DIR] [--db-container NAME] [--db NAME]
#
# The key bundle is sealed with a passphrase. Provide it in BACKUP_PASSPHRASE or
# be prompted. Lose it and the bundle is scrap — it is the only thing protecting
# a file that will sit in object storage.

set -euo pipefail

OUT_DIR="./backups"
DB_CONTAINER="tidasone-db-v2"
DB_NAME="tidasone"
API_CONTAINER="tid-wayfarer-core"
KEEP=14

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)           OUT_DIR="$2"; shift 2 ;;
    --db-container)  DB_CONTAINER="$2"; shift 2 ;;
    --db)            DB_NAME="$2"; shift 2 ;;
    --api-container) API_CONTAINER="$2"; shift 2 ;;
    --keep)          KEEP="$2"; shift 2 ;;
    -h|--help)       sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

die() { echo "✗ $*" >&2; exit 1; }
note() { echo "  $*"; }

command -v docker >/dev/null || die "docker is required"
command -v jq     >/dev/null || die "jq is required"

docker inspect "$DB_CONTAINER" >/dev/null 2>&1 \
  || die "database container '$DB_CONTAINER' not found (pass --db-container)"
docker inspect "$API_CONTAINER" >/dev/null 2>&1 \
  || die "api container '$API_CONTAINER' not found (pass --api-container)"

# --- Passphrase, before anything expensive happens ---
#
# Asked for first on purpose: finding out the passphrase is too short *after* a
# nine-gigabyte dump is a waste of everyone's evening.
if [[ -z "${BACKUP_PASSPHRASE:-}" ]]; then
  read -r -s -p "  Backup passphrase (20+ chars, four words is fine): " BACKUP_PASSPHRASE
  echo
  read -r -s -p "  Again: " CONFIRM
  echo
  [[ "$BACKUP_PASSPHRASE" == "$CONFIRM" ]] || die "passphrases do not match"
fi
(( ${#BACKUP_PASSPHRASE} >= 20 )) \
  || die "passphrase must be at least 20 characters — this file is offline-attackable, so length is what protects it"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUTPOST="$(docker exec "$API_CONTAINER" printenv OUTPOST_NAME 2>/dev/null || echo outpost)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "▸ Backing up $OUTPOST at $STAMP"

# --- 1. Keys first ---
#
# Deliberately before the database. If the keys cannot be read there is no point
# taking a dump, and discovering that at the end is how people end up with a
# directory of useless database-only backups.
note "reading keys from the running container…"
docker exec "$API_CONTAINER" cat /app/keys/identity.json > "$WORK/identity.json" 2>/dev/null \
  || die "could not read keys/identity.json — without it a restore produces a different outpost"
docker exec "$API_CONTAINER" cat /app/keys/commsec.json > "$WORK/commsec.json" 2>/dev/null \
  || die "could not read keys/commsec.json — without it every queued DTN bundle is unreadable after a restore"

NODE_ID="$(jq -r .node_id "$WORK/identity.json")"
[[ "$NODE_ID" != "null" && -n "$NODE_ID" ]] || die "identity.json has no node_id"
note "node id: $NODE_ID"

# Sealed by the binary itself, not by openssl.
#
# `openssl enc` cannot do AEAD ciphers at all — it refuses aes-256-gcm outright
# — so doing this in shell would have meant CBC plus a hand-rolled HMAC: a
# second implementation of a security-critical format beside the Rust one. Two
# implementations of a path like this means one of them is the weak one, which
# is the same mistake as the unauthenticated relay inbox. So there is one
# format, Argon2id + AES-256-GCM, and `services::keyvault` is the only thing
# that writes it.
note "sealing with the outpost binary (argon2id + aes-256-gcm)…"
docker exec -e BACKUP_PASSPHRASE="$BACKUP_PASSPHRASE" "$API_CONTAINER" \
  /app/api keys-seal > "$WORK/keys.sealed" \
  || die "sealing the key bundle failed"

# A zero-length file would tar up perfectly and restore nothing.
[[ -s "$WORK/keys.sealed" ]] || die "the sealed bundle is empty"
jq -e '.ciphertext and .salt and .nonce' "$WORK/keys.sealed" >/dev/null \
  || die "the sealed bundle is not in the expected format"

rm -f "$WORK/identity.json" "$WORK/commsec.json"
note "keys sealed"

# --- 2. The database ---
note "dumping $DB_NAME…"
docker exec "$DB_CONTAINER" pg_dump -U postgres --format=custom --no-owner --no-acl "$DB_NAME" \
  > "$WORK/database.pgdump" \
  || die "pg_dump failed"
note "dump: $(du -h "$WORK/database.pgdump" | cut -f1)"

# --- 3. One archive, checksummed ---
#
# A manifest inside the archive, so a restore five months from now does not
# depend on anyone remembering what these files were.
jq -n --arg node "$NODE_ID" --arg name "$OUTPOST" --arg at "$(date -u +%FT%TZ)" \
   --arg db "$DB_NAME" \
   '{outpost_name:$name, node_id:$node, taken_at:$at, database:$db,
     contents:["database.pgdump","keys.sealed"],
     restore:"scripts/restore_outpost.sh — against a STOPPED outpost"}' \
   > "$WORK/manifest.json"

mkdir -p "$OUT_DIR"
ARCHIVE="$OUT_DIR/${OUTPOST}-${STAMP}.tar.gz"
tar -czf "$ARCHIVE" -C "$WORK" manifest.json database.pgdump keys.sealed

CHECKSUM="$(sha256sum "$ARCHIVE" | cut -d' ' -f1)"
echo "$CHECKSUM  $(basename "$ARCHIVE")" > "$ARCHIVE.sha256"
SIZE="$(stat -c%s "$ARCHIVE")"

echo "✓ $ARCHIVE ($(du -h "$ARCHIVE" | cut -f1))"
note "sha256: $CHECKSUM"

# --- 4. Record it, so staleness is visible ---
#
# This is what makes the habit survive a busy month: GET /api/ops/keys/status
# reports the age of the last backup, and an operator who can see "never" acts
# on it in a way that a runbook nobody opens does not produce.
docker exec -i "$DB_CONTAINER" psql -U postgres -d "$DB_NAME" -q -v ON_ERROR_STOP=1 <<SQL || \
  echo "  ! could not record the backup in the database (the archive is still good)"
INSERT INTO backups (node_id, outpost_name, kind, location, size_bytes, checksum, includes_keys, note)
VALUES ('$NODE_ID'::uuid, '$OUTPOST', 'full', '$ARCHIVE', $SIZE, '$CHECKSUM', TRUE,
        'scripts/backup_outpost.sh');
SQL

# --- 5. Prune, loudly ---
if [[ "$KEEP" -gt 0 ]]; then
  mapfile -t OLD < <(ls -1t "$OUT_DIR/${OUTPOST}-"*.tar.gz 2>/dev/null | tail -n +$((KEEP + 1)))
  if (( ${#OLD[@]} )); then
    # Said out loud rather than done quietly: silent deletion of backups is how
    # a retention policy nobody chose becomes a retention policy.
    echo "  pruning ${#OLD[@]} archive(s) beyond --keep $KEEP:"
    for f in "${OLD[@]}"; do echo "    $(basename "$f")"; rm -f "$f" "$f.sha256"; done
  fi
fi

cat <<EOF

  Store this off the machine it came from. Losing both the volume and the
  archive is unrecoverable — a new identity cannot authenticate to peers, and
  every DTN bundle sealed to the old key becomes permanently unreadable.

  Restore:  scripts/restore_outpost.sh $ARCHIVE
  Verify:   sha256sum -c $ARCHIVE.sha256
EOF
