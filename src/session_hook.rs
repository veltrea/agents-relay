use anyhow::Result;
use rusqlite::Connection;
use crate::db;

/// SessionStart hook 用の軽量な記憶ダイジェストを生成する。
///
/// 戻り値の文字列はそのまま stdout に出力され、Claude Code の
/// SessionStart hook 経由でコンテキストに注入される（モデルの
/// 「呼ぶ/呼ばない」判断を介さないため deferred 化の影響を受けない）。
///
/// コンテキスト節約のため、直近セッションの一覧と各セッションの
/// 最初のユーザー発言だけを載せる。詳細が必要なときに能動的に
/// memory_search を呼ばせる「土台」を作るのが目的。
pub fn build_digest(conn: &Connection, workspace: Option<&str>, limit: i64) -> Result<String> {
    let sessions = db::list_sessions(conn, None, limit, workspace)?;
    if sessions.is_empty() {
        return Ok(String::new());
    }

    let mut out = String::new();
    out.push_str("## 🧠 agents-relay — past session memory (this workspace)\n\n");

    for (session_id, _first, _last, date, count) in &sessions {
        let headline = first_user_line(conn, session_id).unwrap_or_default();
        let preview: String = headline.chars().take(100).collect();
        if preview.is_empty() {
            out.push_str(&format!("- {date} ({count} entries)\n"));
        } else {
            out.push_str(&format!("- {date} ({count} entries): {preview}\n"));
        }
    }

    out.push_str(
        "\nThese are summaries of prior sessions in this workspace. \
         To recall specific details, use the `memory_search` tool.\n",
    );
    Ok(out)
}

/// セッションの最初の「自然な」ユーザー発言を1行で返す。
/// slash command 展開・ローカルコマンド出力・システム注入はスキップする。
fn first_user_line(conn: &Connection, session_id: &str) -> Option<String> {
    let entries = db::get_session_entries(conn, session_id, Some("user"), 10).ok()?;
    for e in entries {
        let line = e
            .content
            .replace('\n', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if is_natural_user_text(&line) {
            return Some(line);
        }
    }
    None
}

/// slash command 展開・ツール結果・システム注入などのノイズを除外する。
fn is_natural_user_text(s: &str) -> bool {
    let t = s.trim();
    if t.is_empty() {
        return false;
    }
    // 構造化メッセージ（slash command 展開・画像・tool result は JSON 配列で保存される）
    if t.starts_with("[{") || t.starts_with("[\"") {
        return false;
    }
    const NOISE_MARKERS: &[&str] = &[
        "<command-",
        "<local-command",
        "<system-reminder",
        "Caveat:",
        "[Tool:",
        "tool_use_id",
        "tool_result",
    ];
    !NOISE_MARKERS.iter().any(|m| t.contains(m))
}
