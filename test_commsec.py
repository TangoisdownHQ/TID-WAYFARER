import requests, base64
from pqcrypto.kem import mlkem1024

API_BASE = "http://127.0.0.1:3000"

# 1️⃣ Fetch server’s PQ public key
resp = requests.get(f"{API_BASE}/commsec/keys/pq").json()
kem_pk_b64 = resp["kem_public_key"]
kem_pk_bytes = base64.b64decode(kem_pk_b64)

print("🔑 Received KEM Public Key (len={}):".format(len(kem_pk_bytes)))
print(kem_pk_b64[:80] + "...")

# 2️⃣ Encapsulate using server’s public key
ciphertext, shared_secret_client = mlkem1024.encapsulate(kem_pk_bytes)
print("🤝 Client Shared Secret (b64):", base64.b64encode(shared_secret_client).decode())

# 3️⃣ Send ciphertext to server for decapsulation
payload = {"ciphertext": base64.b64encode(ciphertext).decode()}
resp2 = requests.post(f"{API_BASE}/commsec/decapsulate", json=payload).json()

server_shared_secret = resp2
print("🖥️  Server Shared Secret (b64):", server_shared_secret)

# 4️⃣ Compare secrets
if server_shared_secret == base64.b64encode(shared_secret_client).decode():
    print("✅ Handshake complete — shared secret matches")
else:
    print("❌ Handshake mismatch!")

