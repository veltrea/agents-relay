#!/usr/bin/env bash
# memory_recall_test.sh — agents-relay E2Eリコールテスト
# Claude CLIでフレーズを送信 → ingest → memory_searchで検索 → 結果集計
#
# 使い方: bash tests/memory_recall_test.sh [フレーズ数(default:30)]

set -euo pipefail

# nodeが見つからない問題を回避
export PATH="/opt/homebrew/bin:$PATH"

NUM_PHRASES="${1:-30}"
CLAUDE="/opt/homebrew/bin/claude"
RELAY="/Volumes/DISK/dev/agents-relay/target/release/agents-relay"
RESULTS_DIR="/tmp/relay_recall_test_$(date +%Y%m%d_%H%M%S)"
JSOL_DIR="$HOME/.claude/projects"
SEED_LOG="$RESULTS_DIR/seed.log"
VERIFY_LOG="$RESULTS_DIR/verify.log"
SUMMARY="$RESULTS_DIR/summary.txt"

mkdir -p "$RESULTS_DIR"

echo "=== agents-relay Memory Recall Test ==="
echo "Phrases: $NUM_PHRASES"
echo "Results: $RESULTS_DIR"
echo "Start:   $(date)"
echo ""

# ── Phase 1: フレーズ生成 ──
echo "[Phase 1] Generating $NUM_PHRASES unique phrases..."
PHRASES_FILE="$RESULTS_DIR/phrases.txt"
for i in $(seq 1 "$NUM_PHRASES"); do
    printf "RELAY_TEST_%04d_%s\n" "$i" "$(openssl rand -hex 4)"
done > "$PHRASES_FILE"
echo "  → $PHRASES_FILE"

# ── Phase 2: Claudeにフレーズを送信（逐次） ──
echo ""
echo "[Phase 2] Seeding phrases via claude -p (sequential)..."
seed_ok=0
seed_fail=0

while IFS= read -r phrase; do
    echo -n "  [$seed_ok/$NUM_PHRASES] $phrase ... "

    # Claudeにフレーズを含む短い文を送る（最小トークン消費）
    # < /dev/null: claudeがwhileループのstdinを食わないようにする
    if "$CLAUDE" -p "Please acknowledge: $phrase" \
        --output-format text \
        --max-turns 1 \
        < /dev/null \
        > "$RESULTS_DIR/seed_${phrase}.txt" 2>&1; then
        echo "OK"
        seed_ok=$((seed_ok + 1))
    else
        echo "FAIL"
        seed_fail=$((seed_fail + 1))
    fi
done < "$PHRASES_FILE"

echo ""
echo "  Seed complete: OK=$seed_ok FAIL=$seed_fail"
echo "seed_ok=$seed_ok seed_fail=$seed_fail" >> "$SEED_LOG"

# ── Phase 3: Ingest（全JSOLディレクトリを対象） ──
echo ""
echo "[Phase 3] Running ingest..."
"$RELAY" ingest "$JSOL_DIR" 2>&1 | tail -5
echo "  Ingest done."

# 少し待つ（WALフラッシュ）
sleep 2

# ── Phase 4: 検索して検証 ──
echo ""
echo "[Phase 4] Verifying recall via memory_search..."
found=0
not_found=0

while IFS= read -r phrase; do
    echo -n "  $phrase ... "

    result=$("$RELAY" tool memory_search --query "$phrase" --limit 5 2>&1)

    if echo "$result" | grep -q "$phrase"; then
        echo "FOUND"
        found=$((found + 1))
    else
        echo "NOT_FOUND"
        not_found=$((not_found + 1))
        # 失敗したものは詳細ログ
        echo "--- $phrase ---" >> "$VERIFY_LOG"
        echo "$result" >> "$VERIFY_LOG"
        echo "" >> "$VERIFY_LOG"
    fi
done < "$PHRASES_FILE"

# ── Phase 5: 集計 ──
echo ""
echo "========================================="
echo "         RESULTS SUMMARY"
echo "========================================="
recall_rate=0
if [ "$NUM_PHRASES" -gt 0 ]; then
    recall_rate=$(echo "scale=1; $found * 100 / $NUM_PHRASES" | bc)
fi

cat <<EOF | tee "$SUMMARY"
Total phrases:  $NUM_PHRASES
Seed OK:        $seed_ok
Seed FAIL:      $seed_fail
Found:          $found
Not Found:      $not_found
Recall Rate:    ${recall_rate}%
End:            $(date)

Verdict: $(
    if [ "$found" -eq "$NUM_PHRASES" ]; then
        echo "PASS — 100% recall"
    elif [ "$found" -ge $((NUM_PHRASES * 9 / 10)) ]; then
        echo "MOSTLY PASS — ${recall_rate}% recall (>90%)"
    else
        echo "FAIL — ${recall_rate}% recall"
    fi
)
EOF

echo ""
echo "Detailed logs: $RESULTS_DIR"
echo "Failed lookups: $VERIFY_LOG"
