# ============================================================
# 🌐 tidasone-mesh — Go networking sidecar
# Multi-stage build: tiny final image (~20 MB)
# ============================================================

FROM golang:1.22-alpine AS builder

WORKDIR /src

# Copy source. We tidy here so go.sum gets generated/refreshed on every
# build — handles fresh clones without a committed go.sum.
COPY . .
RUN go mod tidy

# Static binary, stripped — runs on minimal alpine without glibc.
RUN CGO_ENABLED=0 GOOS=linux go build \
    -ldflags="-s -w" \
    -o /out/mesh-relay .

# ============================================================
# Runtime
# ============================================================
FROM alpine:3.19

RUN apk add --no-cache ca-certificates wget && rm -rf /var/cache/apk/*

COPY --from=builder /out/mesh-relay /usr/local/bin/mesh-relay

EXPOSE 3100

HEALTHCHECK --interval=10s --timeout=5s --retries=10 \
  CMD wget -qO- http://127.0.0.1:3100/health > /dev/null || exit 1

ENTRYPOINT ["/usr/local/bin/mesh-relay"]
