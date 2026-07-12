use anyhow::Result;
use axum::{extract::State, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, atomic::{AtomicI32, Ordering}};
use tokio::sync::Mutex;

use crate::{config::Config, db, ingest};

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
    let port = crate::client::default_port();

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
            let mut result = format_entries(&entries);

            // 意味検索: 埋め込みが有効なら、蒸留済み要約・事実からも近傍を引いて
            // 追記する。FTS が言い回しの違いで取りこぼすものを補完する。
            // 埋め込みサービス不達時は静かに FTS のみとなる。
            result.extend(semantic_hits(conn, query));

            Ok(serde_json::to_string_pretty(&result)?)
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
                    // 全文原本 (fdb) は数 MB になりうるので、MCP 転送はエージェントの
                    // コンテキスト保護のため上限で切る。DB 内の原本は無損失のまま。
                    "content": cap_for_transport(&e.content),
                    "cwd": e.cwd,
                    "git_branch": e.git_branch,
                    "source": e.source,
                }))?),
                None => Ok(format!("No entry found with id: {id}")),
            }
        }

        // CoT を必要時のみ取得する。mode='summary'(既定)は圧縮版を遅延生成・キャッシュ、
        // mode='full' は生 CoT を返す。検索/セッション結果は CoT を含まないので、
        // has_reasoning=true のエントリについてのみ id を渡して呼ぶ。
        "memory_get_reasoning" => {
            let id = args.get("id")
                .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
                .unwrap_or(0);
            let mode = args.get("mode").and_then(|m| m.as_str()).unwrap_or("summary");

            let entry = db::get_entry(conn, id)?;
            let entry = match entry {
                Some(e) => e,
                None => return Ok(format!("No entry found with id: {id}")),
            };
            let raw = match entry.reasoning {
                Some(r) => r,
                None => return Ok(json!({
                    "id": id,
                    "has_reasoning": false,
                    "message": "No chain-of-thought stored for this entry.",
                }).to_string()),
            };

            if mode == "full" {
                Ok(json!({
                    "id": id,
                    "mode": "full",
                    "reasoning": cap_for_transport(&raw),
                    "reasoning_tokens": entry.reasoning_tokens,
                }).to_string())
            } else {
                // summary: キャッシュがあれば使い、なければ生成して保存
                let (summary, summarizer) = match entry.reasoning_summary {
                    Some(s) => (s, "cached"),
                    None => {
                        let cfg = Config::load().unwrap_or_default();
                        let llm = crate::reasoning::LlmConfig {
                            enabled: cfg.reasoning_llm_enabled,
                            base_url: cfg.reasoning_llm_base_url,
                            model: cfg.reasoning_llm_model,
                            timeout_secs: cfg.reasoning_llm_timeout_secs,
                        };
                        let result = crate::reasoning::summarize_best(&raw, Some(&llm));
                        if let Err(e) = db::set_reasoning_summary(conn, id, &result.text) {
                            eprintln!("agents-relay server: failed to cache reasoning summary: {e}");
                        }
                        let kind = if result.used_llm { "llm" } else { "heuristic" };
                        (result.text, kind)
                    }
                };
                Ok(json!({
                    "id": id,
                    "mode": "summary",
                    "summarizer": summarizer,
                    "reasoning_summary": summary,
                    "summary_tokens": crate::reasoning::estimate_tokens(&summary),
                    "full_tokens": entry.reasoning_tokens,
                    "hint": "Call again with mode='full' for the complete chain-of-thought.",
                }).to_string())
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
                .map(|(sid, first, last, d, count)| {
                    let mut obj = json!({
                        "session_id": sid,
                        "first_timestamp": first,
                        "last_timestamp": last,
                        "date": d,
                        "entry_count": count,
                    });
                    // consolidate 済みならセッション要約の先頭を添える
                    if let Ok(Some(summary)) = db::get_summary(conn, sid) {
                        let text = strip_summarizer_marker(&summary);
                        obj["summary"] = json!(truncate_chars_srv(text, 300));
                    }
                    obj
                })
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
            // detail: 会話をどの層で返すか。既定 "min" (最大圧縮 = 最小トークン)。
            //   gist = セッション要約だけ / min = 骨子 / mid = 流れと意図 / full = 本文全文
            let detail = match args.get("detail").and_then(|d| d.as_str()).unwrap_or("min") {
                d @ ("gist" | "min" | "mid" | "full") => d,
                _ => "min",
            };

            // gist: セッション要約だけを返す (要約が無ければ min にフォールバック)。
            if detail == "gist" {
                if let Some(sid) = session_id {
                    if let Ok(Some(summary)) = db::get_summary(conn, sid) {
                        return Ok(serde_json::to_string_pretty(&json!([{
                            "type": "session_summary",
                            "session_id": sid,
                            "content": strip_summarizer_marker(&summary),
                            "detail": "gist",
                            "hint": "Call again with detail='min'|'mid'|'full' for the actual messages.",
                        }]))?);
                    }
                }
            }
            // DB から取り出す実 detail (gist は要約が無かった場合 min で本文を返す)。
            let db_detail = if detail == "gist" { "min" } else { detail };

            let entries = if let Some(sid) = session_id {
                db::get_session_entries(conn, sid, entry_type, db_detail, limit)?
            } else if date.is_some() || date_from.is_some() || date_to.is_some() {
                let d_from = date.or(date_from).unwrap_or("2000-01-01");
                let d_to = date.or(date_to).unwrap_or("2099-12-31");
                db::get_entries_by_date_range(conn, d_from, d_to, entry_type, db_detail, limit)?
            } else {
                anyhow::bail!("Provide session_id or date/date_from/date_to");
            };

            let mut result: Vec<Value> = entries
                .iter()
                .map(|e| {
                    let mut obj = json!({
                        "id": e.id,
                        "session_id": e.session_id,
                        "timestamp": e.timestamp,
                        "time": e.time,
                        "type": e.entry_type,
                        "tool_name": e.tool_name,
                        // detail=full は fdb 全文原本 (数 MB 可) を返しうるので、
                        // 1 エントリずつ転送上限で切る (min/mid はレシピ上限で有界なので実質 no-op)。
                        // DB 内の原本は無損失のまま。
                        "content": cap_for_transport(&e.content),
                        "source": e.source,
                    });
                    add_reasoning_pointer(&mut obj, e.reasoning_tokens);
                    add_detail_menu(&mut obj, e);
                    obj
                })
                .collect();
            // 先頭に、現在の detail と切り替え方を示すヘッダを 1 つ付ける。
            result.insert(0, json!({
                "type": "detail_info",
                "detail": db_detail,
                "hint": format!(
                    "Returned at detail='{db_detail}'. Levels gist<min<mid<full cost progressively more tokens; \
                     each message's 'tokens' shows the per-level cost. Default is 'min' — escalate only for messages you need. \
                     memory_get_entry = one full message, memory_get_reasoning = chain-of-thought."
                ),
            }));
            // セッション指定なら、consolidate 済み要約を先頭に付ける
            if let Some(sid) = session_id {
                if let Ok(Some(summary)) = db::get_summary(conn, sid) {
                    result.insert(0, json!({
                        "type": "session_summary",
                        "session_id": sid,
                        "content": strip_summarizer_marker(&summary),
                    }));
                }
            }
            Ok(serde_json::to_string_pretty(&result)?)
        }

        // consolidate が蓄積した持続的事実 (決定・制約・好み・環境) を検索する
        "memory_facts" => {
            let query = args.get("query").and_then(|q| q.as_str()).unwrap_or("");
            let ws_filter = args.get("workspace").and_then(|w| w.as_str()).or(workspace);
            let limit = args.get("limit")
                .and_then(|l| l.as_i64().or_else(|| l.as_f64().map(|f| f as i64)))
                .unwrap_or(20);

            let facts = db::search_facts(conn, query, ws_filter, limit)?;
            let result: Vec<Value> = facts
                .iter()
                .map(|(id, category, fact, ws, sid, created)| json!({
                    "id": id,
                    "category": category,
                    "fact": fact,
                    "workspace": ws,
                    "session_id": sid,
                    "created_at": created,
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
        .map(|e| {
            let mut obj = json!({
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
            });
            // CoT 本文は返さず、存在を示すポインタだけ付ける（必要時 memory_get_reasoning で取得）
            add_reasoning_pointer(&mut obj, e.reasoning_tokens);
            obj
        })
        .collect()
}

/// 検索/セッション結果に CoT ポインタを付与する。
/// 本文は含めず、has_reasoning と概算トークン数のみ。これでデフォルトのトークン消費を抑える。
/// 意味検索: クエリを埋め込み、要約・事実の近傍を返す。
/// 埋め込み無効・サービス不達・索引未生成のときは空を返す (検索自体は FTS で成立)。
fn semantic_hits(conn: &rusqlite::Connection, query: &str) -> Vec<Value> {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return Vec::new();
    }
    let cfg = Config::load().unwrap_or_default();
    if cfg.embedding_model.is_empty() {
        return Vec::new();
    }
    // 索引が未生成なら何もしない
    if !matches!(db::vec_state(conn), Ok(Some(_))) {
        return Vec::new();
    }
    let emb = match crate::reasoning::embed_texts(
        &[trimmed.to_string()],
        &cfg.embedding_model,
        &cfg.reasoning_llm_base_url,
        10, // クエリ埋め込みは検索の体感を守るため短いタイムアウト
    ) {
        Ok(mut v) if !v.is_empty() => v.remove(0),
        _ => return Vec::new(),
    };

    let mut out = Vec::new();

    if let Ok(hits) = db::vec_search(conn, "vec_summaries", &emb, 3) {
        for (id, distance) in hits {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT session_id, summary_md FROM summaries WHERE id = ?1",
                    rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            if let Some((session_id, summary)) = row {
                out.push(json!({
                    "match": "semantic",
                    "kind": "session_summary",
                    "session_id": session_id,
                    "content": truncate_chars_srv(strip_summarizer_marker(&summary), 300),
                    "distance": (distance * 1000.0).round() / 1000.0,
                }));
            }
        }
    }

    if let Ok(hits) = db::vec_search(conn, "vec_facts", &emb, 3) {
        for (id, distance) in hits {
            let row: Option<(String, String, Option<String>)> = conn
                .query_row(
                    "SELECT category, fact, workspace FROM facts WHERE id = ?1 AND superseded_by IS NULL",
                    rusqlite::params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .ok();
            if let Some((category, fact, ws)) = row {
                out.push(json!({
                    "match": "semantic",
                    "kind": "fact",
                    "category": category,
                    "content": fact,
                    "workspace": ws,
                    "distance": (distance * 1000.0).round() / 1000.0,
                }));
            }
        }
    }

    out
}

/// summaries の summary_md 先頭にある `<!-- summarizer: ... -->` マーカーを剥がす
fn strip_summarizer_marker(summary: &str) -> &str {
    if let Some(rest) = summary.strip_prefix("<!--") {
        if let Some(end) = rest.find("-->") {
            return rest[end + 3..].trim_start();
        }
    }
    summary
}

/// マルチバイト境界を壊さずに文字数で切り詰める (表示用)
fn truncate_chars_srv(s: &str, max_chars: usize) -> String {
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

/// MCP レスポンスに載せる content の上限 (bytes)。fdb の全文原本は数 MB に
/// なりうるため、呼び出し側エージェントのコンテキストを守る目的で転送だけ切る。
const TRANSPORT_CONTENT_MAX: usize = 200_000;

fn cap_for_transport(content: &str) -> String {
    if content.len() <= TRANSPORT_CONTENT_MAX {
        return content.to_string();
    }
    let end = db::truncate_at_char_boundary(content, TRANSPORT_CONTENT_MAX);
    format!(
        "{}... [transport limit: full {} bytes stored losslessly in DB]",
        &content[..end],
        content.len()
    )
}

fn add_reasoning_pointer(obj: &mut Value, reasoning_tokens: Option<i64>) {
    if let Some(tokens) = reasoning_tokens {
        if tokens > 0 {
            obj["has_reasoning"] = json!(true);
            obj["reasoning_tokens"] = json!(tokens);
        }
    }
}

/// エントリに detail 別の概算トークン量メニュー ({min, mid, full}) を付ける。
/// 呼び出し側エージェントが「今は min、重ければ mid/full」を自分で選べるようにする
/// (ポインタ経済: 本体は返さず、コストの目安だけ返す)。値が無いレベルは省く。
fn add_detail_menu(obj: &mut Value, e: &db::RawEntry) {
    let mut menu = serde_json::Map::new();
    if let Some(t) = e.min_tokens {
        menu.insert("min".to_string(), json!(t));
    }
    if let Some(t) = e.mid_tokens {
        menu.insert("mid".to_string(), json!(t));
    }
    if let Some(t) = e.content_tokens {
        menu.insert("full".to_string(), json!(t));
    }
    if !menu.is_empty() {
        obj["tokens"] = Value::Object(menu);
    }
}
