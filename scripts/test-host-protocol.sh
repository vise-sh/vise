#!/usr/bin/env bash
# End-to-end walkthrough of the host pull protocol (M3 + host auth).
# Requires: server running (just run-server), curl, jq.
set -euo pipefail

BASE_URL="${BASE_URL:-http://localhost:3000}"

PASS=0
FAIL=0

check() {
    local desc="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        echo "ok   - $desc"
        PASS=$((PASS + 1))
    else
        echo "FAIL - $desc (expected: $expected, got: $actual)"
        FAIL=$((FAIL + 1))
    fi
}

# req METHOD PATH [BODY] [TOKEN] — curl writes the status code on the last line
req() {
    local method="$1" path="$2" body="${3:-}" token="${4:-}"
    local args=(-s -w '\n%{http_code}' -X "$method" "$BASE_URL$path")
    if [[ -n "$body" ]]; then
        args+=(-H 'content-type: application/json' -d "$body")
    fi
    if [[ -n "$token" ]]; then
        args+=(-H "Authorization: Bearer $token")
    fi
    curl "${args[@]}"
}

body() { sed '$d' <<<"$1"; }
code() { tail -n1 <<<"$1"; }

echo "# 0. enroll hosts"
RES=$(req POST /hosts "{\"name\":\"test-host-$RANDOM\"}")
check "enroll returns 201" 201 "$(code "$RES")"
HOST_ID=$(body "$RES" | jq -r '.host.id')
TOKEN=$(body "$RES" | jq -r '.token')
check "token has vhost_ prefix" vhost_ "$(cut -c1-6 <<<"$TOKEN")"

RES=$(req POST /hosts "{\"name\":\"other-host-$RANDOM\"}")
OTHER_TOKEN=$(body "$RES" | jq -r '.token')

RES=$(req GET /hosts)
check "list hosts returns 200" 200 "$(code "$RES")"

echo "# 1. create session"
RES=$(req POST /sessions '{
  "agent": { "harness": "claude-code", "model": "claude-sonnet-4-6", "instructions": "test", "mcp_servers": [] },
  "environment": { "kind": "self_hosted" },
  "input": "Say hello"
}')
SESSION=$(body "$RES")
SESSION_ID=$(jq -r '.id' <<<"$SESSION")
check "create returns 201" 201 "$(code "$RES")"
check "create status is pending" pending "$(jq -r '.status' <<<"$SESSION")"

echo "# 2. claim requires auth"
RES=$(req POST /hosts/claim '{}')
check "claim without token returns 401" 401 "$(code "$RES")"
RES=$(req POST /hosts/claim '{}' vhost_bogus)
check "claim with bad token returns 401" 401 "$(code "$RES")"

echo "# 3. claim"
RES=$(req POST /hosts/claim '{"harnesses":["claude-code"],"environment_types":["self_hosted"]}' "$TOKEN")
CLAIMED=$(body "$RES" | jq '.session')
check "claim returns 200" 200 "$(code "$RES")"
check "claim returns the created session" "$SESSION_ID" "$(jq -r '.id' <<<"$CLAIMED")"
check "claimed status is running" running "$(jq -r '.status' <<<"$CLAIMED")"
check "claimed host_id is the enrolled host" "$HOST_ID" "$(jq -r '.host_id' <<<"$CLAIMED")"

echo "# 4. claim again (queue empty)"
RES=$(req POST /hosts/claim '{}' "$TOKEN")
check "claim with no pending work returns 200" 200 "$(code "$RES")"
check "claim with no pending work returns null session" null "$(body "$RES" | jq '.session')"

echo "# 5. heartbeat"
RES=$(req POST "/hosts/sessions/$SESSION_ID/heartbeat" "" "$TOKEN")
check "heartbeat returns 200" 200 "$(code "$RES")"
check "heartbeat cancel_requested false" false "$(body "$RES" | jq -r '.cancel_requested')"

echo "# 6. heartbeat from non-owning host"
RES=$(req POST "/hosts/sessions/$SESSION_ID/heartbeat" "" "$OTHER_TOKEN")
check "heartbeat from non-owning host returns 409" 409 "$(code "$RES")"

echo "# 7. report events"
EVENTS='{"events":[
  {"seq":1,"payload":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello"}}},
  {"seq":2,"payload":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":" world"}}}
]}'
RES=$(req POST "/hosts/sessions/$SESSION_ID/events" "$EVENTS" "$TOKEN")
check "report events returns 204" 204 "$(code "$RES")"

echo "# 8. report same events again (idempotent retry)"
RES=$(req POST "/hosts/sessions/$SESSION_ID/events" "$EVENTS" "$TOKEN")
check "retry returns 204" 204 "$(code "$RES")"

echo "# 9. read events back"
RES=$(req GET "/sessions/$SESSION_ID/events")
check "get events returns 200" 200 "$(code "$RES")"
check "exactly 2 events (no duplicates)" 2 "$(body "$RES" | jq 'length')"

echo "# 10. finish with non-terminal status rejected"
RES=$(req POST "/hosts/sessions/$SESSION_ID/finish" '{"status":"running","stop_reason":null,"error":null}' "$TOKEN")
check "finish with running returns 422" 422 "$(code "$RES")"

echo "# 11. finish"
RES=$(req POST "/hosts/sessions/$SESSION_ID/finish" '{"status":"completed","stop_reason":"end_turn","error":null}' "$TOKEN")
FINISHED=$(body "$RES")
check "finish returns 200" 200 "$(code "$RES")"
check "finished status is completed" completed "$(jq -r '.status' <<<"$FINISHED")"
check "finished stop_reason recorded" end_turn "$(jq -r '.stop_reason' <<<"$FINISHED")"

echo "# 12. protocol calls after finish rejected"
RES=$(req POST "/hosts/sessions/$SESSION_ID/heartbeat" "" "$TOKEN")
check "heartbeat after finish returns 409" 409 "$(code "$RES")"
RES=$(req POST "/hosts/sessions/$SESSION_ID/finish" '{"status":"failed","stop_reason":null,"error":"nope"}' "$TOKEN")
check "double finish returns 409" 409 "$(code "$RES")"

echo "# 13. final state via public API"
RES=$(req GET "/sessions/$SESSION_ID")
check "get session returns 200" 200 "$(code "$RES")"
check "public view shows completed" completed "$(body "$RES" | jq -r '.status')"

echo "# 14. events pagination"
RES=$(req GET "/sessions/$SESSION_ID/events?after_seq=1")
check "after_seq filters events" 1 "$(body "$RES" | jq 'length')"
RES=$(req GET "/sessions/$SESSION_ID/events?limit=1")
check "limit caps events" 1 "$(body "$RES" | jq 'length')"

echo "# 15. cancel"
RES=$(req POST "/sessions/$SESSION_ID/cancel")
check "cancel of finished session returns 409" 409 "$(code "$RES")"
RES=$(req POST /sessions/nonexistent/cancel)
check "cancel of unknown session returns 404" 404 "$(code "$RES")"

RES=$(req POST /sessions '{
  "agent": { "harness": "claude-code", "model": "", "instructions": "", "mcp_servers": [] },
  "environment": { "kind": "self_hosted" },
  "input": "never runs"
}')
PENDING_ID=$(body "$RES" | jq -r '.id')
RES=$(req POST "/sessions/$PENDING_ID/cancel")
check "cancel of pending session returns 200" 200 "$(code "$RES")"
check "pending session cancelled immediately" cancelled "$(body "$RES" | jq -r '.status')"

RES=$(req POST /hosts/claim '{}' "$TOKEN")
check "cancelled session is not claimable" null "$(body "$RES" | jq '.session')"

echo
echo "$PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
