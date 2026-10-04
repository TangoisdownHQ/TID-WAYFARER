#!/usr/bin/env bash
# Restore an outpost from an archive made by backup_outpost.sh.
#
# Run this against a **stopped** outpost. Restoring under a running one would
# swap its identity mid-flight, leaving the in-memory keys, the peer registry
# and the organisation certificate disagreeing with each other — and if the keys
# already on the volume were the good ones, destroy them.
#
# The restored outpost keeps its original node id, which is the whole value:
# nothing at any peer has to change, its organisation certificate still matches,
# and DTN bundles queued for it while it was gone still open.
#
# Usage:
#   scripts/restore_outpost.sh ARCHIVE [--db-container NAME] [--db NAME] [--yes]

set -euo pipefail

ARCHIVE="${1:-}"
[[ -n "$ARCHIVE" ]] || { sed -n '2,16p' "$0"; exit 2; }
shift

DB_CONTAINER="tidasone-db-v2"
DB_NAME="tidasone"
KEYS_VOLUME="tid-wayfarer_core-keys"
ASSUME_YES=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --db-container) DB_CONTAINER="$2"; shift 2 ;;
    --db)           DB_NAME="$2"; shift 2 ;;
    --keys-volume)  KEYS_VOLUME="$2"; shift 2 ;;
    --yes)          ASSUME_YES=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done

die() { echo "✗ $*" >&2; exit 1; }
note() { echo "  $*"; }

[[ -f "$ARCHIVE" ]] || die "no such archive: $ARCHIVE"
command -v docker >/dev/null || die "docker is required"
command -v jq     >/dev/null || die "jq is required"

# --- Verify the bytes before trusting them ---
#
# A truncated archive that restores "successfully" is worse than one that fails:
# the operator believes they have recovered.
if [[ -f "$ARCHIVE.sha256" ]]; then
  note "verifying checksum…"
  (cd "$(dirname "$ARCHIVE")" && sha256sum -c "$(basename "$ARCHIVE").sha256" >/dev/null) \
    || die "checksum mismatch — this archive is not the one that was taken"
  note "checksum ok"
else
  echo "  ! no .sha256 beside the archive; cannot verify it is intact"
fi

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
tar -xzf "$ARCHIVE" -C "$WORK"

[[ -f "$WORK/manifest.json" ]] || die "archive has no manifest — not one of ours"
NODE_ID="$(jq -r .node_id "$WORK/manifest.json")"
OUTPOST="$(jq -r .outpost_name "$WORK/manifest.json")"
TAKEN="$(jq -r .taken_at "$WORK/manifest.json")"

echo "▸ Archive contents"
note "outpost:  $OUTPOST"
note "node id:  $NODE_ID"
note "taken:    $TAKEN"
note "keys:     $([[ -f "$WORK/keys.sealed" ]] && echo present || echo "MISSING — restores a different outpost")"

[[ -f "$WORK/keys.sealed" ]] \
  || die "this archive has no key bundle; restoring it would produce an outpost the fabric does not recognise"

# --- Refuse to run against a live outpost ---
RUNNING="$(docker ps --filter "name=tid-wayfarer-core" --format '{{.Names}}' || true)"
if [[ -n "$RUNNING" ]]; then
  die "$RUNNING is running — stop it first (docker compose stop tid-wayfarer-core).
    Restoring underneath a live process swaps its identity mid-flight."
fi

if [[ "$ASSUME_YES" -ne 1 ]]; then
  cat <<EOF

  This will REPLACE:
    • the '$DB_NAME' database in container $DB_CONTAINER
    • the keys on volume $KEYS_VOLUME

  Anything currently in either is lost.
EOF
  read -r -p "  Type the outpost name to continue: " CONFIRM
  [[ "$CONFIRM" == "$OUTPOST" ]] || die "aborted"
fi

# --- Open the key bundle first ---
#
# Before touching the database: a wrong passphrase discovered after the restore
# leaves a database with no matching identity, which is the broken half-state
# this whole script exists to avoid.
if [[ -z "${BACKUP_PASSPHRASE:-}" ]]; then
  read -r -s -p "  Backup passphrase: " BACKUP_PASSPHRASE
  echo
fi

# Opened and written by the outpost binary itself, in a throwaway container
# mounted on the keys volume. One sealing implementation, and it works with the
# outpost stopped — which is the state it is in when it most needs restoring.
#
# `keyvault::write_to_disk` refuses to overwrite existing key files, so an
# identity already on the volume is never silently replaced; move it aside
# deliberately if that is what you mean to do.
note "opening the key bundle and writing it to volume $KEYS_VOLUME…"
BUNDLE_NODE="$(docker run --rm -i \
    -e BACKUP_PASSPHRASE="$BACKUP_PASSPHRASE" \
    -v "$KEYS_VOLUME":/app/keys \
    tid-wayfarer:latest /app/api keys-unseal < "$WORK/keys.sealed")" \
  || die "could not open or install the key bundle — wrong passphrase, the archive was altered,
    or keys already exist on $KEYS_VOLUME (move them aside first)"

[[ "$BUNDLE_NODE" == "$NODE_ID" ]] \
  || die "the sealed bundle names node $BUNDLE_NODE but the manifest says $NODE_ID — mismatched archive"
note "keys restored; identity $BUNDLE_NODE"

# --- Database ---
note "restoring $DB_NAME…"
docker exec "$DB_CONTAINER" psql -U postgres -q -c \
  "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname='$DB_NAME' AND pid <> pg_backend_pid()" \
  >/dev/null 2>&1 || true
docker exec "$DB_CONTAINER" psql -U postgres -q -c "DROP DATABASE IF EXISTS $DB_NAME" >/dev/null
docker exec "$DB_CONTAINER" psql -U postgres -q -c "CREATE DATABASE $DB_NAME" >/dev/null
docker exec -i "$DB_CONTAINER" pg_restore -U postgres -d "$DB_NAME" --no-owner --no-acl \
  < "$WORK/database.pgdump" \
  || echo "  ! pg_restore reported problems; check the output above before trusting this restore"
note "database restored"

# The restore is itself worth recording, so the next operator can see that this
# database is a recovery rather than a continuous history.
docker exec -i "$DB_CONTAINER" psql -U postgres -d "$DB_NAME" -q <<SQL >/dev/null 2>&1 || true
INSERT INTO backups (node_id, outpost_name, kind, location, includes_keys, note)
VALUES ('$NODE_ID'::uuid, '$OUTPOST', 'full', '$ARCHIVE', TRUE,
        'RESTORED from this archive on $(date -u +%FT%TZ)');
SQL

cat <<EOF

✓ Restored $OUTPOST ($NODE_ID) from $TAKEN

  Next:
    docker compose up -d tid-wayfarer-core
    curl -s localhost:4000/api/health

  Then check the identity actually matches what peers expect:
    docker compose logs tid-wayfarer-core | grep -i identity
    # "Generated new node identity" means the keys did NOT land — stop and
    # investigate, because the outpost has just become a different one.

  Anything written to the live outpost after $TAKEN is not in this restore.
EOF
