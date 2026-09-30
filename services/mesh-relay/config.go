package main

import (
	"fmt"
	"os"
	"strconv"
	"time"
)

// Config is loaded once from environment variables at startup. In K8s this
// comes from a ConfigMap + Secret; in docker-compose, from the `environment:`
// block. Everything except DATABASE_URL has a sensible default.
type Config struct {
	DatabaseURL  string
	ListenAddr   string
	NodeID       string        // this outpost's UUID — stamped on outgoing bundles
	OutpostName  string        // human label for logs
	PollInterval time.Duration // how often the forwarder scans dtn_outbox
	HTTPTimeout  time.Duration // per-bundle delivery timeout
	BatchSize    int           // max bundles claimed per loop iteration
}

func loadConfig() (*Config, error) {
	dbURL := os.Getenv("DATABASE_URL")
	if dbURL == "" {
		return nil, fmt.Errorf("DATABASE_URL is required")
	}
	return &Config{
		DatabaseURL:  dbURL,
		ListenAddr:   envOr("LISTEN_ADDR", ":3100"),
		NodeID:       os.Getenv("NODE_ID"),
		OutpostName:  envOr("OUTPOST_NAME", "tidasone-outpost"),
		PollInterval: envDuration("POLL_INTERVAL", 5*time.Second),
		HTTPTimeout:  envDuration("HTTP_TIMEOUT", 30*time.Second),
		BatchSize:    envInt("BATCH_SIZE", 50),
	}, nil
}

// envOr returns the env value or `def` if unset/empty.
func envOr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

// envInt parses an env value as int, falling back to `def` on miss or parse error.
func envInt(key string, def int) int {
	v := os.Getenv(key)
	if v == "" {
		return def
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		return def
	}
	return n
}

// envDuration parses an env value as a Go duration (e.g. "5s", "1m30s"),
// falling back to `def` on miss or parse error.
func envDuration(key string, def time.Duration) time.Duration {
	v := os.Getenv(key)
	if v == "" {
		return def
	}
	d, err := time.ParseDuration(v)
	if err != nil {
		return def
	}
	return d
}
