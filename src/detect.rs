/// MCPサーバーを起動した親プロセスからクライアント名を推定する

/// 親プロセスのPIDを取得
fn get_ppid() -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("PPid:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
            })
    }

    #[cfg(target_os = "macos")]
    {
        let pid = std::process::id();
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "ppid="])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()
    }

    #[cfg(target_os = "windows")]
    {
        // Windows: WMIC で親プロセスを取得
        let pid = std::process::id();
        let output = std::process::Command::new("wmic")
            .args(["process", "where", &format!("ProcessId={pid}"), "get", "ParentProcessId", "/value"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|l| l.starts_with("ParentProcessId="))
            .and_then(|l| l.split('=').nth(1))
            .and_then(|v| v.trim().parse().ok())
    }
}

/// PIDからプロセスのコマンドパスを取得
fn get_process_path(pid: u32) -> Option<String> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&output.stdout).trim().to_lowercase();
        if name.is_empty() { None } else { Some(name) }
    }

    #[cfg(target_os = "windows")]
    {
        let output = std::process::Command::new("wmic")
            .args(["process", "where", &format!("ProcessId={pid}"), "get", "Name", "/value"])
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&output.stdout)
            .lines()
            .find(|l| l.starts_with("Name="))
            .and_then(|l| l.split('=').nth(1))
            .map(|s| s.trim().to_lowercase())?;
        if name.is_empty() { None } else { Some(name) }
    }
}

/// プロセス名からクライアント名を正規化
fn normalize_client(process_name: &str) -> &'static str {
    let n = process_name.to_lowercase();
    if n.contains("claude") {
        "claude-code"
    } else if n.contains("antigravity") || n.contains("gemini") {
        "antigravity"
    } else if n.contains("cursor") {
        "cursor"
    } else if n.contains("windsurf") {
        "windsurf"
    } else if n == "code" || n.starts_with("code ") || n.starts_with("code-server") || n.contains("vscode") || n == "codium" {
        "vscode"
    } else if n.contains("zed") {
        "zed"
    } else {
        "unknown"
    }
}

/// PPIDからクライアント名を推定する。失敗時は \"unknown\" を返す。
pub fn detect_from_ppid() -> String {
    get_ppid()
        .and_then(|ppid| get_process_path(ppid))
        .map(|name| normalize_client(&name).to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// MCP initialize の clientInfo.name からクライアント名を正規化する
pub fn normalize_client_info(client_info_name: &str) -> String {
    normalize_client(client_info_name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_client() {
        assert_eq!(normalize_client("code"), "vscode");
        assert_eq!(normalize_client("code --version"), "vscode");
        assert_eq!(normalize_client("vscode"), "vscode");
        assert_eq!(normalize_client("codium"), "vscode");
        assert_eq!(normalize_client("code-server"), "vscode");

        // 誤検知しないことの確認
        assert_eq!(normalize_client("recode"), "unknown");
        assert_eq!(normalize_client("node"), "unknown");
        assert_eq!(normalize_client("electron"), "unknown");
        assert_eq!(normalize_client("xcode"), "unknown");

        // 既存の他のクライアント
        assert_eq!(normalize_client("claude"), "claude-code");
        assert_eq!(normalize_client("cursor"), "cursor");
        assert_eq!(normalize_client("windsurf"), "windsurf");
        assert_eq!(normalize_client("zed"), "zed");
    }
}
