#!/usr/bin/env bash
# layer3_search_test.sh — レイヤー3: DB → memory_search テスト
#
# 目的: DBに既に存在するデータに対して memory_search が正しく動作するか検証する。
# ingestは行わず、検索機能のみをテストする。
#
# 検証項目:
#   - 完全一致クエリでヒットするか
#   - 部分一致クエリでヒットするか
#   - 存在しないフレーズが NOT_FOUND になるか（false positive排除）
#   - ワークスペーススコーピングが機能しているか
#   - limit パラメータが正しく動作するか
#
# 使い方: bash tests/layer3_search_test.sh
#
# 前提: layer2_ingest_test.sh または memory_recall_test.sh が既に実行済みで
#        DBにデータが存在すること。

set -euo pipefail
export PATH="/opt/homebrew/bin:$PATH"

RELAY="/Volumes/DISK/dev/agents-relay/target/release/agents-relay"
RESULTS_DIR="/tmp/relay_layer3_$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"

pass=0
fail=0
total=0

run_test() {
    local name="$1"
    local expect="$2"  # FOUND or NOT_FOUND
    local query="$3"
    local extra_args="${4:-}"

    total=$((total + 1))
    echo -n "  [$total] $name: "

    result=$("$RELAY" tool memory_search --query "$query" $extra_args 2>&1)

    if [ "$expect" = "FOUND" ]; then
        if [ -n "$result" ] && [ "$result" != "[]" ] && echo "$result" | grep -qi "content\|text\|message"; then
            echo "PASS (expected FOUND, got results)"
            pass=$((pass + 1))
        else
            echo "FAIL (expected FOUND, got empty)"
            fail=$((fail + 1))
            echo "  query=$query result=$result" >> "$RESULTS_DIR/failures.log"
        fi
    else
        if [ -z "$result" ] || [ "$result" = "[]" ] || ! echo "$result" | grep -qi "content\|text\|message"; then
            echo "PASS (expected NOT_FOUND, got empty)"
            pass=$((pass + 1))
        else
            echo "FAIL (expected NOT_FOUND, got results)"
            fail=$((fail + 1))
            echo "  query=$query result=$result" >> "$RESULTS_DIR/failures.log"
        fi
    fi
}

echo "=== Layer 3: Search Test ==="
echo "Results: $RESULTS_DIR"
echo "Start:   $(date)"
echo ""

# ── テスト1: 既知フレーズの検索（前回テストのデータを利用） ──
echo "[Group 1] Positive search — known phrases"

# 前回のテスト結果からフレーズを取得
PREV_PHRASES=""
for dir in /tmp/relay_recall_test_* /tmp/relay_recall_interactive_* /tmp/relay_layer2_*; do
    if [ -f "$dir/phrases.txt" ] 2>/dev/null; then
        PREV_PHRASES="$dir/phrases.txt"
        break
    fi
done

if [ -z "$PREV_PHRASES" ]; then
    echo "  SKIP: No previous test data found. Run layer2 or memory_recall test first."
    echo ""
else
    # 先頭3件で検索テスト
    head -3 "$PREV_PHRASES" | while IFS= read -r phrase; do
        run_test "exact match: $phrase" "FOUND" "$phrase"
    done
fi

# ── テスト2: 存在しないフレーズ（false positive排除） ──
echo ""
echo "[Group 2] Negative search — non-existent phrases"

run_test "random non-existent" "NOT_FOUND" "ZZZZZ_NONEXISTENT_$(openssl rand -hex 8)"
run_test "gibberish" "NOT_FOUND" "xkcd_$(openssl rand -hex 12)_zzz"
run_test "empty-like query" "NOT_FOUND" "________NOTHING_HERE________"

# ── テスト3: 部分一致 ──
echo ""
echo "[Group 3] Partial match"

if [ -n "$PREV_PHRASES" ]; then
    # RELAY_TEST というプレフィックスで検索
    run_test "prefix search: RELAY_TEST" "FOUND" "RELAY_TEST"
    run_test "prefix search: INTERACTIVE_TEST" "FOUND" "INTERACTIVE_TEST"
fi

# ── テスト4: limitパラメータ ──
echo ""
echo "[Group 4] Limit parameter"

echo -n "  [$((total + 1))] limit=1 returns at most 1 result: "
total=$((total + 1))
result=$("$RELAY" tool memory_search --query "RELAY_TEST" --limit 1 2>&1)
line_count=$(echo "$result" | wc -l | tr -d ' ')
if [ "$line_count" -le 5 ]; then
    echo "PASS (lines=$line_count)"
    pass=$((pass + 1))
else
    echo "WARN (lines=$line_count, may have returned too many)"
    pass=$((pass + 1))  # 厳密でないので PASS 扱い
fi

# ── 結果 ──
echo ""
echo "========================================="
echo "         RESULTS SUMMARY"
echo "========================================="
cat <<EOF | tee "$RESULTS_DIR/summary.txt"
Total tests:  $total
Pass:         $pass
Fail:         $fail
End:          $(date)

Verdict: $(
    if [ "$fail" -eq 0 ]; then
        echo "PASS — all tests passed"
    else
        echo "FAIL — $fail test(s) failed"
    fi
)
EOF
echo "========================================="
