// tidasone-mesh — Go networking sidecar for TIDasONE.
//
// One process per outpost, runs next to the Rust core-api, shares the same
// Postgres. Three responsibilities:
//
//  1. Poll dtn_outbox and POST bundles to peer outposts (the forwarder).
//  2. Receive POSTs to /inbox and write them into dtn_inbox.
//  3. Expose /health and /metrics for ops.
//
// Why a separate service? Network I/O fans out wide and gets retried often;
// Go goroutines + the standard `net/http` client are tailor-made for that
// shape, and keeping it out of the API process means a flaky peer can't
// stall request handling. Both services treat the outbox/inbox tables as
// the queue between them.
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

	fwd := &Forwarder{
		DB:           db,
		HTTPTimeout:  cfg.HTTPTimeout,
		PollInterval: cfg.PollInterval,
		BatchSize:    cfg.BatchSize,
		NodeID:       cfg.NodeID,
		Metrics:      metrics,
	}
	go fwd.Run(ctx)

	inboxHandler := &InboxHandler{DB: db, Metrics: metrics}

	mux := http.NewServeMux()
	mux.HandleFunc("/health", healthHandler)
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

func healthHandler(w http.ResponseWriter, _ *http.Request) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusOK)
	_, _ = w.Write([]byte(`{"status":"ok","service":"tidasone-mesh-relay"}`))
}
