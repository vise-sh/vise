#!/usr/bin/env bash
# Live end-to-end test: create a session and watch a running host execute it.
# Requires: server running (just run-server), host running (cargo run -p vise-host),
# curl, jq. For HARNESS=claude-code: Node 18+ and Claude credentials on the host.
#
# Usage:
#   ./scripts/test-session-e2e.sh              # claude-code (real agent)
#   HARNESS=echo ./scripts/test-session-e2e.sh # echo runtime (control-loop only)
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:3000}"
HARNESS="${HARNESS:-claude-code}"
TIMEOUT_SECS="${TIMEOUT_SECS:-180}"

if [[ "$HARNESS" == "claude-code" ]]; then
    INPUT="Create a file named hello.txt containing exactly the word hello. Then stop."
else
    INPUT="Say hello"
fi

echo "# creating session (harness: $HARNESS)"
SESSION=$(curl -sf -X POST "$BASE_URL/sessions" -H 'content-type: application/json' -d '{
  "agent": { "harness": "'"$HARNESS"'", "model": "", "instructions": "", "mcp_servers": [] },
  "environment": { "kind": "self_hosted" },
  "input": "'"$INPUT"'"
}')
SESSION_ID=$(jq -r '.id' <<<"$SESSION")
echo "created $SESSION_ID"

echo "# waiting for terminal status (timeout ${TIMEOUT_SECS}s)"
DEADLINE=$((SECONDS + TIMEOUT_SECS))
while true; do
    SESSION=$(curl -sf "$BASE_URL/sessions/$SESSION_ID")
    STATUS=$(jq -r '.status' <<<"$SESSION")

    case "$STATUS" in
        completed|failed|cancelled) break ;;
    esac

    if ((SECONDS >= DEADLINE)); then
        echo "TIMEOUT: session still $STATUS after ${TIMEOUT_SECS}s (is the host running?)"
        exit 1
    fi
    sleep 2
done

echo "# final session"
jq '{id, status, host_id, stop_reason, error, started_at, finished_at}' <<<"$SESSION"

echo "# events"
EVENTS=$(curl -sf "$BASE_URL/sessions/$SESSION_ID/events")
COUNT=$(jq 'length' <<<"$EVENTS")
echo "$COUNT events recorded"
jq -r '.[].payload | (.update.sessionUpdate // (if .permissionRequest then "permission (auto-approved)" else .sessionUpdate // "unknown" end))' \
    <<<"$EVENTS" | sort | uniq -c

FAIL=0

if [[ "$STATUS" != "completed" ]]; then
    echo "FAIL - expected status completed, got $STATUS"
    FAIL=1
fi

if ((COUNT == 0)); then
    echo "FAIL - no events recorded"
    FAIL=1
fi

if [[ "$HARNESS" == "claude-code" ]]; then
    WORKSPACE="${TMPDIR:-/tmp}/vise-sessions/$SESSION_ID/workspace"
    if [[ "$(cat "$WORKSPACE/hello.txt" 2>/dev/null | tr -d '[:space:]')" == "hello" ]]; then
        echo "ok   - agent created hello.txt in $WORKSPACE"
    else
        echo "FAIL - $WORKSPACE/hello.txt missing or wrong content"
        FAIL=1
    fi
fi

if ((FAIL == 0)); then
    echo "PASS"
else
    exit 1
fi
