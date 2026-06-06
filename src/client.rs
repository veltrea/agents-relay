use anyhow::Result;
use serde_json::{json, Value};

/// agents-relay server のデフォルトポート (antigravity-relay の 54321 と被らないよう 54322)
pub const DEFAULT_PORT: u16 = 54322;

fn port_file_path() -> std::path::PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    home.join(".agents-relay").join("server.port")
}

pub fn read_server_port() -> u16 {
    std::fs::read_to_string(port_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(DEFAULT_PORT)
}

fn health_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/health")
}

fn tool_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/tool")
}

/// サーバーの health を取得。失敗時は None
fn get_health(port: u16) -> Option<Value> {
    reqwest::blocking::Client::new()
        .get(health_url(port))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .ok()?
        .json::<Value>()
        .ok()
}

pub fn is_server_alive(port: u16) -> bool {
    get_health(port).is_some()
}

/// サーバーが起動していなければ自動起動し、ポートを返す
pub fn ensure_server_running() -> Result<u16> {
    let port = read_server_port();

    if is_server_alive(port) {
        return Ok(port);
    }

    // サーバーをデタッチ起動
    let exe = std::env::current_exe()?;
    std::process::Command::new(&exe)
        .arg("server")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    eprintln!("agents-relay: starting server...");

    // 最大 5 秒待機（ポートファイルが書き出されるまで）
    for _ in 0..50 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let p = read_server_port();
        if is_server_alive(p) {
            eprintln!("agents-relay: server ready on port {p}");
            return Ok(p);
        }
    }

    anyhow::bail!("agents-relay server did not start within 5 seconds")
}

pub fn register(port: u16) {
    let _ = reqwest::blocking::Client::new()
        .post(format!("http://127.0.0.1:{port}/register"))
        .timeout(std::time::Duration::from_secs(2))
        .send();
}

pub fn deregister(port: u16) {
    let _ = reqwest::blocking::Client::new()
        .post(format!("http://127.0.0.1:{port}/deregister"))
        .timeout(std::time::Duration::from_secs(2))
        .send();
}

pub struct ToolResponse {
    pub content: String,
    pub is_error: bool,
}

fn do_call_tool(
    port: u16,
    name: &str,
    arguments: &Value,
    workspace: Option<&str>,
    source: Option<&str>,
    client: &str,
) -> Result<ToolResponse> {
    let body = json!({
        "name": name,
        "arguments": arguments,
        "workspace": workspace,
        "source": source,
        "client": client,
    });

    let resp = reqwest::blocking::Client::new()
        .post(tool_url(port))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()?
        .json::<Value>()?;

    let content = resp
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let is_error = resp
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);

    Ok(ToolResponse { content, is_error })
}

/// ツール呼び出しをサーバーに転送する。
/// 失敗時はサーバーを再起動して 1 回リトライする。
pub fn call_tool(
    port: u16,
    name: &str,
    arguments: &Value,
    workspace: Option<&str>,
    source: Option<&str>,
    client: &str,
) -> Result<(ToolResponse, u16)> {
    match do_call_tool(port, name, arguments, workspace, source, client) {
        Ok(resp) => Ok((resp, port)),
        Err(e) => {
            eprintln!("agents-relay: server call failed ({e}), restarting server...");
            let new_port = ensure_server_running()?;
            register(new_port);
            let resp = do_call_tool(new_port, name, arguments, workspace, source, client)?;
            Ok((resp, new_port))
        }
    }
}
