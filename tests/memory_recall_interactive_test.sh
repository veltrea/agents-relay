#!/usr/bin/env bash
# memory_recall_interactive_test.sh — インタラクティブ版E2Eリコールテスト
# tmux経由でClaudeをインタラクティブに起動しフレーズを送信 → ingest → 検証
#
# 使い方: bash tests/memory_recall_interactive_test.sh [フレーズ数(default:30)]
#
# claude -p ではなく実際のインタラクティブセッションを通す。
# 各セッション: 起動(~10s) → フレーズ送信 → 応答待ち(~10s) → /exit → 完了待ち(~5s)
# 30件で約15分。

set -euo pipefail

export PATH="/opt/homebrew/bin:$PATH"

NUM_PHRASES="${1:-30}"
CLAUDE="/opt/homebrew/bin/claude"
RELAY="/Volumes/DISK/dev/agents-relay/target/release/agents-relay"
RESULTS_DIR="/tmp/relay_recall_interactive_$(date +%Y%m%d_%H%M%S)"
JSOL_DIR="$HOME/.claude/projects"
VERIFY_LOG="$RESULTS_DIR/verify.log"
SUMMARY="$RESULTS_DIR/summary.txt"
TMUX_SESSION="relay_test_$$"
STARTUP_WAIT=12   # Claude起動待ち（秒）
RESPONSE_WAIT=15  # 応答待ち（秒）
EXIT_WAIT=5       # /exit後の終了待ち（秒）

mkdir -p "$RESULTS_DIR"

echo "=== agents-relay Interactive Memory Recall Test ==="
echo "Phrases:  $NUM_PHRASES"
echo "Results:  $RESULTS_DIR"
echo "Mode:     interactive (tmux)"
echo "Est time: ~$((NUM_PHRASES * (STARTUP_WAIT + RESPONSE_WAIT + EXIT_WAIT) / 60 + 1)) min"
echo "Start:    $(date)"
echo ""

# ── Phase 1: フレーズ生成 ──
echo "[Phase 1] Generating $NUM_PHRASES unique phrases..."
PHRASES_FILE="$RESULTS_DIR/phrases.txt"
for i in $(seq 1 "$NUM_PHRASES"); do
    printf "INTERACTIVE_TEST_%04d_%s\n" "$i" "$(openssl rand -hex 4)"
done > "$PHRASES_FILE"
echo "  → $PHRASES_FILE"

# ── Phase 2: tmux経由でインタラクティブにフレーズ送信 ──
echo ""
echo "[Phase 2] Seeding phrases via interactive Claude (tmux)..."
seed_ok=0
seed_fail=0

while IFS= read -r phrase; do
    idx=$((seed_ok + seed_fail + 1))
    echo -n "  [$idx/$NUM_PHRASES] $phrase ... "
    log_file="$RESULTS_DIR/session_${phrase}.log"

    # tmuxセッション作成（detached）
    tmux new-session -d -s "$TMUX_SESSION" -x 200 -y 50

    # Claude起動
    tmux send-keys -t "$TMUX_SESSION" "$CLAUDE" Enter

    # 起動待ち
    sleep "$STARTUP_WAIT"

    # フレーズを送信
    tmux send-keys -t "$TMUX_SESSION" "Please remember this unique identifier: $phrase" Enter

    # 応答待ち
    sleep "$RESPONSE_WAIT"

    # ペインの内容をキャプチャ
    tmux capture-pane -t "$TMUX_SESSION" -p -S -100 > "$log_file" 2>&1

    # /exitで終了
    tmux send-keys -t "$TMUX_SESSION" "/exit" Enter
    sleep "$EXIT_WAIT"

    # セッション終了確認＆クリーンアップ
    if tmux has-session -t "$TMUX_SESSION" 2>/dev/null; then
        tmux kill-session -t "$TMUX_SESSION" 2>/dev/null || true
    fi

    # ログにフレーズが含まれていればOK（Claudeが応答した証拠）
    if grep -q "$phrase" "$log_file" 2>/dev/null; then
        echo "OK"
        seed_ok=$((seed_ok + 1))
    else
        echo "FAIL"
        seed_fail=$((seed_fail + 1))
    fi
done < "$PHRASES_FILE"

echo ""
echo "  Seed complete: OK=$seed_ok FAIL=$seed_fail"

# ── Phase 3: Ingest ──
echo ""
echo "[Phase 3] Running ingest..."
"$RELAY" ingest "$JSOL_DIR" 2>&1 | tail -5
echo "  Ingest done."

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
Mode:           interactive (tmux)
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
echo "Session captures: $RESULTS_DIR/session_*.log"
echo "Failed lookups: $VERIFY_LOG"
