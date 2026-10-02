package main

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"github.com/jackc/pgx/v5"
	"github.com/jackc/pgx/v5/pgxpool"
)

// DB wraps a pgx connection pool. pgx is the idiomatic Postgres driver for
// modern Go — type-safe, faster than database/sql, supports JSON/UUID natively.
type DB struct {
	pool *pgxpool.Pool
}

func openDB(ctx context.Context, dbURL string) (*DB, error) {
	cfg, err := pgxpool.ParseConfig(dbURL)
	if err != nil {
		return nil, fmt.Errorf("parse db url: %w", err)
	}
	cfg.MaxConns = 10
	pool, err := pgxpool.NewWithConfig(ctx, cfg)
	if err != nil {
		return nil, fmt.Errorf("connect db: %w", err)
	}
	if err := pool.Ping(ctx); err != nil {
		pool.Close()
		return nil, fmt.Errorf("ping db: %w", err)
	}
	return &DB{pool: pool}, nil
}

func (d *DB) Close() {
	d.pool.Close()
}

func (d *DB) BeginTx(ctx context.Context) (pgx.Tx, error) {
	return d.pool.Begin(ctx)
}

// OutboxBundle mirrors one row of dtn_outbox.
//
// dest_node_id and the UUID type are returned as a string for ease of
// logging and re-encoding into outgoing JSON. The payload is kept as a
// raw JSON message — we don't need to unmarshal it, just forward it.
type OutboxBundle struct {
	ID         int64
	DestNodeID string
	Endpoint   string
	Payload    json.RawMessage
	Attempts   int
}

// FetchDueBundles selects and LOCKS up to `limit` bundles that are ready
// to retry (next_try_at <= NOW). Uses FOR UPDATE SKIP LOCKED so multiple
// relay replicas can run safely — each picks rows the others haven't claimed.
func (d *DB) FetchDueBundles(ctx context.Context, tx pgx.Tx, limit int) ([]OutboxBundle, error) {
	rows, err := tx.Query(ctx, `
		SELECT id, dest_node_id::text, endpoint, payload, attempts
		FROM dtn_outbox
		WHERE next_try_at <= NOW()
		ORDER BY next_try_at
		LIMIT $1
		FOR UPDATE SKIP LOCKED
	`, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()

	var out []OutboxBundle
	for rows.Next() {
		var b OutboxBundle
		if err := rows.Scan(&b.ID, &b.DestNodeID, &b.Endpoint, &b.Payload, &b.Attempts); err != nil {
			return nil, err
		}
		out = append(out, b)
	}
	return out, rows.Err()
}

// DeleteBundle is called after a successful POST — the bundle is delivered,
// we don't need to keep the row around.
func (d *DB) DeleteBundle(ctx context.Context, tx pgx.Tx, id int64) error {
	_, err := tx.Exec(ctx, `DELETE FROM dtn_outbox WHERE id = $1`, id)
	return err
}

// BumpAttempt is called after a failed POST — increments the retry counter
// and pushes next_try_at out per the caller's backoff calculation.
func (d *DB) BumpAttempt(ctx context.Context, tx pgx.Tx, id int64, nextTry time.Time) error {
	_, err := tx.Exec(ctx,
		`UPDATE dtn_outbox SET attempts = attempts + 1, next_try_at = $2 WHERE id = $1`,
		id, nextTry)
	return err
}

// WriteInbox is gone, deliberately.
//
// It inserted into dtn_inbox with no authentication of the sender and no
// replay check, serving an endpoint published to 0.0.0.0. The comment it
// carried — "peers may post anonymously during bootstrap before HMAC is wired
// up" — described a temporary state that became permanent, which is the usual
// way this happens.
//
// Reception now belongs to the core-api's POST /api/dtn/receive, which
// verifies the sender's Ed25519 signature over an envelope that binds the
// recipient and the bundle lifetime, and refuses a message id it has already
// seen. Removing the function rather than leaving it unused means a future
// handler cannot quietly reintroduce the path.
