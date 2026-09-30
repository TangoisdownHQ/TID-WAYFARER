package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"net/http"
	"strconv"
	"time"
)

// Forwarder drives the outbox → peer delivery loop.
//
// Each iteration:
//  1. Open a tx
//  2. Claim up to BatchSize due bundles via SELECT FOR UPDATE SKIP LOCKED
//  3. For each: POST to its endpoint
//     - 2xx  → DELETE the row
//     - else → bump attempts + push next_try_at out (exp backoff)
//  4. Commit
//
// The transaction holds the row locks for the duration of the HTTP calls,
// so other relay replicas skip these and pick different rows. Good enough
// for small fleets; for very large fleets, split into per-bundle txs.
type Forwarder struct {
	DB           *DB
	HTTPTimeout  time.Duration
	PollInterval time.Duration
	BatchSize    int
	NodeID       string // stamped onto outbound bundles as src_node_id
	Metrics      *Metrics
}

// Run blocks until ctx is cancelled, ticking on PollInterval.
func (f *Forwarder) Run(ctx context.Context) {
	ticker := time.NewTicker(f.PollInterval)
	defer ticker.Stop()

	slog.Info("forwarder started",
		"poll_interval", f.PollInterval.String(),
		"batch_size", f.BatchSize,
		"http_timeout", f.HTTPTimeout.String(),
	)

	for {
		select {
		case <-ctx.Done():
			slog.Info("forwarder stopping")
			return
		case <-ticker.C:
			f.processBatch(ctx)
		}
	}
}

func (f *Forwarder) processBatch(ctx context.Context) {
	f.Metrics.LoopIters.Inc()

	tx, err := f.DB.BeginTx(ctx)
	if err != nil {
		slog.Error("begin tx", "err", err)
		return
	}
	// Rollback is a no-op after a successful Commit, so it's safe to defer.
	defer tx.Rollback(ctx)

	bundles, err := f.DB.FetchDueBundles(ctx, tx, f.BatchSize)
	if err != nil {
		slog.Error("fetch bundles", "err", err)
		return
	}
	if len(bundles) == 0 {
		return
	}

	client := &http.Client{Timeout: f.HTTPTimeout}

	for _, b := range bundles {
		if err := f.deliver(ctx, client, b); err != nil {
			f.Metrics.BundlesFailed.Inc()
			attempt := b.Attempts + 1
			nextTry := time.Now().Add(backoff(attempt))
			slog.Warn("delivery failed",
				"bundle_id", b.ID,
				"endpoint", b.Endpoint,
				"attempt", attempt,
				"next_try_in", time.Until(nextTry).Round(time.Second).String(),
				"err", err)
			if err := f.DB.BumpAttempt(ctx, tx, b.ID, nextTry); err != nil {
				slog.Error("bump attempt", "bundle_id", b.ID, "err", err)
			}
			continue
		}
		f.Metrics.BundlesForwarded.Inc()
		slog.Info("delivered", "bundle_id", b.ID, "endpoint", b.Endpoint)
		if err := f.DB.DeleteBundle(ctx, tx, b.ID); err != nil {
			slog.Error("delete delivered bundle", "bundle_id", b.ID, "err", err)
		}
	}

	if err := tx.Commit(ctx); err != nil {
		slog.Error("commit tx", "err", err)
	}
}

// deliver POSTs one bundle to its destination endpoint. Returns nil on 2xx,
// non-nil on any HTTP error or non-2xx status.
func (f *Forwarder) deliver(ctx context.Context, client *http.Client, b OutboxBundle) error {
	envelope := map[string]any{
		"payload": json.RawMessage(b.Payload),
	}
	if f.NodeID != "" {
		envelope["src_node_id"] = f.NodeID
	}
	body, err := json.Marshal(envelope)
	if err != nil {
		return fmt.Errorf("marshal envelope: %w", err)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, b.Endpoint, bytes.NewReader(body))
	if err != nil {
		return fmt.Errorf("build request: %w", err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("X-TIDasONE-Bundle-Id", strconv.FormatInt(b.ID, 10))
	req.Header.Set("X-TIDasONE-Dest-Node", b.DestNodeID)

	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode >= 200 && resp.StatusCode < 300 {
		return nil
	}
	return fmt.Errorf("peer returned status %d", resp.StatusCode)
}

// backoff returns the delay before the next attempt: 5s * 2^(attempt-1),
// capped at one hour. So: 5s, 10s, 20s, 40s, 80s, ..., 1h, 1h, …
func backoff(attempt int) time.Duration {
	if attempt < 1 {
		attempt = 1
	}
	secs := 5.0 * math.Pow(2, float64(attempt-1))
	if secs > 3600 {
		secs = 3600
	}
	return time.Duration(secs) * time.Second
}
