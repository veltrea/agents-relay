# agents-relay: Session Memory for AI Coding Agents

**他の言語で読む:** [日本語](README.md)

A single binary that ingests Claude Code and Antigravity session history into SQLite and serves "past memory" as MCP tools. Formerly claude-relay; renamed when it went multi-agent.

## Background

I forked [claude-mem](https://github.com/anthropics/claude-mem) (a session memory plugin for Claude Code) intending to make it work with local LLMs, but after reading through the source code, I honestly found it unusable.

Firing an AI compression request per tool use, fetches without timeouts, no retry strategy, confusing liveness with readiness, and irreversibly discarding raw data after compression — the fundamentals of computer science just weren't there. I wrote more about it in [a separate article](https://note.com/veltrea/n/n791d1defada0).

Then it hit me: there is no reason to compress with AI at ingest time in the first place. Claude Code already writes every session to `~/.claude/projects/` as JSONL. Load that into SQLite **losslessly**, and treat summaries and compression as derived artifacts you can regenerate at any time.

So I built this from scratch.

## Design principles

1. **Never lose the original.** No truncation at ingest. Every deletion is preceded by a lossless export. Swap the summarization algorithm later and regenerate everything from the originals.
2. **Save tokens on the presentation side, not the storage side.** Search results carry pointers instead of bulky data (chains of thought, huge bodies). Agents fetch the full data with a dedicated tool only when they decide they need it.
3. **No daemons.** Incremental ingest by byte offset whenever an MCP tool is called.

## Storage layout (three tiers + derived artifacts)

| File | Contents | Safe to delete? |
|---|---|---|
| `memory.db` | Entry bodies (inline) + FTS index + summaries + facts | No (the core) |
| `fulltext.db` | **Full original text** of bodies over 50KB | Deleting loses full texts |
| `reasoning.db` | Raw chain-of-thought (thinking) originals | Search survives deletion |
| `archive/YYYY/MM/DD.jsonl` | Lossless originals of archived entries (append-only) | Deleting loses restore data |
| `memory-light.db` | Derived output of `db export-light` (for transport) | Regenerable anytime |

## Install

Requires a Rust build environment.

```bash
git clone https://github.com/veltrea/agents-relay.git
cd agents-relay
cargo build --release

# Claude Code の MCP に登録
claude mcp add --transport stdio --scope user agents-relay -- $(pwd)/target/release/agents-relay serve
```

## Usage

### From Claude Code / Antigravity

Register it as an MCP server and just ask inside a session. Ingest runs automatically on tool calls.

- "What did I work on yesterday?"
- "Find the session where we fixed OAuth"
- "What did we decide in this project?" (→ `memory_facts` returns distilled decisions and constraints)
- "Why did we choose that design back then?" (→ `memory_get_reasoning` returns the chain of thought from that moment)

Main MCP tools:

| Tool | Purpose |
|---|---|
| `memory_search` | FTS full-text search (also matches workspace and branch names) |
| `memory_list_sessions` | Session list (with summary snippets once consolidated) |
| `memory_get_session` | Full session history (session summary prepended) |
| `memory_get_entry` | Full text of one entry (from the fulltext originals) |
| `memory_get_reasoning` | Stored chain of thought (summary / full) |
| `memory_facts` | Distilled durable facts (very low token cost) |

### Sleep-time consolidation (distilling summaries and facts)

Finished sessions are distilled with a local LLM (LM Studio or any OpenAI-compatible API). If the LLM is unreachable, it falls back to extractive compression, upgradeable later with `--upgrade`.

```bash
# 直近の終了セッションを蒸留（要約 + 事実抽出）
agents-relay consolidate

# ヒューリスティック要約を LLM 要約に格上げ
agents-relay consolidate --upgrade

# LLM を有効化（既定はオフ）
agents-relay config set reasoning_llm_enabled true
agents-relay config set reasoning_llm_base_url http://localhost:1234/v1
```

### Admin CLI

```bash
agents-relay db stats             # 件数・DB 別サイズ
agents-relay db backfill-fulltext # 過去に切り詰めた本文・CoT を元 JSONL から回収
agents-relay db export-light      # 軽量コピー生成（持ち出し用）
agents-relay db rebuild-fts       # FTS 索引の再構築
agents-relay archive              # 期限切れエントリを無損失 JSONL + Markdown にアーカイブして削除
agents-relay db restore-archive ~/.agents-relay/archive/2026/01/05.jsonl  # アーカイブから復元
agents-relay list --date 2026-07-12
agents-relay export <session_id>
agents-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"
```

## Configuration

`~/.agents-relay/config.json`:

```jsonc
{
  "retention_days": 30,                              // archive の対象になる日数
  "archive_dir": "~/.agents-relay/archive",
  "reasoning_llm_enabled": false,                    // consolidate / CoT 要約に LLM を使うか
  "reasoning_llm_base_url": "http://localhost:1234/v1",
  "reasoning_llm_model": "",                         // 空なら /v1/models から自動検出
  "reasoning_llm_timeout_secs": 120
}
```

See [SPEC.en.md](SPEC.en.md) for design details.

## Caveats

Built to run on my own machine (macOS). Other environments are untested.

If you find a bug or it doesn't work for you, an [Issue](https://github.com/veltrea/agents-relay/issues) would be appreciated.

PRs are not accepted. I tend to rewrite the whole thing whenever an idea strikes, so your PR's base code probably won't survive. Feel free to fork and do whatever you like.

## License

MIT License
