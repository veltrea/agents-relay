//! 睡眠時整理 (sleep-time consolidation)。
//!
//! アイドル時に走らせるバッチで、終了したセッションを蒸留する:
//! 1. セッション要約 → summaries テーブル (LLM 優先・ヒューリスティックにフォールバック)
//! 2. 持続的な事実の抽出 → facts テーブル (LLM 必須。新規性ゲートで重複排除)
//!
//! 原本 (raw_entries / fulltext / reasoning) は一切変更しない。要約・事実は
//! すべて再生成可能な派生物であり、アルゴリズムを差し替えたら作り直せる。

use anyhow::Result;
use rusqlite::Connection;

use crate::config::Config;
use crate::db;
use crate::reasoning;

/// ダイジェストの全体予算 (文字数)。LLM 入力上限 (16k) にマージンを持たせる。
const DIGEST_BUDGET_CHARS: usize = 14_000;
/// 1 メッセージあたりの上限 (文字数)
const DIGEST_PER_MSG_CHARS: usize = 400;
/// 1 パスで生成する compact 投影の最大件数 (バックログはパスをまたいで消化)。
const COMPACT_PASS_MAX: i64 = 2_000;

pub struct ConsolidateStats {
    pub sessions: u64,
    pub llm_summaries: u64,
    pub facts_added: u64,
    pub facts_skipped: u64,
    /// このパスで materialize した compact 投影の件数。
    pub compact_generated: u64,
}

/// 要約が古い/無いセッションを蒸留する。
pub fn run(
    conn: &mut Connection,
    cfg: &Config,
    limit: i64,
    min_entries: i64,
    idle_mins: i64,
    with_facts: bool,
    upgrade_heuristic: bool,
) -> Result<ConsolidateStats> {
    let idle_cutoff = (chrono::Utc::now() - chrono::Duration::minutes(idle_mins))
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string();

    let llm = reasoning::LlmConfig {
        enabled: cfg.reasoning_llm_enabled,
        base_url: cfg.reasoning_llm_base_url.clone(),
        model: cfg.reasoning_llm_model.clone(),
        timeout_secs: cfg.reasoning_llm_timeout_secs,
    };

    let mut stats = ConsolidateStats {
        sessions: 0,
        llm_summaries: 0,
        facts_added: 0,
        facts_skipped: 0,
        compact_generated: 0,
    };

    let targets =
        db::sessions_needing_summary(conn, min_entries, &idle_cutoff, limit, upgrade_heuristic)?;
    for (session_id, started, ended, count) in targets {
        let (digest, cwd, git_branch) = build_digest(conn, &session_id)?;
        if digest.trim().is_empty() {
            continue;
        }

        // 1. セッション要約
        let summary = reasoning::summarize_session_best(&digest, Some(&llm));
        // どちらの経路で作られたかを機械可読マーカーで残す
        // (後で LLM 生成に upgrade できるように)
        let marker = if summary.used_llm { "llm" } else { "heuristic" };
        let summary_md = format!("<!-- summarizer: {marker} -->\n{}", summary.text);
        db::upsert_summary(
            conn,
            &session_id,
            cwd.as_deref(),
            git_branch.as_deref(),
            &started,
            &ended,
            &summary_md,
        )?;
        stats.sessions += 1;
        if summary.used_llm {
            stats.llm_summaries += 1;
        }
        eprintln!(
            "  [consolidate] {} ({} entries) summarized ({})",
            session_id, count, marker
        );

        // 2. 事実抽出 (LLM が使えるときだけ)
        if with_facts && llm.enabled {
            match reasoning::extract_facts_llm(&digest, &llm) {
                Ok(facts) => {
                    for (category, fact) in facts {
                        if db::find_duplicate_fact(conn, &fact)?.is_some() {
                            stats.facts_skipped += 1;
                            continue;
                        }
                        db::insert_fact(conn, &category, &fact, cwd.as_deref(), &session_id)?;
                        stats.facts_added += 1;
                    }
                }
                Err(e) => {
                    eprintln!("  [consolidate] fact extraction failed for {session_id}: {e}");
                }
            }
        }
    }

    // compact 投影を materialize する (会話再構成用の軽量 min/mid)。原本には触れない
    // 再生成可能な派生物。要約対象セッションだけでなく未生成の全エントリを対象に、
    // 1 パス上限までバックログを消化する (残りは次パスへ)。
    if cfg.compact_enabled {
        match db::generate_compact_batch(conn, COMPACT_PASS_MAX) {
            Ok(n) => {
                stats.compact_generated = n;
                if n > 0 {
                    eprintln!("  [consolidate] materialized compact for {n} entries");
                }
            }
            Err(e) => eprintln!("  [consolidate] compact generation skipped: {e}"),
        }
    }

    // 新しく作られた要約・事実の埋め込みを生成 (best-effort。失敗しても蒸留自体は成立)
    if let Err(e) = embed_missing(conn, cfg) {
        eprintln!("  [consolidate] embedding skipped: {e}");
    }

    Ok(stats)
}

/// 常時圧縮モード。バックログを消化し、空になったらポーリング間隔で待つ。
/// 設定は毎サイクル読み直すので、config だけで cadence 変更・一時停止できる。
/// SIGTERM (launchd 停止) で素直に終了する。
pub fn run_watch(conn: &mut Connection) -> Result<()> {
    eprintln!("agents-relay: consolidate watcher started");
    loop {
        // 設定を毎回読み直す (live tuning)。読めなければ既定値。
        let cfg = Config::load().unwrap_or_default();

        if !cfg.consolidate_enabled {
            std::thread::sleep(std::time::Duration::from_secs(cfg.consolidate_idle_secs));
            continue;
        }

        // 新しいセッションを取り込む (server が動いていなくても自己完結するため)。
        // オフセット方式なので新規分が無ければほぼ無コスト。
        if let Err(e) = crate::ingest::sync_all(conn) {
            eprintln!("agents-relay: watcher sync failed: {e}");
        }

        let stats = match run(
            conn,
            &cfg,
            cfg.consolidate_batch,
            6,    // min_entries
            60,   // idle_mins: 終了とみなす閾値
            true, // with_facts
            true, // upgrade: LLM 復帰後にヒューリスティック要約を自己修復
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("agents-relay: consolidate pass failed: {e}");
                // LLM 一時不達などは長めに待って再試行
                std::thread::sleep(std::time::Duration::from_secs(cfg.consolidate_idle_secs));
                continue;
            }
        };

        // 処理があれば短い間隔で次パスへ (バックログを速やかに消化)。
        // 要約と compact のどちらかが動いていれば busy 扱いにして、compact の
        // バックログも滞りなく消化する。空なら終了セッションを待つ間隔で休む。
        let sleep_secs = if stats.sessions > 0 || stats.compact_generated > 0 {
            cfg.consolidate_busy_secs
        } else {
            cfg.consolidate_idle_secs
        };
        std::thread::sleep(std::time::Duration::from_secs(sleep_secs));
    }
}

/// セッションの user/assistant 発言を LLM 予算内のダイジェストに畳む。
/// 予算超過時は先頭 60% + 末尾 40% を残し中間を省略する
/// (セッションの目的は冒頭に、結末は末尾に現れやすいため)。
fn build_digest(
    conn: &Connection,
    session_id: &str,
) -> Result<(String, Option<String>, Option<String>)> {
    let mut stmt = conn.prepare(
        "SELECT type, content, cwd, git_branch FROM raw_entries
         WHERE session_id = ?1 AND type IN ('user', 'assistant')
         ORDER BY timestamp ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;

    let mut lines = Vec::new();
    let mut cwd = None;
    let mut git_branch = None;
    for row in rows {
        let (ty, content, c, b) = row?;
        if cwd.is_none() {
            cwd = c;
        }
        if git_branch.is_none() {
            git_branch = b;
        }
        let role = if ty == "user" { "USER" } else { "ASSISTANT" };
        let text = truncate_chars(content.trim(), DIGEST_PER_MSG_CHARS);
        if !text.is_empty() {
            lines.push(format!("{role}: {text}"));
        }
    }

    let total: usize = lines.iter().map(|l| l.chars().count() + 1).sum();
    let digest = if total <= DIGEST_BUDGET_CHARS {
        lines.join("\n")
    } else {
        // 行単位で先頭 60% / 末尾 40% の予算に収める
        let head_budget = DIGEST_BUDGET_CHARS * 6 / 10;
        let tail_budget = DIGEST_BUDGET_CHARS - head_budget;
        let mut head = Vec::new();
        let mut used = 0usize;
        let mut i = 0;
        while i < lines.len() {
            let n = lines[i].chars().count() + 1;
            if used + n > head_budget {
                break;
            }
            used += n;
            head.push(lines[i].clone());
            i += 1;
        }
        let mut tail = Vec::new();
        let mut used = 0usize;
        let mut j = lines.len();
        while j > i {
            let n = lines[j - 1].chars().count() + 1;
            if used + n > tail_budget {
                break;
            }
            used += n;
            tail.push(lines[j - 1].clone());
            j -= 1;
        }
        tail.reverse();
        format!(
            "{}\n[... {} messages omitted ...]\n{}",
            head.join("\n"),
            j.saturating_sub(i),
            tail.join("\n")
        )
    };

    Ok((digest, cwd, git_branch))
}

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

/// 埋め込み未生成の要約・事実をベクトル化して vdb に格納する。
/// embedding_model が未設定なら何もしない。冪等・増分。
/// 戻り値: (要約の埋め込み数, 事実の埋め込み数)
pub fn embed_missing(conn: &Connection, cfg: &Config) -> Result<(u64, u64)> {
    if cfg.embedding_model.is_empty() {
        return Ok((0, 0));
    }
    let model = &cfg.embedding_model;
    let base = &cfg.reasoning_llm_base_url;
    let timeout = cfg.reasoning_llm_timeout_secs;

    // 次元数の確定 (初回はプローブで 1 件埋め込んで判定)
    let dim = match db::vec_state(conn)? {
        Some((m, d)) if &m == model => d,
        _ => {
            let probe = reasoning::embed_texts(&["dimension probe".to_string()], model, base, timeout)?;
            let d = probe.first().map(|v| v.len()).unwrap_or(0);
            if d == 0 {
                anyhow::bail!("embedding model returned empty vector");
            }
            db::vec_init(conn, model, d)?;
            d
        }
    };
    let _ = dim;

    let mut sum_count = 0u64;
    let mut fact_count = 0u64;
    const BATCH: i64 = 32;

    loop {
        let missing = db::summaries_missing_embedding(conn, BATCH)?;
        if missing.is_empty() {
            break;
        }
        let texts: Vec<String> = missing.iter().map(|(_, _, md)| md.clone()).collect();
        let embs = reasoning::embed_texts(&texts, model, base, timeout)?;
        if embs.len() != texts.len() {
            anyhow::bail!("embeddings response count mismatch ({} != {})", embs.len(), texts.len());
        }
        for ((id, _, _), emb) in missing.iter().zip(embs.iter()) {
            db::vec_upsert(conn, "vec_summaries", *id, emb)?;
            sum_count += 1;
        }
    }

    loop {
        let missing = db::facts_missing_embedding(conn, BATCH)?;
        if missing.is_empty() {
            break;
        }
        let texts: Vec<String> = missing.iter().map(|(_, f)| f.clone()).collect();
        let embs = reasoning::embed_texts(&texts, model, base, timeout)?;
        if embs.len() != texts.len() {
            anyhow::bail!("embeddings response count mismatch ({} != {})", embs.len(), texts.len());
        }
        for ((id, _), emb) in missing.iter().zip(embs.iter()) {
            db::vec_upsert(conn, "vec_facts", *id, emb)?;
            fact_count += 1;
        }
    }

    Ok((sum_count, fact_count))
}
