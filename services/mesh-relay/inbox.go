package main

import (
	"log/slog"
	"net/http"
	"os"
)

// InboxHandler implements http.Handler for POST /inbox.
//
// It no longer accepts bundles. It used to, and that was a hole: this handler
// read a JSON envelope and wrote the payload straight into dtn_inbox with no
// authentication of any kind — no fabric signature, no envelope signature, no
// replay check — on a port published to 0.0.0.0. Anything that could reach it
// could put arbitrary rows in front of every consumer of the inbox, as many
// times as it liked.
//
// The Rust core-api has an endpoint for this, POST /api/dtn/receive, which
// requires a named peer, verifies the envelope signature, binds the recipient
// and the bundle lifetime into that signature, and refuses a message id it has
// already seen. None of that was true here, and a second implementation of a
// security-critical path in a second language is exactly how one of the two
// ends up being the weak one — which is what happened.
//
// So the receive side belongs to the core-api, and this process keeps the job
// it is actually better at: draining dtn_outbox and delivering to peers, where
// wide network fan-out and frequent retries suit Go's concurrency model.
//
// The route is kept, rather than removed, so a peer still pointed here gets an
// explanation instead of a 404 it has to guess at.
type InboxHandler struct {
	DB      *DB
	Metrics *Metrics
}

// Where a peer should deliver instead. Overridable because the core-api's
// external address is deployment-specific.
func receiveEndpointHint() string {
	if v := os.Getenv("CORE_API_PUBLIC_URL"); v != "" {
		return v + "/api/dtn/receive"
	}
	return "<core-api>/api/dtn/receive"
}

func (h *InboxHandler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	// Counted, because a peer still delivering here is a misconfiguration
	// someone has to fix, and silence would hide it.
	h.Metrics.InboxErrors.Inc()
	slog.Warn("inbox delivery refused: this endpoint no longer accepts bundles",
		"from", r.RemoteAddr,
		"deliver_to", receiveEndpointHint())

	w.Header().Set("Content-Type", "application/json")
	// 410 rather than 404: the endpoint existed, and the distinction tells a
	// peer operator they are out of date rather than simply wrong.
	w.WriteHeader(http.StatusGone)
	_, _ = w.Write([]byte(`{"error":"inbox_moved",` +
		`"detail":"This relay no longer accepts DTN bundles; it had no authentication. ` +
		`Deliver to the core-api's POST /api/dtn/receive, which verifies the sender, ` +
		`the recipient and the bundle lifetime, and refuses replays.",` +
		`"deliver_to":"` + receiveEndpointHint() + `"}`))
}
