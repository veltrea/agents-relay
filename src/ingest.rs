use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

use crate::db;

// content / reasoning の切り詰めポリシーは db::insert_entry_full に集約されている。
// 上限超の content は先頭だけがメイン DB に inline 保存され、全文は fdb.fulltext に
// 無損失で退避される（バックアップ・将来の再圧縮のための原本）。

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
            db::insert_entry_full(
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
                n.reasoning.as_deref(),
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
    /// 生 CoT（assistant の thinking ブロック）。content とは別フィールドに退避する。
    reasoning: Option<String>,
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

    let (content, tool_name, reasoning) = extract_content(v, entry_type);

    // reasoning (生 CoT) は無損失で保存する（切り詰めは行わない）
    let reasoning = reasoning.and_then(|r| {
        let r = r.trim().to_string();
        if r.is_empty() { None } else { Some(r) }
    });

    // 小さな base64 画像のみのエントリはスキップ（テキスト記憶として価値がないため）。
    // 上限超の content は全文のまま db 層に渡す（inline 頭出し + fdb 全文退避）。
    if content.contains("data:image/") && content.len() <= db::CONTENT_INLINE_MAX {
        return None;
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
        reasoning,
    })
}

/// 戻り値: (content=最終応答, tool_name, reasoning=生 CoT)。
/// CoT は content から分離し、別フィールドへ退避する。
fn extract_content(v: &Value, entry_type: &str) -> (String, Option<String>, Option<String>) {
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
            (content, None, None)
        }
        "assistant" => {
            let message = v.get("message");
            if let Some(msg) = message {
                let content_arr = msg.get("content");
                if let Some(Value::Array(arr)) = content_arr {
                    let mut texts = Vec::new();
                    let mut thoughts = Vec::new();
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
                                // thinking は content には混ぜず reasoning として退避する
                                if let Some(t) = item.get("thinking").and_then(|t| t.as_str()) {
                                    thoughts.push(t.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    let reasoning = if thoughts.is_empty() {
                        None
                    } else {
                        Some(thoughts.join("\n"))
                    };
                    return (texts.join("\n"), tool_name, reasoning);
                }
            }
            // slug ベースのエントリ（ストリーミング中間データ）
            if let Some(slug) = v.get("slug").and_then(|s| s.as_str()) {
                return (format!("[streaming: {slug}]"), None, None);
            }
            (String::new(), None, None)
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
            (content, None, None)
        }
        "progress" => {
            let content = v.to_string();
            (content, None, None)
        }
        _ => {
            let content = v.to_string();
            (content, None, None)
        }
    }
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

/// 過去のエントリについて、元 JSONL から失われていたデータを回収する:
/// - 切り詰め保存された本文の全文 → fdb.fulltext
/// - 旧バイナリが捨てていた thinking (生 CoT) → rdb.cot
/// 元ファイルが Claude Code のクリーンアップで消える前に一度実行しておくことで、
/// 原本を DB 側で自己完結させる。冪等（再実行しても二重登録しない）。
/// 戻り値: (全文回収件数, 未回収の切り詰めエントリ数, CoT 回収件数)
pub fn backfill_fulltext(conn: &mut Connection) -> Result<(u64, u64, u64)> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;
    let projects_dir = home.join(".claude").join("projects");
    let mut filled = 0u64;
    let mut cot_filled = 0u64;

    if projects_dir.exists() {
        // .jsonl.bak は Claude Code がセッションを書き直した際の旧版で、
        // 現行ファイルから消えた内容が残っている場合があるため両方走査する
        let patterns = [
            format!("{}/**/*.jsonl", projects_dir.to_string_lossy()),
            format!("{}/**/*.jsonl.bak", projects_dir.to_string_lossy()),
        ];
        for pattern in &patterns {
        for entry in glob::glob(pattern)? {
            let path = entry?;
            let file_session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .trim_end_matches(".jsonl")
                .to_string();

            let file = match File::open(&path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader = BufReader::new(file);
            let tx = conn.transaction()?;

            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break, // 不正な UTF-8 等はファイルごと打ち切り
                };
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let parsed = match process_entry(&v, &file_session_id) {
                    Some(p) => p,
                    None => continue,
                };

                // --- thinking (生 CoT) の回収 ---
                // 旧バイナリは thinking を捨てて取り込んでいたため、既存行に
                // CoT が付いていなければ元 JSONL から補完する。誤マッチで別の
                // エントリに CoT を付けると害が大きいので、こちらは timestamp
                // まで含めた厳密照合とし、さらに content の先頭も突き合わせる。
                if let Some(ref reasoning) = parsed.reasoning {
                    let candidates: Vec<(i64, String)> = {
                        let mut stmt = tx.prepare(
                            "SELECT r.id, substr(r.content, 1, 200) FROM raw_entries r
                             LEFT JOIN rdb.cot c ON c.entry_id = r.id
                             WHERE r.session_id = ?1 AND r.timestamp = ?2 AND r.type = ?3
                               AND c.entry_id IS NULL",
                        )?;
                        let rows: Vec<(i64, String)> = stmt
                            .query_map(
                                rusqlite::params![
                                    parsed.session_id,
                                    parsed.timestamp,
                                    parsed.entry_type
                                ],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )?
                            .collect::<std::result::Result<_, _>>()?;
                        rows
                    };
                    for (id, stored_head) in candidates {
                        let limit = stored_head.len().min(parsed.content.len()).min(100);
                        let n = db::truncate_at_char_boundary(&parsed.content, limit);
                        let matches = n == 0 || stored_head.starts_with(&parsed.content[..n]);
                        if matches {
                            tx.execute(
                                "INSERT OR IGNORE INTO rdb.cot (entry_id, reasoning) VALUES (?1, ?2)",
                                rusqlite::params![id, reasoning],
                            )?;
                            tx.execute(
                                "UPDATE raw_entries SET reasoning_tokens = ?1
                                 WHERE id = ?2 AND reasoning_tokens IS NULL",
                                rusqlite::params![crate::reasoning::estimate_tokens(reasoning), id],
                            )?;
                            cot_filled += 1;
                        }
                    }
                }

                // --- 切り詰め本文の全文回収 ---
                if parsed.content.len() <= db::CONTENT_INLINE_MAX {
                    continue;
                }

                // 切り詰めマーカーの末尾 (元のバイト数入り) で対象行を特定する。
                // Claude Code はセッションファイルを書き直すことがあり (compact 等)、
                // 過去行のタイムスタンプが現在のファイルとずれている場合があるため、
                // timestamp は照合キーに使わない。先頭 1000 バイト + 正確なバイト数の
                // 一致で誤マッチを防ぐ。
                let suffix = format!("... [truncated, {} bytes]", parsed.content.len());
                let ids: Vec<i64> = {
                    let mut stmt = tx.prepare(
                        "SELECT r.id FROM raw_entries r
                         LEFT JOIN fdb.fulltext f ON f.entry_id = r.id
                         WHERE r.session_id = ?1 AND r.type = ?2
                           AND f.entry_id IS NULL
                           AND r.content LIKE '%' || ?3",
                    )?;
                    let rows: Vec<i64> = stmt
                        .query_map(
                            rusqlite::params![
                                parsed.session_id,
                                parsed.entry_type,
                                suffix
                            ],
                            |row| row.get(0),
                        )?
                        .collect::<std::result::Result<_, _>>()?;
                    rows
                };

                // ヘッドも一致する候補すべてに埋める（過去の再取り込みで生じた
                // 重複行も同じ原本を共有しているため、全行に同じ全文を与える）
                for id in ids {
                    let inline: String = tx.query_row(
                        "SELECT content FROM raw_entries WHERE id = ?1",
                        rusqlite::params![id],
                        |row| row.get(0),
                    )?;
                    let head = inline.strip_suffix(&suffix).unwrap_or("");
                    if !head.is_empty() && parsed.content.starts_with(head) {
                        tx.execute(
                            "INSERT OR IGNORE INTO fdb.fulltext (entry_id, content) VALUES (?1, ?2)",
                            rusqlite::params![id, parsed.content],
                        )?;
                        filled += 1;
                    }
                }
            }
            tx.commit()?;
        }
        }
    }

    // 回収できず残った切り詰めエントリ（元ファイルが既に消えている等）
    let remaining: u64 = conn.query_row(
        "SELECT COUNT(*) FROM raw_entries r
         LEFT JOIN fdb.fulltext f ON f.entry_id = r.id
         WHERE r.content LIKE '%... [truncated, % bytes]' AND f.entry_id IS NULL",
        [],
        |row| row.get(0),
    )?;

    Ok((filled, remaining, cot_filled))
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
