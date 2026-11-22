//! Cursor Agent transcript ingest.
//!
//! Source: `~/.cursor/projects/<project-id>/agent-transcripts/<uuid>/<uuid>.jsonl`
//!
//! Presentation (`content`) keeps user/assistant text plus a one-line `[tools]` summary.
//! Raw `tool_use` payloads go to `fdb.tool_trace` and are fetched via `memory_get_tools`.

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use crate::db;

const SOURCE: &str = "cursor";
const TOOLS_SUMMARY_MAX_LINES: usize = 20;
const ARG_PREVIEW_CHARS: usize = 120;

pub fn sync_all(conn: &mut Connection) -> Result<u64> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let projects = home.join(".cursor").join("projects");
    if !projects.exists() {
        return Ok(0);
    }

    let mut total = 0u64;
    let pattern = format!("{}/*/agent-transcripts/*/*.jsonl", projects.to_string_lossy());
    for entry in glob::glob(&pattern)? {
        let path = entry?;
        // Skip non-session sidecars if any; expect <uuid>/<uuid>.jsonl
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let parent_name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if parent_name != session_id {
            continue;
        }
        let project_id = path
            .parent() // <uuid>
            .and_then(|p| p.parent()) // agent-transcripts
            .and_then(|p| p.parent()) // <project-id>
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let cwd = guess_cwd_from_project_id(project_id);
        match ingest_file(conn, &path, session_id, cwd.as_deref()) {
            Ok(n) => {
                if n > 0 {
                    eprintln!("  [cursor] {} (+{} entries)", path.display(), n);
                }
                total += n;
            }
            Err(e) => eprintln!("  [cursor] skip {}: {e}", path.display()),
        }
    }
    Ok(total)
}

/// Ingest one transcript JSONL (incremental by byte offset).
pub fn ingest_file(
    conn: &mut Connection,
    path: &Path,
    session_id: &str,
    cwd: Option<&str>,
) -> Result<u64> {
    let path_str = path.to_string_lossy().to_string();
    let mut offset = db::get_sync_offset(conn, &path_str)?;

    let file = File::open(path)?;
    let meta = file.metadata()?;
    let file_len = meta.len() as i64;
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Shrunk file → rewrite; drop prior cursor rows for this session and re-read.
    if file_len < offset {
        db::delete_cursor_session(conn, session_id)?;
        offset = 0;
        db::set_sync_offset(conn, &path_str, 0)?;
    }
    if file_len <= offset {
        return Ok(0);
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset as u64))?;

    let mut count = 0u64;
    let mut current_offset = offset;
    let mut line_index: u64 = 0;
    // Approximate prior lines for timestamp ordering when resuming mid-file.
    if offset > 0 {
        line_index = estimate_prior_lines(path, offset);
    }

    let mut line_buf = String::new();
    let mut tx = conn.transaction()?;

    loop {
        line_buf.clear();
        let bytes_read = reader.read_line(&mut line_buf)?;
        if bytes_read == 0 {
            break;
        }
        current_offset += bytes_read as i64;
        line_index += 1;

        let line = line_buf.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(parsed) = parse_line(&v, session_id, mtime_secs, line_index, cwd) {
            let id = db::insert_entry_full(
                &tx,
                &parsed.session_id,
                &parsed.timestamp,
                &parsed.date,
                &parsed.time,
                &parsed.entry_type,
                parsed.tool_name.as_deref(),
                &parsed.content,
                parsed.cwd.as_deref(),
                None,
                SOURCE,
                None,
            )?;
            if let Some(ref tools_json) = parsed.tools_json {
                db::set_tool_trace(
                    &tx,
                    id,
                    tools_json,
                    parsed.tools_summary.as_deref(),
                )?;
            }
            count += 1;

            if count % 100 == 0 {
                db::set_sync_offset(&tx, &path_str, current_offset)?;
                tx.commit()?;
                tx = conn.transaction()?;
            }
        }
    }

    db::set_sync_offset(&tx, &path_str, current_offset)?;
    tx.commit()?;
    Ok(count)
}

struct ParsedLine {
    session_id: String,
    timestamp: String,
    date: String,
    time: String,
    entry_type: String,
    tool_name: Option<String>,
    content: String,
    cwd: Option<String>,
    tools_json: Option<String>,
    tools_summary: Option<String>,
}

fn parse_line(
    v: &Value,
    session_id: &str,
    mtime_secs: i64,
    line_index: u64,
    cwd: Option<&str>,
) -> Option<ParsedLine> {
    // Skip non-message status rows etc.
    if v.get("role").is_none() && v.get("message").is_none() {
        return None;
    }

    let role = v
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant");
    let entry_type = match role {
        "user" => "user",
        "system" => "system",
        _ => "assistant",
    };

    let content_val = v
        .get("message")
        .and_then(|m| m.get("content"))
        .or_else(|| v.get("content"));

    let (texts, tools) = split_parts(content_val);
    if texts.is_empty() && tools.is_empty() {
        return None;
    }

    let tools_summary = if tools.is_empty() {
        None
    } else {
        Some(summarize_tools(&tools))
    };
    let tools_json = if tools.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&tools).unwrap_or_else(|_| "[]".into()))
    };
    let tool_name = representative_tool_name(&tools);

    let mut content = texts.join("\n\n");
    if let Some(ref summary) = tools_summary {
        if content.is_empty() {
            content = summary.clone();
        } else {
            content.push_str("\n\n");
            content.push_str(summary);
        }
    }
    if content.is_empty() {
        return None;
    }

    let (timestamp, date, time) = synthesize_timestamp(v, mtime_secs, line_index);

    Some(ParsedLine {
        session_id: session_id.to_string(),
        timestamp,
        date,
        time,
        entry_type: entry_type.to_string(),
        tool_name,
        content,
        cwd: cwd.map(|s| s.to_string()),
        tools_json,
        tools_summary,
    })
}

fn split_parts(content: Option<&Value>) -> (Vec<String>, Vec<Value>) {
    let mut texts = Vec::new();
    let mut tools = Vec::new();
    let Some(content) = content else {
        return (texts, tools);
    };

    match content {
        Value::String(s) => {
            if !s.is_empty() {
                texts.push(s.clone());
            }
        }
        Value::Array(parts) => {
            for part in parts {
                let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ptype {
                    "text" => {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                texts.push(t.to_string());
                            }
                        }
                    }
                    "tool_use" => tools.push(part.clone()),
                    // Keep other structured blobs out of presentation content;
                    // store only tool_use in tool_trace for v1.
                    _ => {}
                }
            }
        }
        other => {
            // Unexpected shape — keep a compact JSON preview as text.
            texts.push(other.to_string());
        }
    }
    (texts, tools)
}

fn summarize_tools(tools: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push("[tools]".to_string());
    let shown = tools.len().min(TOOLS_SUMMARY_MAX_LINES);
    for tool in tools.iter().take(shown) {
        let name = tool
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        let input = tool.get("input").cloned().unwrap_or(json!({}));
        let preview = summarize_tool_input(name, &input);
        if preview.is_empty() {
            lines.push(format!("- {name}"));
        } else {
            lines.push(format!("- {name}: {preview}"));
        }
    }
    if tools.len() > shown {
        lines.push(format!("- …and {} more", tools.len() - shown));
    }
    lines.join("\n")
}

fn summarize_tool_input(name: &str, input: &Value) -> String {
    let pick = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(v) = input.get(*k) {
                let s = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if !s.is_empty() {
                    return Some(truncate_chars(&s, ARG_PREVIEW_CHARS));
                }
            }
        }
        None
    };

    match name {
        "Shell" | "shell" => pick(&["command"]).unwrap_or_default(),
        "Read" | "read_file" => pick(&["path", "target_file", "file_path"]).unwrap_or_default(),
        "Write" | "write" | "search_replace" | "StrReplace" => {
            pick(&["path", "file_path", "target_file"]).unwrap_or_default()
        }
        "Glob" => pick(&["glob_pattern", "pattern", "target_directory"]).unwrap_or_default(),
        "Grep" => {
            let pattern = pick(&["pattern"]).unwrap_or_default();
            let path = pick(&["path"]).unwrap_or_default();
            if path.is_empty() {
                pattern
            } else {
                format!("{pattern} in {path}")
            }
        }
        "CallDynamicTool" => {
            let ns = pick(&["namespace"]).unwrap_or_default();
            let tool = pick(&["toolName", "tool_name"]).unwrap_or_default();
            if ns.is_empty() {
                tool
            } else if tool.is_empty() {
                ns
            } else {
                format!("{ns}/{tool}")
            }
        }
        "GetDynamicTools" => pick(&["namespace", "pattern", "toolName"]).unwrap_or_default(),
        _ => {
            // Generic: first short string field, else empty (no giant JSON in summary).
            if let Some(obj) = input.as_object() {
                for (k, v) in obj {
                    if let Value::String(s) = v {
                        if s.len() <= 200 {
                            return format!("{k}={}", truncate_chars(s, ARG_PREVIEW_CHARS));
                        }
                    }
                }
            }
            String::new()
        }
    }
}

fn representative_tool_name(tools: &[Value]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    if names.is_empty() {
        return None;
    }
    if names.len() == 1 {
        return Some(names[0].to_string());
    }
    // Compact: first + count
    Some(format!("{}+{}", names[0], names.len() - 1))
}

fn synthesize_timestamp(v: &Value, mtime_secs: i64, line_index: u64) -> (String, String, String) {
    if let Some(ts) = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
    {
        let (date, time) = split_date_time(ts);
        return (ts.to_string(), date, time);
    }
    // Monotonic proxy: file mtime + line index milliseconds.
    let base = if mtime_secs > 0 { mtime_secs as u64 } else { 0 };
    let secs = base.saturating_add(line_index.saturating_sub(1) / 1000);
    let millis = ((line_index.saturating_sub(1)) % 1000) as u32;
    let dt = UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis as u64);
    let datetime: chrono::DateTime<chrono::Utc> = dt.into();
    let ts = datetime.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let date = datetime.format("%Y-%m-%d").to_string();
    let time = datetime.format("%H:%M:%S").to_string();
    (ts, date, time)
}

fn split_date_time(ts: &str) -> (String, String) {
    // Accept RFC3339-ish or "YYYY-MM-DDTHH:MM:SS"
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return (
            dt.format("%Y-%m-%d").to_string(),
            dt.format("%H:%M:%S").to_string(),
        );
    }
    if ts.len() >= 19 {
        return (ts[0..10].to_string(), ts[11..19].to_string());
    }
    ("1970-01-01".into(), "00:00:00".into())
}

/// Best-effort cwd from Cursor project folder id.
/// Cursor encodes absolute paths by stripping the leading `/` and replacing `/` with `-`
/// (and sometimes `_` with `-`), so reverse mapping is lossy — only apply for clear prefixes
/// and only when the reconstructed path exists.
fn guess_cwd_from_project_id(project_id: &str) -> Option<String> {
    if project_id.is_empty() || project_id.starts_with("var-folders-") || project_id == "empty-window"
    {
        return None;
    }
    if !(project_id.starts_with("Users-") || project_id.starts_with("Volumes-")) {
        return None;
    }
    let candidate = PathBuf::from(format!("/{}", project_id.replace('-', "/")));
    if candidate.exists() {
        return Some(candidate.to_string_lossy().to_string());
    }
    None
}

fn estimate_prior_lines(path: &Path, offset: i64) -> u64 {
    let Ok(file) = File::open(path) else {
        return 0;
    };
    let mut reader = BufReader::new(file);
    let mut buf = String::new();
    let mut read = 0i64;
    let mut lines = 0u64;
    while read < offset {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                read += n as i64;
                lines += 1;
            }
            Err(_) => break,
        }
    }
    lines
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summarize_shell_and_read() {
        let tools = vec![
            json!({"type":"tool_use","name":"Shell","input":{"command":"cargo test --locked"}}),
            json!({"type":"tool_use","name":"Read","input":{"path":"src/ingest.rs"}}),
        ];
        let s = summarize_tools(&tools);
        assert!(s.contains("[tools]"));
        assert!(s.contains("Shell: cargo test --locked"));
        assert!(s.contains("Read: src/ingest.rs"));
        assert!(!s.contains("tool_use"));
    }

    #[test]
    fn parse_assistant_separates_tools() {
        let v = json!({
            "role": "assistant",
            "message": {
                "content": [
                    {"type":"text","text":"Looking into ingest."},
                    {"type":"tool_use","name":"Grep","input":{"pattern":"tool_use","path":"src"}},
                    {"type":"tool_use","name":"Shell","input":{"command":"echo HUGE_PAYLOAD_SHOULD_NOT_APPEAR_IN_SUMMARY && cat /etc/passwd"}}
                ]
            }
        });
        let parsed = parse_line(&v, "sess-1", 1_700_000_000, 1, Some("/tmp/ws")).unwrap();
        assert_eq!(parsed.entry_type, "assistant");
        assert!(parsed.content.contains("Looking into ingest."));
        assert!(parsed.content.contains("[tools]"));
        assert!(parsed.content.contains("Grep: tool_use in src"));
        assert!(parsed.content.contains("Shell:"));
        // Presentation stays short; full command lives in tools_json.
        assert!(parsed.content.len() < 2000);
        let tools_json = parsed.tools_json.unwrap();
        assert!(tools_json.contains("Grep"));
        assert!(tools_json.contains("echo HUGE_PAYLOAD_SHOULD_NOT_APPEAR_IN_SUMMARY"));
    }

    #[test]
    fn parse_user_text_only() {
        let v = json!({
            "role": "user",
            "message": {"content": [{"type":"text","text":"CURSOR_FIXTURE_PHRASE_ABC"}]}
        });
        let parsed = parse_line(&v, "sess-1", 1_700_000_000, 2, None).unwrap();
        assert_eq!(parsed.entry_type, "user");
        assert_eq!(parsed.content, "CURSOR_FIXTURE_PHRASE_ABC");
        assert!(parsed.tools_json.is_none());
    }
}
