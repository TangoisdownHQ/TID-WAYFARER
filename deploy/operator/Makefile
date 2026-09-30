# ============================================================
# tid-wayfarer operator — Makefile
# ============================================================
IMG ?= tid-wayfarer-operator:latest
CONTROLLER_GEN ?= go run sigs.k8s.io/controller-tools/cmd/controller-gen@v0.16.1
KUSTOMIZE ?= go run sigs.k8s.io/kustomize/kustomize/v5@v5.4.3

.PHONY: all
all: build

## --- Development ---
.PHONY: fmt vet build test
fmt:
	go fmt ./...

vet:
	go vet ./...

build: fmt vet
	go build -o bin/manager ./cmd

test:
	go test ./... -count=1

run: build
	go run ./cmd

## --- Code / manifest generation (regenerate after editing api/) ---
.PHONY: generate manifests
generate:
	$(CONTROLLER_GEN) object:headerFile="hack/boilerplate.go.txt" paths="./api/..."

manifests:
	$(CONTROLLER_GEN) crd rbac:roleName=tid-wayfarer-operator-manager-role \
		paths="./..." output:crd:artifacts:config=config/crd/bases

## --- Container image ---
.PHONY: docker-build docker-push
docker-build:
	docker build -t $(IMG) .

docker-push:
	docker push $(IMG)

## --- Cluster install ---
.PHONY: install-crd deploy undeploy
install-crd:
	kubectl apply -f config/crd/bases/wayfarer.tid.net_outposts.yaml

deploy:
	$(KUSTOMIZE) build config/default | kubectl apply -f -

undeploy:
	$(KUSTOMIZE) build config/default | kubectl delete --ignore-not-found -f -

## --- Local kind e2e (build API image + operator, load, apply a sample) ---
.PHONY: kind-load
kind-load: docker-build
	kind load docker-image $(IMG)
