package main

import (
	"encoding/json"
	"io"
	"log/slog"
	"net/http"
)

// InboxHandler implements http.Handler for POST /inbox — the receive side
// of the mesh. Peers POST a JSON envelope here; we persist the payload
// into dtn_inbox and the Rust core-api picks it up from there.
type InboxHandler struct {
	DB      *DB
	Metrics *Metrics
}

// InboxRequest is the JSON envelope peers send us. SrcNodeID is a pointer
// so we can distinguish "field missing" from "field set to null" — for now
// they both mean "unknown sender" and we store NULL in the DB.
type InboxRequest struct {
	SrcNodeID *string         `json:"src_node_id"`
	Payload   json.RawMessage `json:"payload"`
}

// ServeHTTP is Go's http.Handler interface. By implementing this method,
// InboxHandler satisfies the interface and can be passed to http.Handle.
func (h *InboxHandler) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}

	// Cap body at 1 MiB so a hostile peer can't OOM us.
	body, err := io.ReadAll(io.LimitReader(r.Body, 1<<20))
	if err != nil {
		h.Metrics.InboxErrors.Inc()
		http.Error(w, "read body: "+err.Error(), http.StatusBadRequest)
		return
	}

	var req InboxRequest
	if err := json.Unmarshal(body, &req); err != nil {
		h.Metrics.InboxErrors.Inc()
		http.Error(w, "invalid json: "+err.Error(), http.StatusBadRequest)
		return
	}
	if len(req.Payload) == 0 {
		h.Metrics.InboxErrors.Inc()
		http.Error(w, "missing payload", http.StatusBadRequest)
		return
	}

	src := ""
	if req.SrcNodeID != nil {
		src = *req.SrcNodeID
	}

	if err := h.DB.WriteInbox(r.Context(), src, req.Payload); err != nil {
		h.Metrics.InboxErrors.Inc()
		slog.Error("inbox write", "err", err)
		http.Error(w, "db error", http.StatusInternalServerError)
		return
	}

	h.Metrics.BundlesReceived.Inc()
	slog.Info("inbox received", "src_node_id", src, "bytes", len(req.Payload))

	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(http.StatusAccepted)
	_, _ = w.Write([]byte(`{"status":"accepted"}`))
}
