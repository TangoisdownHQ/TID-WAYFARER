package main

import (
	"fmt"
	"os"
	"strconv"
	"strings"
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

	// Whether to drain dtn_outbox at all. Off by default: this forwarder
	// sends no fabric signature, so a current peer refuses every bundle it
	// delivers, and its failed attempts back off rows the core-api forwarder
	// would have delivered. See the package comment in main.go.
	ForwarderEnabled bool
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
		// Off unless explicitly asked for. The core-api forwarder signs its
		// requests; this one does not, so enabling it produces refused
		// deliveries and delays the one that works.
		ForwarderEnabled: envBool("RELAY_FORWARDER", false),
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

// envBool reads a boolean-ish environment variable. Accepts the spellings
// people actually type rather than only Go's strconv set, because a config
// flag that silently means "off" when someone wrote "on" is worse than no
// flag.
func envBool(key string, def bool) bool {
	switch strings.ToLower(strings.TrimSpace(os.Getenv(key))) {
	case "":
		return def
	case "1", "true", "yes", "on", "enabled":
		return true
	default:
		return false
	}
}
