# agents-relay: Session Memory for AI Coding Agents

**Read this in other languages:** [日本語](README.ja.md)

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
| `vectors.db` | Embedding vectors of summaries/facts (semantic search) | Regenerable via `embed` |
| `memory-light.db` | Derived output of `db export-light` (for transport) | Regenerable anytime |

## Install

Requires a Rust build environment.

```bash
git clone https://github.com/veltrea/agents-relay.git
cd agents-relay
cargo build --release

# Register with Claude Code as an MCP server
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
# Distill recently finished sessions (summary + fact extraction)
agents-relay consolidate

# Upgrade heuristic summaries to LLM summaries
agents-relay consolidate --upgrade

# Enable the LLM (off by default)
agents-relay config set reasoning_llm_enabled true
agents-relay config set reasoning_llm_base_url http://localhost:1234/v1
```

Scheduling is up to you — the work is a pure background batch over already-persisted data, so timing only affects freshness, never correctness (originals stay lossless and everything derived is regenerable). Two common policies:

- **Continuous** — `agents-relay consolidate --watch` drains the backlog, then idle-polls for newly finished sessions. Cadence and on/off are read live from config each cycle (`consolidate_enabled`, `consolidate_batch`, `consolidate_busy_secs`, `consolidate_idle_secs`). Run it under launchd/systemd with restart-on-exit.
- **Periodic** — a cron/launchd job running one-shot `agents-relay consolidate` (e.g. nightly).

### Semantic search (hybrid retrieval)

FTS5 handles Japanese poorly (no word segmentation), so rephrased queries miss. Once an embedding model (e.g. bge-m3) is configured, `memory_search` augments FTS results with semantic neighbors from summaries and facts.

```bash
agents-relay config set embedding_model text-embedding-bge-m3
agents-relay embed   # embed existing summaries/facts (consolidate embeds new ones automatically)
```

### Admin CLI

```bash
agents-relay db stats             # counts and per-database sizes
agents-relay db backup            # consistent snapshot of the 3 DBs to backup_dir (with rotation)
agents-relay db backfill-fulltext # recover truncated bodies / CoT from source JSONL
agents-relay db export-light      # generate a lightweight copy (for transport)
agents-relay db rebuild-fts       # rebuild the FTS index
agents-relay archive              # archive expired entries to lossless JSONL + Markdown, then delete
agents-relay db restore-archive ~/.agents-relay/archive/2026/01/05.jsonl  # restore from an archive
agents-relay list --date 2026-07-12
agents-relay export <session_id>
agents-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"
```

## Configuration

`~/.agents-relay/config.json`:

```jsonc
{
  "retention_days": 30,                              // days before entries become archive targets
  "archive_dir": "~/.agents-relay/archive",
  "reasoning_llm_enabled": false,                    // use the LLM for consolidate / CoT summaries
  "reasoning_llm_base_url": "http://localhost:1234/v1",
  "reasoning_llm_model": "",                         // empty = auto-detect via /v1/models
  "reasoning_llm_timeout_secs": 120,
  "backup_dir": "~/.agents-relay/backups",           // output dir for db backup (separate disk recommended)
  "embedding_model": "",                             // embedding model for semantic search (empty = disabled)
  "consolidate_enabled": true,                       // master switch for `consolidate --watch`
  "consolidate_batch": 10,                           // sessions per pass in watch mode
  "consolidate_busy_secs": 15,                       // sleep between passes while draining
  "consolidate_idle_secs": 900                       // poll interval when the backlog is empty
}
```

See [SPEC.md](SPEC.md) for design details.

## Caveats

Built to run on my own machine (macOS). Other environments are untested.

If you find a bug or it doesn't work for you, an [Issue](https://github.com/veltrea/agents-relay/issues) would be appreciated.

PRs are not accepted. I tend to rewrite the whole thing whenever an idea strikes, so your PR's base code probably won't survive. Feel free to fork and do whatever you like.

## License

MIT License
