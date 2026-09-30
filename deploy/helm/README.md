# tid-wayfarer Helm Chart

Deploy a TIDasONE outpost to any Kubernetes cluster — Core HQ, terrestrial regional outpost, or surface outpost on the Moon/Mars/wherever. **One Helm release == one outpost.** Outposts sync via the existing mesh (DTN forwarder + node_registry).

```
deploy/helm/tid-wayfarer/
├── Chart.yaml
├── values.yaml                              # defaults
├── templates/
│   ├── _helpers.tpl
│   ├── configmap.yaml
│   ├── secret.yaml
│   ├── postgres-statefulset.yaml
│   ├── postgres-service.yaml
│   ├── api-deployment.yaml
│   ├── api-service.yaml
│   ├── api-ingress.yaml
│   ├── api-pvc-keys.yaml
│   └── migrate-job.yaml
└── examples/
    ├── values-core-hq.yaml                  # US East
    ├── values-outpost-uk-london.yaml        # UK (Earth)
    └── values-outpost-mars-jezero.yaml      # Mars (Jezero)
```

## Quick start — local cluster (kind / minikube / k3d)

```bash
cd ~/LOTL/TIDHQ.NETWORK/TIDHQ.NET-OUTPOSTS

# 1. Build the API image (one-time, until you push to a registry)
sudo docker build -t tid-wayfarer:latest \
  -f TIDasONE/dockerfile TIDasONE

# 2. Load into your cluster (skip if your kubelet pulls from a registry)
# kind:
kind load docker-image tid-wayfarer:latest
# minikube:
minikube image load tid-wayfarer:latest
# k3d:
k3d image import tid-wayfarer:latest

# 3. Install Core HQ
helm install core ./TIDasONE/deploy/helm/tid-wayfarer \
  -f ./TIDasONE/deploy/helm/tid-wayfarer/examples/values-core-hq.yaml \
  -n tid-wayfarer --create-namespace

# 4. Wait + verify
kubectl -n tid-wayfarer wait --for=condition=Ready pod \
  -l app.kubernetes.io/instance=core,wayfarer.tid.net/component=api --timeout=300s

kubectl -n tid-wayfarer port-forward svc/core-tid-wayfarer-api 4000:3000 &
curl -s http://127.0.0.1:4000/api/bodies | jq 'length'   # → 57
curl -s http://127.0.0.1:4000/api/bodies/499 | jq '.name' # → "Mars"
```

## Multi-region deployment

Each outpost is its own Helm release with its own values file. They register with each other on startup and continue syncing through the mesh.

```bash
# US Core HQ — public ingress for outposts to register against
helm install core ./TIDasONE/deploy/helm/tid-wayfarer \
  -f examples/values-core-hq.yaml \
  -n tid-wayfarer-us --create-namespace

# UK London — registers with Core on boot via peers.coreApiUrl
helm install outpost-uk ./TIDasONE/deploy/helm/tid-wayfarer \
  -f examples/values-outpost-uk-london.yaml \
  -n tid-wayfarer-uk --create-namespace

# Mars Jezero — no synchronous comms; DTN forwarder handles eventual sync
helm install outpost-mars ./TIDasONE/deploy/helm/tid-wayfarer \
  -f examples/values-outpost-mars-jezero.yaml \
  -n tid-wayfarer-mars --create-namespace
```

Add new countries/states/sites by copying one of the examples, changing `outpost.name`, `outpost.location`, and the secrets.

## What the chart does

| Resource | When | Purpose |
|---|---|---|
| `ConfigMap` | install | non-secret env (OUTPOST_NAME, body/region, peers, host/port) |
| `Secret` | install | `JWT_SECRET` and `POSTGRES_PASSWORD` |
| `StatefulSet` (postgres) | install | embedded postgres w/ PVC (skip if using managed DB) |
| `Service` (postgres) | install | headless service for StatefulSet DNS |
| `PVC` (keys) | install | persistent identity keys → stable `node_id` across pod restarts |
| `Deployment` (api) | install | the core-api binary w/ startup/liveness/readiness probes |
| `Service` (api) | install | cluster-internal API service |
| `Ingress` (api) | optional | external HTTP access (set `ingress.enabled=true`) |
| `Job` (migrate) | post-install, post-upgrade | runs every `packages/db/migrations/*.sql` against the live DB |

The migration `Job` runs **after** the chart's resources exist (so postgres is reachable) and uses the same image as the API — `psql` is now bundled in the runtime stage along with the migration SQL files at `/app/migrations/`. The API pod has a `startupProbe` with a 150s budget to cover the migration runtime, so on first install the API stays Pending until migrations complete then transitions Ready.

## External / managed Postgres

For production multi-region, you'll often want a managed DB per region (RDS, Cloud SQL, Neon, Supabase). Disable the embedded postgres and point at the external one:

```yaml
postgres:
  enabled: false

externalDatabase:
  host: my-region.rds.amazonaws.com
  port: 5432
  user: tidasone
  database: tidasone

secret:
  dbPassword: <password for the external DB>
```

The chart will skip the StatefulSet/Service and the migration Job will target the external host instead.

## Values reference

See `values.yaml` for the full schema and defaults. The fields most worth knowing:

| key | example | what it controls |
|---|---|---|
| `outpost.name` | `tid-wayfarer-outpost-uk-london` | unique outpost identifier (also `OUTPOST_NAME` env) |
| `outpost.role` | `core` / `outpost` | label only — distinguishes Core HQ from regional outposts |
| `outpost.bodyId` | `399` / `301` / `499` | NAIF body id — drives map / routing / region semantics |
| `outpost.location.region` | `"Mars-Jezero"` | human label, propagates as label `wayfarer.tid.net/region` |
| `replicaCount` | `1` | API replicas — keep at 1 unless you provision RWX keys |
| `peers.coreApiUrl` | `https://core…/api/nodes/register` | outpost registers with Core on boot |
| `postgres.enabled` | `true` | embedded postgres vs. external |
| `ingress.enabled` | `false` | external HTTP via Ingress |
| `secret.jwt` | `…` | **REPLACE** before applying |
| `secret.dbPassword` | `…` | **REPLACE** before applying |

## Upgrading

```bash
# Add new SQL files to packages/db/migrations/, rebuild the image,
# then upgrade. The migrate Job re-runs and picks up the new files.
helm upgrade core ./TIDasONE/deploy/helm/tid-wayfarer \
  -f examples/values-core-hq.yaml -n tid-wayfarer
```

All migrations use `IF NOT EXISTS` and `ON CONFLICT DO NOTHING`, so re-running is safe.

## Uninstalling

```bash
helm uninstall core -n tid-wayfarer
# StatefulSet + keys PVCs persist by design (so data survives uninstall).
# To wipe them:
kubectl -n tid-wayfarer delete pvc -l app.kubernetes.io/instance=core
```

## Verifying after install

```bash
# 1. All pods Ready
kubectl -n tid-wayfarer get pods

# 2. Migration Job logs (should end with "🎉 migrations complete")
kubectl -n tid-wayfarer logs job/core-tid-wayfarer-migrate

# 3. API health + body count
kubectl -n tid-wayfarer port-forward svc/core-tid-wayfarer-api 4000:3000 &
curl -s http://127.0.0.1:4000/api/health | jq
curl -s http://127.0.0.1:4000/api/bodies | jq 'length'   # 57

# 4. The bodies smoke test still works against the K8s service
API_BASE=http://127.0.0.1:4000 ./TIDasONE/scripts/test_bodies.sh
```

## What's next (Phase B)

A Kubernetes Operator with CRDs — `kind: Outpost`, `kind: AstroNetRoute`, `kind: AssetTwin`. The controller reconciles by managing Deployments and DB rows. That turns "spawn an outpost on Mars" into a single `kubectl apply -f mars-outpost.yaml`. Worth doing once you have ~5+ outposts; not before.
