// tidasone-mesh — Go networking sidecar for TIDasONE.
//
// One process per outpost, runs next to the Rust core-api, shares the same
// Postgres.
//
// # Current state: the forwarder is off by default, and it has to be
//
// This process and the core-api both drained dtn_outbox. Two forwarders doing
// one job is bad on its own; what made it a defect is that this one **cannot
// authenticate**. It POSTs bundles with a Content-Type and two informational
// headers and no fabric signature at all — no x-node-id, no x-node-timestamp,
// no x-node-signature, no x-node-token. The core-api's /api/dtn/receive now
// requires a named peer, so every bundle this forwarder delivers is refused.
//
// It then did active harm rather than merely failing. On a failed delivery it
// bumps `attempts` and pushes `next_try_at` into the future — so a bundle it
// could never deliver got backed off, and the Rust forwarder, which signs
// correctly and *would* have delivered it, had to wait out a delay caused
// entirely by this process losing the race to claim the row.
//
// So `RELAY_FORWARDER` defaults to `off`. Responsibilities while off:
//
//  1. Refuse POSTs to /inbox, which used to write dtn_inbox unauthenticated;
//     reception belongs to the core-api's /api/dtn/receive (see inbox.go).
//  2. Expose /health and /metrics for ops.
//
// # Why this service exists at all
//
// The original reasoning is still sound: network I/O fans out wide and gets
// retried often, Go's goroutines and net/http suit that shape, and keeping it
// out of the API process means a flaky peer cannot stall request handling.
// Turning it back on is a matter of teaching `forwarder.go` to sign requests
// the way services/fabric_auth.rs does — canonical string over
// method|path|timestamp|body-hash, Ed25519 over that, three headers. Until
// then, enabling it only produces rejected deliveries and delayed ones.
package main

import (
	"context"
	"errors"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

func main() {
	// Structured JSON logs — friendly to docker logs, loki, etc.
	logger := slog.New(slog.NewJSONHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelInfo}))
	slog.SetDefault(logger)

	cfg, err := loadConfig()
	if err != nil {
		slog.Error("config load failed", "err", err)
		os.Exit(1)
	}

	// signal.NotifyContext gives us a Context that's cancelled on Ctrl-C
	// or SIGTERM from K8s/Docker. Every long-running goroutine watches it.
	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	db, err := openDB(ctx, cfg.DatabaseURL)
	if err != nil {
		slog.Error("db connect failed", "err", err)
		os.Exit(1)
	}
	defer db.Close()
	slog.Info("connected to postgres")

	metrics := newMetrics()

	// Opt-in, and loudly, because an enabled forwarder here is currently a
	// regression rather than extra capacity. See the package comment.
	if cfg.ForwarderEnabled {
		slog.Warn("forwarder ENABLED — it cannot sign fabric requests, so peers running "+
			"a current build will refuse its deliveries, and its failures will delay the "+
			"core-api forwarder by backing off rows it claimed",
			"disable_with", "RELAY_FORWARDER=off")
		fwd := &Forwarder{
			DB:           db,
			HTTPTimeout:  cfg.HTTPTimeout,
			PollInterval: cfg.PollInterval,
			BatchSize:    cfg.BatchSize,
			NodeID:       cfg.NodeID,
			Metrics:      metrics,
		}
		go fwd.Run(ctx)
	} else {
		slog.Info("forwarder disabled; the core-api drains dtn_outbox",
			"enable_with", "RELAY_FORWARDER=on")
	}

	inboxHandler := &InboxHandler{DB: db, Metrics: metrics}

	mux := http.NewServeMux()
	mux.HandleFunc("/health", healthHandlerFor(cfg))
	mux.Handle("/inbox", inboxHandler)
	mux.Handle("/metrics", metrics.Handler())

	srv := &http.Server{
		Addr:              cfg.ListenAddr,
		Handler:           mux,
		ReadHeaderTimeout: 10 * time.Second,
	}

	go func() {
		slog.Info("listening",
			"addr", cfg.ListenAddr,
			"outpost", cfg.OutpostName,
			"node_id", cfg.NodeID,
		)
		if err := srv.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
			slog.Error("http server crashed", "err", err)
			cancel()
		}
	}()

	// Block until the OS sends a signal or anything calls cancel().
	<-ctx.Done()
	slog.Info("shutting down")

	// Drain in-flight requests with a bounded timeout.
	shutdownCtx, cancel2 := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel2()
	if err := srv.Shutdown(shutdownCtx); err != nil {
		slog.Error("graceful shutdown failed", "err", err)
	}
	slog.Info("bye 👋")
}

// Health says what this process is actually doing, not just that it is up.
// A green check on a service whose only job is switched off is the kind of
// reassurance that costs someone an afternoon.
func healthHandlerFor(cfg *Config) http.HandlerFunc {
	role := "metrics-only (forwarder disabled; core-api drains dtn_outbox)"
	if cfg.ForwarderEnabled {
		role = "forwarding (unsigned — peers on a current build will refuse these)"
	}
	return func(w http.ResponseWriter, _ *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte(`{"status":"ok","service":"tidasone-mesh-relay","role":"` + role + `"}`))
	}
}
