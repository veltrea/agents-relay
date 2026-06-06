use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// cross/all スコープの許可設定: "ask" (毎回確認), "allow" (常に許可), "deny" (常に拒否)
    #[serde(default = "default_cross_scope")]
    pub cross_scope: String,
    /// このワークスペースでの有効/無効 (None = グローバル設定に従う)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn default_cross_scope() -> String {
    "ask".to_string()
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            cross_scope: default_cross_scope(),
            enabled: None,
        }
    }
}

impl WorkspaceConfig {
    pub fn config_path(workspace: &str) -> PathBuf {
        Path::new(workspace).join(".agents-relay.json")
    }

    pub fn load(workspace: Option<&str>) -> Self {
        let Some(ws) = workspace else {
            return Self::default();
        };
        std::fs::read_to_string(Self::config_path(ws))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, workspace: Option<&str>) -> Result<()> {
        let Some(ws) = workspace else {
            anyhow::bail!("No workspace path to save config to");
        };
        let path = Self::config_path(ws);
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        eprintln!("agents-relay: saved workspace config to {}", path.display());
        Ok(())
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "cross_scope" => {
                match value {
                    "ask" | "allow" | "deny" => self.cross_scope = value.to_string(),
                    _ => anyhow::bail!("Invalid value for cross_scope: {value} (use ask/allow/deny)"),
                }
            }
            "enabled" => {
                self.enabled = Some(match value {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => anyhow::bail!("Invalid value for enabled: {value} (use true/false)"),
                });
            }
            _ => anyhow::bail!("Unknown workspace config key: {key}"),
        }
        Ok(())
    }

    pub fn show(&self, workspace: &str) -> String {
        format!(
            "Workspace config: {}\n\
             enabled:     {}\n\
             cross_scope: {}",
            Self::config_path(workspace).display(),
            self.enabled.map(|v| v.to_string()).unwrap_or_else(|| "(inherit global)".to_string()),
            self.cross_scope,
        )
    }
}
