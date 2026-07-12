//! compact 投影: 過去ログを会話として再構成するための軽量表現を決定的に生成する。
//!
//! 設計方針: 原本 (raw_entries.content の全文 = fdb.fulltext, 生 CoT = rdb.cot) は
//! 一切変更しない。ここで作るのは「意味の流れは保つがトークンを畳んだ投影」で、
//! すべて再生成可能な派生物 (cdb.entry_compact に materialize)。CoT は取り込み時に
//! すでに分離済みなので、compact が主に畳むのは **ツールのペイロード** である:
//!   - assistant の tool_use (`[Tool: NAME] {input-json}`) → 署名化
//!   - user 側 tool_result (JSON 配列) の巨大な出力 → 行数/バイト数の要約 (+ mid は先頭)
//!   - 冗長に長い本文 → 文字数上限で切り詰め
//!
//! エラー系の tool_result は短く、かつ会話の意味に直結するので **両レベルで残す**。
//!
//! 2 段の詳細度を持つ:
//!   - Min: 会話の骨子。本文は要点だけ、ツールは名前と対象だけ (既定で返す = 最大節約)。
//!   - Mid: 流れと意図が追える。本文はほぼ保持、ツールは署名 + 結果の先頭。
//!
//! レシピを改良したら [`RECIPE_VERSION`] を上げる。stale 行は `db rebuild-compact` /
//! consolidate パスで再生成される (要約の `<!-- summarizer -->` マーカーと同じ発想)。

use serde_json::Value;

/// 生成レシピの版。cdb.entry_compact.recipe_version に刻む。
/// これより小さい版で作られた行は stale とみなして再生成する。
pub const RECIPE_VERSION: i64 = 1;

/// 詳細度。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// 最大圧縮。会話の骨子だけ (既定)。本文も上限で切り詰める。
    Min,
    /// 中圧縮。流れと意図が追える。
    Mid,
    /// 検索索引用。**本文(散文)は全保持**し、ツールのペイロードだけ畳む。
    /// min と違い本文を切り詰めない = 検索語を落とさない。tool_use は名前+対象、
    /// 成功 tool_result はスタブに畳んでノイズを除き、エラーは検索可能に残す。
    /// FTS はこのレベルで索引する (会話再構成用の min/mid とは目的が別)。
    Surface,
}

// --- 各レベルの畳み込み予算 (文字数) ---
/// 本文 1 メッセージあたりの上限。
const MIN_TEXT_CAP: usize = 300;
const MID_TEXT_CAP: usize = 1_500;
/// tool_result (成功) を mid で残す先頭サイズ。
const MID_RESULT_HEAD_LINES: usize = 15;
const MID_RESULT_HEAD_CHARS: usize = 600;
/// tool_result (エラー) を残す上限。
const MIN_ERROR_CAP: usize = 300;
const MID_ERROR_CAP: usize = 1_200;
/// tool_use の対象識別子 (file_path / command 等) を残す上限。
const TARGET_CAP: usize = 120;

/// (min, mid) をまとめて生成する。呼び出し側は全文 content を渡す。
pub fn compact_pair(entry_type: &str, tool_name: Option<&str>, content: &str) -> (String, String) {
    (
        fold(entry_type, tool_name, content, Level::Min),
        fold(entry_type, tool_name, content, Level::Mid),
    )
}

/// 検索索引 (FTS) 用の投影。散文は全保持し、ツール出力だけを畳む。
/// 「表層 (会話) で検索して詳細へ辿る」ための索引本文を作る。
pub fn surface(entry_type: &str, tool_name: Option<&str>, content: &str) -> String {
    fold(entry_type, tool_name, content, Level::Surface)
}

/// 1 エントリの content を指定レベルの投影に畳む。
pub fn fold(entry_type: &str, _tool_name: Option<&str>, content: &str, level: Level) -> String {
    match entry_type {
        "assistant" => fold_assistant(content, level),
        "user" => fold_user(content, level),
        // system / progress / summary 等はそのまま切り詰めるだけ。
        _ => fold_plain(content, level),
    }
}

/// assistant: 行を走査し、`[Tool: NAME] {json}` 行はツール署名に畳み、
/// それ以外は本文としてまとめて上限で切り詰める。
fn fold_assistant(content: &str, level: Level) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut prose: Vec<String> = Vec::new();
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("[Tool: ") {
            flush_prose(&mut prose, &mut out, level);
            out.push(fold_tool_use_line(rest, level));
        } else {
            prose.push(line.to_string());
        }
    }
    flush_prose(&mut prose, &mut out, level);
    out.join("\n")
}

/// たまった本文行を 1 ブロックとして切り詰めて出力に足す。
fn flush_prose(prose: &mut Vec<String>, out: &mut Vec<String>, level: Level) {
    if prose.is_empty() {
        return;
    }
    let joined = prose.join("\n");
    prose.clear();
    let t = joined.trim();
    if !t.is_empty() {
        out.push(cap_text(t, text_cap(level)));
    }
}

/// `"NAME] {json}"` (先頭の `[Tool: ` を剥がした残り) をツール署名に畳む。
fn fold_tool_use_line(rest: &str, level: Level) -> String {
    let (name, json_str) = match rest.split_once("] ") {
        Some((n, j)) => (n.trim(), j),
        None => (rest.trim_end_matches(']').trim(), ""),
    };
    let input: Value = serde_json::from_str(json_str).unwrap_or(Value::Null);
    fold_tool_signature(name, &input, level)
}

/// ツール名 + input から署名文字列を作る。
fn fold_tool_signature(name: &str, input: &Value, level: Level) -> String {
    let target = tool_target(input);
    match level {
        // Min と Surface は同じ署名 (名前 + 対象)。Surface は本文だけ切り詰めない。
        Level::Min | Level::Surface => match target {
            Some(t) => format!("[{name} {t}]"),
            None => format!("[{name}]"),
        },
        Level::Mid => {
            let mut s = format!("[use: {name}");
            if let Some(t) = target {
                s.push(' ');
                s.push_str(&t);
            }
            if let Some(hint) = payload_hint(input) {
                s.push(' ');
                s.push_str(&hint);
            }
            s.push(']');
            s
        }
    }
}

/// tool_use の input から最も分かりやすい対象識別子を 1 つ選ぶ。
fn tool_target(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    for key in [
        "file_path",
        "notebook_path",
        "path",
        "command",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(cap_text(&v.replace('\n', " "), TARGET_CAP));
            }
        }
    }
    None
}

/// mid 用: 大きなペイロード (書き込み内容など) を行数ヒントで示す。
fn payload_hint(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    for key in ["content", "new_string", "file_text"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                let lines = s.lines().count().max(1);
                return Some(format!("(+{lines} lines)"));
            }
        }
    }
    None
}

/// user: tool_result などは JSON 配列で来る。プレーン文字列ならそのまま切り詰める。
fn fold_user(content: &str, level: Level) -> String {
    let trimmed = content.trim_start();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(content) {
            return fold_user_json(&v, level);
        }
    }
    fold_plain(content, level)
}

fn fold_user_json(v: &Value, level: Level) -> String {
    match v {
        Value::Array(arr) => arr
            .iter()
            .map(|item| fold_content_block(item, level))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => fold_content_block(v, level),
        _ => fold_plain(&v.to_string(), level),
    }
}

/// content ブロック 1 個 (text / tool_result / tool_use / image / その他) を畳む。
fn fold_content_block(item: &Value, level: Level) -> String {
    let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "tool_result" => fold_tool_result(item, level),
        "text" => {
            let t = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
            cap_text(t.trim(), text_cap(level))
        }
        "tool_use" => {
            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
            let input = item.get("input").cloned().unwrap_or(Value::Null);
            fold_tool_signature(name, &input, level)
        }
        "image" => "[image]".to_string(),
        _ => cap_text(item.to_string().trim(), text_cap(level)),
    }
}

/// tool_result を畳む。成功は行数/バイト数の要約 (mid は先頭も)、エラーは本文を残す。
fn fold_tool_result(item: &Value, level: Level) -> String {
    let text = extract_result_text(item);
    let is_error = item
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    let lines = text.lines().count().max(if text.is_empty() { 0 } else { 1 });
    let bytes = text.len();

    if is_error {
        let cap = match level {
            Level::Min => MIN_ERROR_CAP,
            Level::Mid => MID_ERROR_CAP,
            // 検索用: エラー文 (探し物になりやすい) は全文を残す。
            Level::Surface => usize::MAX,
        };
        return format!("[result ERROR: {}]", cap_text(text.trim(), cap));
    }

    match level {
        // 成功したツール出力の本体は索引しない (スタブのみ)。ノイズ源を断つ。
        Level::Min | Level::Surface => format!("[result: {lines} lines, {bytes} bytes]"),
        Level::Mid => {
            let head = head_of(&text, MID_RESULT_HEAD_LINES, MID_RESULT_HEAD_CHARS);
            if head.trim().is_empty() {
                format!("[result: {lines} lines, {bytes} bytes]")
            } else {
                format!("[result: {lines} lines, {bytes} bytes] head:\n{head}")
            }
        }
    }
}

/// tool_result の content からテキストを取り出す (文字列 / ブロック配列の両対応)。
fn extract_result_text(item: &Value) -> String {
    match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => {
            let mut parts = Vec::new();
            for b in arr {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    parts.push(t.to_string());
                } else if b.get("type").and_then(|t| t.as_str()) == Some("image") {
                    parts.push("[image]".to_string());
                }
            }
            parts.join("\n")
        }
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// system / progress など: 全文を上限で切り詰めるだけ。
fn fold_plain(content: &str, level: Level) -> String {
    cap_text(content.trim(), text_cap(level))
}

fn text_cap(level: Level) -> usize {
    match level {
        Level::Min => MIN_TEXT_CAP,
        Level::Mid => MID_TEXT_CAP,
        // 検索用は散文を切り詰めない (検索語を落とさないため)。
        Level::Surface => usize::MAX,
    }
}

/// 文字数で切り詰める。切ったら省略分を `… [+N chars]` で明示する
/// (マルチバイト境界は char 単位なので壊れない)。
fn cap_text(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push_str(&format!("… [+{} chars]", count - max_chars));
    out
}

/// 先頭 min(max_lines 行, max_chars 文字) を返す。
fn head_of(s: &str, max_lines: usize, max_chars: usize) -> String {
    let mut out = String::new();
    let mut newlines = 0usize;
    for (i, ch) in s.chars().enumerate() {
        if i >= max_chars {
            break;
        }
        if ch == '\n' {
            newlines += 1;
            if newlines >= max_lines {
                break;
            }
        }
        out.push(ch);
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assistant_tool_use_write_folds_to_signature() {
        // ingest が作る形式: text 行 + `[Tool: NAME] {json}` 行
        let content = "Writing the file now.\n[Tool: Write] {\"file_path\":\"/x/db.rs\",\"content\":\"line1\\nline2\\nline3\"}";
        let (min, mid) = compact_pair("assistant", Some("Write"), content);
        assert_eq!(min, "Writing the file now.\n[Write /x/db.rs]");
        assert_eq!(mid, "Writing the file now.\n[use: Write /x/db.rs (+3 lines)]");
    }

    #[test]
    fn assistant_bash_uses_command_as_target() {
        let content = "[Tool: Bash] {\"command\":\"cargo test --release\"}";
        let (min, mid) = compact_pair("assistant", Some("Bash"), content);
        assert_eq!(min, "[Bash cargo test --release]");
        assert_eq!(mid, "[use: Bash cargo test --release]");
    }

    #[test]
    fn tool_use_with_no_input_keeps_name_only() {
        let content = "[Tool: TodoList] ";
        let (min, _mid) = compact_pair("assistant", None, content);
        assert_eq!(min, "[TodoList]");
    }

    #[test]
    fn user_tool_result_success_summarized() {
        // Read が 3 行返した tool_result
        let content = r#"[{"type":"tool_result","tool_use_id":"x","content":"aaa\nbbb\nccc"}]"#;
        let (min, mid) = compact_pair("user", None, content);
        assert_eq!(min, "[result: 3 lines, 11 bytes]");
        assert!(mid.starts_with("[result: 3 lines, 11 bytes] head:\n"));
        assert!(mid.contains("aaa"));
    }

    #[test]
    fn user_tool_result_error_kept_in_both_levels() {
        let content = r#"[{"type":"tool_result","content":"error: file not found","is_error":true}]"#;
        let (min, mid) = compact_pair("user", None, content);
        assert!(min.contains("ERROR"));
        assert!(min.contains("file not found"));
        assert!(mid.contains("ERROR"));
        assert!(mid.contains("file not found"));
    }

    #[test]
    fn user_tool_result_content_as_block_array() {
        // content が {type:text,text} のブロック配列で来るパターン
        let content =
            r#"[{"type":"tool_result","content":[{"type":"text","text":"hello\nworld"}]}]"#;
        let (min, _mid) = compact_pair("user", None, content);
        assert_eq!(min, "[result: 2 lines, 11 bytes]");
    }

    #[test]
    fn plain_user_text_truncated_at_min() {
        let long = "x".repeat(500);
        let (min, mid) = compact_pair("user", None, &long);
        assert!(min.chars().count() < 350);
        assert!(min.contains("[+200 chars]"));
        // mid は 500 文字なら丸ごと残る (上限 1500)
        assert_eq!(mid, long);
    }

    #[test]
    fn multibyte_truncation_is_char_safe() {
        let jp = "あ".repeat(400);
        let (min, _mid) = compact_pair("assistant", None, &jp);
        // パニックしないこと + 省略マーカーが付くこと
        assert!(min.contains("[+100 chars]"));
        assert!(min.starts_with("あ"));
    }

    #[test]
    fn system_entry_folds_as_plain() {
        let content = "Session Started in workspace: /x/y";
        let (min, _mid) = compact_pair("system", Some("session_start"), content);
        assert_eq!(min, content);
    }

    #[test]
    fn recipe_version_is_positive() {
        assert!(RECIPE_VERSION >= 1);
    }

    // --- Surface (検索索引用) ---

    #[test]
    fn surface_keeps_full_prose_but_folds_tool_use() {
        let prose = "決定的なバグの原因はイベントループの競合状態だった。".repeat(40); // 300字超
        let content = format!("{prose}\n[Tool: Read] {{\"file_path\":\"/x/db.rs\"}}");
        let s = surface("assistant", Some("Read"), &content);
        assert!(s.contains(&prose)); // 散文は丸ごと残る
        assert!(!s.contains("[+")); // 切り詰めマーカーは付かない
        assert!(s.contains("[Read /x/db.rs]")); // tool_use は署名に畳む
    }

    #[test]
    fn surface_drops_successful_tool_result_body() {
        let content = r#"[{"type":"tool_result","content":"secretword\nline2\nline3"}]"#;
        let s = surface("user", None, content);
        assert!(!s.contains("secretword")); // 成功出力の本体は索引しない
        assert!(s.contains("lines")); // スタブだけ
    }

    #[test]
    fn surface_keeps_error_text_searchable() {
        let content =
            r#"[{"type":"tool_result","content":"error: cannot find symbol foobar_xyz","is_error":true}]"#;
        let s = surface("user", None, content);
        assert!(s.contains("foobar_xyz")); // エラーは検索可能に残す
    }
}
