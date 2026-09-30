# tid-wayfarer operator

A Kubernetes operator that makes tid-wayfarer **natively Kubernetes**: one custom
resource — `kind: Outpost` — spawns and reconciles a whole outpost (API
Deployment + Service, embedded Postgres, identity-keys PVC, and a migration Job).

```
kubectl apply -f outpost.yaml    # → a running outpost, reconciled continuously
```

This is the controller form of the [Helm chart](../helm/tid-wayfarer). The chart
is one-shot templating; the operator watches `Outpost` objects and drives the
cluster toward their spec forever (self-healing, status reporting, GC on delete).

## Custom resource

`Outpost` (`wayfarer.tid.net/v1alpha1`, namespaced, short names `op`/`outpost`):

| field | default | meaning |
|---|---|---|
| `spec.role` | `outpost` | `core` (HQ) or `outpost` (regional/surface) |
| `spec.bodyId` | `399` | NAIF body id (Earth 399, Moon 301, Mars 499 …) |
| `spec.region` | — | human label, propagated to `wayfarer.tid.net/region` |
| `spec.replicas` | `1` | API replicas (keep 1 unless keys PVC is RWX) |
| `spec.image.*` | `tid-wayfarer:latest` | API image repo/tag/pullPolicy |
| `spec.peers.coreApiUrl` | — | Core HQ registration endpoint for outposts |
| `spec.postgres.*` | enabled, `tidasone` db, 5Gi | embedded database |
| `spec.secretName` | — | existing Secret; if empty the operator generates JWT + DB password |
| `spec.migrate` | `true` | run bundled SQL migrations as a Job |

`status`: `phase` (Pending/Provisioning/Ready/Degraded), `readyReplicas`,
`observedGeneration`, `conditions`.

## What the controller reconciles per Outpost

| Child object | Notes |
|---|---|
| `Secret` | generated once (JWT_SECRET + POSTGRES_PASSWORD) unless `secretName` set |
| `ConfigMap` | OUTPOST_NAME / role / body / region / peers env |
| `StatefulSet` + headless `Service` (postgres) | skipped when `postgres.enabled=false` |
| `PersistentVolumeClaim` (keys) | stable `node_id` across restarts |
| `Job` (migrate) | applies `/app/migrations/*.sql`; create-once |
| `Deployment` + `Service` (api) | the tid-wayfarer API with startup/liveness/readiness probes |

All children are owner-referenced to the `Outpost`, so `kubectl delete outpost X`
garbage-collects everything.

## Build & run

```bash
# build the operator binary (fmt + vet + compile)
make build

# regenerate deepcopy + CRD after editing api/ (needs network for controller-gen)
make generate manifests

# container image
make docker-build IMG=tid-wayfarer-operator:latest
```

## Install to a cluster

```bash
# 1. CRD only (quickest smoke test)
make install-crd

# 2. Full install: CRD + RBAC + controller manager (namespace tid-wayfarer-system)
make deploy IMG=tid-wayfarer-operator:latest

# 3. Create an outpost
kubectl create namespace tid-wayfarer
kubectl apply -f config/samples/wayfarer_v1alpha1_outpost.yaml
kubectl -n tid-wayfarer get outposts
# NAME                   ROLE      BODY   REGION        PHASE   READY   AGE
# core-hq                core      399    US-East-NYC   Ready   1       2m
# outpost-mars-jezero    outpost   499    Mars-Jezero   Ready   1       2m
```

## Local kind end-to-end

```bash
# from repo root: build the API image the Outpost references
docker build -t tid-wayfarer:latest -f TIDasONE/dockerfile TIDasONE
kind load docker-image tid-wayfarer:latest

# build + load the operator, deploy, apply a sample
cd TIDasONE/deploy/operator
make kind-load
make deploy
kubectl apply -f config/samples/wayfarer_v1alpha1_outpost.yaml
```

## Roadmap

- `kind: AstroNetRoute` — declarative interplanetary routing between outposts.
- `kind: AssetTwin` — reconcile a cyber-physical asset's desired state.
- Ingress management + cert-manager integration on the `Outpost` spec.
- Emit Kubernetes Events and richer `status.conditions`.
