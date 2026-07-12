use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_archive_dir")]
    pub archive_dir: String,
    /// db backup の出力先。別物理ディスクを指定するのが望ましい。
    #[serde(default = "default_backup_dir")]
    pub backup_dir: String,
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
    /// 意味検索用の埋め込みモデル名 (OpenAI 互換 /embeddings)。
    /// 空なら意味検索は無効。API は reasoning_llm_base_url を共用する。
    #[serde(default = "default_embedding_model")]
    pub embedding_model: String,
    /// consolidate --watch (常時圧縮) を実行するか。false ならウォッチャは
    /// 何もせず待機する (launchd を触らずに config だけで一時停止できる)。
    #[serde(default = "default_consolidate_enabled")]
    pub consolidate_enabled: bool,
    /// --watch の 1 パスあたり処理セッション数。
    #[serde(default = "default_consolidate_batch")]
    pub consolidate_batch: i64,
    /// --watch でバックログを消化中、パス間に空ける秒数。
    #[serde(default = "default_consolidate_busy_secs")]
    pub consolidate_busy_secs: u64,
    /// --watch でバックログが空のとき、新しい終了セッションを待つポーリング間隔 (秒)。
    #[serde(default = "default_consolidate_idle_secs")]
    pub consolidate_idle_secs: u64,
    /// compact 投影 (会話再構成用の軽量 min/mid 表現) を consolidate パスで
    /// materialize するか。false なら生成しない (get_session は inline 先頭に
    /// フォールバックする)。既定 true。
    #[serde(default = "default_compact_enabled")]
    pub compact_enabled: bool,
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

fn default_backup_dir() -> String {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".agents-relay")
        .join("backups")
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

fn default_embedding_model() -> String {
    String::new()
}

fn default_consolidate_enabled() -> bool {
    true
}

fn default_consolidate_batch() -> i64 {
    10
}

fn default_consolidate_busy_secs() -> u64 {
    15
}

fn default_consolidate_idle_secs() -> u64 {
    900
}

fn default_compact_enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
            archive_dir: default_archive_dir(),
            backup_dir: default_backup_dir(),
            enabled: default_enabled(),
            reasoning_llm_enabled: default_reasoning_llm_enabled(),
            reasoning_llm_base_url: default_reasoning_llm_base_url(),
            reasoning_llm_model: default_reasoning_llm_model(),
            reasoning_llm_timeout_secs: default_reasoning_llm_timeout_secs(),
            embedding_model: default_embedding_model(),
            consolidate_enabled: default_consolidate_enabled(),
            consolidate_batch: default_consolidate_batch(),
            consolidate_busy_secs: default_consolidate_busy_secs(),
            consolidate_idle_secs: default_consolidate_idle_secs(),
            compact_enabled: default_compact_enabled(),
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
            "backup_dir" => {
                self.backup_dir = value.to_string();
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
            "embedding_model" => {
                self.embedding_model = value.to_string();
            }
            "consolidate_enabled" => {
                self.consolidate_enabled = match value {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => anyhow::bail!("Invalid value for consolidate_enabled: {value} (use true/false)"),
                };
            }
            "consolidate_batch" => {
                self.consolidate_batch = value.parse()?;
            }
            "consolidate_busy_secs" => {
                self.consolidate_busy_secs = value.parse()?;
            }
            "consolidate_idle_secs" => {
                self.consolidate_idle_secs = value.parse()?;
            }
            "compact_enabled" => {
                self.compact_enabled = match value {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => anyhow::bail!("Invalid value for compact_enabled: {value} (use true/false)"),
                };
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
             backup_dir:                 {}\n\
             reasoning_llm_enabled:      {}\n\
             reasoning_llm_base_url:     {}\n\
             reasoning_llm_model:        {}\n\
             reasoning_llm_timeout_secs: {}\n\
             embedding_model:            {}\n\
             consolidate_enabled:        {}\n\
             consolidate_batch:          {}\n\
             consolidate_busy_secs:      {}\n\
             consolidate_idle_secs:      {}\n\
             compact_enabled:            {}",
            Self::config_path().display(),
            self.enabled,
            self.retention_days,
            self.archive_dir,
            self.backup_dir,
            self.reasoning_llm_enabled,
            self.reasoning_llm_base_url,
            if self.reasoning_llm_model.is_empty() { "(auto-detect)" } else { &self.reasoning_llm_model },
            self.reasoning_llm_timeout_secs,
            if self.embedding_model.is_empty() { "(disabled)" } else { &self.embedding_model },
            self.consolidate_enabled,
            self.consolidate_batch,
            self.consolidate_busy_secs,
            self.consolidate_idle_secs,
            self.compact_enabled
        )
    }
}

/// DB パスを expand して返す
pub fn resolve_archive_dir(config: &Config) -> PathBuf {
    let expanded = shellexpand::tilde(&config.archive_dir);
    PathBuf::from(expanded.as_ref())
}

pub fn resolve_backup_dir(config: &Config) -> PathBuf {
    let expanded = shellexpand::tilde(&config.backup_dir);
    PathBuf::from(expanded.as_ref())
}
