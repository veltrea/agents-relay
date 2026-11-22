use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use crate::db;

const SOURCE: &str = "antigravity";

/// Antigravity のメモリ全体を取り込み
/// - agyhub_summaries_proto.pb: 全会話のタイトル・workspace・タイムスタンプ（平文 protobuf）
/// - brain/<session-uuid>/: アーティファクト (.md + .metadata.json + .resolved)
/// - brain/<session-uuid>/.system_generated/logs/transcript.jsonl: 実トランスクリプト
/// - knowledge/<topic>/: 蒸留済みナレッジ (metadata.json + artifacts/**/*.md)
///
/// なお conversations/*.pb と implicit/*.pb は暗号化されており取り込み不可（確認済み）。
pub fn sync_all(conn: &mut Connection) -> Result<u64> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let ag_dir = home.join(".gemini").join("antigravity");
    if !ag_dir.exists() {
        return Ok(0);
    }

    let mut total = 0u64;

    // 会話UUID → workspace パスのマップ。brain/knowledge の cwd 解決にも使う
    let summaries = load_agyhub_summaries(&ag_dir);
    let workspaces: HashMap<String, String> = summaries
        .iter()
        .filter_map(|s| s.workspace.clone().map(|w| (s.uuid.clone(), w)))
        .collect();

    total += ingest_summaries(conn, &ag_dir, &summaries)?;

    let brain_dir = ag_dir.join("brain");
    if brain_dir.exists() {
        total += ingest_brain(conn, &brain_dir, &workspaces)?;
    }

    let knowledge_dir = ag_dir.join("knowledge");
    if knowledge_dir.exists() {
        total += ingest_knowledge(conn, &knowledge_dir, &workspaces)?;
    }

    Ok(total)
}

// ---------------------------------------------------------------------------
// agyhub_summaries_proto.pb: 会話タイトル・workspace・タイムスタンプ
// ---------------------------------------------------------------------------

struct ConvSummary {
    uuid: String,
    title: String,
    workspace: Option<String>,
    /// 最終更新 (unix秒)
    updated: Option<i64>,
}

fn load_agyhub_summaries(ag_dir: &Path) -> Vec<ConvSummary> {
    let path = ag_dir.join("agyhub_summaries_proto.pb");
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    parse_agyhub(&data)
}

/// 全会話のサマリーエントリを取り込み（brain が空のセッションも検索可能にする）
fn ingest_summaries(conn: &Connection, ag_dir: &Path, summaries: &[ConvSummary]) -> Result<u64> {
    if summaries.is_empty() {
        return Ok(0);
    }
    let path = ag_dir.join("agyhub_summaries_proto.pb");
    let path_str = path.to_string_lossy().to_string();

    // サイズ + mtime の両方が一致すればスキップ
    let size = file_size(&path);
    let mtime = mtime_secs(&path);
    let prev_size = db::get_sync_offset(conn, &path_str)?;
    let prev_mtime = db::get_sync_offset(conn, &format!("{path_str}#mtime"))?;
    if size == prev_size && mtime == prev_mtime && prev_size > 0 {
        return Ok(0);
    }

    let mut count = 0u64;
    for s in summaries {
        if s.title.is_empty() {
            continue;
        }
        let ts = s.updated.map(unix_to_iso).unwrap_or_default();
        if ts.is_empty() {
            continue;
        }
        let (date, time) = parse_datetime(&ts);
        let content = match &s.workspace {
            Some(w) => format!("[conversation] {} (workspace: {})", s.title, w),
            None => format!("[conversation] {}", s.title),
        };

        delete_marked(conn, &s.uuid, "ag:conv-summary")?;
        db::insert_entry(
            conn,
            &s.uuid,
            &ts,
            &date,
            &time,
            "assistant",
            Some("ag:conv-summary"),
            &content,
            s.workspace.as_deref(),
            None,
            SOURCE,
        )?;
        count += 1;
    }

    db::set_sync_offset(conn, &path_str, size)?;
    db::set_sync_offset(conn, &format!("{path_str}#mtime"), mtime)?;
    if count > 0 {
        eprintln!("  [antigravity] conversation summaries (+{count} entries)");
    }
    Ok(count)
}

// --- 最小 protobuf wire format パーサ（依存を増やさないため手書き） ---

enum PbField<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Skipped,
}

struct PbReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> PbReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            if shift >= 64 {
                return None;
            }
            result |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(result);
            }
            shift += 7;
        }
    }

    fn next_field(&mut self) -> Option<(u32, PbField<'a>)> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let tag = self.varint()?;
        let field = (tag >> 3) as u32;
        match tag & 7 {
            0 => Some((field, PbField::Varint(self.varint()?))),
            1 => {
                self.pos = self.pos.checked_add(8)?;
                if self.pos > self.buf.len() {
                    return None;
                }
                Some((field, PbField::Skipped))
            }
            2 => {
                let len = self.varint()? as usize;
                let end = self.pos.checked_add(len)?;
                if end > self.buf.len() {
                    return None;
                }
                let bytes = &self.buf[self.pos..end];
                self.pos = end;
                Some((field, PbField::Bytes(bytes)))
            }
            5 => {
                self.pos = self.pos.checked_add(4)?;
                if self.pos > self.buf.len() {
                    return None;
                }
                Some((field, PbField::Skipped))
            }
            _ => None, // 未知の wire type: 以降は解析不能
        }
    }
}

/// 構造 (実データからのリバースエンジニアリング):
/// file = repeated record(f1)
/// record { f1: 会話UUID, f2: detail }
/// detail { f1: タイトル, f3: Timestamp(更新), f7: Timestamp(作成), f9: { f1: workspace URI } }
/// Timestamp { f1: seconds, f2: nanos }
fn parse_agyhub(data: &[u8]) -> Vec<ConvSummary> {
    let mut out = Vec::new();
    let mut r = PbReader::new(data);
    while let Some((field, val)) = r.next_field() {
        if field == 1 {
            if let PbField::Bytes(rec) = val {
                if let Some(cs) = parse_agyhub_record(rec) {
                    out.push(cs);
                }
            }
        }
    }
    out
}

fn parse_agyhub_record(rec: &[u8]) -> Option<ConvSummary> {
    let mut uuid = None;
    let mut title = String::new();
    let mut workspace = None;
    let mut updated = None;

    let mut r = PbReader::new(rec);
    while let Some((field, val)) = r.next_field() {
        match (field, val) {
            (1, PbField::Bytes(b)) => {
                uuid = std::str::from_utf8(b).ok().map(String::from);
            }
            (2, PbField::Bytes(detail)) => {
                let mut d = PbReader::new(detail);
                while let Some((f, v)) = d.next_field() {
                    match (f, v) {
                        (1, PbField::Bytes(b)) => {
                            if let Ok(s) = std::str::from_utf8(b) {
                                title = s.to_string();
                            }
                        }
                        (3, PbField::Bytes(ts)) => {
                            updated = parse_pb_timestamp(ts);
                        }
                        (9, PbField::Bytes(ws)) => {
                            workspace = parse_pb_workspace(ws);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    uuid.map(|uuid| ConvSummary {
        uuid,
        title,
        workspace,
        updated,
    })
}

fn parse_pb_timestamp(buf: &[u8]) -> Option<i64> {
    let mut r = PbReader::new(buf);
    while let Some((field, val)) = r.next_field() {
        if field == 1 {
            if let PbField::Varint(v) = val {
                return i64::try_from(v).ok();
            }
        }
    }
    None
}

fn parse_pb_workspace(buf: &[u8]) -> Option<String> {
    let mut r = PbReader::new(buf);
    while let Some((field, val)) = r.next_field() {
        if field == 1 {
            if let PbField::Bytes(b) = val {
                let uri = std::str::from_utf8(b).ok()?;
                return Some(file_uri_to_path(uri));
            }
        }
    }
    None
}

/// "file:///Volumes/..." → パーセントデコード済みのローカルパス
fn file_uri_to_path(uri: &str) -> String {
    let s = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(s)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = [bytes[i + 1], bytes[i + 2]];
            if let Some(v) = std::str::from_utf8(&hex)
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unix_to_iso(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// brain/: アーティファクト + トランスクリプト
// ---------------------------------------------------------------------------

/// brain ディレクトリ配下の全セッションを取り込み
fn ingest_brain(
    conn: &mut Connection,
    brain_dir: &Path,
    workspaces: &HashMap<String, String>,
) -> Result<u64> {
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

        let mut count = ingest_session(conn, &path, &session_id, workspaces)?;
        count += ingest_transcript(conn, &path, &session_id, workspaces)?;
        if count > 0 {
            eprintln!("  [antigravity] {} (+{} entries)", session_id, count);
        }
        total += count;
    }

    Ok(total)
}

/// 1セッション分のアーティファクト (.md + .metadata.json) を取り込み
fn ingest_session(
    conn: &Connection,
    session_dir: &Path,
    session_id: &str,
    workspaces: &HashMap<String, String>,
) -> Result<u64> {
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

        // 変更検知: サイズ合計 + mtime 合計の両方が一致すればスキップ
        // (Antigravity は append 型でなくファイル置換型のため、同サイズ置換を mtime で拾う)
        let path_str = path.to_string_lossy().to_string();
        let combined_size = file_size(&path) + file_size(&md_path);
        let combined_mtime = mtime_secs(&path) + mtime_secs(&md_path);
        let prev_size = db::get_sync_offset(conn, &path_str)?;
        let prev_mtime = db::get_sync_offset(conn, &format!("{path_str}#mtime"))?;

        if combined_size == prev_size && combined_mtime == prev_mtime && prev_size > 0 {
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

        // コンテンツ: サマリー + アーティファクト内容（全文。上限超過分は db 層が fdb に退避）
        let content = if !summary.is_empty() && !md_content.is_empty() {
            format!("[{}] {}\n\n{}", artifact_type, summary, md_content)
        } else if !summary.is_empty() {
            format!("[{}] {}", artifact_type, summary)
        } else {
            md_content.clone()
        };

        if content.is_empty() {
            continue;
        }

        // cwd: agyhub の正確な workspace を優先し、なければ .md 内の file:// パスから推定
        let cwd = workspaces
            .get(session_id)
            .cloned()
            .or_else(|| extract_workspace_from_content(&md_content));

        // 既存エントリがあれば削除して再挿入（更新対応）
        let entry_marker = format!("ag:{}:{}", session_id, md_name);
        delete_marked(conn, session_id, &entry_marker)?;

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
                delete_marked(conn, session_id, &resolved_entry_marker)?;

                let resolved_cwd = extract_workspace_from_content(&resolved_content);
                db::insert_entry(
                    conn,
                    session_id,
                    updated_at,
                    &date,
                    &time,
                    "user", // resolved は元のタスク指示 = user 扱い
                    Some(&resolved_entry_marker),
                    &resolved_content,
                    resolved_cwd.as_deref().or(cwd.as_deref()),
                    None,
                    SOURCE,
                )?;
                count += 1;
            }
        }

        db::set_sync_offset(conn, &path_str, combined_size)?;
        db::set_sync_offset(conn, &format!("{path_str}#mtime"), combined_mtime)?;
    }

    Ok(count)
}

// ---------------------------------------------------------------------------
// .system_generated/logs/transcript.jsonl: 実トランスクリプト
// ---------------------------------------------------------------------------

/// トランスクリプト (JSONL・追記型) を offset ベースで増分取り込み
fn ingest_transcript(
    conn: &mut Connection,
    session_dir: &Path,
    session_id: &str,
    workspaces: &HashMap<String, String>,
) -> Result<u64> {
    let path = session_dir
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    if !path.exists() {
        return Ok(0);
    }

    let path_str = path.to_string_lossy().to_string();
    let mut offset = db::get_sync_offset(conn, &path_str)?;

    let file = File::open(&path)?;
    let file_len = file.metadata()?.len() as i64;

    // ファイルが縮んでいたら書き直されたとみなし、既存分を消して読み直す
    if file_len < offset {
        delete_marked_prefix(conn, session_id, "ag:step:")?;
        offset = 0;
    }
    if file_len <= offset {
        return Ok(0);
    }

    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(offset as u64))?;

    let cwd = workspaces.get(session_id).cloned();
    let mut count = 0u64;
    let mut current_offset = offset;
    let mut line_buf = String::new();
    let tx = conn.transaction()?;

    loop {
        line_buf.clear();
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
            Err(_) => continue,
        };

        if let Some((entry_type, content)) = transcript_entry(&v) {
            let timestamp = v
                .get("created_at")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if timestamp.is_empty() {
                continue;
            }
            let (date, time) = parse_datetime(timestamp);
            let step = v.get("step_index").and_then(|s| s.as_i64()).unwrap_or(-1);
            let marker = format!("ag:step:{step}");

            db::insert_entry(
                &tx,
                session_id,
                timestamp,
                &date,
                &time,
                entry_type,
                Some(&marker),
                &content,
                cwd.as_deref(),
                None,
                SOURCE,
            )?;
            count += 1;
        }
    }

    db::set_sync_offset(&tx, &path_str, current_offset)?;
    tx.commit()?;

    Ok(count)
}

/// transcript の1行を (エントリ種別, コンテンツ) に変換。取り込まない行は None。
fn transcript_entry(v: &Value) -> Option<(&'static str, String)> {
    let step_type = v.get("type").and_then(|t| t.as_str())?;
    let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");

    match step_type {
        "USER_INPUT" => {
            let text = extract_user_request(content);
            if text.is_empty() {
                None
            } else {
                Some(("user", text))
            }
        }
        "PLANNER_RESPONSE" => {
            let mut parts = Vec::new();
            if !content.is_empty() {
                parts.push(content.to_string());
            }
            if let Some(Value::Array(calls)) = v.get("tool_calls") {
                for call in calls {
                    let name = call
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");
                    let args = call
                        .get("args")
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    parts.push(format!("[Tool: {}] {}", name, truncate(&args, 4_000)));
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(("assistant", parts.join("\n")))
            }
        }
        // 内部制御行 (履歴再構築マーカー) は保存しない
        "CONVERSATION_HISTORY" => None,
        "ERROR_MESSAGE" => {
            let err = v.get("error").and_then(|e| e.as_str()).unwrap_or(content);
            if err.is_empty() {
                None
            } else {
                Some(("system", format!("[ERROR] {err}")))
            }
        }
        // ツール実行ステップ (MCP_TOOL / VIEW_FILE / LIST_DIRECTORY / CODE_ACTION 等)
        _ => {
            if content.is_empty() {
                None
            } else {
                Some(("system", format!("[{step_type}] {content}")))
            }
        }
    }
}

/// USER_INPUT の content から <USER_REQUEST>...</USER_REQUEST> の中身を抽出。
/// ラッパーがなければ全体を返す。
fn extract_user_request(content: &str) -> String {
    if let Some(start) = content.find("<USER_REQUEST>") {
        let rest = &content[start + "<USER_REQUEST>".len()..];
        let end = rest.find("</USER_REQUEST>").unwrap_or(rest.len());
        rest[..end].trim().to_string()
    } else {
        content.trim().to_string()
    }
}

// ---------------------------------------------------------------------------
// knowledge/: 蒸留済みナレッジ
// ---------------------------------------------------------------------------

/// knowledge/<topic>/ (metadata.json + artifacts/**/*.md) を取り込み
fn ingest_knowledge(
    conn: &Connection,
    knowledge_dir: &Path,
    workspaces: &HashMap<String, String>,
) -> Result<u64> {
    let mut total = 0u64;

    for entry in std::fs::read_dir(knowledge_dir)? {
        let entry = entry?;
        let topic_dir = entry.path();
        if !topic_dir.is_dir() {
            continue;
        }
        let topic = topic_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let meta_path = topic_dir.join("metadata.json");
        if !meta_path.exists() {
            continue;
        }

        // 変更検知: トピック配下の全ファイルのサイズ合計 + mtime 合計
        let (size_sum, mtime_sum) = dir_stamp(&topic_dir);
        let key = topic_dir.to_string_lossy().to_string();
        let prev_size = db::get_sync_offset(conn, &key)?;
        let prev_mtime = db::get_sync_offset(conn, &format!("{key}#mtime"))?;
        if size_sum == prev_size && mtime_sum == prev_mtime && prev_size > 0 {
            continue;
        }

        let session_id = format!("ag-knowledge:{topic}");
        let count = ingest_knowledge_topic(conn, &topic_dir, &session_id, workspaces)?;
        if count > 0 {
            eprintln!("  [antigravity] knowledge:{} (+{} entries)", topic, count);
        }
        total += count;

        db::set_sync_offset(conn, &key, size_sum)?;
        db::set_sync_offset(conn, &format!("{key}#mtime"), mtime_sum)?;
    }

    Ok(total)
}

fn ingest_knowledge_topic(
    conn: &Connection,
    topic_dir: &Path,
    session_id: &str,
    workspaces: &HashMap<String, String>,
) -> Result<u64> {
    let mut count = 0u64;

    // タイムスタンプ: timestamps.json の modified を使う
    let timestamp = std::fs::read_to_string(topic_dir.join("timestamps.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("modified")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| unix_to_iso(mtime_secs(&topic_dir.join("metadata.json"))));
    let (date, time) = parse_datetime(&timestamp);

    // metadata.json: タイトル・要約・参照会話
    let meta: Value =
        serde_json::from_str(&std::fs::read_to_string(topic_dir.join("metadata.json"))?)?;
    let title = meta.get("title").and_then(|t| t.as_str()).unwrap_or("");
    let summary = meta.get("summary").and_then(|s| s.as_str()).unwrap_or("");

    // cwd: 参照している会話の workspace から解決
    let cwd = meta
        .get("references")
        .and_then(|r| r.as_array())
        .and_then(|refs| {
            refs.iter().find_map(|r| {
                let id = r.get("value").and_then(|v| v.as_str())?;
                workspaces.get(id).cloned()
            })
        });

    if !title.is_empty() || !summary.is_empty() {
        let content = format!("[knowledge] {}\n\n{}", title, summary);
        delete_marked(conn, session_id, "ag:knowledge:metadata.json")?;
        db::insert_entry(
            conn,
            session_id,
            &timestamp,
            &date,
            &time,
            "assistant",
            Some("ag:knowledge:metadata.json"),
            content.trim(),
            cwd.as_deref(),
            None,
            SOURCE,
        )?;
        count += 1;
    }

    // artifacts/**/*.md
    let artifacts_dir = topic_dir.join("artifacts");
    let mut md_files = Vec::new();
    collect_md_files(&artifacts_dir, &mut md_files);
    for md_path in md_files {
        let content = std::fs::read_to_string(&md_path).unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        let rel = md_path
            .strip_prefix(topic_dir)
            .unwrap_or(&md_path)
            .to_string_lossy()
            .to_string();
        let marker = format!("ag:knowledge:{rel}");
        delete_marked(conn, session_id, &marker)?;
        db::insert_entry(
            conn,
            session_id,
            &timestamp,
            &date,
            &time,
            "assistant",
            Some(&marker),
            &content,
            cwd.as_deref(),
            None,
            SOURCE,
        )?;
        count += 1;
    }

    Ok(count)
}

fn collect_md_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// 共通ヘルパー
// ---------------------------------------------------------------------------

/// マーカー (tool_name) 付きエントリを FTS も含めて削除する。
/// raw_entries だけ消すと FTS に幽霊行が残り、rowid 再利用で誤ヒットするため必ずこれを使う。
fn delete_marked(conn: &Connection, session_id: &str, marker: &str) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id FROM raw_entries WHERE session_id = ?1 AND tool_name = ?2 AND source = 'antigravity'",
    )?;
    let ids: Vec<i64> = stmt
        .query_map(rusqlite::params![session_id, marker], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for id in ids {
        delete_entry_everywhere(conn, id)?;
    }
    Ok(())
}

/// 1 エントリを raw_entries・FTS・分離 DB (cot / fulltext / tool_trace / compact) から完全に削除する
fn delete_entry_everywhere(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM raw_entries_fts WHERE rowid = ?1",
        rusqlite::params![id],
    )?;
    conn.execute("DELETE FROM rdb.cot WHERE entry_id = ?1", rusqlite::params![id])?;
    conn.execute("DELETE FROM fdb.fulltext WHERE entry_id = ?1", rusqlite::params![id])?;
    conn.execute("DELETE FROM fdb.tool_trace WHERE entry_id = ?1", rusqlite::params![id])?;
    conn.execute("DELETE FROM cdb.entry_compact WHERE entry_id = ?1", rusqlite::params![id])?;
    conn.execute("DELETE FROM raw_entries WHERE id = ?1", rusqlite::params![id])?;
    Ok(())
}

/// マーカーの前方一致で削除（transcript の書き直し検知時に使用）
fn delete_marked_prefix(conn: &Connection, session_id: &str, prefix: &str) -> Result<()> {
    let pattern = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
    let mut stmt = conn.prepare(
        "SELECT id FROM raw_entries WHERE session_id = ?1 AND tool_name LIKE ?2 ESCAPE '\\' AND source = 'antigravity'",
    )?;
    let ids: Vec<i64> = stmt
        .query_map(rusqlite::params![session_id, pattern], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    for id in ids {
        delete_entry_everywhere(conn, id)?;
    }
    Ok(())
}

fn file_size(path: &Path) -> i64 {
    path.metadata().map(|m| m.len() as i64).unwrap_or(0)
}

fn mtime_secs(path: &Path) -> i64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// ディレクトリ配下全ファイルの (サイズ合計, mtime合計) — 変更検知用
fn dir_stamp(dir: &Path) -> (i64, i64) {
    let mut size = 0i64;
    let mut mtime = 0i64;
    fn walk(dir: &Path, size: &mut i64, mtime: &mut i64) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, size, mtime);
            } else {
                *size += file_size(&path);
                *mtime += mtime_secs(&path);
            }
        }
    }
    walk(dir, &mut size, &mut mtime);
    (size, mtime)
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
