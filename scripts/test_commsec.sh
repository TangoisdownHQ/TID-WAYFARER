#!/usr/bin/env bash
set -euo pipefail

# CommSec is mounted under /api/commsec and sits behind the require_auth
# guard. Authenticate with either:
#   TOKEN=<jwt>                (see scripts/get_test_jwt.sh), or
#   NODE_SHARED_SECRET=<secret> (matches the server's env)
API="${API:-http://127.0.0.1:3000/api/commsec}"

AUTH=()
if [ -n "${TOKEN:-}" ]; then
    AUTH=(-H "Authorization: Bearer $TOKEN")
elif [ -n "${NODE_SHARED_SECRET:-}" ]; then
    AUTH=(-H "X-Node-Token: $NODE_SHARED_SECRET")
else
    echo "⚠️  No TOKEN or NODE_SHARED_SECRET set — requests will get 401" >&2
fi

echo "[1] Requesting KEM keypair..."
KEYS=$(curl -s -X POST "$API/keypair" "${AUTH[@]}")
PK=$(echo "$KEYS" | jq -r .public_key)
SK=$(echo "$KEYS" | jq -r .secret_key)
echo "Public Key:  ${PK:0:32}..."
echo "Secret Key:  ${SK:0:32}..."

echo "[2] Encapsulating..."
ENCAP=$(curl -s -X POST "$API/encapsulate" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"public_key\":\"$PK\"}")
CT=$(echo "$ENCAP" | jq -r .ciphertext)
SS1=$(echo "$ENCAP" | jq -r .shared_secret)
echo "Ciphertext:  ${CT:0:32}..."
echo "Shared Sec1: ${SS1:0:32}..."

echo "[3] Decapsulating..."
DECAP=$(curl -s -X POST "$API/decapsulate" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"ciphertext\":\"$CT\", \"secret_key\":\"$SK\"}")
SS2=$(echo "$DECAP" | jq -r .shared_secret)
echo "Shared Sec2: ${SS2:0:32}..."

if [ "$SS1" != "$SS2" ]; then
    echo "❌ Shared secrets mismatch!"
    exit 1
fi
echo "✅ Shared secrets match"

echo "[4] AEAD encrypt/decrypt..."
KEY="$SS1"
NONCE=$(head -c12 /dev/urandom | base64 -w0)
PLAINTEXT="hello_tidasonesec"
echo "Plaintext: $PLAINTEXT"

CIPHERTEXT=$(curl -s -X POST "$API/aead/encrypt" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"$KEY\",\"nonce\":\"$NONCE\",\"plaintext\":\"$PLAINTEXT\"}" \
    | jq -r .ciphertext)

echo "Ciphertext: ${CIPHERTEXT:0:32}..."

DECRYPTED=$(curl -s -X POST "$API/aead/decrypt" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"$KEY\",\"nonce\":\"$NONCE\",\"ciphertext\":\"$CIPHERTEXT\"}" \
    | jq -r .plaintext)

echo "Decrypted: $DECRYPTED"
if [ "$DECRYPTED" == "$PLAINTEXT" ]; then
    echo "✅ AEAD round-trip success"
else
    echo "❌ AEAD round-trip failed"
    exit 1
fi

echo "[5] AEAD with Associated Data..."
AD="metadata_test"

CIPHERTEXT_AD=$(curl -s -X POST "$API/aead/encrypt" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"$KEY\",\"nonce\":\"$NONCE\",\"plaintext\":\"$PLAINTEXT\",\"associated_data\":\"$AD\"}" \
    | jq -r .ciphertext)

echo "Ciphertext (with AD): ${CIPHERTEXT_AD:0:32}..."

DECRYPTED_AD=$(curl -s -X POST "$API/aead/decrypt" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"$KEY\",\"nonce\":\"$NONCE\",\"ciphertext\":\"$CIPHERTEXT_AD\",\"associated_data\":\"$AD\"}" \
    | jq -r .plaintext)

echo "Decrypted (with AD): $DECRYPTED_AD"
if [ "$DECRYPTED_AD" == "$PLAINTEXT" ]; then
    echo "✅ AEAD with AD round-trip success"
else
    echo "❌ AEAD with AD round-trip failed"
    exit 1
fi

echo "[6] AEAD with Wrong Associated Data..."
BAD_DECRYPT=$(curl -s -X POST "$API/aead/decrypt" "${AUTH[@]}" \
    -H "Content-Type: application/json" \
    -d "{\"key\":\"$KEY\",\"nonce\":\"$NONCE\",\"ciphertext\":\"$CIPHERTEXT_AD\",\"associated_data\":\"wrong_ad\"}" \
    || true)

if echo "$BAD_DECRYPT" | grep -q "decryption failed"; then
    echo "✅ AEAD rejected tampered associated data"
else
    echo "❌ AEAD accepted wrong AD (BUG)"
    exit 1
fi
