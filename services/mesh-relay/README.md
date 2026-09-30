# tidasone-mesh — Go networking sidecar

Cross-outpost network plane for TIDasONE. Runs next to the Rust `core-api`, shares the same Postgres, and owns the store-and-forward mesh that ties outposts together across regions, countries, and (eventually) bodies.

## What it does

1. **Forwarder** — polls `dtn_outbox`, POSTs each bundle to its destination endpoint. On 2xx, deletes the row. On error, bumps `attempts` and pushes `next_try_at` out with exponential backoff (5s → 10s → 20s → … → 1h cap).
2. **Inbox** — `POST /inbox` accepts `{src_node_id, payload}` from peer outposts and writes the payload into `dtn_inbox`. The Rust core-api's DTN worker then acts on it.
3. **Metrics** — Prometheus at `/metrics` (forwarded/failed/received counts + loop iterations).
4. **Health** — `/health` returns `200 {"status":"ok"}`.

## Why Go (and why a separate service)

Network fan-out + retries + concurrent peer talk is Go's strongest workload — goroutines, the `net/http` client, `context.Context` cancellation, structured logging via `log/slog`. Keeping it out of the Rust API process means a flaky peer can't stall request handling, and a relay restart doesn't blip the API.

Both processes treat the **outbox/inbox tables** (already in the schema since `20251102_ops_brain.sql`) as the queue between them. No new wire protocol; just rows in Postgres.

## Files

| file | purpose |
|---|---|
| `main.go` | wiring, HTTP server, graceful shutdown |
| `config.go` | env var loading w/ defaults |
| `db.go` | pgx pool + outbox/inbox queries |
| `forwarder.go` | poll loop + exp backoff + HTTP POST |
| `inbox.go` | POST /inbox handler |
| `metrics.go` | Prometheus instruments |
| `Dockerfile` | multi-stage build → ~20MB alpine image |

## Build & run locally (host)

```bash
cd TIDasONE/services/mesh-relay
go mod tidy              # generates go.sum on first run

DATABASE_URL=postgres://postgres:CosmicExplorer@127.0.0.1:5432/tidasone \
NODE_ID=00000000-0000-0000-0000-000000000001 \
OUTPOST_NAME=local-dev \
LISTEN_ADDR=":3100" \
go run .
```

## Build & run via docker-compose

The `tidasone-mesh-relay` service is wired into `docker-compose.yml`:

```bash
sudo docker compose up -d --build tidasone-mesh-relay
sudo docker compose logs -f tidasone-mesh-relay
```

## Smoke test

```bash
./TIDasONE/scripts/test_relay.sh
```

The test:
1. Hits `/health` and `/metrics` (both must return 200).
2. Inserts a bundle into `dtn_outbox` with an unreachable endpoint, waits up to 30s, verifies the forwarder attempted delivery (`attempts >= 1`).
3. Posts a valid bundle to `/inbox`, verifies it landed in `dtn_inbox`.
4. Cleans up the fixtures.

## Environment

| var | default | what it controls |
|---|---|---|
| `DATABASE_URL` | — *(required)* | same Postgres as core-api |
| `LISTEN_ADDR` | `:3100` | HTTP bind for /health /inbox /metrics |
| `NODE_ID` | `""` | this outpost's UUID — stamped on outbound bundles |
| `OUTPOST_NAME` | `tidasone-outpost` | log label |
| `POLL_INTERVAL` | `5s` | how often the forwarder scans `dtn_outbox` |
| `HTTP_TIMEOUT` | `30s` | per-bundle delivery timeout |
| `BATCH_SIZE` | `50` | max bundles claimed per loop iteration |

## SQL the forwarder runs (one transaction per iteration)

```sql
SELECT id, dest_node_id, endpoint, payload, attempts
FROM dtn_outbox
WHERE next_try_at <= NOW()
ORDER BY next_try_at
LIMIT $1
FOR UPDATE SKIP LOCKED;       -- ← multiple replicas don't race
```

For each row:
- 2xx → `DELETE FROM dtn_outbox WHERE id = $1`
- else → `UPDATE dtn_outbox SET attempts = attempts + 1, next_try_at = NOW() + backoff(attempts+1) WHERE id = $1`

## Wire format (outgoing POST)

```http
POST <endpoint>
Content-Type: application/json
X-TIDasONE-Bundle-Id: <id>
X-TIDasONE-Dest-Node: <uuid>

{
  "src_node_id": "<this outpost's uuid>",
  "payload": { ...whatever the Rust side wrote to dtn_outbox.payload... }
}
```

## TODO / not yet wired

- **HMAC verification** of inbound bundles via `node_registry.hmac_secret`. Schema is ready; handler stub is in place. Need to compute and compare a per-bundle MAC.
- **Dead-letter** when `attempts` exceeds a configurable threshold — currently rows keep backing off at the 1h cap forever.
- **Peer health gauge** — Prometheus `peers_alive{trust_level="trusted"}` from `node_registry` + `peer_nodes`.
- **mTLS / Tailscale / wireguard** between outposts — relies for now on TLS termination at the ingress + the JWT/HMAC layer above HTTP.
