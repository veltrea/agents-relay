#!/usr/bin/env bash
# layer2_ingest_test.sh — レイヤー2: JSOL → ingest → DB テスト
#
# 目的: テスト用JSOLファイルを直接生成し、ingestが正しくDBに取り込むか検証する。
# Claudeを介さないため、ingestロジック単体の信頼性を確認できる。
#
# 検証項目:
#   - 手動作成したJSOLをingestが読み込めるか
#   - ingest後にDBにエントリが存在するか
#   - 重複ingestで二重登録されないか
#
# 使い方: bash tests/layer2_ingest_test.sh [フレーズ数(default:10)]

set -euo pipefail
export PATH="/opt/homebrew/bin:$PATH"

NUM="${1:-10}"
RELAY="/Volumes/DISK/dev/agents-relay/target/release/agents-relay"
RESULTS_DIR="/tmp/relay_layer2_$(date +%Y%m%d_%H%M%S)"
TEST_JSOL_DIR="$RESULTS_DIR/jsol"
mkdir -p "$TEST_JSOL_DIR"

echo "=== Layer 2: Ingest Test ==="
echo "Phrases:  $NUM"
echo "Results:  $RESULTS_DIR"
echo "Start:    $(date)"
echo ""

# ── Phase 1: テスト用JSOLファイルを生成 ──
echo "[Phase 1] Generating $NUM test JSOL files..."
PHRASES_FILE="$RESULTS_DIR/phrases.txt"

for i in $(seq 1 "$NUM"); do
    phrase=$(printf "INGEST_TEST_%04d_%s" "$i" "$(openssl rand -hex 4)")
    echo "$phrase" >> "$PHRASES_FILE"
    session_id=$(uuidgen | tr '[:upper:]' '[:lower:]')
    ts=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

    # 最小限のJSOLエントリ（user + assistant）
    cat > "$TEST_JSOL_DIR/${session_id}.jsonl" <<JSOL
{"type":"summary","sessionId":"${session_id}","timestamp":"${ts}"}
{"type":"user","sessionId":"${session_id}","message":{"role":"user","content":"Please remember: ${phrase}"},"timestamp":"${ts}"}
{"type":"assistant","sessionId":"${session_id}","message":{"role":"assistant","content":"I have noted the identifier: ${phrase}"},"timestamp":"${ts}"}
JSOL
done

echo "  Generated $NUM JSOL files in $TEST_JSOL_DIR"

# ── Phase 2: Ingest実行 ──
echo ""
echo "[Phase 2] Running ingest..."
ingest_output=$("$RELAY" ingest "$TEST_JSOL_DIR" 2>&1)
echo "$ingest_output" | tail -5
echo "  Ingest done."

sleep 1

# ── Phase 3: DB検索で検証 ──
echo ""
echo "[Phase 3] Verifying entries in DB..."
found=0
not_found=0

while IFS= read -r phrase; do
    echo -n "  $phrase ... "
    result=$("$RELAY" tool memory_search --query "$phrase" --limit 3 2>&1)

    if echo "$result" | grep -q "$phrase"; then
        echo "FOUND"
        found=$((found + 1))
    else
        echo "NOT_FOUND"
        not_found=$((not_found + 1))
    fi
done < "$PHRASES_FILE"

# ── Phase 4: 重複ingestテスト ──
echo ""
echo "[Phase 4] Re-running ingest (duplicate check)..."
reingest_output=$("$RELAY" ingest "$TEST_JSOL_DIR" 2>&1)
echo "  $reingest_output" | tail -3

# 再検索して件数が変わらないことを確認
echo -n "  Duplicate check (first phrase): "
first_phrase=$(head -1 "$PHRASES_FILE")
dup_result=$("$RELAY" tool memory_search --query "$first_phrase" --limit 10 2>&1)
dup_count=$(echo "$dup_result" | grep -c "$first_phrase" || true)
if [ "$dup_count" -le 2 ]; then
    echo "OK (count=$dup_count, no duplicate)"
    dup_ok=true
else
    echo "WARN (count=$dup_count, possible duplicate)"
    dup_ok=false
fi

# ── 結果 ──
echo ""
echo "========================================="
echo "         RESULTS SUMMARY"
echo "========================================="
recall_rate=0
if [ "$NUM" -gt 0 ]; then
    recall_rate=$(echo "scale=1; $found * 100 / $NUM" | bc)
fi

cat <<EOF | tee "$RESULTS_DIR/summary.txt"
Total phrases:   $NUM
Found:           $found
Not Found:       $not_found
Recall Rate:     ${recall_rate}%
Duplicate Safe:  $dup_ok
End:             $(date)

Verdict: $(
    if [ "$found" -eq "$NUM" ] && $dup_ok; then
        echo "PASS"
    else
        echo "FAIL (found=$found/$NUM, dup_ok=$dup_ok)"
    fi
)
EOF

echo "========================================="
