use anyhow::Result;
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use crate::client;
use crate::config::Config;
use crate::detect;
use crate::workspace::WorkspaceConfig;


/// サーバー起動時に自動決定されるスコープ + セッション中の許可状態
struct ServerScope {
    client: String,
    /// DB上の source 値にマッピング (claude, antigravity, ...)
    default_source: Option<String>,
    default_workspace: Option<String>,
    ws_config: WorkspaceConfig,
    /// セッション中の一時上書き (None=ws_configに従う, Some(true/false)=確認済み)
    session_cross_override: Option<bool>,
    /// 接続中のサーバーポート
    server_port: u16,
    /// グローバル+ワークスペース設定を合成した有効/無効フラグ
    enabled: bool,
}

impl ServerScope {
    fn is_cross_allowed(&self) -> Option<bool> {
        if let Some(v) = self.session_cross_override {
            return Some(v);
        }
        match self.ws_config.cross_scope.as_str() {
            "allow" => Some(true),
            "deny"  => Some(false),
            _       => None, // "ask" = 未確認
        }
    }
}

/// クライアント名 → DB の source 値へマッピング
fn client_to_source(client: &str) -> Option<String> {
    match client {
        "claude-code"  => Some("claude".to_string()),
        "antigravity"  => Some("antigravity".to_string()),
        _ => None,
    }
}

/// 起動ログをファイルとstderrに同時出力するヘルパー
fn timelog(log: &mut std::fs::File, t0: std::time::Instant, msg: &str) {
    use std::io::Write;
    let elapsed = t0.elapsed().as_millis();
    let line = format!("[+{}ms] {}\n", elapsed, msg);
    let _ = log.write_all(line.as_bytes());
    eprint!("agents-relay: {}", line);
}

/// MCP stdio サーバーを起動（thin proxy）
pub fn serve(workspace: Option<String>) -> Result<()> {
    let t0 = std::time::Instant::now();

    // ログファイルを開く
    let log_path = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".agents-relay")
        .join("serve.log");
    std::fs::create_dir_all(log_path.parent().unwrap()).ok();
    let mut log = std::fs::OpenOptions::new()
        .create(true).append(true).open(&log_path)
        .unwrap_or_else(|_| std::fs::OpenOptions::new()
            .create(true).write(true).open("/dev/null").unwrap());

    {
        use std::io::Write;
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(log, "\n=== serve started at {} ===", now);
    }
    timelog(&mut log, t0, "start");

    // サーバーを自動起動（または既存に接続）
    let server_port = client::ensure_server_running()?;
    timelog(&mut log, t0, &format!("ensure_server_running done (port={})", server_port));

    client::register(server_port);
    timelog(&mut log, t0, "register done");

    // スコープ自動決定
    let cli = detect::detect_from_ppid();
    timelog(&mut log, t0, &format!("detect_from_ppid done (client={})", cli));
    let default_source = client_to_source(&cli);
    let default_workspace = workspace.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    });

    let ws_config = WorkspaceConfig::load(default_workspace.as_deref());
    timelog(&mut log, t0, &format!("workspace config loaded (cross_scope={})", ws_config.cross_scope));

    // enabled: ワークスペース設定 > グローバル設定 > デフォルト(true)
    let global_config = Config::load().unwrap_or_default();
    let enabled = ws_config.enabled.unwrap_or(global_config.enabled);
    timelog(&mut log, t0, &format!("config loaded (enabled={enabled})"));

    let mut scope = ServerScope {
        client: cli,
        default_source,
        default_workspace,
        ws_config,
        session_cross_override: None,
        server_port,
        enabled,
    };

    timelog(&mut log, t0, &format!(
        "scope ready: client={}, source={}, workspace={}",
        scope.client,
        scope.default_source.as_deref().unwrap_or("(all)"),
        scope.default_workspace.as_deref().unwrap_or("(all)"),
    ));

    let stdin = io::stdin();
    let mut stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // リクエストログ
        {
            let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("?");
            let tool_name = request.get("params")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str());
            let tool_args_summary = request.get("params")
                .and_then(|p| p.get("arguments"))
                .map(|a| {
                    let keys: Vec<&str> = a.as_object()
                        .map(|o| o.keys().map(|k| k.as_str()).collect())
                        .unwrap_or_default();
                    keys.join(",")
                });
            if let Some(tn) = tool_name {
                let args_str = tool_args_summary.as_deref().unwrap_or("");
                timelog(&mut log, t0, &format!(">> {} tool={} args=[{}]", method, tn, args_str));
            } else {
                timelog(&mut log, t0, &format!(">> {}", method));
            }
        }

        // initialize から clientInfo / roots を拾って上書き
        if request.get("method").and_then(|m| m.as_str()) == Some("initialize") {
            if let Some(name) = request
                .get("params")
                .and_then(|p| p.get("clientInfo"))
                .and_then(|c| c.get("name"))
                .and_then(|n| n.as_str())
            {
                eprintln!("agents-relay: clientInfo.name={name}");
                scope.client = detect::normalize_client_info(name);
                scope.default_source = client_to_source(&scope.client);
            }

            if scope.default_workspace.is_none() {
                if let Some(uri) = request
                    .get("params")
                    .and_then(|p| p.get("roots"))
                    .and_then(|r| r.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|r| r.get("uri"))
                    .and_then(|u| u.as_str())
                {
                    let path = uri.strip_prefix("file://").unwrap_or(uri);
                    scope.default_workspace = Some(path.to_string());
                    eprintln!("agents-relay: workspace from roots={path}");
                }
            }
        }

        let response = handle_request(&request, &mut scope);
        if response.is_null() {
            continue;
        }
        let response_str = serde_json::to_string(&response)?;
        writeln!(stdout, "{response_str}")?;
        stdout.flush()?;
    }

    eprintln!("agents-relay: client disconnected");
    client::deregister(scope.server_port);

    Ok(())
}

fn handle_request(request: &Value, scope: &mut ServerScope) -> Value {
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let params = request.get("params").cloned().unwrap_or(json!({}));

    match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "agents-relay",
                    "version": env!("CARGO_PKG_VERSION"),
                    "detectedClient": scope.client,
                    "defaultSource": scope.default_source,
                    "defaultWorkspace": scope.default_workspace,
                },
                "instructions": "This server provides cross-session memory search. To minimize token usage, ONLY call memory_search when the user explicitly asks about past sessions (e.g., 'what did we do last time', 'remember when...', 'in a previous conversation'). Never search speculatively or proactively. Most tasks can be completed without cross-session context."
            }
        }),
        "notifications/initialized" => Value::Null,
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": tool_definitions(scope) }
        }),
        "tools/call" => {
            let result = handle_tool_call(&params, scope);
            match result {
                Ok(content) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": content }]
                    }
                }),
                Err(e) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": format!("Error: {e}") }],
                        "isError": true
                    }
                }),
            }
        }
        _ => {
            if request.get("id").is_none() {
                eprintln!("agents-relay: ignoring notification: {method}");
                return Value::Null;
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            })
        }
    }
}

fn tool_definitions(scope: &ServerScope) -> Value {
    let mut tools = vec![
        json!({
            "name": "memory_search",
            "description": "Search past session memory. ONLY use when the user explicitly references prior sessions (e.g., 'what did we do last time', 'remember when...', 'in a previous conversation'). Do NOT call proactively or speculatively — the token cost is high and most tasks do not require cross-session context. When in doubt, do NOT search. By default, results are scoped to the current agent's source and workspace. Use scope='cross' for all agents in same workspace, or scope='all' for everything (both require user confirmation). If results are empty, do NOT fall back to sqlite3 or other tools.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query":      { "type": "string", "description": "FTS search query" },
                    "date":       { "type": "string", "description": "Filter by date (YYYY-MM-DD)" },
                    "date_from":  { "type": "string", "description": "Date range start" },
                    "date_to":    { "type": "string", "description": "Date range end" },
                    "type":       { "type": "string", "description": "Filter by type (user/assistant/system)" },
                    "session_id": { "type": "string", "description": "Filter by session" },
                    "limit":      { "type": "number", "description": "Max results (default 20)" },
                    "scope":      { "type": "string", "description": "'self' (default), 'cross' (all agents, same workspace), 'all' (everything). cross/all require user confirmation." },
                    "confirmed":  { "type": "boolean", "description": "Set true ONLY after user explicitly approved cross/all scope." },
                    "opt_out":    { "type": "boolean", "description": "Set true if user declined. Saves 'deny' to .agents-relay.json." },
                    "workspace":  { "type": "string", "description": "Override workspace filter (advanced)." },
                    "source":     { "type": "string", "description": "Override source filter: claude, antigravity (advanced)." }
                },
                "required": ["query"]
            },
            "_meta": { "anthropic/alwaysLoad": true }
        }),
        json!({
            "name": "memory_get_session",
            "description": "Retrieve full conversation history of a specific session. High token cost — only use when the user needs to review or continue specific prior work and has identified the session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "date":       { "type": "string" },
                    "date_from":  { "type": "string" },
                    "date_to":    { "type": "string" },
                    "type":       { "type": "string", "description": "Filter type (default: user,assistant)" },
                    "limit":      { "type": "number", "description": "Max results (default 50)" }
                }
            },
            "_meta": { "anthropic/alwaysLoad": true }
        }),
    ];

    tools.push(json!({
        "name": "memory_set_enabled",
        "description": "Enable or disable agents-relay for the current workspace or globally. Changes are saved to .agents-relay.json (workspace) or ~/.agents-relay/config.json (global).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "enabled": { "type": "boolean", "description": "true to enable, false to disable" },
                "scope":   { "type": "string", "description": "\"workspace\" (default) or \"global\"" }
            },
            "required": ["enabled"]
        }
    }));

    // Antigravity 専用ツール
    if scope.client == "antigravity" || scope.default_source.as_deref() == Some("antigravity") {
        tools.push(json!({
            "name": "memory_find_workspace",
            "description": "Identify and record which workspace this session belongs to. \
                IMPORTANT: Call this at the very start of every session. \
                Pass the workspace root as an absolute path (e.g. /Volumes/DISK/dev/my-project).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "conversation_id": { "type": "string" },
                    "workspace":       { "type": "string", "description": "Absolute path to workspace root" },
                    "started_at":      { "type": "string", "description": "Session start time ISO8601 (optional)" }
                },
                "required": ["conversation_id", "workspace"]
            }
        }));
        tools.push(json!({
            "name": "memory_unlock_cross_scope",
            "description": "Request permission to search memory across ALL workspaces. Ask user first: 'May I also search memory from other workspaces?'",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "confirmed": { "type": "boolean" }
                }
            }
        }));
    }

    Value::Array(tools)
}

/// thin proxy: スコープを解決してからサーバーに転送
fn handle_tool_call(params: &Value, scope: &mut ServerScope) -> Result<String> {
    let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    // enabled チェック: false なら全ツールが即 no-op を返す
    if !scope.enabled {
        return Ok(json!({
            "disabled": true,
            "message": "agents-relay is disabled. To enable, set \"enabled\": true in ~/.agents-relay/config.json or .agents-relay.json in your workspace."
        }).to_string());
    }

    // memory_set_enabled: グローバルまたはワークスペースの有効/無効を切り替え
    if tool_name == "memory_set_enabled" {
        let enabled = args.get("enabled")
            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
            .ok_or_else(|| anyhow::anyhow!("enabled parameter is required"))?;
        let target_scope = args.get("scope").and_then(|s| s.as_str()).unwrap_or("workspace");

        match target_scope {
            "global" => {
                let mut cfg = Config::load().unwrap_or_default();
                cfg.enabled = enabled;
                cfg.save()?;
                scope.enabled = enabled;
                return Ok(json!({
                    "status": "ok",
                    "scope": "global",
                    "enabled": enabled,
                    "message": format!("agents-relay globally {}. Saved to ~/.agents-relay/config.json.",
                        if enabled { "enabled" } else { "disabled" }),
                }).to_string());
            }
            _ => {
                // workspace (default)
                scope.ws_config.enabled = Some(enabled);
                scope.ws_config.save(scope.default_workspace.as_deref())?;
                scope.enabled = enabled;
                let ws = scope.default_workspace.as_deref().unwrap_or("(unknown)");
                return Ok(json!({
                    "status": "ok",
                    "scope": "workspace",
                    "enabled": enabled,
                    "workspace": ws,
                    "message": format!("agents-relay {} for workspace '{}'. Saved to .agents-relay.json.",
                        if enabled { "enabled" } else { "disabled" }, ws),
                }).to_string());
            }
        }
    }

    // memory_unlock_cross_scope はサーバー呼び出し不要（serve 側のセッション状態操作のみ）
    if tool_name == "memory_unlock_cross_scope" {
        let confirmed = args.get("confirmed")
            .map(|c| c.as_bool().unwrap_or_else(|| c.as_str() == Some("true")))
            .unwrap_or(false);
        if !confirmed {
            return Ok(json!({
                "status": "confirmation_required",
                "message": "Cross-workspace memory access requested.\n\
                    Please ask the user: 'May I also search memory from other workspaces?'\n\
                    If approved, call again with confirmed=true.\n\
                    If declined, do NOT call again."
            }).to_string());
        }
        scope.session_cross_override = Some(true);
        eprintln!("agents-relay: cross-scope unlocked via memory_unlock_cross_scope");
        return Ok(json!({
            "status": "unlocked",
            "message": "Cross-workspace memory access is now enabled for this session."
        }).to_string());
    }

    // memory_search のスコープゲート処理
    if tool_name == "memory_search" {
        let search_scope = args.get("scope").and_then(|s| s.as_str()).unwrap_or("self");
        let confirmed = args.get("confirmed")
            .map(|c| c.as_bool().unwrap_or_else(|| c.as_str() == Some("true")))
            .unwrap_or(false);
        let opt_out = args.get("opt_out")
            .map(|c| c.as_bool().unwrap_or_else(|| c.as_str() == Some("true")))
            .unwrap_or(false);

        // opt_out: 永続拒否を保存
        if opt_out {
            scope.session_cross_override = Some(false);
            scope.ws_config.cross_scope = "deny".to_string();
            if let Err(e) = scope.ws_config.save(scope.default_workspace.as_deref()) {
                eprintln!("agents-relay: failed to save workspace config: {e}");
            }
            return Ok(json!({
                "status": "opt_out_accepted",
                "message": "Cross-scope access disabled for this workspace. Saved to .agents-relay.json.",
            }).to_string());
        }

        let needs_cross = search_scope == "cross" || search_scope == "all";
        if needs_cross {
            let allowed = scope.is_cross_allowed();

            // 拒否済み → self にフォールバックして実行
            if allowed == Some(false) {
                let notice = "Cross-scope denied for this workspace. Falling back to scope='self'.";
                eprintln!("agents-relay: {notice}");
                let ws = args.get("workspace").and_then(|w| w.as_str())
                    .or(scope.default_workspace.as_deref());
                let src = args.get("source").and_then(|s| s.as_str())
                    .or(scope.default_source.as_deref());
                let (resp, _) = client::call_tool(
                    scope.server_port, tool_name, &args,
                    ws, src, &scope.client,
                )?;
                let entries: Value = serde_json::from_str(&resp.content).unwrap_or(json!([]));
                let mut result = entries.as_array().cloned().unwrap_or_default();
                result.insert(0, json!({"_notice": notice}));
                return Ok(serde_json::to_string_pretty(&result)?);
            }

            // 未確認 かつ confirmed なし → 確認を要求
            if allowed.is_none() && !confirmed {
                let scope_desc = if search_scope == "all" {
                    "all agents and ALL workspaces (including other projects)"
                } else {
                    "all agents in this workspace (including other AI tools' memories)"
                };
                return Ok(json!({
                    "status": "confirmation_required",
                    "scope_requested": search_scope,
                    "message": format!(
                        "⚠️ This search requires access to memories beyond your current scope.\n\
                         Requested: {scope_desc}\n\n\
                         Please ask the user: 'May I also search memory from {}?'\n\n\
                         If approved: call again with confirmed=true.\n\
                         If declined: call again with opt_out=true.",
                        if search_scope == "all" { "other workspaces and AI tools" } else { "other AI tools in this workspace" }
                    ),
                    "query": args.get("query"),
                }).to_string());
            }

            // confirmed=true → セッション中は許可
            if confirmed {
                scope.session_cross_override = Some(true);
                eprintln!("agents-relay: cross-scope granted by user");
            }
        }

        // スコープ解決: serve 側で workspace/source を確定してからサーバーへ
        let (workspace, source) = match search_scope {
            "all" => (None, None),
            "cross" => {
                let ws = args.get("workspace").and_then(|w| w.as_str())
                    .or(scope.default_workspace.as_deref());
                (ws, None)
            }
            _ => {
                let ws = args.get("workspace").and_then(|w| w.as_str())
                    .or(scope.default_workspace.as_deref());
                let src = args.get("source").and_then(|s| s.as_str())
                    .or(scope.default_source.as_deref());
                (ws, src)
            }
        };

        let (resp, new_port) = client::call_tool(
            scope.server_port, tool_name, &args,
            workspace, source, &scope.client,
        )?;
        scope.server_port = new_port;
        if resp.is_error {
            anyhow::bail!("{}", resp.content);
        }

        // _context 付与: 結果に自分以外の source が混在していたら毎回 AI に通知する
        {
            let mut entries: Value = serde_json::from_str(&resp.content).unwrap_or(json!([]));
            if let Some(arr) = entries.as_array_mut() {
                if !arr.is_empty() {
                    // source ごとのカウントを集計
                    let mut breakdown: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
                    for entry in arr.iter() {
                        let src = entry.get("source").and_then(|s| s.as_str()).unwrap_or("unknown");
                        *breakdown.entry(src.to_string()).or_insert(0) += 1;
                    }

                    let my_source = scope.default_source.as_deref();
                    let has_foreign = match my_source {
                        Some(s) => breakdown.keys().any(|k| k != s),
                        // source 不明クライアント（cursor/vscode 等）は全結果が他エージェントのものなので
                        // 結果が1件でもあれば必ず通知する
                        None => !breakdown.is_empty(),
                    };

                    if has_foreign {
                        // 自分以外の source が何件あるかを説明する warning を生成
                        let foreign_summary: Vec<String> = breakdown.iter()
                            .filter(|(k, _)| my_source.map(|s| k.as_str() != s).unwrap_or(false) || my_source.is_none())
                            .map(|(k, v)| format!("{}: {}", k, v))
                            .collect();

                        let warning = if let Some(s) = my_source {
                            let foreign_count: usize = breakdown.iter()
                                .filter(|(k, _)| k.as_str() != s)
                                .map(|(_, v)| v)
                                .sum();
                            format!(
                                "{} {} from other agent(s) ({}) are included in these results. \
                                 Do NOT treat them as your own memories. \
                                 Check the 'source' field of each entry.",
                                foreign_count,
                                if foreign_count == 1 { "entry" } else { "entries" },
                                foreign_summary.join(", ")
                            )
                        } else {
                            format!(
                                "Source filter is not available for client '{}'. \
                                 Results include memories from ALL agents. \
                                 Check the 'source' field of each entry to identify which agent it came from.",
                                scope.client
                            )
                        };

                        let breakdown_json: serde_json::Map<String, Value> = breakdown.iter()
                            .map(|(k, v)| (k.clone(), json!(v)))
                            .collect();

                        arr.insert(0, json!({
                            "_context": {
                                "your_source": my_source.unwrap_or("unknown"),
                                "results_breakdown": breakdown_json,
                                "warning": warning,
                            }
                        }));
                    }
                }
            }
            return Ok(serde_json::to_string_pretty(&entries)?);
        }
    }

    // その他のツール: workspace/source をデフォルトで渡す
    let (resp, new_port) = client::call_tool(
        scope.server_port, tool_name, &args,
        scope.default_workspace.as_deref(),
        scope.default_source.as_deref(),
        &scope.client,
    )?;
    scope.server_port = new_port;
    if resp.is_error {
        anyhow::bail!("{}", resp.content);
    }

    // memory_find_workspace の成功レスポンスで scope.default_workspace を更新する
    // 起動時の workspace 検出が失敗していても、このツール呼び出しでスコープを確定できる
    if tool_name == "memory_find_workspace" {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&resp.content) {
            if let Some(ws) = parsed.get("workspace").and_then(|w| w.as_str()) {
                eprintln!("agents-relay: workspace confirmed from memory_find_workspace: {ws}");
                scope.default_workspace = Some(ws.to_string());
            }
        }
    }

    Ok(resp.content)
}
