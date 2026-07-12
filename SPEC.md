# agents-relay Design Specification

**Read this in other languages:** [日本語](SPEC.ja.md)

Applies to version: 0.2.2

## 1. Purpose and design principles

Provide the session history of AI coding agents (Claude Code / Antigravity) as searchable cross-session "memory".

- **Lossless principle**: originals (full bodies, raw chain-of-thought) are never lost on any path — ingest, update, delete, or archive. Truncation exists only for display, indexing, and transport, and always leaves a reference to the original.
- **Derived artifacts are regenerable**: summaries, facts, light copies, and the FTS index can all be rebuilt from the originals. Compression and summarization algorithms can be swapped later.
- **Token economy via pointers**: search and list responses never carry heavy data; they return pointers such as `has_reasoning` and token counts. Retrieval is explicit, through dedicated tools.

## 2. Process architecture

```
serve (MCP stdio, ワークスペースごと) ──┐
serve (MCP stdio)                       ├─→ HTTP 127.0.0.1:54322 → server (単一プロセス) → SQLite
serve (MCP stdio)                       ──┘
```

- `agents-relay serve` — MCP stdio server; a thin proxy. It only resolves scope (workspace / source) and forwards tool calls over HTTP to `server`. Auto-starts `server` via `ensure_server_running()`.
- `agents-relay server` — Axum HTTP server; the single process that serializes all DB access. Endpoints: `/health` `/register` `/deregister` `/tool`. Port managed in `~/.agents-relay/server.port` (default 54322). Detects binary updates by mtime and restarts stale servers; auto-exits when no clients remain.

## 3. Storage tiers

All under `~/.agents-relay/`. `db::open()` ATTACHes the four side databases into one connection.

### 3.1 memory.db (core)

- `raw_entries` — entry bodies. `content` holds the **inline representation** (see below); `reasoning_tokens` is the CoT pointer.
- `raw_entries_fts` — FTS5 index (`content`, `tool_name`, `session_id`, `context`). `context` is "workspace basename + git branch + source", enabling search by project name. The indexed `content` is the **surface projection** (`compact::surface`), not the raw body: prose is kept in full, `tool_use` collapses to name + target, successful `tool_result` bodies collapse to a `[result: N lines, M bytes]` stub, and error results are kept in full. This keeps the conversation searchable while excluding tool-output noise (which is ~97% of an agent's bytes). Rebuilt on upgrade, tracked by `meta.fts_recipe`.
- `meta` — key/value schema & index-recipe markers (e.g. `fts_recipe = surface_v1` triggers a one-time FTS rebuild when the projection changes).
- `summaries` — session summaries produced by consolidate. `summary_md` starts with a `<!-- summarizer: llm|heuristic -->` marker.
- `facts` — durable facts. `category` (DECISION / CONSTRAINT / PREFERENCE / ENVIRONMENT), `superseded_by` (expiry instead of deletion). Searched via `facts_fts`.
- `sync_state` — ingested byte offsets / change-detection stamps (two keys: `<path>` and `<path>#mtime`).

### 3.2 fulltext.db (full-text originals, ATTACHed as `fdb`)

- `fulltext(entry_id, content)` — the full text of bodies exceeding `CONTENT_INLINE_MAX` (50,000 bytes).
- Inline representation = first 1,000 bytes + `... [truncated, N bytes]` marker. Truncation policy is centralized in `db::insert_entry_full`; callers always pass the full text.
- `memory_get_entry` returns the full text via `COALESCE(fdb.content, inline)`.

### 3.3 reasoning.db (raw CoT, ATTACHed as `rdb`)

- `cot(entry_id, reasoning, reasoning_summary)` — thinking originals (never truncated) and a summary cache.
- Not indexed by FTS. Search and session results carry only the pointer `has_reasoning=true, reasoning_tokens≈N`.
- Deleting the file does not break search or session retrieval (`memory_get_reasoning` simply reports nothing stored).

### 3.4 Archive (lossless, append-only)

For each date older than `retention_days`, the `archive` command:

1. **Appends** all columns + raw CoT + full-text originals to `archive_dir/YYYY/MM/DD.jsonl` and fsyncs
2. Writes a human-readable Markdown digest `DD.md` (reused if present)
3. Only then deletes from raw_entries / cot / fulltext

Because the JSONL is append-only, re-runs and crash retries may duplicate lines but can never lose data. `db restore-archive <file.jsonl>` restores idempotently (existing entries skipped; content re-split into inline/fulltext tiers).

### 3.5 vectors.db (embeddings, ATTACHed as `vdb`)

- `vec_summaries` / `vec_facts` — sqlite-vec (vec0) embeddings of summaries and facts. `vec_meta` records the model name and dimension; a model change discards and rebuilds the index.
- Embeddings come from an OpenAI-compatible `/embeddings` endpoint (only when `embedding_model` is set). `consolidate` embeds new items automatically; the `embed` command backfills in bulk.
- `memory_search` augments FTS results with `match: "semantic"` neighbors (3 summaries + 3 facts). If the embedding service is unreachable, search silently degrades to FTS only.
- Fully derived: the file can be deleted and regenerated with `embed`.

### 3.6 compact.db (conversation projections, ATTACHed as `cdb`)

- `entry_compact(entry_id, min_text, mid_text, min_tokens, mid_tokens, recipe_version)` — token-frugal projections of each entry for conversation replay, at two levels: **min** (skeleton: prose points, tool calls collapsed to name + target, tool outputs to a one-line lines/bytes summary) and **mid** (flow and intent: near-full prose, tool signatures with payload hints, the head of each tool output). Error tool_results are kept at both levels; chain-of-thought is excluded at both (it was already split out at ingest).
- Generated deterministically (no LLM) by a versioned recipe. `recipe_version` marks stale rows; after a recipe change, `db rebuild-compact` regenerates everything.
- Materialized by the consolidate pass (gated by `compact_enabled`); `db backfill-compact` backfills in bulk. Fully derived — deleting the file only makes `memory_get_session` fall back to inline heads.
- Served through `memory_get_session detail='min'|'mid'` (default `min`). Together with `raw_entries.content_tokens` (estimated tokens of the full body), `min_tokens` / `mid_tokens` form the per-entry token menu `{min, mid, full}` returned to the caller, so it can escalate selectively.

### 3.7 Transport cap

MCP response `content` / `reasoning` is capped at 200,000 bytes (annotated with `[transport limit: full N bytes stored losslessly in DB]`). Applies per entry to `memory_get_entry`, `memory_get_reasoning`, and `memory_get_session detail='full'`. Stored data remains lossless.

## 4. Ingest

### 4.1 Claude Code

- Incremental ingest of `~/.claude/projects/**/*.jsonl` by byte offset (transaction commit every 100 entries; the offset is persisted in the same transaction).
- Assistant `thinking` blocks are split out of content into reasoning. Small entries consisting only of `data:image/` are skipped.
- `db backfill-fulltext` — recovers bodies truncated by old versions and thinking they discarded, from source JSONL and `.jsonl.bak` files. Matching keys:
  - Bodies: session + type + first 1,000 bytes + exact byte length (timestamp is deliberately not used, to survive Claude Code's session-file rewrites)
  - CoT: session + timestamp + type + content-head comparison (stricter, because attaching a wrong CoT is worse than missing one)

### 4.2 Antigravity

- `agyhub_summaries_proto.pb` — plain protobuf (hand-written wire-format reader). Every conversation's UUID, title, workspace (percent-decoded file:// URI), and timestamps. Upserted under the `ag:conv-summary` marker.
- `brain/<uuid>/*.metadata.json` + matching `.md` + latest `.resolved.N` — artifacts. Change detection by size + mtime; updates via marker-scoped delete-insert.
- `brain/<uuid>/.system_generated/logs/transcript.jsonl` — real transcripts, ingested incrementally by offset. USER_INPUT→user / PLANNER_RESPONSE→assistant / others→system. A shrinking file (rewrite) triggers deletion of `ag:step:*` rows and re-ingest.
- `knowledge/<topic>/` — metadata.json + `artifacts/**/*.md`. Workspace resolved through referenced conversation ids.
- `conversations/*.pb` and `implicit/*.pb` are encrypted and cannot be ingested (verified).

## 5. Sleep-time consolidation

`agents-relay consolidate [--limit N] [--min-entries N] [--idle-mins N] [--no-facts] [--upgrade]`

1. Target selection: sessions with at least `min_entries` user/assistant entries, whose last entry is older than `idle_mins` minutes, and whose summary is missing or stale. `--upgrade` also includes heuristic summaries.
2. Digest construction: 400 chars per message, 14,000 chars total; over budget keeps the first 60% + last 40% with an elision marker in between.
3. Summarization: LLM (OpenAI-compatible `/chat/completions`) first; falls back to extractive compression on failure or runaway output. The path taken is recorded in the marker.
4. Fact extraction (LLM required): extracts `CATEGORY: fact` lines, and only facts passing the **novelty gate** (character-bigram Jaccard >= 0.6 against existing active facts counts as duplicate) are inserted into `facts`.
5. Compact projections: materializes min/mid projections (`cdb.entry_compact`) for entries that lack them or carry a stale `recipe_version` (up to 2,000 per pass; the backlog drains across passes). Gated by `compact_enabled`.

Originals are never modified. Summaries, facts, and compact projections are regenerable derived artifacts.

### 5.1 Scheduling (`--watch`)

Because consolidation is a pure background batch over already-persisted data, when it runs only affects freshness, never correctness. Two policies are supported:

- **Continuous**: `consolidate --watch` loops — sync new JSONL, run one pass (batch = `consolidate_batch`), embed, then sleep `consolidate_busy_secs` if it did work (summaries or compact projections) or `consolidate_idle_secs` if the backlog was empty. Config is re-read every cycle, so cadence and the `consolidate_enabled` master switch take effect live without restarting the process. Intended to run under launchd/systemd with restart-on-exit. The watcher also runs `sync_all` each cycle, so it is self-sufficient even when no `server` is active.
- **Periodic**: a cron/launchd job invoking one-shot `consolidate`.

Deployment separates responsibilities: a KeepAlive daemon runs `consolidate --watch` (continuous compression + embedding), while a daily job runs `db backup` (periodic snapshot).

## 6. MCP tools

| Tool | Summary |
|---|---|
| `memory_search` | FTS search over the conversation surface. `deep=true` adds a full-body substring (`LIKE`) fallback for strings that live inside tool output. scope=self/cross/all (cross/all gated behind user approval) |
| `memory_list_sessions` | Session list + summary snippets |
| `memory_get_session` | Session history (summary prepended). `detail` selects the tier: gist (summary only) / min (default, skeleton) / mid / full; each message carries a `tokens` menu `{min,mid,full}`. Default type filter: user,assistant |
| `memory_get_entry` | Full text of one entry (fulltext originals; transport-capped) |
| `memory_get_reasoning` | CoT. mode=summary (lazily generated and cached) / full |
| `memory_facts` | Search distilled facts (workspace filter supported) |
| `memory_set_enabled` | Enable/disable per workspace or globally |
| `memory_find_workspace` etc. | Antigravity-only (workspace registration, cross-scope unlock) |

## 7. CLI

`serve` / `server` / `ingest` / `sync` / `list` / `export` / `write` / `query` / `summarize` / `consolidate` / `embed` / `archive` / `workspace` / `config` / `db {stats, vacuum, reset, rebuild-fts, export-light, backfill-fulltext, backfill-compact, rebuild-compact, restore-archive, backup}` / `tool` (direct invocation for debugging)

The server port can be overridden with the `AGENTS_RELAY_PORT` environment variable (for tests and parallel setups; default 54322).

## 8. Configuration (~/.agents-relay/config.json)

| Key | Default | Meaning |
|---|---|---|
| `retention_days` | 30 | Days before entries become archive targets |
| `archive_dir` | `~/.agents-relay/archive` | Archive output directory |
| `enabled` | true | Global enable flag |
| `reasoning_llm_enabled` | false | Use the LLM for consolidate / CoT summaries |
| `reasoning_llm_base_url` | `http://localhost:1234/v1` | OpenAI-compatible API |
| `reasoning_llm_model` | (empty) | Empty = auto-detect via `/v1/models` |
| `reasoning_llm_timeout_secs` | 120 | LLM call timeout |
| `backup_dir` | `~/.agents-relay/backups` | Output directory for `db backup` (separate physical disk recommended) |
| `embedding_model` | (empty) | Embedding model for semantic search; empty = disabled |
| `consolidate_enabled` | true | Master switch for `consolidate --watch` |
| `consolidate_batch` | 10 | Sessions per pass in watch mode |
| `consolidate_busy_secs` | 15 | Sleep between passes while draining |
| `consolidate_idle_secs` | 900 | Poll interval when the backlog is empty |
| `compact_enabled` | true | Materialize min/mid compact projections in the consolidate pass |

## 9. Backup and restore

- **Full backup** = the trio of `memory.db` + `fulltext.db` + `reasoning.db` (plus `archive/` if desired). `db backup` creates consistent snapshots via `VACUUM INTO` under `backup_dir` with rotation (`--keep N`, default 3 sets).
- **Light copy** = `db export-light` (no CoT, no FTS payload, includes sync_state). Run `db rebuild-fts` on the restored copy for full function including search.
- A broken FTS index can always be rebuilt with `db rebuild-fts` (the index is a derived artifact).
