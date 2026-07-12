use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_archive_dir")]
    pub archive_dir: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// CoT 要約に LLM (LM Studio 等 OpenAI 互換 API) を使うか。既定 false（opt-in）。
    /// false の場合は LLM 不使用のヒューリスティック圧縮を使う。
    #[serde(default = "default_reasoning_llm_enabled")]
    pub reasoning_llm_enabled: bool,
    /// OpenAI 互換 API のベース URL（末尾 /v1 まで）。
    #[serde(default = "default_reasoning_llm_base_url")]
    pub reasoning_llm_base_url: String,
    /// 使用モデル名。空文字なら /v1/models から自動検出する。
    #[serde(default = "default_reasoning_llm_model")]
    pub reasoning_llm_model: String,
    /// LLM 呼び出しの合計タイムアウト秒。
    #[serde(default = "default_reasoning_llm_timeout_secs")]
    pub reasoning_llm_timeout_secs: u64,
}

fn default_retention_days() -> u32 {
    30
}

fn default_archive_dir() -> String {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".agents-relay")
        .join("archive")
        .to_string_lossy()
        .to_string()
}

fn default_enabled() -> bool {
    true
}

fn default_reasoning_llm_enabled() -> bool {
    false
}

fn default_reasoning_llm_base_url() -> String {
    // LM Studio 等の OpenAI 互換サーバー。既定はローカルホスト。
    // リモートマシンを使う場合は config set reasoning_llm_base_url で上書きする。
    "http://localhost:1234/v1".to_string()
}

fn default_reasoning_llm_model() -> String {
    String::new()
}

fn default_reasoning_llm_timeout_secs() -> u64 {
    30
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
            archive_dir: default_archive_dir(),
            enabled: default_enabled(),
            reasoning_llm_enabled: default_reasoning_llm_enabled(),
            reasoning_llm_base_url: default_reasoning_llm_base_url(),
            reasoning_llm_model: default_reasoning_llm_model(),
            reasoning_llm_timeout_secs: default_reasoning_llm_timeout_secs(),
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let config: Config = serde_json::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    pub fn config_path() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        home.join(".agents-relay").join("config.json")
    }

    pub fn db_path() -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let dir = home.join(".agents-relay");
        std::fs::create_dir_all(&dir).ok();
        dir.join("memory.db")
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "retention_days" => {
                self.retention_days = value.parse()?;
            }
            "archive_dir" => {
                self.archive_dir = value.to_string();
            }
            "enabled" => {
                self.enabled = match value {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => anyhow::bail!("Invalid value for enabled: {value} (use true/false)"),
                };
            }
            "reasoning_llm_enabled" => {
                self.reasoning_llm_enabled = match value {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => anyhow::bail!("Invalid value for reasoning_llm_enabled: {value} (use true/false)"),
                };
            }
            "reasoning_llm_base_url" => {
                self.reasoning_llm_base_url = value.to_string();
            }
            "reasoning_llm_model" => {
                self.reasoning_llm_model = value.to_string();
            }
            "reasoning_llm_timeout_secs" => {
                self.reasoning_llm_timeout_secs = value.parse()?;
            }
            _ => {
                anyhow::bail!("Unknown config key: {key}");
            }
        }
        self.save()?;
        Ok(())
    }

    pub fn show(&self) -> String {
        format!(
            "Config file: {}\n\
             enabled:                    {}\n\
             retention_days:             {}\n\
             archive_dir:                {}\n\
             reasoning_llm_enabled:      {}\n\
             reasoning_llm_base_url:     {}\n\
             reasoning_llm_model:        {}\n\
             reasoning_llm_timeout_secs: {}",
            Self::config_path().display(),
            self.enabled,
            self.retention_days,
            self.archive_dir,
            self.reasoning_llm_enabled,
            self.reasoning_llm_base_url,
            if self.reasoning_llm_model.is_empty() { "(auto-detect)" } else { &self.reasoning_llm_model },
            self.reasoning_llm_timeout_secs
        )
    }
}

/// DB パスを expand して返す
pub fn resolve_archive_dir(config: &Config) -> PathBuf {
    let expanded = shellexpand::tilde(&config.archive_dir);
    PathBuf::from(expanded.as_ref())
}
