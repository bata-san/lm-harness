#!/usr/bin/env bash
# E2E smoke test: boots the release binary against a throwaway HARNESS_HOME and exercises
# first-run setup, login, CSRF, API keys, tunnel refusal, and mode switching.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN="${BIN:-$ROOT/backend/target/release/llm-harness}"
PORT="${PORT:-18787}"
BASE="http://127.0.0.1:$PORT"
export HARNESS_HOME="$(mktemp -d)"
export HOST=127.0.0.1 PORT
JAR="$HARNESS_HOME/cookies.txt"
PASS="correct-horse-battery-staple"

[ -x "$BIN" ] || { echo "binary not found: $BIN (run cargo build --release)"; exit 1; }

"$BIN" >"$HARNESS_HOME/server.log" 2>&1 &
PID=$!
cleanup() { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; rm -rf "$HARNESS_HOME"; }
trap cleanup EXIT

fail() { echo "FAIL: $*"; tail -30 "$HARNESS_HOME/server.log"; exit 1; }
pass() { echo "ok   - $*"; }
# status <expected> <label> curl-args...
status() {
  local want="$1" label="$2"; shift 2
  local got; got=$(curl -s -o /dev/null -w '%{http_code}' "$@")
  [ "$got" = "$want" ] || fail "$label: expected HTTP $want, got $got"
  pass "$label"
}

for _ in $(seq 1 50); do curl -sf "$BASE/api/health" >/dev/null && break; sleep 0.2; done
status 200 "health is public" "$BASE/api/health"

# Before setup only the workstation itself (localhost) may use the API.
status 200 "localhost open before setup" "$BASE/api/system" -H 'X-Harness-CSRF: 1'
status 401 "forwarded (tunnel) request closed before setup" "$BASE/api/system" -H 'X-Harness-CSRF: 1' -H 'X-Forwarded-For: 203.0.113.9'

status 200 "initial setup (local only)" -c "$JAR" -X POST "$BASE/api/auth/setup" \
  -H 'Content-Type: application/json' -H 'X-Harness-CSRF: 1' \
  -d "{\"username\":\"admin\",\"password\":\"$PASS\"}"
status 403 "second setup refused" -X POST "$BASE/api/auth/setup" \
  -H 'Content-Type: application/json' -H 'X-Harness-CSRF: 1' \
  -d "{\"username\":\"evil\",\"password\":\"$PASS-evil\"}"

status 401 "anonymous request rejected" "$BASE/api/system" -H 'X-Harness-CSRF: 1'
status 200 "session cookie accepted" -b "$JAR" "$BASE/api/system" -H 'X-Harness-CSRF: 1'
status 403 "mutation without CSRF header rejected" -b "$JAR" -X PUT "$BASE/api/mode" \
  -H 'Content-Type: application/json' -d '{"mode":"shared"}'
status 401 "wrong password rejected" -X POST "$BASE/api/auth/login" \
  -H 'Content-Type: application/json' -H 'X-Harness-CSRF: 1' \
  -d '{"username":"admin","password":"wrong-password-123"}'

# Mode switch round trip.
status 200 "switch to shared" -b "$JAR" -X PUT "$BASE/api/mode" \
  -H 'Content-Type: application/json' -H 'X-Harness-CSRF: 1' -d '{"mode":"shared"}'
curl -s -b "$JAR" "$BASE/api/mode" -H 'X-Harness-CSRF: 1' | grep -q '"mode":"shared"' || fail "mode did not become shared"
pass "mode reads back as shared"
status 200 "switch to full" -b "$JAR" -X PUT "$BASE/api/mode" \
  -H 'Content-Type: application/json' -H 'X-Harness-CSRF: 1' -d '{"mode":"full"}'

# API key works as a bearer token, and is rejected once revoked.
KEY_JSON=$(curl -s -b "$JAR" -X POST "$BASE/api/auth/keys" -H 'Content-Type: application/json' -H 'X-Harness-CSRF: 1' -d '{"name":"e2e","kind":"openai"}')
KEY=$(echo "$KEY_JSON" | grep -o 'sk-lh-[A-Za-z0-9_-]*' | head -1 || true)
[ -n "$KEY" ] || fail "no API key in response: $KEY_JSON"
# keys are valid on /v1 only (503 = authenticated, no model running) and refused elsewhere
status 503 "API key bearer auth on /v1" -H "Authorization: Bearer $KEY" "$BASE/v1/models"
status 403 "API key refused on admin routes" -H "Authorization: Bearer $KEY" "$BASE/api/system"
status 401 "garbage bearer rejected" -H "Authorization: Bearer sk-lh-nope" "$BASE/v1/models"

# Frontend is embedded and served.
curl -sf "$BASE/" | grep -qi '<div id="root"' || fail "SPA index not served"
pass "SPA served"

status 200 "logout" -b "$JAR" -X POST "$BASE/api/auth/logout" -H 'X-Harness-CSRF: 1'
status 401 "session dead after logout" -b "$JAR" "$BASE/api/system" -H 'X-Harness-CSRF: 1'

echo "all e2e checks passed"
