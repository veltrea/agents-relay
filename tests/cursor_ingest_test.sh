#!/usr/bin/env bash
# cursor_ingest_test.sh — Cursor Agent JSONL → ingest → search / get_tools
#
# Usage: bash tests/cursor_ingest_test.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [ -z "${RELAY:-}" ]; then
  if [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/debug/agents-relay" ]; then
    RELAY="$CARGO_TARGET_DIR/debug/agents-relay"
  else
    RELAY="$ROOT/target/debug/agents-relay"
  fi
fi
FIXTURE="$ROOT/tests/fixtures/cursor/sample.jsonl"
RESULTS_DIR="/tmp/relay_cursor_$(date +%Y%m%d_%H%M%S)"
export HOME="$RESULTS_DIR/home"
mkdir -p "$HOME/.cursor/projects/Volumes-test-project/agent-transcripts/fixture-session-001"
SESSION_JSONL="$HOME/.cursor/projects/Volumes-test-project/agent-transcripts/fixture-session-001/fixture-session-001.jsonl"
cp "$FIXTURE" "$SESSION_JSONL"

echo "=== Cursor Ingest Test ==="
echo "HOME=$HOME"
echo "Relay=$RELAY"
echo ""

if [ ! -x "$RELAY" ]; then
  echo "Building agents-relay..."
  (cd "$ROOT" && cargo build --locked)
fi

echo "[1] First ingest (via sync_all path: ingest on projects dir is through tool/sync)"
# Direct file ingest uses Claude parser; exercise Cursor module via a tiny Rust-less path:
# call sync by running `tool memory_search` which calls sync_all.
"$RELAY" tool memory_search --query "CURSOR_FIXTURE_PHRASE_ZZ9" --limit 5 > "$RESULTS_DIR/search1.txt" 2>"$RESULTS_DIR/sync1.err" || true
cat "$RESULTS_DIR/sync1.err" | tee "$RESULTS_DIR/sync1.log" || true

if ! grep -q "CURSOR_FIXTURE_PHRASE_ZZ9" "$RESULTS_DIR/search1.txt"; then
  echo "FAIL: phrase not found after sync"
  cat "$RESULTS_DIR/search1.txt"
  exit 1
fi
echo "  search: FOUND"

echo "[2] Presentation must not include giant nested arguments blob"
ASSIST=$("$RELAY" tool memory_search --query "I will look that up" --limit 5 2>/dev/null || true)
if echo "$ASSIST" | grep -q "SHOULD_NOT_APPEAR_IN_CONTENT"; then
  echo "FAIL: giant tool arguments leaked into search content"
  echo "$ASSIST"
  exit 1
fi
echo "  content clean: OK"

echo "[3] Re-sync should add 0 new cursor rows from same offset"
BEFORE=$("$RELAY" db stats 2>/dev/null | awk '/^Entries:/{print $2}')
"$RELAY" tool memory_search --query "CURSOR_FIXTURE_PHRASE_ZZ9" --limit 1 >/dev/null 2>&1 || true
AFTER=$("$RELAY" db stats 2>/dev/null | awk '/^Entries:/{print $2}')
if [ "$BEFORE" != "$AFTER" ]; then
  echo "FAIL: duplicate ingest grew entries ($BEFORE -> $AFTER)"
  exit 1
fi
echo "  idempotent: OK ($AFTER entries)"

echo "[4] memory_get_tools summary + full"
# Find an assistant id with tools via SQL
ID=$("$RELAY" query "SELECT id FROM raw_entries WHERE source='cursor' AND tools_tokens IS NOT NULL AND tools_tokens > 0 ORDER BY id LIMIT 1" \
  | awk 'NR==3{print $1}')
if [ -z "${ID:-}" ] || [ "$ID" = "id" ]; then
  echo "FAIL: no cursor entry with tools_tokens"
  "$RELAY" query "SELECT id, type, tools_tokens, substr(content,1,80) FROM raw_entries WHERE source='cursor'"
  exit 1
fi
SUMMARY=$("$RELAY" tool memory_get_tools --id "$ID" 2>/dev/null)
echo "$SUMMARY" | grep -q "\[tools\]" || { echo "FAIL: summary missing [tools]"; echo "$SUMMARY"; exit 1; }
FULL=$("$RELAY" tool memory_get_tools --id "$ID" --query full 2>/dev/null)
echo "$FULL" | grep -q "CallDynamicTool\|Read\|Shell\|Grep" || { echo "FAIL: full tools_json missing tools"; echo "$FULL"; exit 1; }
echo "  get_tools id=$ID: OK"

echo "[5] db stats shows tool traces"
"$RELAY" db stats | grep -q "Tool traces:" || { echo "FAIL: Tool traces line missing"; exit 1; }

echo ""
echo "result=PASS"
echo "PASS"
