use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use crate::db;

/// content の最大保存サイズ (bytes)
const MAX_CONTENT_SIZE: usize = 50_000;

/// JSONL ファイルを差分取り込み
pub fn ingest_file(conn: &mut Connection, path: &Path) -> Result<u64> {
    let path_str = path.to_string_lossy().to_string();
    let offset = db::get_sync_offset(conn, &path_str)?;

    let file = File::open(path)?;
    let file_len = file.metadata()?.len() as i64;

    if file_len <= offset {
        return Ok(0);
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset as u64))?;

    let mut count = 0u64;
    let mut current_offset = offset;

    // session_id をファイル名(拡張子なし)から取得
    let file_session_id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();

    let mut line_buf = String::new();
    let mut tx = conn.transaction()?;

    loop {
        line_buf.clear();
        // read_line で実際のバイト数を取得（CRLF 対応）
        let bytes_read = reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        current_offset += bytes_read as i64;

        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }

        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // パース失敗は無視
        };

        if let Some(n) = process_entry(&v, &file_session_id) {
            db::insert_entry(
                &tx,
                &n.session_id,
                &n.timestamp,
                &n.date,
                &n.time,
                &n.entry_type,
                n.tool_name.as_deref(),
                &n.content,
                n.cwd.as_deref(),
                n.git_branch.as_deref(),
                "claude",
            )?;
            count += 1;

            // 100件ごとにオフセットも同じトランザクション内で保存してからコミット
            if count % 100 == 0 {
                db::set_sync_offset(&tx, &path_str, current_offset)?;
                tx.commit()?;
                tx = conn.transaction()?;
            }
        }
    }

    // 残りをコミット
    db::set_sync_offset(&tx, &path_str, current_offset)?;
    tx.commit()?;

    Ok(count)
}

/// ディレクトリ配下の JSONL を全部取り込み
pub fn ingest_dir(conn: &mut Connection, dir: &Path) -> Result<u64> {
    let pattern = format!("{}/**/*.jsonl", dir.to_string_lossy());
    let mut total = 0u64;
    for entry in glob::glob(&pattern)? {
        let path = entry?;
        let count = ingest_file(conn, &path)?;
        if count > 0 {
            eprintln!("  {} (+{} entries)", path.display(), count);
        }
        total += count;
    }
    Ok(total)
}

/// Claude Code + Antigravity の全ソースを取り込み
pub fn sync_all(conn: &mut Connection) -> Result<u64> {
    let mut total = 0u64;

    // Claude Code: ~/.claude/projects 配下
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let projects_dir = home.join(".claude").join("projects");
    if projects_dir.exists() {
        total += ingest_dir(conn, &projects_dir)?;
    }

    // Antigravity: ~/.gemini/antigravity/brain 配下
    total += crate::ingest_antigravity::sync_all(conn)?;

    Ok(total)
}

struct ParsedEntry {
    session_id: String,
    timestamp: String,
    date: String,
    time: String,
    entry_type: String,
    tool_name: Option<String>,
    content: String,
    cwd: Option<String>,
    git_branch: Option<String>,
}

fn process_entry(v: &Value, file_session_id: &str) -> Option<ParsedEntry> {
    let entry_type = v.get("type")?.as_str()?;

    // file-history-snapshot は保存しない
    if entry_type == "file-history-snapshot" {
        return None;
    }

    let session_id = v
        .get("sessionId")
        .and_then(|s| s.as_str())
        .unwrap_or(file_session_id)
        .to_string();

    let timestamp = v
        .get("timestamp")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();

    // timestamp が空なら created_at 等から推測、なければスキップ
    if timestamp.is_empty() {
        return None;
    }

    // 日付・時刻を分離 (ISO 8601: "2026-03-21T15:28:53.127Z")
    let (date, time) = parse_datetime(&timestamp);

    let cwd = v.get("cwd").and_then(|s| s.as_str()).map(String::from);
    let git_branch = v
        .get("gitBranch")
        .and_then(|s| s.as_str())
        .map(String::from);

    let (content, tool_name) = extract_content(v, entry_type);

    // base64 画像データを含むなら除外
    if content.contains("data:image/") || content.len() > MAX_CONTENT_SIZE {
        let truncated = if content.len() > MAX_CONTENT_SIZE {
            let safe_end = truncate_at_char_boundary(&content, 1000);
            format!("{}... [truncated, {} bytes]", &content[..safe_end], content.len())
        } else {
            return None; // 画像データのみなら完全にスキップ
        };
        return Some(ParsedEntry {
            session_id,
            timestamp,
            date,
            time,
            entry_type: entry_type.to_string(),
            tool_name,
            content: truncated,
            cwd,
            git_branch,
        });
    }

    Some(ParsedEntry {
        session_id,
        timestamp,
        date,
        time,
        entry_type: entry_type.to_string(),
        tool_name,
        content,
        cwd,
        git_branch,
    })
}

fn extract_content(v: &Value, entry_type: &str) -> (String, Option<String>) {
    match entry_type {
        "user" => {
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .map(|c| match c {
                    Value::String(s) => s.clone(),
                    _ => c.to_string(),
                })
                .unwrap_or_default();
            (content, None)
        }
        "assistant" => {
            let message = v.get("message");
            if let Some(msg) = message {
                let content_arr = msg.get("content");
                if let Some(Value::Array(arr)) = content_arr {
                    let mut texts = Vec::new();
                    let mut tool_name = None;
                    for item in arr {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                                    texts.push(t.to_string());
                                }
                            }
                            Some("tool_use") => {
                                let name = item
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("unknown");
                                tool_name = Some(name.to_string());
                                let input = item
                                    .get("input")
                                    .map(|i| i.to_string())
                                    .unwrap_or_default();
                                texts.push(format!("[Tool: {name}] {input}"));
                            }
                            Some("thinking") => {
                                // thinking は省略（大きすぎる）
                            }
                            _ => {}
                        }
                    }
                    return (texts.join("\n"), tool_name);
                }
            }
            // slug ベースのエントリ（ストリーミング中間データ）
            if let Some(slug) = v.get("slug").and_then(|s| s.as_str()) {
                return (format!("[streaming: {slug}]"), None);
            }
            (String::new(), None)
        }
        "system" => {
            let content = v
                .get("message")
                .and_then(|m| m.get("content"))
                .map(|c| match c {
                    Value::String(s) => s.clone(),
                    _ => c.to_string(),
                })
                .unwrap_or_else(|| "[system]".to_string());
            (content, None)
        }
        "progress" => {
            let content = v.to_string();
            (content, None)
        }
        _ => {
            let content = v.to_string();
            (content, None)
        }
    }
}

/// マルチバイト文字の境界を壊さないように切り詰め位置を返す
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn parse_datetime(ts: &str) -> (String, String) {
    // "2026-03-21T15:28:53.127Z" → ("2026-03-21", "15:28:53")
    if ts.len() >= 19 {
        let date = ts[..10].to_string();
        let time = ts[11..19].to_string();
        (date, time)
    } else {
        (ts.to_string(), "00:00:00".to_string())
    }
}

/// sync_state の状態表示
pub fn sync_status(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT file_path, last_offset FROM sync_state ORDER BY file_path")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let mut output = String::from("Tracked files:\n");
    for row in rows {
        let (path, offset) = row?;
        // ファイルの現在サイズと比較
        let current_size = std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(-1);
        let pending = if current_size >= 0 {
            current_size - offset
        } else {
            -1
        };
        let status = if pending == 0 {
            "up to date".to_string()
        } else if pending > 0 {
            format!("{pending} bytes pending")
        } else {
            "file missing".to_string()
        };
        output.push_str(&format!("  {path}\n    offset: {offset}, {status}\n"));
    }
    Ok(output)
}

/// sync_state リセット
pub fn sync_reset(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM sync_state", [])?;
    Ok(())
}
