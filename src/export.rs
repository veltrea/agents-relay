use anyhow::Result;
use rusqlite::Connection;
use std::io::Write;
use std::path::Path;

use crate::db;

/// 指定日の全エントリを無損失 JSONL としてファイルに追記する。
/// 全カラムに加え、分離 DB の生 CoT (rdb.cot) と全文原本 (fdb.fulltext) を
/// 結合して書き出すため、この 1 ファイルから完全に再構築・再処理できる。
/// 追記方式なので、再実行やクラッシュ後のリトライでも過去の書き出しは消えない
/// （重複行が生じうるが、損失よりは重複を選ぶ）。
/// 戻り値: 書き出したエントリ数
pub fn append_date_jsonl(conn: &Connection, date: &str, file: &Path) -> Result<u64> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.session_id, r.timestamp, r.date, r.time, r.type, r.tool_name,
                COALESCE(f.content, r.content) AS content,
                r.cwd, r.git_branch, r.source, r.created_at,
                c.reasoning, c.reasoning_summary, r.reasoning_tokens
         FROM raw_entries r
         LEFT JOIN rdb.cot c ON c.entry_id = r.id
         LEFT JOIN fdb.fulltext f ON f.entry_id = r.id
         WHERE r.date = ?1 ORDER BY r.id",
    )?;

    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    let mut w = std::io::BufWriter::new(out);
    let mut count = 0u64;

    let rows = stmt.query_map(rusqlite::params![date], |row| {
        Ok(serde_json::json!({
            "id":                row.get::<_, i64>(0)?,
            "session_id":        row.get::<_, String>(1)?,
            "timestamp":         row.get::<_, String>(2)?,
            "date":              row.get::<_, String>(3)?,
            "time":              row.get::<_, String>(4)?,
            "type":              row.get::<_, String>(5)?,
            "tool_name":         row.get::<_, Option<String>>(6)?,
            "content":           row.get::<_, String>(7)?,
            "cwd":               row.get::<_, Option<String>>(8)?,
            "git_branch":        row.get::<_, Option<String>>(9)?,
            "source":            row.get::<_, String>(10)?,
            "created_at":        row.get::<_, Option<String>>(11)?,
            "reasoning":         row.get::<_, Option<String>>(12)?,
            "reasoning_summary": row.get::<_, Option<String>>(13)?,
            "reasoning_tokens":  row.get::<_, Option<i64>>(14)?,
        }))
    })?;

    for row in rows {
        let v = row?;
        serde_json::to_writer(&mut w, &v)?;
        w.write_all(b"\n")?;
        count += 1;
    }

    // 削除はこの書き出しが物理ディスクに届いてから行う前提なので fsync まで行う
    w.flush()?;
    w.into_inner()?.sync_all()?;

    Ok(count)
}

/// archive が書き出した無損失 JSONL を DB に復元する。
/// 同一 (session_id, timestamp, type, tool_name) の行が既に存在すればスキップ
/// （冪等・二重復元防止）。content は insert_entry_full 経由で inline/全文原本に
/// 再分配され、生 CoT と要約も分離 DB へ戻る。
/// 戻り値: (復元した件数, スキップした件数)
pub fn restore_archive_jsonl(conn: &Connection, path: &Path) -> Result<(u64, u64)> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut restored = 0u64;
    let mut skipped = 0u64;

    for line in std::io::BufRead::lines(reader) {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        let (session_id, timestamp, date, time, entry_type, content) = match (
            s("session_id"), s("timestamp"), s("date"), s("time"), s("type"), s("content"),
        ) {
            (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => (a, b, c, d, e, f),
            _ => continue,
        };
        let tool_name = s("tool_name");
        let cwd = s("cwd");
        let git_branch = s("git_branch");
        let source = s("source").unwrap_or_else(|| "claude".to_string());
        let reasoning = s("reasoning");
        let reasoning_summary = s("reasoning_summary");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM raw_entries
                 WHERE session_id = ?1 AND timestamp = ?2 AND type = ?3 AND tool_name IS ?4
                 LIMIT 1",
                rusqlite::params![session_id, timestamp, entry_type, tool_name],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if exists {
            skipped += 1;
            continue;
        }

        let id = db::insert_entry_full(
            conn,
            &session_id,
            &timestamp,
            &date,
            &time,
            &entry_type,
            tool_name.as_deref(),
            &content,
            cwd.as_deref(),
            git_branch.as_deref(),
            &source,
            reasoning.as_deref(),
        )?;
        if let Some(sum) = reasoning_summary {
            db::set_reasoning_summary(conn, id, &sum)?;
        }
        restored += 1;
    }

    Ok((restored, skipped))
}

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
