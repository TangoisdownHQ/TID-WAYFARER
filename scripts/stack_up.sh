#!/usr/bin/env bash
# Bring the Wayfarer stack up only when the host actually has the RAM for it,
# then report memory and any pod security/error signals.
#
# Why this exists: this box runs a second (SOC) docker stack, a browser, a
# language server and sometimes a VM, on 15 GiB with zram swap. A bare
# `docker compose up -d` has OOM-killed work before. This gates the start on
# available memory, surfaces the known claude-mem chroma-mcp leak as a reclaim
# option, and after start prints per-container memory plus a scan of recent
# logs for auth failures, panics and other security-relevant lines.
#
# Usage:
#   scripts/stack_up.sh                      # whole stack, gated
#   scripts/stack_up.sh tidasone-db-v2       # a subset of services
#   STACK_MIN_AVAIL_MB=2000 scripts/stack_up.sh
#   STACK_FORCE=1 scripts/stack_up.sh        # start despite the gate (logged)
#
# Env:
#   STACK_MIN_AVAIL_MB  available-RAM floor in MB (default 1500)
#   STACK_SWAP_WARN_PCT swap-usage % that raises a pressure alert (default 85)
#   STACK_FORCE=1       proceed even if below the floor
set -uo pipefail
cd "$(dirname "$0")/.."

MIN_AVAIL_MB="${STACK_MIN_AVAIL_MB:-1500}"
SWAP_WARN_PCT="${STACK_SWAP_WARN_PCT:-85}"
FORCE="${STACK_FORCE:-0}"
SERVICES="$*"

mem_avail_mb() { awk '/^MemAvailable:/{print int($2/1024)}' /proc/meminfo; }
swap_used_pct() {
  awk '/^SwapTotal:/{t=$2} /^SwapFree:/{f=$2} END{ if(t<=0) print 0; else printf "%d",(t-f)*100/t }' /proc/meminfo
}

echo "══ RAM preflight ══"
free -h
AVAIL=$(mem_avail_mb); SWAP=$(swap_used_pct)
echo "available: ${AVAIL} MB (floor ${MIN_AVAIL_MB} MB)  |  swap used: ${SWAP}%"

# The known claude-mem orphan. Flag it as a reclaim option rather than killing
# it unasked — it is documented and safe to `pkill`, but that is the operator's
# call.
if pgrep -f 'chroma-mc[p]' >/dev/null 2>&1; then
  echo "⚠ claude-mem chroma-mcp leak present — reclaim ~0.5 GB with: pkill -f 'chroma-mc[p]'"
fi

if [ "${AVAIL:-0}" -lt "$MIN_AVAIL_MB" ]; then
  echo "🛑 ALERT: only ${AVAIL} MB available (< ${MIN_AVAIL_MB} MB). Refusing to start — this would risk an OOM kill."
  echo "   Free memory (close a heavy app, or kill the chroma leak above), then retry."
  if [ "$FORCE" != "1" ]; then exit 1; fi
  echo "   STACK_FORCE=1 set — proceeding against the gate."
fi
if [ "${SWAP:-0}" -ge "$SWAP_WARN_PCT" ]; then
  echo "⚠ ALERT: swap ${SWAP}% used — host is already under memory pressure; watch for slowdowns."
fi

echo; echo "══ bringing up: ${SERVICES:-<all services>} ══"
# shellcheck disable=SC2086
if ! docker compose up -d $SERVICES; then
  echo "🛑 docker compose up failed."
  exit 1
fi

echo; echo "══ waiting for health (≤90s) ══"
for _ in $(seq 1 30); do
  pending=$(docker compose ps --format '{{.Name}}\t{{.Status}}' 2>/dev/null \
            | grep -iE 'health: starting|\(starting\)' | wc -l)
  [ "$pending" -eq 0 ] && break
  sleep 3
done

echo; echo "══ container status ══"
docker compose ps --format 'table {{.Name}}\t{{.Status}}'

CIDS=$(docker compose ps -q 2>/dev/null)
if [ -n "$CIDS" ]; then
  echo; echo "══ per-container memory ══"
  # shellcheck disable=SC2086
  docker stats --no-stream --format 'table {{.Name}}\t{{.MemUsage}}\t{{.MemPerc}}' $CIDS
fi

echo; echo "══ host memory after start ══"
free -h

echo; echo "══ pod security / error scan (last 3m of logs) ══"
LOGS=$(docker compose logs --since=3m --no-color 2>/dev/null)
HITS=$(printf '%s\n' "$LOGS" \
  | grep -iE 'panic|fatal|unauthoriz|forbidden|denied|permission denied|auth[^a-z]*fail|bad_signature|signature does not|replay|refused connection|segfault|out of memory|oom|killed|traceback|unhandled' \
  | grep -ivE '"level":"info"|level=info| INFO | DEBUG ' \
  | head -40)
if [ -n "$HITS" ]; then
  echo "⚠ SECURITY/ERROR signals in recent logs (review):"
  printf '%s\n' "$HITS"
else
  echo "✓ no security/error signals in recent logs"
fi

# A container that keeps restarting is itself an alert.
RESTARTING=$(docker compose ps --format '{{.Name}}\t{{.Status}}' 2>/dev/null \
             | grep -iE 'restarting|unhealthy|exited \([^0]' || true)
if [ -n "$RESTARTING" ]; then
  echo; echo "⚠ ALERT: containers not healthy:"; printf '%s\n' "$RESTARTING"
fi

echo; echo "✓ stack_up complete."
