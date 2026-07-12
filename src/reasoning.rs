//! CoT (chain-of-thought) ユーティリティ。
//!
//! 設計方針: 生 CoT は取り込み時に最終応答 (`content`) と分離して `reasoning`
//! カラムへ退避する。検索・セッション取得のデフォルト結果には CoT を含めず、
//! `has_reasoning` / `reasoning_tokens` のポインタだけを返す。実際の CoT は
//! `memory_get_reasoning` で必要時のみ取得する。これにより、過去ログを参照する
//! 後続エージェントのトークン消費を抑える。
//!
//! 要約は 2 経路:
//! - `summarize()`  : LLM 不使用の抽出的圧縮 (Step-Entropy 系の軽量近似)。常に利用可能。
//! - `summarize_llm()`: LM Studio 等 OpenAI 互換 API による高品質要約。
//!
//! `summarize_best()` が入口。LLM が有効かつ到達可能ならそれを使い、失敗・未到達・
//! 無効時は必ず `summarize()` にフォールバックする (サーバーを壊さない)。

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// 粗いトークン数推定。英語主体の思考テキスト向けに約 4 文字 = 1 トークンで概算する。
/// 予算判断 (どの層を返すか) に使うだけなので厳密さは不要。
pub fn estimate_tokens(s: &str) -> i64 {
    (s.chars().count() as i64 + 3) / 4
}

/// LLM 要約の設定 (Config から組み立てる)。
pub struct LlmConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
}

/// 要約結果。どちらの経路で生成したかを併せて返す。
pub struct Summary {
    pub text: String,
    /// true=LLM, false=ヒューリスティック
    pub used_llm: bool,
}

/// LLM に送る生 CoT の最大文字数。長すぎるとローカルモデルが遅くなるため頭側を切る。
const LLM_INPUT_MAX_CHARS: usize = 16_000;

const LLM_SYSTEM_PROMPT: &str = "You compress an AI assistant's chain-of-thought into a concise summary. \
Keep only decisions, the reasoning behind them, key facts discovered, and conclusions. \
Drop filler, false starts, restatements, and self-talk. \
Output 3-8 short bullet points in plain text. Do not add a preamble.";

/// 入口: LLM が有効なら使い、失敗時はヒューリスティックへフォールバックする。
/// LLM が暴走して元 CoT より長い「要約」を返した場合もフォールバックする
/// （圧縮になっていなければ意味がない。小型/弱いモデルの繰り返しループ対策）。
pub fn summarize_best(raw: &str, llm: Option<&LlmConfig>) -> Summary {
    if let Some(cfg) = llm {
        if cfg.enabled {
            match summarize_llm(raw, cfg) {
                Ok(s) if s.trim().is_empty() => {
                    eprintln!("agents-relay: LLM returned empty summary, using heuristic");
                }
                Ok(s) if estimate_tokens(&s) > estimate_tokens(raw) => {
                    eprintln!(
                        "agents-relay: LLM summary ({} tok) longer than source ({} tok) — not a compression, using heuristic",
                        estimate_tokens(&s), estimate_tokens(raw)
                    );
                }
                Ok(s) => {
                    return Summary { text: s, used_llm: true };
                }
                Err(e) => {
                    eprintln!("agents-relay: LLM summarize failed ({e}), using heuristic");
                }
            }
        }
    }
    Summary { text: summarize(raw), used_llm: false }
}

/// OpenAI 互換 chat/completions で要約する。model が空なら /v1/models から自動検出。
///
/// 重要: `reqwest::blocking` のクライアントは内部に current-thread ランタイムを持ち、
/// それを **tokio の非同期文脈で drop すると panic** する
/// ("Cannot drop a runtime in a context where blocking is not allowed")。
/// server 経由の呼び出しは tokio ワーカー上で動くため、ブロッキング HTTP 一式を
/// 専用 OS スレッドに隔離し、クライアントの生成〜drop を非 tokio 文脈で完結させる。
/// (offline CLI からの呼び出しでも追加スレッド 1 本で動作は同じ。)
fn summarize_llm(raw: &str, cfg: &LlmConfig) -> Result<String> {
    let input = truncate_chars(raw.trim(), LLM_INPUT_MAX_CHARS);
    // max_tokens を入力サイズに連動させ、暴走(繰り返しループ)の出力長を抑える。
    // 要約は入力の半分以下を狙う。下限 96 / 上限 512 トークン。
    let max_tokens = (estimate_tokens(&input) / 2).clamp(96, 512);
    chat_llm(
        LLM_SYSTEM_PROMPT,
        &format!("Compress this chain-of-thought:\n\n{input}"),
        cfg,
        max_tokens,
    )
}

/// OpenAI 互換 chat/completions を 1 回呼ぶ汎用ヘルパー。
/// summarize_llm のスレッド隔離 (tokio 文脈での blocking クライアント drop 対策) を
/// そのまま引き継ぐ。
fn chat_llm(system: &str, user: &str, cfg: &LlmConfig, max_tokens: i64) -> Result<String> {
    let system = system.to_string();
    let user = user.to_string();
    let base = cfg.base_url.trim_end_matches('/').to_string();
    let model_cfg = cfg.model.clone();
    let timeout = cfg.timeout_secs;

    std::thread::spawn(move || -> Result<String> {
        let model = if model_cfg.is_empty() {
            discover_model(&base, timeout)?
        } else {
            model_cfg
        };

        let body = json!({
            "model": model,
            "messages": [
                { "role": "system", "content": system },
                { "role": "user", "content": user }
            ],
            "temperature": 0.2,
            "max_tokens": max_tokens,
            "stream": false,
        });

        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(timeout))
            .build()?;
        let resp = client
            .post(format!("{base}/chat/completions"))
            .json(&body)
            .send()?
            .error_for_status()?
            .json::<Value>()?;

        // content が空なら reasoning_content を見る (gpt-oss / deepseek-r1 等の
        // reasoning モデルは最終出力を別チャネルに入れることがある)
        let msg = &resp["choices"][0]["message"];
        let text = msg["content"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| msg["reasoning_content"].as_str())
            .ok_or_else(|| anyhow!("no content in LLM response"))?
            .trim()
            .to_string();
        Ok(text)
    })
    .join()
    .map_err(|_| anyhow!("summarize worker thread panicked"))?
}

/// セッション要約用のシステムプロンプト。
const SESSION_SUMMARY_PROMPT: &str = "You summarize a work session between a user and an AI coding agent. \
Output 3-10 concise bullet points covering: the goal, key decisions and their reasons, \
what was built or changed, verification results, and unresolved issues or next steps. \
Write in the language predominantly used in the session (Japanese sessions get Japanese summaries). \
No preamble, no headers — just the bullets.";

/// セッション全体のダイジェストを要約する。LLM 優先・ヒューリスティックにフォールバック。
pub fn summarize_session_best(digest: &str, llm: Option<&LlmConfig>) -> Summary {
    if let Some(cfg) = llm {
        if cfg.enabled {
            let input = truncate_chars(digest.trim(), LLM_INPUT_MAX_CHARS);
            match chat_llm(
                SESSION_SUMMARY_PROMPT,
                &format!("Summarize this session:\n\n{input}"),
                cfg,
                512,
            ) {
                Ok(s) if !s.trim().is_empty() && estimate_tokens(&s) <= estimate_tokens(digest) => {
                    return Summary { text: s.trim().to_string(), used_llm: true };
                }
                Ok(_) => {
                    eprintln!("agents-relay: LLM session summary unusable, using heuristic");
                }
                Err(e) => {
                    eprintln!("agents-relay: LLM session summary failed ({e}), using heuristic");
                }
            }
        }
    }
    Summary { text: summarize(digest), used_llm: false }
}

/// 事実抽出用のシステムプロンプト。1 行 1 事実の固定形式で出させる。
const FACT_EXTRACT_PROMPT: &str = "Extract durable facts from this work session that will still matter in future sessions. \
Only include: decisions made (and why), constraints discovered, user preferences expressed, \
and environment facts (paths, hosts, tool versions, gotchas). \
Exclude: one-off actions, transient states, anything obvious from reading the code. \
Output one fact per line in the exact format 'CATEGORY: fact text' where CATEGORY is one of \
DECISION, CONSTRAINT, PREFERENCE, ENVIRONMENT. Keep each fact self-contained and under 200 characters. \
Write facts in the language predominantly used in the session. \
If there is nothing durable, output exactly 'NONE'.";

/// セッションダイジェストから持続的な事実を抽出する (LLM 必須・フォールバックなし)。
/// 戻り値: (カテゴリ, 事実) のリスト。
pub fn extract_facts_llm(digest: &str, cfg: &LlmConfig) -> Result<Vec<(String, String)>> {
    let input = truncate_chars(digest.trim(), LLM_INPUT_MAX_CHARS);
    let out = chat_llm(
        FACT_EXTRACT_PROMPT,
        &format!("Extract durable facts from this session:\n\n{input}"),
        cfg,
        512,
    )?;

    let mut facts = Vec::new();
    for line in out.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ']);
        if line.is_empty() || line.eq_ignore_ascii_case("none") {
            continue;
        }
        if let Some((cat, fact)) = line.split_once(':') {
            let cat = cat.trim().to_uppercase();
            let fact = fact.trim();
            if ["DECISION", "CONSTRAINT", "PREFERENCE", "ENVIRONMENT"].contains(&cat.as_str())
                && !fact.is_empty()
            {
                facts.push((cat, fact.to_string()));
            }
        }
    }
    Ok(facts)
}

/// /v1/models からロード済みモデル id を取得する (ゼロ設定用)。
/// 互換のため複数のレスポンス形に対応:
///   - OpenAI / LM Studio: {"data":[{"id":...}]}
///   - llama.cpp / Ollama: {"models":[{"id"|"name":...}]}
fn discover_model(base: &str, timeout_secs: u64) -> Result<String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(timeout_secs))
        .build()?;
    let resp = client
        .get(format!("{base}/models"))
        .send()?
        .error_for_status()?
        .json::<Value>()?;

    let first = ["data", "models"]
        .iter()
        .find_map(|key| resp[*key].get(0).cloned())
        .ok_or_else(|| anyhow!("no model list found at {base}/models"))?;

    first["id"]
        .as_str()
        .or_else(|| first["name"].as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("model entry has no id/name at {base}/models"))
}

/// 抽出的圧縮の出力上限 (文字数)。
const SUMMARY_MAX_CHARS: usize = 1800;

/// 決定・結論・根拠を示唆するシグナル語。これらを含む行は残す。
const SIGNAL_TOKENS: &[&str] = &[
    "so ", "therefore", "because", "the key", "the issue", "the problem",
    "i need", "i'll", "i will", "let me", "instead", "approach", "decision",
    "should", "must", "note that", "important", "the plan", "the goal",
    "in summary", "conclusion", "root cause", "the fix", "turns out",
];

/// 単独で現れたら捨てる短いフィラー行 (小文字化して完全一致で判定)。
const FILLER_LINES: &[&str] = &[
    "wait", "hmm", "okay", "ok", "actually", "right", "now", "yes", "no",
    "let me think", "let me think.", "so", "well", "hmm.", "wait.", "ok.",
];

/// 生 CoT を抽出的に圧縮する。フィラーを落とし、決定・結論・根拠・コード片を含む
/// セグメントを上限まで残す。何も残らなければ生 CoT の先頭を切り詰めて返す。
/// 行単位だけでなく、改行のない長い段落は文単位に分割してから判定する
/// (Claude の thinking は 1 段落にまとまることがあるため)。
pub fn summarize(raw: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;

    for line in raw.lines() {
        for seg in split_segments(line) {
            let trimmed = seg.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lower = trimmed.to_lowercase();
            // 末尾の句読点を除いた形でフィラー判定する
            let key = lower.trim_end_matches(|c: char| ".,!?;: ".contains(c));
            if FILLER_LINES.contains(&key) {
                continue;
            }

            let has_signal = SIGNAL_TOKENS.iter().any(|t| lower.contains(t));
            let is_structural = trimmed.ends_with(':')
                || trimmed.starts_with('-')
                || trimmed.starts_with('*')
                || trimmed.starts_with('#')
                || trimmed.starts_with(|c: char| c.is_ascii_digit());
            let is_codey = trimmed.contains("::")
                || trimmed.contains("()")
                || trimmed.contains('`')
                || trimmed.contains('/');

            if has_signal || is_structural || is_codey {
                kept.push(trimmed.to_string());
                total += trimmed.chars().count() + 1;
                if total >= SUMMARY_MAX_CHARS {
                    return kept.join("\n");
                }
            }
        }
    }

    if kept.is_empty() {
        // 抽出に失敗した場合は先頭を切り詰めて返す
        return truncate_chars(raw.trim(), SUMMARY_MAX_CHARS);
    }
    kept.join("\n")
}

/// 1 行を判定単位に分割する。短い行はそのまま、長い行 (>200字) は文末
/// (". " "? " "! ") で分割する。ファイルパスの "." では切らない (空白が続く時のみ)。
fn split_segments(line: &str) -> Vec<String> {
    if line.chars().count() <= 200 {
        return vec![line.to_string()];
    }
    line.replace(". ", ".\u{0}")
        .replace("? ", "?\u{0}")
        .replace("! ", "!\u{0}")
        .split('\u{0}')
        .map(|s| s.to_string())
        .collect()
}

/// マルチバイト境界を壊さずに文字数で切り詰める。
fn truncate_chars(s: &str, max_chars: usize) -> String {
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
