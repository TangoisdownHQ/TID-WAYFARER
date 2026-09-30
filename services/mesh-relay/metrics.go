package main

import (
	"net/http"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promhttp"
)

// Metrics holds the Prometheus instruments and the registry that exposes
// them at /metrics. We use a private registry (not the default global one)
// so the binary doesn't expose Go runtime metrics by default — keeps the
// /metrics output tight and focused on what this service does.
type Metrics struct {
	registry         *prometheus.Registry
	BundlesForwarded prometheus.Counter
	BundlesFailed    prometheus.Counter
	BundlesReceived  prometheus.Counter
	InboxErrors      prometheus.Counter
	LoopIters        prometheus.Counter
}

func newMetrics() *Metrics {
	reg := prometheus.NewRegistry()
	m := &Metrics{
		registry: reg,
		BundlesForwarded: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "tidasone_mesh_bundles_forwarded_total",
			Help: "Bundles successfully delivered to a peer (HTTP 2xx response).",
		}),
		BundlesFailed: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "tidasone_mesh_bundles_failed_total",
			Help: "Bundle delivery attempts that hit a network error or non-2xx response.",
		}),
		BundlesReceived: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "tidasone_mesh_bundles_received_total",
			Help: "Bundles received on /inbox and successfully written to dtn_inbox.",
		}),
		InboxErrors: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "tidasone_mesh_inbox_errors_total",
			Help: "Inbox requests rejected (bad JSON, missing payload, DB write failure).",
		}),
		LoopIters: prometheus.NewCounter(prometheus.CounterOpts{
			Name: "tidasone_mesh_forwarder_loop_iters_total",
			Help: "Forwarder ticks since startup (including empty ones).",
		}),
	}
	reg.MustRegister(
		m.BundlesForwarded,
		m.BundlesFailed,
		m.BundlesReceived,
		m.InboxErrors,
		m.LoopIters,
	)
	return m
}

// Handler returns the HTTP handler that serves /metrics.
func (m *Metrics) Handler() http.Handler {
	return promhttp.HandlerFor(m.registry, promhttp.HandlerOpts{})
}
