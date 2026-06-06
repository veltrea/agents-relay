use anyhow::Result;
use rusqlite::Connection;

use crate::db;

/// セッションを Markdown 形式で書き出し
pub fn export_session(conn: &Connection, session_id: &str) -> Result<String> {
    let entries = db::get_session_entries(conn, session_id, None, 1000)?;
    if entries.is_empty() {
        anyhow::bail!("No entries found for session: {session_id}");
    }

    let mut md = String::new();
    md.push_str(&format!("# Session: {session_id}\n\n"));

    if let Some(first) = entries.first() {
        md.push_str(&format!(
            "- **Date**: {}\n- **CWD**: {}\n- **Branch**: {}\n\n---\n\n",
            first.date,
            first.cwd.as_deref().unwrap_or("(unknown)"),
            first.git_branch.as_deref().unwrap_or("(none)"),
        ));
    }

    for e in &entries {
        let role = match e.entry_type.as_str() {
            "user" => "🧑 User",
            "assistant" => "🤖 Assistant",
            "system" => "⚙️ System",
            "progress" => "📊 Progress",
            _ => &e.entry_type,
        };
        md.push_str(&format!("### {} ({})\n\n", role, e.time));

        if let Some(ref tool) = e.tool_name {
            md.push_str(&format!("**Tool**: `{tool}`\n\n"));
        }

        md.push_str(&e.content);
        md.push_str("\n\n---\n\n");
    }

    Ok(md)
}

/// 日付指定で全セッションをエクスポート
pub fn export_date(conn: &Connection, date: &str) -> Result<String> {
    let sessions = db::list_sessions(conn, Some(date), 100, None)?;
    if sessions.is_empty() {
        anyhow::bail!("No sessions found for date: {date}");
    }

    let mut md = format!("# Sessions on {date}\n\n");
    for (sid, _, _, _, count) in &sessions {
        md.push_str(&format!("- `{sid}` ({count} entries)\n"));
    }
    md.push_str("\n---\n\n");

    for (sid, _, _, _, _) in &sessions {
        let session_md = export_session(conn, sid)?;
        md.push_str(&session_md);
        md.push_str("\n\n");
    }

    Ok(md)
}
