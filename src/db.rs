use anyhow::Result;
use rusqlite::{params, Connection};
use std::path::Path;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // 複数プロセスが同時起動した場合の BUSY 対策: 最大 5 秒待ってリトライ
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    Ok(conn)
}

pub fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS raw_entries (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            timestamp TEXT NOT NULL,
            date TEXT NOT NULL,
            time TEXT NOT NULL,
            type TEXT NOT NULL,
            tool_name TEXT,
            content TEXT NOT NULL,
            cwd TEXT,
            git_branch TEXT,
            source TEXT NOT NULL DEFAULT 'claude',
            created_at TEXT DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_raw_session ON raw_entries(session_id);
        CREATE INDEX IF NOT EXISTS idx_raw_timestamp ON raw_entries(timestamp);
        CREATE INDEX IF NOT EXISTS idx_raw_date ON raw_entries(date);
        CREATE INDEX IF NOT EXISTS idx_raw_type ON raw_entries(type);
        CREATE VIRTUAL TABLE IF NOT EXISTS raw_entries_fts USING fts5(
            content, tool_name, session_id
        );

        CREATE TABLE IF NOT EXISTS sync_state (
            file_path TEXT PRIMARY KEY,
            last_offset INTEGER DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS summaries (
            id INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL UNIQUE,
            project_path TEXT,
            git_branch TEXT,
            started_at TEXT,
            ended_at TEXT,
            summary_md TEXT NOT NULL,
            created_at TEXT DEFAULT (datetime('now'))
        );
        ",
    )?;

    // マイグレーション: 既存DBに source カラムがなければ追加
    migrate(conn)?;

    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    // source カラムの存在チェック
    let has_source: bool = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name='raw_entries'")?
        .query_row([], |row| row.get::<_, String>(0))
        .map(|sql| sql.contains("source"))
        .unwrap_or(false);

    if !has_source {
        conn.execute_batch(
            "ALTER TABLE raw_entries ADD COLUMN source TEXT NOT NULL DEFAULT 'claude';"
        )?;
        eprintln!("Migration: added 'source' column to raw_entries");
    }

    // インデックスは常に確保（IF NOT EXISTS なので安全）
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_raw_source ON raw_entries(source);
         CREATE INDEX IF NOT EXISTS idx_raw_cwd ON raw_entries(cwd);"
    )?;

    Ok(())
}

/// FTS インデックスを安全に再構築する（delete-all + rebuild）
pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        INSERT INTO raw_entries_fts(raw_entries_fts) VALUES('delete-all');
        INSERT INTO raw_entries_fts(rowid, content, tool_name, session_id)
            SELECT id, content, tool_name, session_id FROM raw_entries;
        ",
    )?;
    Ok(())
}

pub fn reset(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS raw_entries_fts;
        DROP TABLE IF EXISTS raw_entries;
        DROP TABLE IF EXISTS sync_state;
        DROP TABLE IF EXISTS summaries;
        ",
    )?;
    init(conn)?;
    Ok(())
}

#[derive(Debug)]
pub struct RawEntry {
    pub id: i64,
    pub session_id: String,
    pub timestamp: String,
    pub date: String,
    pub time: String,
    pub entry_type: String,
    pub tool_name: Option<String>,
    pub content: String,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub source: String,
}

pub fn insert_entry(
    conn: &Connection,
    session_id: &str,
    timestamp: &str,
    date: &str,
    time: &str,
    entry_type: &str,
    tool_name: Option<&str>,
    content: &str,
    cwd: Option<&str>,
    git_branch: Option<&str>,
    source: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO raw_entries (session_id, timestamp, date, time, type, tool_name, content, cwd, git_branch, source)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![session_id, timestamp, date, time, entry_type, tool_name, content, cwd, git_branch, source],
    )?;
    let id = conn.last_insert_rowid();

    // FTS に同期
    conn.execute(
        "INSERT INTO raw_entries_fts (rowid, content, tool_name, session_id)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, content, tool_name, session_id],
    )?;

    Ok(id)
}

pub fn get_sync_offset(conn: &Connection, file_path: &str) -> Result<i64> {
    let offset = conn
        .query_row(
            "SELECT last_offset FROM sync_state WHERE file_path = ?1",
            params![file_path],
            |row| row.get(0),
        )
        .unwrap_or(0i64);
    Ok(offset)
}

pub fn set_sync_offset(conn: &Connection, file_path: &str, offset: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO sync_state (file_path, last_offset) VALUES (?1, ?2)
         ON CONFLICT(file_path) DO UPDATE SET last_offset = ?2",
        params![file_path, offset],
    )?;
    Ok(())
}

pub fn stats(conn: &Connection) -> Result<String> {
    let entry_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM raw_entries", [], |row| row.get(0))?;
    let session_count: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT session_id) FROM raw_entries",
        [],
        |row| row.get(0),
    )?;
    let date_range: (Option<String>, Option<String>) = conn.query_row(
        "SELECT MIN(date), MAX(date) FROM raw_entries",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let sync_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM sync_state", [], |row| row.get(0))?;
    let summary_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))?;

    let min_date = date_range.0.unwrap_or_else(|| "(none)".to_string());
    let max_date = date_range.1.unwrap_or_else(|| "(none)".to_string());

    Ok(format!(
        "Entries:    {entry_count}\n\
         Sessions:   {session_count}\n\
         Date range: {min_date} .. {max_date}\n\
         Summaries:  {summary_count}\n\
         Tracked files: {sync_count}"
    ))
}

/// FTS5クエリを正規化する。
/// ハイフンはFTS5で否定演算子として解釈されるため、単語間のハイフンをスペースに置き換える。
/// ユーザーが明示的にクォートした部分はそのまま通す。
fn normalize_fts_query(query: &str) -> String {
    let mut result = String::with_capacity(query.len());
    let mut in_quote = false;
    let chars: Vec<char> = query.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '"' => {
                in_quote = !in_quote;
                result.push(c);
            }
            '-' if !in_quote => {
                // 単語の先頭の - (NOT演算子) は残す。単語間は空白に変換する
                let prev_is_word = i > 0 && chars[i - 1].is_alphanumeric();
                if prev_is_word {
                    result.push(' ');
                } else {
                    result.push(c);
                }
            }
            _ => result.push(c),
        }
    }
    result
}

/// FTS 検索（クエリが空や "*" の場合は FTS を使わず直接検索）
/// workspace: Some("path") でそのワークスペースのcwdフィルタ、None で全件
pub fn search(
    conn: &Connection,
    query: &str,
    date: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    entry_type: Option<&str>,
    session_id: Option<&str>,
    limit: i64,
    workspace: Option<&str>,
    source: Option<&str>,
) -> Result<Vec<RawEntry>> {
    let trimmed = query.trim();
    let use_fts = !trimmed.is_empty() && trimmed != "*";

    let mut sql;
    let mut param_idx;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut conditions = Vec::new();

    if use_fts {
        sql = String::from(
            "SELECT r.id, r.session_id, r.timestamp, r.date, r.time, r.type,
                    r.tool_name, snippet(raw_entries_fts, 0, '>>','<<', '...', 40) as content,
                    r.cwd, r.git_branch, r.source
             FROM raw_entries_fts f
             JOIN raw_entries r ON r.id = f.rowid
             WHERE raw_entries_fts MATCH ?1",
        );
        // FTS5はハイフンを否定演算子として扱うため、単語間のハイフンをスペースに正規化する
        let fts_query = normalize_fts_query(trimmed);
        param_values.push(Box::new(fts_query));
        param_idx = 2;
    } else {
        sql = String::from(
            "SELECT id, session_id, timestamp, date, time, type,
                    tool_name, substr(content, 1, 200) as content,
                    cwd, git_branch, source
             FROM raw_entries
             WHERE 1=1",
        );
        param_idx = 1;
    }

    // FTS時は r. プレフィックス（JOINのエイリアス）、非FTS時はプレフィックスなし
    let col = |name: &str| -> String {
        if use_fts { format!("r.{name}") } else { name.to_string() }
    };

    if let Some(d) = date {
        conditions.push(format!("{} = ?{param_idx}", col("date")));
        param_values.push(Box::new(d.to_string()));
        param_idx += 1;
    }
    if let Some(df) = date_from {
        conditions.push(format!("{} >= ?{param_idx}", col("date")));
        param_values.push(Box::new(df.to_string()));
        param_idx += 1;
    }
    if let Some(dt) = date_to {
        conditions.push(format!("{} <= ?{param_idx}", col("date")));
        param_values.push(Box::new(dt.to_string()));
        param_idx += 1;
    }
    if let Some(t) = entry_type {
        conditions.push(format!("{} = ?{param_idx}", col("type")));
        param_values.push(Box::new(t.to_string()));
        param_idx += 1;
    }
    if let Some(s) = session_id {
        conditions.push(format!("{} = ?{param_idx}", col("session_id")));
        param_values.push(Box::new(s.to_string()));
        param_idx += 1;
    }
    // ワークスペースフィルタ: cwdが指定パスで始まるエントリに絞り込む
    // BUG-005: LIKE特殊文字をエスケープ / BUG-009: NULL cwdも含める
    if let Some(ws) = workspace {
        let escaped = ws.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let cwd_col = col("cwd");
        conditions.push(format!(
            "({cwd_col} LIKE ?{param_idx} ESCAPE '\\' OR {cwd_col} IS NULL)"
        ));
        param_values.push(Box::new(format!("{escaped}%")));
        param_idx += 1;
    }
    // ソースフィルタ: claude / antigravity 等
    if let Some(src) = source {
        conditions.push(format!("{} = ?{param_idx}", col("source")));
        param_values.push(Box::new(src.to_string()));
        param_idx += 1;
    }

    for c in &conditions {
        sql.push_str(" AND ");
        sql.push_str(c);
    }
    sql.push_str(&format!(" ORDER BY {} DESC LIMIT ?{param_idx}", col("timestamp")));
    param_values.push(Box::new(limit));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = param_values.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok(RawEntry {
            id: row.get(0)?,
            session_id: row.get(1)?,
            timestamp: row.get(2)?,
            date: row.get(3)?,
            time: row.get(4)?,
            entry_type: row.get(5)?,
            tool_name: row.get(6)?,
            content: row.get(7)?,
            cwd: row.get(8)?,
            git_branch: row.get(9)?,
            source: row.get(10)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

/// エントリ全文取得
pub fn get_entry(conn: &Connection, id: i64) -> Result<Option<RawEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, session_id, timestamp, date, time, type, tool_name, content, cwd, git_branch, source
         FROM raw_entries WHERE id = ?1",
    )?;
    let entry = stmt
        .query_row(params![id], |row| {
            Ok(RawEntry {
                id: row.get(0)?,
                session_id: row.get(1)?,
                timestamp: row.get(2)?,
                date: row.get(3)?,
                time: row.get(4)?,
                entry_type: row.get(5)?,
                tool_name: row.get(6)?,
                content: row.get(7)?,
                cwd: row.get(8)?,
                git_branch: row.get(9)?,
                source: row.get(10)?,
            })
        })
        .ok();
    Ok(entry)
}

/// セッション一覧
pub fn list_sessions(
    conn: &Connection,
    date: Option<&str>,
    limit: i64,
    workspace: Option<&str>,
) -> Result<Vec<(String, String, String, String, i64)>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1usize;

    if let Some(d) = date {
        conditions.push(format!("date = ?{idx}"));
        params_vec.push(Box::new(d.to_string()));
        idx += 1;
    }
    // BUG-005: LIKE特殊文字をエスケープ / BUG-009: NULL cwdも含める
    if let Some(ws) = workspace {
        let escaped = ws.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        conditions.push(format!("(cwd LIKE ?{idx} ESCAPE '\\' OR cwd IS NULL)"));
        params_vec.push(Box::new(format!("{escaped}%")));
        idx += 1;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        "SELECT session_id, MIN(timestamp), MAX(timestamp), date, COUNT(*)
         FROM raw_entries {where_clause}
         GROUP BY session_id ORDER BY MIN(timestamp) DESC LIMIT ?{idx}"
    );
    params_vec.push(Box::new(limit));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;

    let mut sessions = Vec::new();
    for row in rows {
        sessions.push(row?);
    }
    Ok(sessions)
}

/// 特定セッションのエントリ取得
pub fn get_session_entries(
    conn: &Connection,
    session_id: &str,
    entry_type: Option<&str>,
    limit: i64,
) -> Result<Vec<RawEntry>> {
    let (sql, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = if let Some(t) = entry_type {
        let types: Vec<&str> = t.split(',').collect();
        let placeholders: Vec<String> = (0..types.len()).map(|i| format!("?{}", i + 2)).collect();
        let mut pv: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(session_id.to_string())];
        for ty in &types {
            pv.push(Box::new(ty.trim().to_string()));
        }
        let limit_idx = pv.len() + 1;
        pv.push(Box::new(limit));
        (
            format!(
                "SELECT id, session_id, timestamp, date, time, type, tool_name,
                        substr(content, 1, 200) as content, cwd, git_branch, source
                 FROM raw_entries WHERE session_id = ?1 AND type IN ({})
                 ORDER BY timestamp ASC LIMIT ?{limit_idx}",
                placeholders.join(", ")
            ),
            pv,
        )
    } else {
        (
            "SELECT id, session_id, timestamp, date, time, type, tool_name,
                    substr(content, 1, 200) as content, cwd, git_branch, source
             FROM raw_entries WHERE session_id = ?1 AND type IN ('user', 'assistant')
             ORDER BY timestamp ASC LIMIT ?2"
                .to_string(),
            vec![Box::new(session_id.to_string()), Box::new(limit)],
        )
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok(RawEntry {
            id: row.get(0)?,
            session_id: row.get(1)?,
            timestamp: row.get(2)?,
            date: row.get(3)?,
            time: row.get(4)?,
            entry_type: row.get(5)?,
            tool_name: row.get(6)?,
            content: row.get(7)?,
            cwd: row.get(8)?,
            git_branch: row.get(9)?,
            source: row.get(10)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

pub fn get_entries_by_date_range(
    conn: &Connection,
    date_from: &str,
    date_to: &str,
    entry_type: Option<&str>,
    limit: i64,
) -> Result<Vec<RawEntry>> {
    let types = entry_type.unwrap_or("user,assistant");
    let type_list: Vec<&str> = types.split(',').map(|t| t.trim()).collect();
    let placeholders: Vec<String> = (0..type_list.len()).map(|i| format!("?{}", i + 3)).collect();

    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(date_from.to_string()),
        Box::new(date_to.to_string()),
    ];
    for ty in &type_list {
        params_vec.push(Box::new(ty.to_string()));
    }
    let limit_idx = params_vec.len() + 1;
    params_vec.push(Box::new(limit));

    let sql = format!(
        "SELECT id, session_id, timestamp, date, time, type, tool_name,
                substr(content, 1, 200) as content, cwd, git_branch, source
         FROM raw_entries WHERE date >= ?1 AND date <= ?2 AND type IN ({})
         ORDER BY timestamp ASC LIMIT ?{limit_idx}",
        placeholders.join(", ")
    );

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), |row| {
        Ok(RawEntry {
            id: row.get(0)?,
            session_id: row.get(1)?,
            timestamp: row.get(2)?,
            date: row.get(3)?,
            time: row.get(4)?,
            entry_type: row.get(5)?,
            tool_name: row.get(6)?,
            content: row.get(7)?,
            cwd: row.get(8)?,
            git_branch: row.get(9)?,
            source: row.get(10)?,
        })
    })?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}
