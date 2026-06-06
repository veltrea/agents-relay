#!/usr/bin/env bash
# layer1_jsol_test.sh — レイヤー1: Claude会話 → JSOL記録テスト
#
# 目的: Claude CLIで会話した結果、JSOLファイルが正しく生成されるか検証する。
# パイプラインの最上流を確認するテスト。
#
# 検証項目:
#   - claude -p 実行後にJSOLファイルが新規作成されるか
#   - JSOLの中にユーザーメッセージが記録されているか
#   - JSOLの中にアシスタント応答が記録されているか
#
# 使い方: bash tests/layer1_jsol_test.sh

set -euo pipefail
export PATH="/opt/homebrew/bin:$PATH"

CLAUDE="/opt/homebrew/bin/claude"
JSOL_DIR="$HOME/.claude/projects"
RESULTS_DIR="/tmp/relay_layer1_$(date +%Y%m%d_%H%M%S)"
mkdir -p "$RESULTS_DIR"

echo "=== Layer 1: JSOL Recording Test ==="
echo "Results: $RESULTS_DIR"
echo "Start:   $(date)"
echo ""

# テスト前のJSOLファイル一覧を取得
find "$JSOL_DIR" -name "*.jsonl" -newer /tmp/.relay_test_marker 2>/dev/null > "$RESULTS_DIR/before.txt" || true
touch /tmp/.relay_test_marker

PHRASE="LAYER1_TEST_$(openssl rand -hex 6)"
echo "[1] Sending phrase: $PHRASE"

# claude -p で送信
"$CLAUDE" -p "Acknowledge this: $PHRASE" \
    --output-format text --max-turns 1 \
    < /dev/null > "$RESULTS_DIR/claude_output.txt" 2>&1

echo "[2] Claude responded. Checking for new JSOL files..."

# テスト後のJSOLファイルを取得
sleep 1
NEW_JSOLS=$(find "$JSOL_DIR" -name "*.jsonl" -newer /tmp/.relay_test_marker 2>/dev/null)

if [ -z "$NEW_JSOLS" ]; then
    echo "  FAIL: No new JSOL file created"
    echo "result=FAIL reason=no_jsol" > "$RESULTS_DIR/result.txt"
    exit 1
fi

echo "  New JSOL file(s):"
echo "$NEW_JSOLS" | while read -r f; do echo "    $f"; done

# フレーズを含むJSOLを探す（新規作成されたもの全てをチェック）
MATCHED_JSOL=""
echo "$NEW_JSOLS" | while read -r f; do
    echo "    $(basename "$f") ($(wc -l < "$f" | tr -d ' ') entries)"
done

echo ""
echo "[3] Searching for phrase in new JSOLs..."

user_ok=false
assistant_ok=false
entry_count=0

while IFS= read -r jsol_file; do
    if grep -q "$PHRASE" "$jsol_file" 2>/dev/null; then
        MATCHED_JSOL="$jsol_file"
        break
    fi
done <<< "$NEW_JSOLS"

if [ -n "$MATCHED_JSOL" ]; then
    echo "  Found in: $(basename "$MATCHED_JSOL")"
    user_ok=true
    entry_count=$(wc -l < "$MATCHED_JSOL" | tr -d ' ')
    echo "  Entries: $entry_count"

    echo -n "  Has assistant response: "
    if grep -q '"role":"assistant"' "$MATCHED_JSOL" || grep -q '"type":"assistant"' "$MATCHED_JSOL"; then
        echo "YES"
        assistant_ok=true
    else
        echo "NO"
    fi
else
    echo "  NOT FOUND in any new JSOL"
fi

echo ""
echo "========================================="
if $user_ok && $assistant_ok; then
    echo "  PASS: JSOL correctly records conversation"
    echo "result=PASS entries=$entry_count" > "$RESULTS_DIR/result.txt"
else
    echo "  FAIL: JSOL missing expected content"
    echo "result=FAIL user=$user_ok assistant=$assistant_ok" > "$RESULTS_DIR/result.txt"
fi
echo "========================================="
echo "End: $(date)"
