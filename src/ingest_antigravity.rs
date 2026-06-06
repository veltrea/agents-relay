use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

use crate::db;

const SOURCE: &str = "antigravity";

/// Antigravity のメモリ全体を取り込み
/// ~/.gemini/antigravity/brain/<session-uuid>/ 配下を走査
pub fn sync_all(conn: &mut Connection) -> Result<u64> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let brain_dir = home
        .join(".gemini")
        .join("antigravity")
        .join("brain");
    if !brain_dir.exists() {
        return Ok(0);
    }
    ingest_brain(conn, &brain_dir)
}

/// brain ディレクトリ配下の全セッションを取り込み
fn ingest_brain(conn: &mut Connection, brain_dir: &Path) -> Result<u64> {
    let mut total = 0u64;

    for entry in std::fs::read_dir(brain_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        // ディレクトリ名がセッションUUID
        let session_id = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // tempmediaStorage 等は無視
        if !looks_like_uuid(&session_id) {
            continue;
        }

        let count = ingest_session(conn, &path, &session_id)?;
        if count > 0 {
            eprintln!("  [antigravity] {} (+{} entries)", session_id, count);
        }
        total += count;
    }

    Ok(total)
}

/// 1セッション分を取り込み
fn ingest_session(conn: &mut Connection, session_dir: &Path, session_id: &str) -> Result<u64> {
    let mut count = 0u64;

    // このセッション内の .metadata.json ファイルを走査
    for entry in std::fs::read_dir(session_dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");

        if !name.ends_with(".metadata.json") {
            continue;
        }

        // 対応する .md ファイル名を取得 (e.g. "task.md.metadata.json" → "task.md")
        let md_name = name.trim_end_matches(".metadata.json");
        let md_path = session_dir.join(md_name);

        // sync_state チェック: metadata.json のパスで管理
        let path_str = path.to_string_lossy().to_string();
        let last_offset = db::get_sync_offset(conn, &path_str)?;

        // ファイルサイズで変更検知（Antigravityはappend型でなくファイル置換型）
        let meta_size = path.metadata().map(|m| m.len() as i64).unwrap_or(0);
        let md_size = md_path.metadata().map(|m| m.len() as i64).unwrap_or(0);
        let combined_size = meta_size + md_size;

        if combined_size <= last_offset && last_offset > 0 {
            continue; // 変更なし
        }

        // metadata.json を読む
        let meta_content = std::fs::read_to_string(&path)?;
        let meta: Value = match serde_json::from_str(&meta_content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let updated_at = meta
            .get("updatedAt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let summary = meta
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let artifact_type = meta
            .get("artifactType")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        if updated_at.is_empty() {
            continue;
        }

        let (date, time) = parse_datetime(updated_at);

        // .md ファイルの内容を読む
        let md_content = if md_path.exists() {
            std::fs::read_to_string(&md_path).unwrap_or_default()
        } else {
            String::new()
        };

        // コンテンツ: サマリー + アーティファクト内容
        let content = if !summary.is_empty() && !md_content.is_empty() {
            format!("[{}] {}\n\n{}", artifact_type, summary, truncate(&md_content, 48_000))
        } else if !summary.is_empty() {
            format!("[{}] {}", artifact_type, summary)
        } else {
            truncate(&md_content, 48_000)
        };

        if content.is_empty() {
            continue;
        }

        // cwdを推定: .md内のfile://パスから
        let cwd = extract_workspace_from_content(&md_content);

        // 既存エントリがあれば削除して再挿入（更新対応）
        let entry_marker = format!("ag:{}:{}", session_id, md_name);
        conn.execute(
            "DELETE FROM raw_entries WHERE session_id = ?1 AND tool_name = ?2 AND source = 'antigravity'",
            rusqlite::params![session_id, entry_marker],
        )?;
        // FTS も同期削除（rowid ベースなので raw_entries 側で消えれば問題ない）

        db::insert_entry(
            conn,
            session_id,
            updated_at,
            &date,
            &time,
            "assistant", // Antigravityのアウトプットは assistant 扱い
            Some(&entry_marker),
            &content,
            cwd.as_deref(),
            None,
            SOURCE,
        )?;
        count += 1;

        // 最新の .resolved ファイルも取り込み（バージョン履歴の最新のみ）
        let latest_resolved = find_latest_resolved(session_dir, md_name);
        if let Some(resolved_path) = latest_resolved {
            let resolved_content = std::fs::read_to_string(&resolved_path).unwrap_or_default();
            if !resolved_content.is_empty() && resolved_content != md_content {
                let resolved_entry_marker = format!("ag:{}:{}.latest", session_id, md_name);
                conn.execute(
                    "DELETE FROM raw_entries WHERE session_id = ?1 AND tool_name = ?2 AND source = 'antigravity'",
                    rusqlite::params![session_id, resolved_entry_marker],
                )?;

                let resolved_cwd = extract_workspace_from_content(&resolved_content);
                db::insert_entry(
                    conn,
                    session_id,
                    updated_at,
                    &date,
                    &time,
                    "user", // resolved は元のタスク指示 = user 扱い
                    Some(&resolved_entry_marker),
                    &truncate(&resolved_content, 48_000),
                    resolved_cwd.as_deref().or(cwd.as_deref()),
                    None,
                    SOURCE,
                )?;
                count += 1;
            }
        }

        db::set_sync_offset(conn, &path_str, combined_size)?;
    }

    Ok(count)
}

/// .resolved.N ファイルのうち最も番号が大きいものを返す
fn find_latest_resolved(session_dir: &Path, md_name: &str) -> Option<std::path::PathBuf> {
    let prefix = format!("{}.resolved.", md_name);
    let mut max_n: i64 = -1;
    let mut max_path = None;

    // まず .resolved (番号なし) をチェック
    let base_resolved = session_dir.join(format!("{}.resolved", md_name));
    if base_resolved.exists() {
        max_path = Some(base_resolved);
    }

    if let Ok(entries) = std::fs::read_dir(session_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(suffix) = name.strip_prefix(&prefix) {
                if let Ok(n) = suffix.parse::<i64>() {
                    if n > max_n {
                        max_n = n;
                        max_path = Some(entry.path());
                    }
                }
            }
        }
    }

    max_path
}

/// コンテンツ内の file:// パスからワークスペースを推定
fn extract_workspace_from_content(content: &str) -> Option<String> {
    // file:///Volumes/... や file:///Users/... からプロジェクトルートを推定
    for line in content.lines() {
        if let Some(pos) = line.find("file:///") {
            let path_start = pos + 7; // "file://" の長さ
            let path_str: String = line[path_start..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != ')' && *c != ']')
                .collect();
            // /Volumes/XXX/dev/project/ や /Users/XXX/project/ のような構造から
            // プロジェクトルートを推定（最初の3-4階層）
            let parts: Vec<&str> = path_str.split('/').collect();
            if parts.len() >= 5 {
                // /Volumes/X/dev/project or /Users/X/project
                let depth = if parts.get(1) == Some(&"Volumes") { 5 } else { 4 };
                let root = parts[..depth.min(parts.len())].join("/");
                return Some(root);
            }
        }
    }
    None
}

fn parse_datetime(ts: &str) -> (String, String) {
    if ts.len() >= 19 {
        let date = ts[..10].to_string();
        let time = ts[11..19].to_string();
        (date, time)
    } else {
        (ts.to_string(), "00:00:00".to_string())
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}... [truncated]", &s[..end])
    }
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().filter(|c| *c == '-').count() == 4
}
