# claude-relay: A Quick-and-Dirty Session Memory Store for Claude Code

## Background

I forked [claude-mem](https://github.com/anthropics/claude-mem) (a session memory plugin for Claude Code) intending to make it work with local LLMs, but after reading through the source code, I honestly found it unusable.

The design fires off an AI compression request for every single tool use, uses fetch with no timeout, has no retry strategy, conflates liveness with readiness, and irreversibly discards raw data after compression -- it's an implementation that misses the fundamentals of computer science. I wrote about the details in a [separate article](https://note.com/veltrea/n/n791d1defada0).

These problems just don't surface when you're using the Claude API, but the moment you switch to a local LLM, every single one of them becomes fatal. I tried patching the fork, but these are design-level issues that can't be fixed with partial patches.

And then it hit me -- there's no reason to use AI compression in the first place. Claude Code already writes all session data as JSONL under `~/.claude/projects/`. Just shove it into SQLite and let Claude's own 1M context understand the raw data at query time. No AI compression, no daemon needed.

So I built **claude-relay** from scratch.

## What It Is

- A single Rust binary (around 1,600 lines)
- Connects to Claude Code as an MCP server and provides tools for searching past sessions
- No daemon required. It incrementally ingests JSONL files at session start and on tool invocations
- You can also archive old data to Markdown and prune it from SQLite if you like

## Installation

You'll need a Rust build environment.

```bash
git clone https://github.com/veltrea/claude-relay.git
cd claude-relay
cargo build --release

# Register as an MCP server in Claude Code
claude mcp add --transport stdio --scope user claude-relay -- $(pwd)/target/release/claude-relay serve
```

If you want it on your PATH, just copy `target/release/claude-relay` wherever you like.

## Usage

### First, Ingest the JSONL Files

```bash
# Ingest all sessions under ~/.claude/projects/
claude-relay ingest ~/.claude/projects/

# Or just a specific file
claude-relay ingest path/to/session.jsonl

# Check how much got imported
claude-relay db stats
```

On my setup, that came out to about 48 sessions and 75,000 entries.

### Using It from Claude Code

Since it's registered as an MCP tool, you can just ask naturally within a Claude Code session:

- "Tell me what I worked on yesterday"
- "Find the thing where I fixed OAuth"
- "What was I doing between March 20th and 23rd?"
- "Show me my recent sessions"

Under the hood, MCP tools like `memory_search`, `memory_list_sessions`, and `memory_get_session` are being called.

### CLI Usage

There are also management commands for direct human use. Since going through MCP tools eats up tokens, admin tasks are designed to be done via the CLI.

```bash
# List sessions
claude-relay list
claude-relay list --date 2026-03-23

# Export a session's contents as Markdown
claude-relay export <session_id>
claude-relay export --date 2026-03-23

# Reset the DB
claude-relay db reset

# Run raw SQL (handy during development)
claude-relay query "SELECT type, COUNT(*) FROM raw_entries GROUP BY type"

# Manually write an entry (for testing)
claude-relay write "Test message" --type user
```

## Design Notes

### Store Everything, Filter on Read

I initially planned to store only `user` and `assistant` entries, but then I thought, "Why not just dump everything in and filter with WHERE clauses on read?" So `system`, `progress`, `queue-operation` -- it all goes in. That way, if you later decide you want to look at some piece of data, it's already there.

### No Daemon

I considered running a file-watching daemon (like chokidar), but decided against it. Instead, it does incremental ingestion on the SessionStart hook and on MCP tool invocations. It records a byte offset for "how far we've read" in each JSONL file and only processes new lines.

### Archiving

If you set `retention_days` in the config file (`~/.claude-relay/config.json`), expired data gets exported to Markdown and removed from the DB. The default is 30 days.

```jsonc
{
  "retention_days": 30,
  "archive_dir": "~/.claude-relay/archive"
}
```

## Heads Up

I threw this together in about 30 minutes. There's barely any testing. It works on my machine (macOS), but I haven't tried it anywhere else.

If you find a bug or it doesn't work for you, I'd appreciate it if you let me know via [Issues](https://github.com/veltrea/claude-relay/issues).

I'm not accepting PRs. I'm the type to rewrite the entire codebase on a whim, so there's a good chance the original code won't even exist by the time a PR comes in. If you're interested, feel free to fork it and do whatever you want with it. Anyone can build something like this with vibe coding.

## License

MIT License
