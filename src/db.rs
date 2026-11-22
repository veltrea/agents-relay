use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

/// sqlite-vec 拡張を全コネクションに登録する（プロセスごとに一度だけ）
fn init_vec_extension() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        type InitFn = unsafe extern "C" fn(
            *mut rusqlite::ffi::sqlite3,
            *mut *const std::os::raw::c_char,
            *const rusqlite::ffi::sqlite3_api_routines,
        ) -> std::os::raw::c_int;
        let init: InitFn = std::mem::transmute(sqlite_vec::sqlite3_vec_init as *const ());
        rusqlite::ffi::sqlite3_auto_extension(Some(init));
    });
}

pub fn open(path: &Path) -> Result<Connection> {
    init_vec_extension();
    let conn = Connection::open(path)?;
    // 複数プロセスが同時起動した場合の BUSY 対策: 最大 5 秒待ってリトライ
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
    attach_side_dbs(&conn, path)?;
    Ok(conn)
}

/// メイン DB (memory.db) を恒久的に低容量に保つため、大きなデータは
/// 同じディレクトリの分離 DB に退避する:
/// - reasoning.db (rdb.cot):      生 CoT と要約。削除しても検索は壊れない
///   （memory_get_reasoning が has_reasoning=false を返すだけ）。
/// - fulltext.db (fdb.fulltext):  CONTENT_INLINE_MAX 超の content 全文。
///   将来の再圧縮・再処理のための無損失原本。メイン DB 側には先頭だけを
///   inline 保存し FTS 索引もそちらで張る。
/// - fulltext.db (fdb.tool_trace): Cursor 等の tool_use 原本。提示面の content
///   には載せず、memory_get_tools で必要時のみ取得する。
/// - compact.db (cdb.entry_compact): 会話再構成用の軽量投影 (min/mid)。
///   派生物であり、削除しても再生成できる。get_session が既定で返す層。
fn attach_side_dbs(conn: &Connection, db_path: &Path) -> Result<()> {
    let rpath = db_path.with_file_name("reasoning.db");
    conn.execute(
        "ATTACH DATABASE ?1 AS rdb",
        params![rpath.to_string_lossy()],
    )?;
    let fpath = db_path.with_file_name("fulltext.db");
    conn.execute(
        "ATTACH DATABASE ?1 AS fdb",
        params![fpath.to_string_lossy()],
    )?;
    // vectors.db (vdb): 要約・事実の埋め込みベクトル。完全に派生物であり、
    // 削除しても embed コマンドで再生成できる。
    let vpath = db_path.with_file_name("vectors.db");
    conn.execute(
        "ATTACH DATABASE ?1 AS vdb",
        params![vpath.to_string_lossy()],
    )?;
    // compact.db (cdb): 会話再構成用の軽量投影 (min/mid 2 段)。生 CoT は取り込み時に
    // 分離済みなので、ここが畳むのは主にツールのペイロード。完全に派生物であり、
    // 削除しても `db rebuild-compact` / consolidate パスで再生成できる。
    let cpath = db_path.with_file_name("compact.db");
    conn.execute(
        "ATTACH DATABASE ?1 AS cdb",
        params![cpath.to_string_lossy()],
    )?;
    conn.execute_batch(
        "PRAGMA rdb.journal_mode=WAL;
         PRAGMA fdb.journal_mode=WAL;
         PRAGMA vdb.journal_mode=WAL;
         PRAGMA cdb.journal_mode=WAL;
         CREATE TABLE IF NOT EXISTS rdb.cot (
             entry_id INTEGER PRIMARY KEY,
             reasoning TEXT,
             reasoning_summary TEXT
         );
         CREATE TABLE IF NOT EXISTS fdb.fulltext (
             entry_id INTEGER PRIMARY KEY,
             content TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS fdb.tool_trace (
             entry_id INTEGER PRIMARY KEY,
             tools_json TEXT NOT NULL,
             tools_summary TEXT,
             tools_tokens INTEGER
         );
         CREATE TABLE IF NOT EXISTS vdb.vec_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS cdb.entry_compact (
             entry_id       INTEGER PRIMARY KEY,
             min_text       TEXT NOT NULL,
             mid_text       TEXT NOT NULL,
             min_tokens     INTEGER NOT NULL,
             mid_tokens     INTEGER NOT NULL,
             recipe_version INTEGER NOT NULL
         );",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ベクトル索引 (sqlite-vec / vdb)
// ---------------------------------------------------------------------------

/// 現在のベクトル索引の (モデル名, 次元数)。未初期化なら None。
pub fn vec_state(conn: &Connection) -> Result<Option<(String, usize)>> {
    let model: Option<String> = conn
        .query_row(
            "SELECT value FROM vdb.vec_meta WHERE key = 'model'",
            [],
            |row| row.get(0),
        )
        .ok();
    let dim: Option<usize> = conn
        .query_row(
            "SELECT value FROM vdb.vec_meta WHERE key = 'dim'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|s| s.parse().ok());
    Ok(match (model, dim) {
        (Some(m), Some(d)) => Some((m, d)),
        _ => None,
    })
}

/// ベクトル索引を (再) 初期化する。モデル・次元が変わったら既存索引は破棄
/// （派生物なので安全）。
pub fn vec_init(conn: &Connection, model: &str, dim: usize) -> Result<()> {
    if let Some((m, d)) = vec_state(conn)? {
        if m == model && d == dim {
            return Ok(());
        }
        eprintln!("agents-relay: embedding model changed ({m}/{d} -> {model}/{dim}), rebuilding vector index");
    }
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS vdb.vec_summaries;
         DROP TABLE IF EXISTS vdb.vec_facts;
         CREATE VIRTUAL TABLE vdb.vec_summaries USING vec0(embedding float[{dim}]);
         CREATE VIRTUAL TABLE vdb.vec_facts USING vec0(embedding float[{dim}]);"
    ))?;
    conn.execute(
        "INSERT OR REPLACE INTO vdb.vec_meta (key, value) VALUES ('model', ?1)",
        params![model],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO vdb.vec_meta (key, value) VALUES ('dim', ?1)",
        params![dim.to_string()],
    )?;
    Ok(())
}

fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// ベクトルを upsert する。table は "vec_summaries" | "vec_facts"。
pub fn vec_upsert(conn: &Connection, table: &str, rowid: i64, embedding: &[f32]) -> Result<()> {
    conn.execute(
        &format!("DELETE FROM vdb.{table} WHERE rowid = ?1"),
        params![rowid],
    )?;
    conn.execute(
        &format!("INSERT INTO vdb.{table} (rowid, embedding) VALUES (?1, ?2)"),
        params![rowid, f32s_to_blob(embedding)],
    )?;
    Ok(())
}

/// KNN 検索。戻り値: (rowid, distance) を近い順に。
pub fn vec_search(
    conn: &Connection,
    table: &str,
    query: &[f32],
    k: usize,
) -> Result<Vec<(i64, f64)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT rowid, distance FROM vdb.{table}
         WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance"
    ))?;
    let rows = stmt
        .query_map(params![f32s_to_blob(query), k as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// まだ埋め込みが無い要約を返す (id, session_id, summary_md)
pub fn summaries_missing_embedding(
    conn: &Connection,
    limit: i64,
) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.session_id, s.summary_md FROM summaries s
         WHERE s.id NOT IN (SELECT rowid FROM vdb.vec_summaries)
         ORDER BY s.id LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// まだ埋め込みが無い有効な事実を返す (id, fact)
pub fn facts_missing_embedding(conn: &Connection, limit: i64) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.fact FROM facts f
         WHERE f.superseded_by IS NULL
           AND f.id NOT IN (SELECT rowid FROM vdb.vec_facts)
         ORDER BY f.id LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// content をメイン DB に inline 保存する上限 (bytes)。
/// これを超える本文は先頭 CONTENT_HEAD_BYTES + マーカーだけを inline 保存し、
/// 全文を fdb.fulltext に無損失で退避する。
pub const CONTENT_INLINE_MAX: usize = 50_000;
const CONTENT_HEAD_BYTES: usize = 1_000;

/// 切り詰めマーカー付きの inline 表現を作る（旧 ingest の形式と互換）
fn inline_head(content: &str) -> String {
    let end = truncate_at_char_boundary(content, CONTENT_HEAD_BYTES);
    format!("{}... [truncated, {} bytes]", &content[..end], content.len())
}

/// マルチバイト文字の境界を壊さないように切り詰め位置を返す
pub fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> usize {
    if max_bytes >= s.len() {
        return s.len();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
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
            reasoning_tokens INTEGER,
            content_tokens INTEGER,
            tools_tokens INTEGER,
            created_at TEXT DEFAULT (datetime('now'))
        );

        CREATE INDEX IF NOT EXISTS idx_raw_session ON raw_entries(session_id);
        CREATE INDEX IF NOT EXISTS idx_raw_timestamp ON raw_entries(timestamp);
        CREATE INDEX IF NOT EXISTS idx_raw_date ON raw_entries(date);
        CREATE INDEX IF NOT EXISTS idx_raw_type ON raw_entries(type);
        CREATE VIRTUAL TABLE IF NOT EXISTS raw_entries_fts USING fts5(
            content, tool_name, session_id, context
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

        -- 持続的な事実 (決定・制約・好み・環境)。consolidate が抽出して蓄積する。
        -- 削除はせず superseded_by で失効させる (bi-temporal の軽量版)。
        CREATE TABLE IF NOT EXISTS facts (
            id INTEGER PRIMARY KEY,
            category TEXT NOT NULL,
            fact TEXT NOT NULL,
            workspace TEXT,
            session_id TEXT,
            created_at TEXT DEFAULT (datetime('now')),
            superseded_by INTEGER
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS facts_fts USING fts5(
            fact, category, workspace
        );

        -- スキーマ/索引レシピの版管理 (FTS を surface 投影に張り替えた等)。
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        ",
    )?;

    // マイグレーション: 既存DBに source カラムがなければ追加
    migrate(conn)?;

    Ok(())
}

/// raw_entries の実カラム名一覧 (PRAGMA table_info)
fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn migrate(conn: &Connection) -> Result<()> {
    let cols = table_columns(conn, "raw_entries")?;
    let has = |name: &str| cols.iter().any(|c| c == name);

    if !has("source") {
        conn.execute_batch(
            "ALTER TABLE raw_entries ADD COLUMN source TEXT NOT NULL DEFAULT 'claude';"
        )?;
        eprintln!("Migration: added 'source' column to raw_entries");
    }

    // CoT ポインタ (概算トークン数) はメイン DB に持つ。実体は rdb.cot に分離。
    if !has("reasoning_tokens") {
        conn.execute_batch("ALTER TABLE raw_entries ADD COLUMN reasoning_tokens INTEGER;")?;
        eprintln!("Migration: added 'reasoning_tokens' column to raw_entries");
    }

    // content 本文の概算トークン数ポインタ。compact 投影の detail 選択材料
    // (full/mid/min のトークン量メニュー) に使う。取り込み時に計算・保存する。
    if !has("content_tokens") {
        conn.execute_batch("ALTER TABLE raw_entries ADD COLUMN content_tokens INTEGER;")?;
        eprintln!("Migration: added 'content_tokens' column to raw_entries");
    }

    // tool_use ポインタ。実体は fdb.tool_trace に分離。
    if !has("tools_tokens") {
        conn.execute_batch("ALTER TABLE raw_entries ADD COLUMN tools_tokens INTEGER;")?;
        eprintln!("Migration: added 'tools_tokens' column to raw_entries");
    }

    // 旧レイアウト (raw_entries に reasoning / reasoning_summary が同居) からの移行:
    // データを rdb.cot に移してからカラムを落とす
    if has("reasoning") || has("reasoning_summary") {
        if has("reasoning") && has("reasoning_summary") {
            conn.execute_batch(
                "INSERT OR REPLACE INTO rdb.cot (entry_id, reasoning, reasoning_summary)
                     SELECT id, reasoning, reasoning_summary FROM raw_entries
                     WHERE reasoning IS NOT NULL OR reasoning_summary IS NOT NULL;",
            )?;
        }
        if has("reasoning") {
            conn.execute_batch("ALTER TABLE raw_entries DROP COLUMN reasoning;")?;
        }
        if has("reasoning_summary") {
            conn.execute_batch("ALTER TABLE raw_entries DROP COLUMN reasoning_summary;")?;
        }
        eprintln!("Migration: moved chain-of-thought to reasoning.db (rdb.cot)");
    }

    // インデックスは常に確保（IF NOT EXISTS なので安全）
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_raw_source ON raw_entries(source);
         CREATE INDEX IF NOT EXISTS idx_raw_cwd ON raw_entries(cwd);"
    )?;

    // FTS 索引を surface 投影ベースに保つ。以下のいずれかで一回きり再構築する:
    //   - context 列が無い旧スキーマ (workspace/branch 検索の追加)
    //   - 索引レシピが最新でない (本文全文 → surface 投影への切替など)
    // surface 投影は成功ツール出力の本体を索引しない = 検索からノイズを排除する。
    // 53万行で 1〜2 分程度の一回きりの処理。以後は取り込み時に surface で索引される。
    let fts_has_context: bool = conn
        .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name='raw_entries_fts'")?
        .query_row([], |row| row.get::<_, String>(0))
        .map(|sql| sql.contains("context"))
        .unwrap_or(true);
    let fts_recipe: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'fts_recipe'", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?;
    if !fts_has_context || fts_recipe.as_deref() != Some(FTS_RECIPE) {
        eprintln!("Migration: rebuilding FTS as surface projection (one-time, ~1-2min)...");
        rebuild_fts(conn)?;
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('fts_recipe', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            params![FTS_RECIPE],
        )?;
        eprintln!("Migration: FTS surface rebuild done");
    }

    Ok(())
}

/// FTS 索引レシピの版。surface 投影で索引していることを示す。
/// 索引の作り方 (投影) を変えたらこの値を上げると一回きり再構築される。
const FTS_RECIPE: &str = "surface_v1";

/// FTS の context 列: workspace 名 (cwd の basename)・branch・source を検索語彙に載せる。
/// 本文には混ぜない (snippet や get_entry の内容を汚さないため)。
fn fts_context(cwd: Option<&str>, git_branch: Option<&str>, source: &str) -> String {
    let ws = cwd
        .map(|c| c.trim_end_matches('/'))
        .and_then(|c| c.rsplit('/').next())
        .unwrap_or("");
    format!("{} {} {}", ws, git_branch.unwrap_or(""), source)
        .trim()
        .to_string()
}

/// FTS インデックスを surface 投影で再構築する（DROP → 再作成 → 全行索引）。
/// 索引本文には content の全文ではなく `compact::surface`（散文は全保持・成功ツール
/// 出力の本体は畳む・エラーは残す）を載せる。これで検索からツール出力ノイズを排除しつつ
/// 会話の表層と tool_use の対象（ファイル名/コマンド等）は検索可能に保つ。
/// 全文原本 (fdb 退避分含む) を読むので、詳細を検索したいときは search(deep=true) で
/// LIKE フォールバックする。メモリ節約のためストリーミングで処理する。
pub fn rebuild_fts(conn: &Connection) -> Result<()> {
    // 'delete-all' は contentless/external-content FTS5 専用のため、
    // 通常の FTS5 テーブルでは DROP → 再作成で全再構築する。
    conn.execute_batch(
        "DROP TABLE IF EXISTS raw_entries_fts;
         CREATE VIRTUAL TABLE raw_entries_fts USING fts5(
             content, tool_name, session_id, context
         );",
    )?;

    let tx = conn.unchecked_transaction()?;
    {
        let mut sel = tx.prepare(
            "SELECT r.id, r.type, r.tool_name, COALESCE(f.content, r.content),
                    r.session_id, r.cwd, r.git_branch, r.source
             FROM raw_entries r
             LEFT JOIN fdb.fulltext f ON f.entry_id = r.id",
        )?;
        let mut ins = tx.prepare(
            "INSERT INTO raw_entries_fts (rowid, content, tool_name, session_id, context)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut rows = sel.query([])?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            let entry_type: String = row.get(1)?;
            let tool_name: Option<String> = row.get(2)?;
            let content: String = row.get(3)?;
            let session_id: String = row.get(4)?;
            let cwd: Option<String> = row.get(5)?;
            let git_branch: Option<String> = row.get(6)?;
            let source: String = row.get(7)?;

            let body = crate::compact::surface(&entry_type, tool_name.as_deref(), &content);
            let ctx = fts_context(cwd.as_deref(), git_branch.as_deref(), &source);
            ins.execute(params![id, body, tool_name, session_id, ctx])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn reset(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS raw_entries_fts;
        DROP TABLE IF EXISTS raw_entries;
        DROP TABLE IF EXISTS sync_state;
        DROP TABLE IF EXISTS summaries;
        DELETE FROM rdb.cot;
        DELETE FROM fdb.fulltext;
        DELETE FROM fdb.tool_trace;
        DELETE FROM cdb.entry_compact;
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
    /// 生 CoT。get_entry でのみ全文を取得する。検索/セッション結果では常に None。
    pub reasoning: Option<String>,
    /// 圧縮済み CoT。memory_get_reasoning(mode=summary) 初回呼び出し時に遅延生成・キャッシュ。
    pub reasoning_summary: Option<String>,
    /// 生 CoT の概算トークン数。ポインタ (has_reasoning) と予算判断に使う。
    pub reasoning_tokens: Option<i64>,
    /// content 本文 (全文) の概算トークン数。detail=full 相当の重さの目安。
    pub content_tokens: Option<i64>,
    /// compact 投影 min/mid の概算トークン数。get_session が detail 選択の
    /// メニューとして返す (「今は min=4tok、mid=18、full=82」)。未生成なら None。
    pub min_tokens: Option<i64>,
    pub mid_tokens: Option<i64>,
    /// 分離保存した tool_use 原本の概算トークン数。ポインタ (has_tools) 用。
    pub tools_tokens: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
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
    insert_entry_full(
        conn, session_id, timestamp, date, time, entry_type,
        tool_name, content, cwd, git_branch, source, None,
    )
}

/// CoT (reasoning) を別フィールドに分離して保存する版。
/// reasoning は FTS インデックスには載せない（検索ノイズと肥大を避けるため）。
#[allow(clippy::too_many_arguments)]
pub fn insert_entry_full(
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
    reasoning: Option<&str>,
) -> Result<i64> {
    let reasoning_tokens = reasoning.map(crate::reasoning::estimate_tokens);
    // content 本文 (全文) の概算トークン数。detail 選択のポインタに使う。
    let content_tokens = crate::reasoning::estimate_tokens(content);

    // 切り詰めポリシーはここに集約する。呼び出し側は常に全文を渡してよい。
    // 上限超過分は inline に頭だけ残し、全文を fdb に無損失退避する。
    let (inline, overflow): (std::borrow::Cow<str>, Option<&str>) =
        if content.len() > CONTENT_INLINE_MAX {
            (std::borrow::Cow::Owned(inline_head(content)), Some(content))
        } else {
            (std::borrow::Cow::Borrowed(content), None)
        };

    conn.execute(
        "INSERT INTO raw_entries (session_id, timestamp, date, time, type, tool_name, content, cwd, git_branch, source, reasoning_tokens, content_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![session_id, timestamp, date, time, entry_type, tool_name, inline, cwd, git_branch, source, reasoning_tokens, content_tokens],
    )?;
    let id = conn.last_insert_rowid();

    // 全文原本は分離 DB (fdb.fulltext) へ
    if let Some(full) = overflow {
        conn.execute(
            "INSERT OR REPLACE INTO fdb.fulltext (entry_id, content) VALUES (?1, ?2)",
            params![id, full],
        )?;
    }

    // 生 CoT は分離 DB (rdb.cot) へ（無損失・切り詰めなし）
    if let Some(r) = reasoning {
        conn.execute(
            "INSERT OR REPLACE INTO rdb.cot (entry_id, reasoning) VALUES (?1, ?2)",
            params![id, r],
        )?;
    }

    // FTS には surface 投影を載せる（散文は全保持・成功ツール出力の本体は畳む・
    // エラーは残す）。全文 content から作るので overflow でも表層は失われない。
    // reasoning / 全文原本 / ツール出力本体は意図的にインデックスしない。
    let fts_body = crate::compact::surface(entry_type, tool_name, content);
    conn.execute(
        "INSERT INTO raw_entries_fts (rowid, content, tool_name, session_id, context)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, fts_body, tool_name, session_id, fts_context(cwd, git_branch, source)],
    )?;

    Ok(id)
}

/// 圧縮済み CoT をキャッシュする（memory_get_reasoning の遅延生成時に呼ぶ）。
pub fn set_reasoning_summary(conn: &Connection, id: i64, summary: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO rdb.cot (entry_id, reasoning_summary) VALUES (?1, ?2)
         ON CONFLICT(entry_id) DO UPDATE SET reasoning_summary = ?2",
        params![id, summary],
    )?;
    Ok(())
}

/// tool_use 原本を分離 DB に保存し、メイン行にポインタ (tools_tokens) を付ける。
/// content / FTS surface は変えず、提示面には載せない（必要時 memory_get_tools）。
pub fn set_tool_trace(
    conn: &Connection,
    id: i64,
    tools_json: &str,
    tools_summary: Option<&str>,
) -> Result<()> {
    let tools_tokens = crate::reasoning::estimate_tokens(tools_json) as i64;
    conn.execute(
        "INSERT OR REPLACE INTO fdb.tool_trace (entry_id, tools_json, tools_summary, tools_tokens)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, tools_json, tools_summary, tools_tokens],
    )?;
    conn.execute(
        "UPDATE raw_entries SET tools_tokens = ?1 WHERE id = ?2",
        params![tools_tokens, id],
    )?;
    Ok(())
}

/// tool_use 原本を取得。(tools_json, tools_summary, tools_tokens)
pub fn get_tool_trace(conn: &Connection, id: i64) -> Result<Option<(String, Option<String>, i64)>> {
    let row = conn
        .query_row(
            "SELECT tools_json, tools_summary, COALESCE(tools_tokens, 0)
             FROM fdb.tool_trace WHERE entry_id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok();
    Ok(row)
}

/// 指定セッションの source=cursor 行と付随 tool_trace / fulltext / cot / compact を削除する
/// （transcript が短く書き直されたときの再 ingest 用）。
pub fn delete_cursor_session(conn: &Connection, session_id: &str) -> Result<u64> {
    let mut ids: Vec<i64> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT id FROM raw_entries WHERE session_id = ?1 AND source = 'cursor'",
        )?;
        let rows = stmt.query_map(params![session_id], |row| row.get(0))?;
        for id in rows {
            ids.push(id?);
        }
    }
    if ids.is_empty() {
        return Ok(0);
    }
    for id in &ids {
        conn.execute("DELETE FROM fdb.tool_trace WHERE entry_id = ?1", params![id])?;
        conn.execute("DELETE FROM fdb.fulltext WHERE entry_id = ?1", params![id])?;
        conn.execute("DELETE FROM rdb.cot WHERE entry_id = ?1", params![id])?;
        conn.execute("DELETE FROM cdb.entry_compact WHERE entry_id = ?1", params![id])?;
        conn.execute("DELETE FROM raw_entries_fts WHERE rowid = ?1", params![id])?;
        conn.execute("DELETE FROM raw_entries WHERE id = ?1", params![id])?;
    }
    Ok(ids.len() as u64)
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

/// 3 つの DB (main / rdb / fdb) をタイムスタンプ付きディレクトリに整合バックアップする。
/// VACUUM INTO を使うため、書き込みと並行しても一貫したスナップショットになり、
/// 空きページも除かれてコンパクトになる。keep 世代を超えた古いセットは削除する。
/// 戻り値: (作成したディレクトリ, 合計バイト数)
pub fn backup(conn: &Connection, out_dir: &Path, keep: usize) -> Result<(std::path::PathBuf, u64)> {
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let dest = out_dir.join(&stamp);
    std::fs::create_dir_all(&dest)?;

    let mut total = 0u64;
    for (schema, fname) in [
        ("main", "memory.db"),
        ("rdb", "reasoning.db"),
        ("fdb", "fulltext.db"),
    ] {
        let path = dest.join(fname);
        // VACUUM INTO は出力先が既存だと失敗する（上書きしない = 安全側）
        conn.execute(
            &format!("VACUUM {schema} INTO ?1"),
            params![path.to_string_lossy()],
        )?;
        total += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    }

    // 世代ローテーション: タイムスタンプ名のディレクトリを新しい順に keep 個残す
    let mut sets: Vec<std::path::PathBuf> = std::fs::read_dir(out_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.len() == 17 && n.as_bytes()[4] == b'-')
                    .unwrap_or(false)
        })
        .collect();
    sets.sort();
    sets.reverse();
    for old in sets.iter().skip(keep.max(1)) {
        std::fs::remove_dir_all(old).ok();
    }

    Ok((dest, total))
}

// ---------------------------------------------------------------------------
// summaries / facts (睡眠時整理 consolidate 用)
// ---------------------------------------------------------------------------

/// セッション要約を UPSERT する
#[allow(clippy::too_many_arguments)]
pub fn upsert_summary(
    conn: &Connection,
    session_id: &str,
    project_path: Option<&str>,
    git_branch: Option<&str>,
    started_at: &str,
    ended_at: &str,
    summary_md: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO summaries (session_id, project_path, git_branch, started_at, ended_at, summary_md)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(session_id) DO UPDATE SET
             project_path = ?2, git_branch = ?3, started_at = ?4, ended_at = ?5, summary_md = ?6",
        params![session_id, project_path, git_branch, started_at, ended_at, summary_md],
    )?;
    Ok(())
}

pub fn get_summary(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    let s = conn
        .query_row(
            "SELECT summary_md FROM summaries WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .ok();
    Ok(s)
}

/// 要約が必要なセッション (未要約、または要約後に新しいエントリが増えたもの) を返す。
/// upgrade_heuristic=true なら、ヒューリスティック要約のままのセッションも
/// 再処理対象に含める (LLM が使えるようになった後の格上げ用)。
/// 戻り値: (session_id, started, ended, entry_count)
pub fn sessions_needing_summary(
    conn: &Connection,
    min_entries: i64,
    idle_cutoff: &str,
    limit: i64,
    upgrade_heuristic: bool,
) -> Result<Vec<(String, String, String, i64)>> {
    let upgrade_clause = if upgrade_heuristic {
        "OR EXISTS (SELECT 1 FROM summaries s2 WHERE s2.session_id = r.session_id
                    AND s2.summary_md LIKE '<!-- summarizer: heuristic -->%')"
    } else {
        ""
    };
    let sql = format!(
        "SELECT r.session_id, MIN(r.timestamp), MAX(r.timestamp), COUNT(*)
         FROM raw_entries r
         WHERE r.type IN ('user', 'assistant')
         GROUP BY r.session_id
         HAVING COUNT(*) >= ?1
            AND MAX(r.timestamp) <= ?2
            AND (MAX(r.timestamp) > COALESCE(
                (SELECT s.ended_at FROM summaries s WHERE s.session_id = r.session_id), '')
                {upgrade_clause})
         ORDER BY MAX(r.timestamp) DESC
         LIMIT ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![min_entries, idle_cutoff, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// 文字バイグラム Jaccard 類似度 (0.0-1.0)。日本語にも頑健な軽量類似判定。
pub fn bigram_similarity(a: &str, b: &str) -> f64 {
    fn bigrams(s: &str) -> std::collections::HashSet<(char, char)> {
        let chars: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
        chars.windows(2).map(|w| (w[0], w[1])).collect()
    }
    let (sa, sb) = (bigrams(a), bigrams(b));
    if sa.is_empty() || sb.is_empty() {
        return if a.trim() == b.trim() { 1.0 } else { 0.0 };
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    inter / union
}

/// 新規性ゲート: 既存の有効な事実に類似するものがあればその id を返す。
/// 類似度は文字バイグラム Jaccard >= 0.6 で「既知」とみなす。
pub fn find_duplicate_fact(conn: &Connection, fact: &str) -> Result<Option<i64>> {
    let mut stmt =
        conn.prepare("SELECT id, fact FROM facts WHERE superseded_by IS NULL")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, existing) = row?;
        if bigram_similarity(fact, &existing) >= 0.6 {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

pub fn insert_fact(
    conn: &Connection,
    category: &str,
    fact: &str,
    workspace: Option<&str>,
    session_id: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO facts (category, fact, workspace, session_id) VALUES (?1, ?2, ?3, ?4)",
        params![category, fact, workspace, session_id],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO facts_fts (rowid, fact, category, workspace) VALUES (?1, ?2, ?3, ?4)",
        params![id, fact, category, workspace],
    )?;
    Ok(id)
}

/// 事実の検索。query が空なら新しい順に一覧。
/// 戻り値: (id, category, fact, workspace, session_id, created_at)
pub fn search_facts(
    conn: &Connection,
    query: &str,
    workspace: Option<&str>,
    limit: i64,
) -> Result<Vec<(i64, String, String, Option<String>, Option<String>, String)>> {
    let trimmed = query.trim();
    let mut out = Vec::new();

    let (sql, use_query) = if trimmed.is_empty() || trimmed == "*" {
        (
            "SELECT f.id, f.category, f.fact, f.workspace, f.session_id, f.created_at
             FROM facts f
             WHERE f.superseded_by IS NULL
               AND (?1 IS NULL OR f.workspace = ?1 OR f.workspace IS NULL)
             ORDER BY f.id DESC LIMIT ?2"
                .to_string(),
            false,
        )
    } else {
        (
            "SELECT f.id, f.category, f.fact, f.workspace, f.session_id, f.created_at
             FROM facts_fts t
             JOIN facts f ON f.id = t.rowid
             WHERE facts_fts MATCH ?3 AND f.superseded_by IS NULL
               AND (?1 IS NULL OR f.workspace = ?1 OR f.workspace IS NULL)
             ORDER BY rank LIMIT ?2"
                .to_string(),
            true,
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let map = |row: &rusqlite::Row| -> rusqlite::Result<_> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        ))
    };
    if use_query {
        let fts_query = normalize_fts_query(trimmed);
        let rows = stmt.query_map(params![workspace, limit, fts_query], map)?;
        for r in rows {
            out.push(r?);
        }
    } else {
        let rows = stmt.query_map(params![workspace, limit], map)?;
        for r in rows {
            out.push(r?);
        }
    }
    Ok(out)
}

/// 持ち出し・バックアップ用の軽量コピーを一方向生成する。
/// 含む: raw_entries (CoT ポインタは NULL 化)・summaries・sync_state・空の FTS テーブル。
/// 含まない: 生 CoT (reasoning.db)・FTS 索引の実体 (復元先で `db rebuild-fts` すれば戻る)。
pub fn export_light(conn: &Connection, out: &Path) -> Result<i64> {
    if out.exists() {
        anyhow::bail!("Output file already exists: {}", out.display());
    }
    conn.execute(
        "ATTACH DATABASE ?1 AS light",
        params![out.to_string_lossy()],
    )?;

    let result = (|| -> Result<i64> {
        conn.execute_batch(
            "
            CREATE TABLE light.raw_entries (
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
                reasoning_tokens INTEGER,
                created_at TEXT DEFAULT (datetime('now'))
            );
            INSERT INTO light.raw_entries
                (id, session_id, timestamp, date, time, type, tool_name, content,
                 cwd, git_branch, source, reasoning_tokens, created_at)
                SELECT id, session_id, timestamp, date, time, type, tool_name, content,
                       cwd, git_branch, source, NULL, created_at
                FROM raw_entries;

            CREATE INDEX light.idx_raw_session ON raw_entries(session_id);
            CREATE INDEX light.idx_raw_timestamp ON raw_entries(timestamp);
            CREATE INDEX light.idx_raw_date ON raw_entries(date);
            CREATE INDEX light.idx_raw_type ON raw_entries(type);
            CREATE INDEX light.idx_raw_source ON raw_entries(source);
            CREATE INDEX light.idx_raw_cwd ON raw_entries(cwd);

            CREATE TABLE light.sync_state (
                file_path TEXT PRIMARY KEY,
                last_offset INTEGER DEFAULT 0
            );
            INSERT INTO light.sync_state SELECT * FROM sync_state;

            CREATE TABLE light.summaries (
                id INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL UNIQUE,
                project_path TEXT,
                git_branch TEXT,
                started_at TEXT,
                ended_at TEXT,
                summary_md TEXT NOT NULL,
                created_at TEXT DEFAULT (datetime('now'))
            );
            INSERT INTO light.summaries SELECT * FROM summaries;

            CREATE VIRTUAL TABLE light.raw_entries_fts USING fts5(
                content, tool_name, session_id, context
            );
            ",
        )?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM light.raw_entries", [], |row| row.get(0))?;
        Ok(count)
    })();

    conn.execute_batch("DETACH DATABASE light")?;
    result
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

    let cot_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM rdb.cot", [], |row| row.get(0))?;
    let fulltext_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM fdb.fulltext", [], |row| row.get(0))?;
    let tool_trace_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM fdb.tool_trace", [], |row| row.get(0))?;
    let compact_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM cdb.entry_compact", [], |row| row.get(0))?;

    // main / rdb それぞれのファイルサイズ
    let mut sizes = String::new();
    let mut stmt = conn.prepare("SELECT name, file FROM pragma_database_list")?;
    let files = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for f in files.flatten() {
        let (name, file) = f;
        if file.is_empty() {
            continue;
        }
        let mb = std::fs::metadata(&file).map(|m| m.len() / 1_048_576).unwrap_or(0);
        sizes.push_str(&format!("\n  {name}: {mb} MB ({file})"));
    }

    Ok(format!(
        "Entries:    {entry_count}\n\
         Sessions:   {session_count}\n\
         Date range: {min_date} .. {max_date}\n\
         Summaries:  {summary_count}\n\
         CoT stored: {cot_count}\n\
         Fulltext overflow: {fulltext_count}\n\
         Tool traces: {tool_trace_count}\n\
         Compact projections: {compact_count}\n\
         Tracked files: {sync_count}\n\
         Databases:{sizes}"
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
#[allow(clippy::too_many_arguments)]
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
    deep: bool,
) -> Result<Vec<RawEntry>> {
    let trimmed = query.trim();
    let has_query = !trimmed.is_empty() && trimmed != "*";
    // 既定 (deep=false): surface 索引の FTS = 会話の表層で検索。
    // deep=true: 全文原本 (ツール出力本体含む) に LIKE フォールバック。
    // 「表層で当てて詳細を辿る」既定 + 「出力の中の文字列も探す」逃げ道。
    let use_fts = has_query && !deep;
    let use_like = has_query && deep;
    let joined = use_fts || use_like; // r. プレフィックス (JOIN) が要るか

    let mut sql;
    let mut param_idx;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut conditions = Vec::new();

    if use_fts {
        sql = String::from(
            "SELECT r.id, r.session_id, r.timestamp, r.date, r.time, r.type,
                    r.tool_name, snippet(raw_entries_fts, 0, '>>','<<', '...', 40) as content,
                    r.cwd, r.git_branch, r.source, r.reasoning_tokens, r.content_tokens,
                    r.tools_tokens
             FROM raw_entries_fts f
             JOIN raw_entries r ON r.id = f.rowid
             WHERE raw_entries_fts MATCH ?1",
        );
        // FTS5はハイフンを否定演算子として扱うため、単語間のハイフンをスペースに正規化する
        let fts_query = normalize_fts_query(trimmed);
        param_values.push(Box::new(fts_query));
        param_idx = 2;
    } else if use_like {
        // ディープ検索: surface 索引に載らないツール出力本体も含めて全文を走査する。
        // FTS を使わない素の LIKE なので低速だが、明示オプトイン時のみの逃げ道。
        sql = String::from(
            "SELECT r.id, r.session_id, r.timestamp, r.date, r.time, r.type,
                    r.tool_name, substr(COALESCE(f.content, r.content), 1, 200) as content,
                    r.cwd, r.git_branch, r.source, r.reasoning_tokens, r.content_tokens,
                    r.tools_tokens
             FROM raw_entries r
             LEFT JOIN fdb.fulltext f ON f.entry_id = r.id
             WHERE COALESCE(f.content, r.content) LIKE ?1 ESCAPE '\\'",
        );
        let escaped = trimmed
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        param_values.push(Box::new(format!("%{escaped}%")));
        param_idx = 2;
    } else {
        sql = String::from(
            "SELECT id, session_id, timestamp, date, time, type,
                    tool_name, substr(content, 1, 200) as content,
                    cwd, git_branch, source, reasoning_tokens, content_tokens,
                    tools_tokens
             FROM raw_entries
             WHERE 1=1",
        );
        param_idx = 1;
    }

    // JOIN 時は r. プレフィックス（エイリアス）、素の raw_entries 走査時はプレフィックスなし
    let col = |name: &str| -> String {
        if joined { format!("r.{name}") } else { name.to_string() }
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
            reasoning: None,
            reasoning_summary: None,
            reasoning_tokens: row.get(11)?,
            content_tokens: row.get(12)?,
            min_tokens: None,
            mid_tokens: None,
            tools_tokens: row.get(13)?,
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
        "SELECT r.id, r.session_id, r.timestamp, r.date, r.time, r.type, r.tool_name,
                COALESCE(f.content, r.content) AS content,
                r.cwd, r.git_branch, r.source,
                c.reasoning, c.reasoning_summary, r.reasoning_tokens, r.content_tokens,
                r.tools_tokens
         FROM raw_entries r
         LEFT JOIN rdb.cot c ON c.entry_id = r.id
         LEFT JOIN fdb.fulltext f ON f.entry_id = r.id
         WHERE r.id = ?1",
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
                reasoning: row.get(11)?,
                reasoning_summary: row.get(12)?,
                reasoning_tokens: row.get(13)?,
                content_tokens: row.get(14)?,
                min_tokens: None,
                mid_tokens: None,
                tools_tokens: row.get(15)?,
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

/// detail に応じた content 列の式を返す (r=raw_entries, cp=cdb.entry_compact, f=fdb.fulltext)。
/// compact 未生成のエントリ (取り込み直後で consolidate 前など) は inline 先頭 200 文字に
/// フォールバックする。これは要約が付くのが consolidate 後という既存挙動と一貫している。
/// detail は呼び出し側で固定集合に検証済みなので、式の埋め込みは安全 (SQL インジェクト無し)。
fn detail_content_expr(detail: &str) -> &'static str {
    match detail {
        "full" => "COALESCE(f.content, r.content)",
        "mid" => "COALESCE(cp.mid_text, substr(r.content, 1, 200))",
        // "min" (既定) とその他は最大圧縮。
        _ => "COALESCE(cp.min_text, substr(r.content, 1, 200))",
    }
}

/// get_session 系の共通 JOIN (compact 投影 + 全文原本)。
const DETAIL_JOINS: &str = "LEFT JOIN cdb.entry_compact cp ON cp.entry_id = r.id \
     LEFT JOIN fdb.fulltext f ON f.entry_id = r.id";

/// RawEntry を get_session 系の列順 (content_tokens/min_tokens/mid_tokens/tools_tokens 付き) で組み立てる。
fn map_session_row(row: &rusqlite::Row) -> rusqlite::Result<RawEntry> {
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
        reasoning: None,
        reasoning_summary: None,
        reasoning_tokens: row.get(11)?,
        content_tokens: row.get(12)?,
        min_tokens: row.get(13)?,
        mid_tokens: row.get(14)?,
        tools_tokens: row.get(15)?,
    })
}

/// 特定セッションのエントリ取得。detail = "gist"|"min"|"mid"|"full" で content の層を選ぶ
/// (gist はサーバ層で要約だけ返すため、ここに来る場合は min と同義に扱う)。
pub fn get_session_entries(
    conn: &Connection,
    session_id: &str,
    entry_type: Option<&str>,
    detail: &str,
    limit: i64,
) -> Result<Vec<RawEntry>> {
    let content_expr = detail_content_expr(detail);
    let cols = format!(
        "r.id, r.session_id, r.timestamp, r.date, r.time, r.type, r.tool_name,
         {content_expr} AS content, r.cwd, r.git_branch, r.source,
         r.reasoning_tokens, r.content_tokens, cp.min_tokens, cp.mid_tokens,
         r.tools_tokens"
    );

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
                "SELECT {cols}
                 FROM raw_entries r {DETAIL_JOINS}
                 WHERE r.session_id = ?1 AND r.type IN ({})
                 ORDER BY r.timestamp ASC LIMIT ?{limit_idx}",
                placeholders.join(", ")
            ),
            pv,
        )
    } else {
        (
            format!(
                "SELECT {cols}
                 FROM raw_entries r {DETAIL_JOINS}
                 WHERE r.session_id = ?1 AND r.type IN ('user', 'assistant')
                 ORDER BY r.timestamp ASC LIMIT ?2"
            ),
            vec![Box::new(session_id.to_string()), Box::new(limit)],
        )
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), map_session_row)?;

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
    detail: &str,
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

    let content_expr = detail_content_expr(detail);
    let sql = format!(
        "SELECT r.id, r.session_id, r.timestamp, r.date, r.time, r.type, r.tool_name,
                {content_expr} AS content, r.cwd, r.git_branch, r.source,
                r.reasoning_tokens, r.content_tokens, cp.min_tokens, cp.mid_tokens,
                r.tools_tokens
         FROM raw_entries r {DETAIL_JOINS}
         WHERE r.date >= ?1 AND r.date <= ?2 AND r.type IN ({})
         ORDER BY r.timestamp ASC LIMIT ?{limit_idx}",
        placeholders.join(", ")
    );

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_ref.as_slice(), map_session_row)?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

// ---------------------------------------------------------------------------
// compact 投影 (cdb.entry_compact) — 会話再構成用の軽量表現 (min/mid 2 段)
// ---------------------------------------------------------------------------

/// compact 未生成 or recipe_version が古いエントリを最大 limit 件生成する。
/// 全文 (fdb 退避分を含む) を畳んで min/mid を作り、旧データの content_tokens も
/// 併せて埋める。戻り値: 生成した件数 (0 ならバックログは空)。
pub fn generate_compact_batch(conn: &mut Connection, limit: i64) -> Result<u64> {
    let rows: Vec<(i64, String, Option<String>, String)> = {
        let mut stmt = conn.prepare(
            "SELECT r.id, r.type, r.tool_name, COALESCE(f.content, r.content)
             FROM raw_entries r
             LEFT JOIN fdb.fulltext f ON f.entry_id = r.id
             LEFT JOIN cdb.entry_compact cp ON cp.entry_id = r.id
             WHERE cp.entry_id IS NULL OR cp.recipe_version < ?1
             ORDER BY r.id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![crate::compact::RECIPE_VERSION, limit], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    if rows.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction()?;
    let mut n = 0u64;
    for (id, entry_type, tool_name, content) in &rows {
        let (min_text, mid_text) =
            crate::compact::compact_pair(entry_type, tool_name.as_deref(), content);
        let min_tokens = crate::reasoning::estimate_tokens(&min_text);
        let mid_tokens = crate::reasoning::estimate_tokens(&mid_text);
        tx.execute(
            "INSERT INTO cdb.entry_compact
                 (entry_id, min_text, mid_text, min_tokens, mid_tokens, recipe_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(entry_id) DO UPDATE SET
                 min_text = ?2, mid_text = ?3, min_tokens = ?4, mid_tokens = ?5, recipe_version = ?6",
            params![id, min_text, mid_text, min_tokens, mid_tokens, crate::compact::RECIPE_VERSION],
        )?;
        // content_tokens が旧データで未設定なら埋める
        // (取り込み時に入る新データは既に値を持つので NULL 条件で二度書きしない)。
        tx.execute(
            "UPDATE raw_entries SET content_tokens = ?1 WHERE id = ?2 AND content_tokens IS NULL",
            params![crate::reasoning::estimate_tokens(content), id],
        )?;
        n += 1;
    }
    tx.commit()?;
    Ok(n)
}

/// compact 投影を全消し → 全再生成する (レシピ更新時)。戻り値: 生成総数。
/// FTS の rebuild と同じく「派生物の作り直し」であり、原本には触れない。
pub fn rebuild_compact(conn: &mut Connection) -> Result<u64> {
    conn.execute("DELETE FROM cdb.entry_compact", [])?;
    let mut total = 0u64;
    loop {
        let n = generate_compact_batch(conn, 2000)?;
        if n == 0 {
            break;
        }
        total += n;
    }
    Ok(total)
}
