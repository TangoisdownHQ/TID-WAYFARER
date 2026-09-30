#!/usr/bin/env bash
# Mint a JWT for tests against the running core-api.
#
# Reads JWT_SECRET from the running tidasone-core-api container's env so the
# token signs correctly without you needing to know the secret.
#
# Usage:
#   ./get_test_jwt.sh <user-uuid> [role=user|admin]
#
# Examples:
#   TOKEN=$(./get_test_jwt.sh 11111111-1111-1111-1111-111111111111)
#   ADMIN=$(./get_test_jwt.sh 22222222-2222-2222-2222-222222222222 admin)

set -euo pipefail

SUB="${1:-}"
ROLE="${2:-user}"

if [ -z "$SUB" ]; then
  echo "usage: $0 <user-uuid> [role]" >&2
  exit 2
fi

# Pull the secret from the running container so script callers don't have to.
SECRET=$(sudo docker exec tidasone-core-api printenv JWT_SECRET 2>/dev/null || true)
if [ -z "$SECRET" ]; then
  echo "❌ Could not read JWT_SECRET from tidasone-core-api container." >&2
  echo "   Is the container running? Try: sudo docker compose up -d tidasone-core-api" >&2
  exit 1
fi

# Build gen_jwt on the host (cached after first run). SQLX_OFFLINE=true
# is required because the bin target lives inside the core-api package,
# which uses sqlx::query!() macros that would otherwise hit the DB at
# compile time. The .sqlx/ offline cache (committed via `cargo sqlx prepare`)
# satisfies them. Stderr goes to a temp file so failures are surfaced
# instead of hidden.
cd "$(dirname "$0")/.."
LOG=$(mktemp)
TOKEN=$(SQLX_OFFLINE=true JWT_SECRET="$SECRET" JWT_SUB="$SUB" JWT_ROLE="$ROLE" \
  cargo run --quiet --bin gen_jwt 2>"$LOG") || {
    echo "❌ gen_jwt build/run failed. cargo stderr:" >&2
    cat "$LOG" >&2
    rm -f "$LOG"
    exit 1
  }
rm -f "$LOG"
echo "$TOKEN"
