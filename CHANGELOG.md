# Changelog

**Read this in other languages:** [日本語](CHANGELOG.ja.md)

## [0.2.2] - 2026-07-13

### Compact conversation projections (token-frugal session replay)

Chain-of-thought was already split out at ingest; the remaining token sink in session replay was tool payloads (file bodies in `Write`, 2,000-line `Read` results, long `Bash` output). This release adds a fourth derived storage tier that folds them.

- New storage tier `compact.db` (`cdb.entry_compact`): deterministic, LLM-free **min/mid** projections of every entry, produced by a versioned recipe (`recipe_version`)
  - min (skeleton): prose points, tool calls collapsed to name + target (`[Write /x/db.rs]`), tool outputs to a one-line lines/bytes summary
  - mid (flow and intent): near-full prose, tool signatures with payload hints (`[use: Write /x/db.rs (+120 lines)]`), the head of each tool output
  - Error tool_results are kept at both levels; chain-of-thought is excluded at both
- `memory_get_session` gains a `detail` parameter: `gist` (session summary only) / `min` (default) / `mid` / `full`; each message carries a `tokens` menu `{min,mid,full}` so the caller can escalate selectively (measured: a 300KB tool result is full≈75,000 tokens → min≈8)
- `detail='full'` is now transport-capped per entry (200KB), same as `memory_get_entry`
- New column `raw_entries.content_tokens` (estimated tokens of the full body; backfilled automatically for old rows)
- The consolidate pass materializes missing/stale projections (up to 2,000 per pass; `--watch` treats a compact backlog as busy work)
- New config key `compact_enabled` (default true)
- New CLI: `db backfill-compact` (bulk backfill) / `db rebuild-compact` (full regeneration after a recipe change); `db stats` shows the projection count
- Fully derived: deleting `compact.db` only makes `memory_get_session` fall back to inline heads

---

## [0.2.1] - 2026-07-12

### Lossless three-tier storage

Storage was split into three tiers around one principle: never lose the original; save tokens on the presentation side.

- Raw chain-of-thought (thinking) moved to `reasoning.db` (`rdb.cot`); `memory.db` keeps only an estimated-token pointer
- Content larger than 50KB keeps a 1KB inline head + marker while the full text is preserved losslessly in `fulltext.db` (`fdb.fulltext`); FTS indexes the inline part only
- All ingest-side truncation (content 50KB / reasoning 200KB / antigravity 48KB) removed; the policy is centralized in `db::insert_entry_full`
- `db backfill-fulltext`: recovers previously-truncated content and thinking that older versions discarded, from source JSONL / `.jsonl.bak` files (idempotent)
- `db export-light`: one-way lightweight copy without CoT or FTS payload (for backup/transport; fully restorable via `db rebuild-fts`)
- `archive` now writes a lossless JSONL (all columns + raw CoT + full text) and fsyncs it before deleting rows (append-only)
- `db restore-archive <file.jsonl>`: idempotent restore from an archive
- Fixed `db rebuild-fts` always failing on regular FTS5 tables (`'delete-all'` is contentless-only; switched to DROP + CREATE)
- Fixed ghost FTS rows left behind by update-time deletes (side databases are now deleted in sync)

### Sleep-time consolidation (`consolidate`)

- `consolidate`: distills finished sessions into the `summaries` table using a local LLM (LM Studio or any OpenAI-compatible server), falling back to extractive compression when the LLM is unreachable; `--upgrade` re-processes heuristic summaries later
- Durable facts (DECISION / CONSTRAINT / PREFERENCE / ENVIRONMENT) are extracted into a `facts` table; a character-bigram Jaccard novelty gate (>= 0.6) drops facts that are already known, and supersession is tracked via `superseded_by` instead of deletion
- New MCP tool `memory_facts` (searches distilled facts — far cheaper than raw-log search)
- `memory_list_sessions` returns a summary snippet; `memory_get_session` prepends the session summary

### Search

- Added a `context` column (workspace basename + git branch + source) to the FTS index, so past memory is searchable by project or branch name (a lightweight take on contextual retrieval)

### Richer Antigravity ingest

- Parses `agyhub_summaries_proto.pb` (plain protobuf, hand-written wire-format reader) for every conversation's title, workspace, and timestamps — sessions with empty brain directories become searchable
- Incremental ingest of `.system_generated/logs/transcript.jsonl` with real user / assistant / tool roles
- Ingests `knowledge/<topic>/` (distilled knowledge base)
- Artifact change detection uses size + mtime (same-size rewrites are no longer missed)

### MCP

- `memory_get_entry` / `memory_get_reasoning` transport is capped at 200KB (stored data remains lossless)

### Semantic search (hybrid retrieval)

- Added an embedding tier `vectors.db` powered by sqlite-vec + an OpenAI-compatible `/embeddings` endpoint (fully derived)
- `memory_search` augments FTS results with semantic neighbors of summaries and facts (`match: "semantic"`), covering the rephrasings that FTS5 misses on Japanese text
- New `embed` command (bulk backfill); `consolidate` embeds new items automatically
- New config key `embedding_model` (empty = disabled)

### Operations

- `db backup`: consistent snapshots of all three databases via `VACUUM INTO` under `backup_dir`, with rotation (`--keep N`)
- New config key `backup_dir`
- Server port can be overridden with the `AGENTS_RELAY_PORT` environment variable (for tests)

---

## [0.2.0] - 2026-04-01

### Breaking Changes

- `agents-relay serve` no longer owns the DB by itself. It auto-starts the HTTP backend server (`agents-relay server`) on startup.
- Environment variable renamed: `CLAUDE_RELAY_WORKSPACE` → `AGENTS_RELAY_WORKSPACE`.

### Architecture: Server/Client split

Ported the antigravity-relay design, moving fully from a single-process model to a Server/Serve split.

```
Before: serve(WS1), serve(WS2), serve(WS3) each wrote directly to SQLite
         → risk of BUSY errors with multiple concurrent workspaces

After:  serve(WS1) ─┐
        serve(WS2) ─┼─→ HTTP:54322 → server (single process) → SQLite
        serve(WS3) ─┘
         → all writes serialized; BUSY errors eliminated
```

#### New files

- **`src/server.rs`** — Axum HTTP server (`127.0.0.1:54322`)
  - `/health` `/register` `/deregister` `/tool` endpoints
  - Owns all tool logic together with the DB
  - First sync 500ms after startup, then background incremental sync every 30 seconds
  - Auto-exits after ~60 seconds with zero serve processes and no `pgrep` match
  - Detects binary updates and restarts stale servers automatically

- **`src/client.rs`** — HTTP client utilities
  - `ensure_server_running()`: starts and connects to the server if it is down
  - `call_tool()`: forwards tool calls over HTTP with automatic retry
  - `register()` / `deregister()`: connection-count management
  - Port number managed via `~/.agents-relay/server.port` (default 54322)

#### Changed files

- **`src/mcp.rs`** — became a thin proxy
  - Direct DB access fully removed
  - `ServerScope`, `WorkspaceConfig`, and the cross-scope gate logic remain on the serve side
  - `memory_unlock_cross_scope` completes within serve (no server call needed)
  - Scope resolution (workspace/source) happens on the serve side; resolved values are sent to the server

- **`src/main.rs`** — added the `server` subcommand
- **`Cargo.toml`** — added `axum 0.7` and `reqwest 0.12`

---

## [0.1.1] - 2026-04-01

### Bug Fixes

- **BUG-001/002: duplicate inserts and FTS inconsistency on crash** (`src/ingest.rs`)
  - Wrapped `ingest_file` in a transaction (batch commit every 100 entries)
  - Offset persistence happens inside the same transaction, preventing duplicate ingestion after a crash
  - The two INSERTs into `raw_entries` and `raw_entries_fts` can no longer be torn apart

- **BUG-003: offset drift on Windows CRLF files** (`src/ingest.rs`)
  - Replaced the `lines()` iterator (which strips newlines) with `read_line()`
  - Offsets are computed from actual bytes read, so CRLF (`\r\n`) files track correctly

- **BUG-004: DB/file inconsistency when `archive` crashes midway** (`src/main.rs`)
  - `DELETE FROM raw_entries` is protected by a transaction
  - Archive file writes are idempotent (existing files are not overwritten)
  - Collected `stmt` borrows into a `Vec` before release so `conn.transaction()` can be called safely

- **BUG-005: LIKE queries misbehaving on special characters in workspace filters** (`src/db.rs`)
  - `%`, `_`, and `\` in workspace paths are escaped with `ESCAPE '\\'`
  - Applied to both `search()` and `list_sessions()`

- **BUG-006: process names containing `code` misdetected as VS Code** (`src/detect.rs`)
  - Changed `contains("code")` to exact/prefix matching (no more false positives on `recode`, `xcode`, etc.)
  - Condition: `n == "code" || n.starts_with("code ") || n.starts_with("code-server") || n.contains("vscode") || n == "codium"`
  - Added a unit test to prevent regressions

- **BUG-007: `memory_get_entry` failing when `id` arrives as a float** (`src/mcp.rs`)
  - AI clients sometimes send `3.0`; handled with an `as_f64()` fallback
  - `memory_search` / `memory_list_sessions` / `memory_get_session` already handled this; only `memory_get_entry` was missing it

- **BUG-009: entries with `cwd IS NULL` excluded under workspace scoping** (`src/db.rs`)
  - Added `OR cwd IS NULL` to the `LIKE` condition so entries without a cwd stay in scope
  - Applied to both `search()` and `list_sessions()` together with the BUG-005 fix

### Internal Changes

- Changed signatures of `ingest_file` / `ingest_dir` / `sync_all` from `&Connection` to `&mut Connection`
- Same change for `ingest_antigravity::sync_all` / `ingest_brain` / `ingest_session`
- `mcp::serve` / `handle_request` / `handle_tool_call` updated for `&mut Connection`

---

## [0.1.0] - 2026-03-xx

Initial release. Fork from [claude-relay](https://github.com/primalcolors/claude-relay) with multi-agent support.

### Features

- Supports both Claude Code (JSONL) and Antigravity (Markdown + JSON) sources
- Workspace scoping (`scope=self/cross/all`)
- Automatic caller-agent detection via PPID
- `memory_search` / `memory_get_entry` / `memory_list_sessions` / `memory_get_session` MCP tools
- `memory_write_session` / `memory_record_summary` / `memory_unlock_cross_scope` MCP tools (Antigravity only)
- Full-text search with SQLite WAL + FTS5
- `archive` / `export` / `list` / `sync` CLI commands
