use anyhow::Result;
use axum::{extract::State, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, atomic::{AtomicI32, Ordering}};
use tokio::sync::Mutex;

use crate::{client::DEFAULT_PORT, config::Config, db, ingest};

#[derive(Clone)]
struct AppState {
    conn: Arc<Mutex<rusqlite::Connection>>,
    /// 接続中の serve プロセス数
    client_count: Arc<AtomicI32>,
}

/// serve → server へのツール呼び出しリクエスト
/// scope 解決は serve 側で行い、server はそのまま DB クエリに使う
#[derive(Deserialize)]
struct ToolRequest {
    name: String,
    arguments: Value,
    /// serve 側でスコープ解決済みのワークスペースフィルタ (None = 全ワークスペース)
    workspace: Option<String>,
    /// serve 側でスコープ解決済みのソースフィルタ (None = 全ソース)
    source: Option<String>,
    /// insert_entry 用クライアント識別子
    client: Option<String>,
}

#[derive(Serialize)]
struct ToolResponse {
    content: String,
    is_error: bool,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
    version: String,
    binary_mtime: u64,
    client_count: i32,
}

fn port_file_path() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".agents-relay").join("server.port")
}

fn current_binary_mtime() -> u64 {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.metadata().ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "serve" サブコマンドを持つ agents-relay プロセスが存在するか確認
fn any_serve_alive() -> bool {
    std::process::Command::new("sh")
        .args(["-c", "ps -eo args | grep -qE 'agents-relay.* serve( |$)'"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tokio::main]
pub async fn run() -> Result<()> {
    let port = DEFAULT_PORT;

    // すでに起動中なら終了
    if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{port}/health")).await {
        if resp.status().is_success() {
            eprintln!("agents-relay server: already running on port {port}, exiting.");
            return Ok(());
        }
    }

    let db_path = Config::db_path();
    let conn = db::open(&db_path)?;
    db::init(&conn)?;

    let client_count = Arc::new(AtomicI32::new(0));

    let state = AppState {
        conn: Arc::new(Mutex::new(conn)),
        client_count: client_count.clone(),
    };

    // ポートファイルを書き出す（listen より前に書くことで serve 側のポーリングが成功する）
    let pf = port_file_path();
    std::fs::create_dir_all(pf.parent().unwrap()).ok();
    std::fs::write(&pf, port.to_string()).ok();

    eprintln!("agents-relay server: listening on 127.0.0.1:{port}");

    // バックグラウンド sync（起動直後1回 + 30秒ごと）+ pgrep 安全網 + 自動終了
    let bg_state = state.clone();
    tokio::spawn(async move {
        let mut idle_ticks = 0u32;
        let mut first_tick = true;
        loop {
            if first_tick {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                first_tick = false;
            } else {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
            }

            // 全ソースを差分取り込み
            {
                let mut conn = bg_state.conn.lock().await;
                if let Err(e) = ingest::sync_all(&mut conn) {
                    eprintln!("agents-relay server: background sync error: {e}");
                }
            }

            // pgrep 安全網: serve プロセスの実態と client_count を同期
            let mut count = bg_state.client_count.load(Ordering::Relaxed);
            let serve_alive = any_serve_alive();

            if count > 0 && !serve_alive {
                eprintln!("agents-relay server: no serve processes found, resetting count");
                bg_state.client_count.store(0, Ordering::Relaxed);
                count = 0;
            }

            // 自動終了: serve が存在せずカウントも 0 なら idle カウントアップ
            // idle_ticks >= 2 = 約60秒で終了
            if count == 0 && !serve_alive {
                idle_ticks += 1;
                eprintln!(
                    "agents-relay server: idle ({idle_ticks}/2) client_count={count}"
                );
                if idle_ticks >= 2 {
                    eprintln!("agents-relay server: no clients for ~60s, shutting down");
                    std::fs::remove_file(port_file_path()).ok();
                    std::process::exit(0);
                }
            } else {
                idle_ticks = 0;
                eprintln!(
                    "agents-relay server: client_count={count} serve_alive={serve_alive}"
                );
            }
        }
    });

    let app = Router::new()
        .route("/health",     get(health_handler))
        .route("/register",   post(register_handler))
        .route("/deregister", post(deregister_handler))
        .route("/tool",       post(tool_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        binary_mtime: current_binary_mtime(),
        client_count: state.client_count.load(Ordering::Relaxed),
    })
}

async fn register_handler(State(state): State<AppState>) -> Json<Value> {
    let count = state.client_count.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("agents-relay server: serve registered (count={count})");
    Json(json!({ "client_count": count }))
}

async fn deregister_handler(State(state): State<AppState>) -> Json<Value> {
    let count = (state.client_count.fetch_sub(1, Ordering::Relaxed) - 1).max(0);
    state.client_count.store(count, Ordering::Relaxed);
    eprintln!("agents-relay server: serve deregistered (count={count})");
    Json(json!({ "client_count": count }))
}

async fn tool_handler(
    State(state): State<AppState>,
    Json(req): Json<ToolRequest>,
) -> Json<ToolResponse> {
    let mut conn = state.conn.lock().await;

    let result = handle_tool(
        &mut conn,
        &req.name,
        &req.arguments,
        req.workspace.as_deref(),
        req.source.as_deref(),
        req.client.as_deref().unwrap_or("unknown"),
    );

    match result {
        Ok(content) => Json(ToolResponse { content, is_error: false }),
        Err(e) => Json(ToolResponse {
            content: format!("Error: {e}"),
            is_error: true,
        }),
    }
}

/// 全ツールのロジック。serve 側でスコープ解決済みの workspace/source を受け取る。
fn handle_tool(
    conn: &mut rusqlite::Connection,
    tool_name: &str,
    args: &Value,
    workspace: Option<&str>,
    source: Option<&str>,
    client: &str,
) -> Result<String> {
    match tool_name {
        "memory_search" => {
            let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
            let date = args.get("date").and_then(|d| d.as_str());
            let date_from = args.get("date_from").and_then(|d| d.as_str());
            let date_to = args.get("date_to").and_then(|d| d.as_str());
            let entry_type = args.get("type").and_then(|t| t.as_str());
            let session_id = args.get("session_id").and_then(|s| s.as_str());
            let limit = args.get("limit")
                .and_then(|l| l.as_i64().or_else(|| l.as_f64().map(|f| f as i64)))
                .unwrap_or(20);

            let entries = db::search(
                conn, query, date, date_from, date_to, entry_type, session_id, limit,
                workspace, source,
            )?;
            Ok(serde_json::to_string_pretty(&format_entries(&entries))?)
        }

        "memory_get_entry" => {
            let id = args.get("id")
                .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                .unwrap_or(0);
            match db::get_entry(conn, id)? {
                Some(e) => Ok(serde_json::to_string_pretty(&json!({
                    "id": e.id,
                    "session_id": e.session_id,
                    "timestamp": e.timestamp,
                    "date": e.date,
                    "time": e.time,
                    "type": e.entry_type,
                    "tool_name": e.tool_name,
                    "content": e.content,
                    "cwd": e.cwd,
                    "git_branch": e.git_branch,
                    "source": e.source,
                }))?),
                None => Ok(format!("No entry found with id: {id}")),
            }
        }

        "memory_list_sessions" => {
            let date = args.get("date").and_then(|d| d.as_str());
            let limit = args.get("limit")
                .and_then(|l| l.as_i64().or_else(|| l.as_f64().map(|f| f as i64)))
                .unwrap_or(10);

            let sessions = db::list_sessions(conn, date, limit, workspace)?;
            let result: Vec<Value> = sessions
                .iter()
                .map(|(sid, first, last, d, count)| json!({
                    "session_id": sid,
                    "first_timestamp": first,
                    "last_timestamp": last,
                    "date": d,
                    "entry_count": count,
                }))
                .collect();
            Ok(serde_json::to_string_pretty(&result)?)
        }

        "memory_get_session" => {
            let session_id = args.get("session_id").and_then(|s| s.as_str());
            let date = args.get("date").and_then(|d| d.as_str());
            let date_from = args.get("date_from").and_then(|d| d.as_str());
            let date_to = args.get("date_to").and_then(|d| d.as_str());
            let entry_type = args.get("type").and_then(|t| t.as_str());
            let limit = args.get("limit")
                .and_then(|l| l.as_i64().or_else(|| l.as_f64().map(|f| f as i64)))
                .unwrap_or(50);

            let entries = if let Some(sid) = session_id {
                db::get_session_entries(conn, sid, entry_type, limit)?
            } else if date.is_some() || date_from.is_some() || date_to.is_some() {
                let d_from = date.or(date_from).unwrap_or("2000-01-01");
                let d_to = date.or(date_to).unwrap_or("2099-12-31");
                db::get_entries_by_date_range(conn, d_from, d_to, entry_type, limit)?
            } else {
                anyhow::bail!("Provide session_id or date/date_from/date_to");
            };

            let result: Vec<Value> = entries
                .iter()
                .map(|e| json!({
                    "id": e.id,
                    "session_id": e.session_id,
                    "timestamp": e.timestamp,
                    "time": e.time,
                    "type": e.entry_type,
                    "tool_name": e.tool_name,
                    "content": e.content,
                    "source": e.source,
                }))
                .collect();
            Ok(serde_json::to_string_pretty(&result)?)
        }

        "memory_find_workspace" => {
            let conversation_id = args
                .get("conversation_id")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if conversation_id.is_empty() {
                anyhow::bail!("conversation_id is required");
            }
            let ws = args.get("workspace")
                .and_then(|s| s.as_str())
                .or(workspace)
                .ok_or_else(|| anyhow::anyhow!(
                    "workspace is required. Pass it as a tool argument: workspace=\"/your/workspace/path\""
                ))?;

            let now = chrono::Utc::now();
            let now_iso = now.to_rfc3339();
            let started_at = args
                .get("started_at")
                .and_then(|s| s.as_str())
                .unwrap_or(&now_iso);

            // session.json を書き出し
            let dir = std::path::Path::new(ws).join(".agents-relay");
            let session_json = serde_json::to_string_pretty(&json!({
                "conversation_id": conversation_id,
                "workspace": ws,
                "started_at": started_at,
            }))?;
            if let Err(e) = std::fs::create_dir_all(&dir)
                .and_then(|_| std::fs::write(dir.join("session.json"), &session_json))
            {
                eprintln!("agents-relay server: session.json write skipped: {e}");
            }

            // Session Start エントリを DB に記録
            // source が確定している場合はそれを使う（"claude-code" → "claude" の変換済み）
            // 確定していない場合はクライアント名をそのまま使う
            let db_source = source.unwrap_or(client);
            let date = now.format("%Y-%m-%d").to_string();
            let time = now.format("%H:%M:%S").to_string();
            let content = format!("Session Started in workspace: {ws}");

            let tx = conn.transaction()?;
            db::insert_entry(
                &tx, conversation_id, &now_iso, &date, &time,
                "system", Some("session_start"), &content, Some(ws), None, db_source,
            )?;
            tx.commit()?;

            eprintln!("agents-relay server: session registered. id={conversation_id} workspace={ws}");

            Ok(json!({
                "status": "success",
                "conversation_id": conversation_id,
                "workspace": ws,
                "message": "Session registered and session.json written."
            }).to_string())
        }

        "memory_record_summary" => {
            let summary = args.get("summary").and_then(|s| s.as_str()).unwrap_or("");
            if summary.is_empty() {
                anyhow::bail!("summary parameter is required");
            }
            let session_id = args
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("agent-summary");
            let tags = args.get("tags").and_then(|t| t.as_str()).unwrap_or("");

            let content = if tags.is_empty() {
                summary.to_string()
            } else {
                format!("[Tags: {}]\n{}", tags, summary)
            };

            let now = chrono::Utc::now();
            let timestamp = now.to_rfc3339();
            let date = now.format("%Y-%m-%d").to_string();
            let time = now.format("%H:%M:%S").to_string();

            let db_source = source.unwrap_or(client);
            let tx = conn.transaction()?;
            let id = db::insert_entry(
                &tx, session_id, &timestamp, &date, &time,
                "summary", Some("skill_hook"), &content,
                workspace, None, db_source,
            )?;
            tx.commit()?;

            Ok(json!({
                "status": "success",
                "id": id,
                "message": format!("Successfully recorded summary with ID {}", id)
            }).to_string())
        }

        _ => anyhow::bail!("Unknown tool: {tool_name}"),
    }
}

fn format_entries(entries: &[db::RawEntry]) -> Vec<Value> {
    entries
        .iter()
        .map(|e| json!({
            "id": e.id,
            "session_id": e.session_id,
            "timestamp": e.timestamp,
            "date": e.date,
            "time": e.time,
            "type": e.entry_type,
            "tool_name": e.tool_name,
            "content": e.content,
            "cwd": e.cwd,
            "git_branch": e.git_branch,
            "source": e.source,
        }))
        .collect()
}
