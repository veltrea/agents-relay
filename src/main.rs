mod client;
mod config;
mod db;
mod detect;
mod export;
mod ingest;
mod ingest_antigravity;
mod mcp;
mod server;
mod session_hook;
mod workspace;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "agents-relay", version, about = "Multi-agent session memory for AI coding tools")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start HTTP backend server (auto-started by serve, rarely needed directly)
    Server,

    /// Start MCP stdio server (thin proxy to HTTP server)
    Serve {
        /// Workspace path for scoped queries (defaults to CWD)
        #[arg(long, env = "AGENTS_RELAY_WORKSPACE")]
        workspace: Option<String>,
    },

    /// Database management
    Db {
        #[command(subcommand)]
        action: DbAction,
    },

    /// Archive old entries to Markdown
    Archive {
        /// Dry run (show what would be archived)
        #[arg(long)]
        dry: bool,
    },

    /// List sessions
    List {
        /// Filter by date (YYYY-MM-DD)
        #[arg(long)]
        date: Option<String>,
        /// Max results
        #[arg(long, default_value = "10")]
        limit: i64,
    },

    /// Export session(s) to Markdown
    Export {
        /// Session ID to export
        session_id: Option<String>,
        /// Export by date
        #[arg(long)]
        date: Option<String>,
        /// Date range start
        #[arg(long)]
        from: Option<String>,
        /// Date range end
        #[arg(long)]
        to: Option<String>,
        /// Output directory
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Write a test entry manually
    Write {
        /// Message content
        content: String,
        /// Entry type (user/assistant/system)
        #[arg(long, default_value = "user")]
        r#type: String,
        /// Session ID
        #[arg(long, default_value = "manual")]
        session: String,
    },

    /// Ingest JSONL file(s)
    Ingest {
        /// Path to JSONL file or directory
        path: PathBuf,
    },

    /// Sync state management
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },

    /// Write a summary entry (for sidecar/batch summarization)
    Summarize {
        /// Summary text
        summary: String,
        /// Session ID to associate with
        #[arg(long)]
        session_id: Option<String>,
        /// Comma-separated tags (optional)
        #[arg(long)]
        tags: Option<String>,
    },

    /// Execute raw SQL query
    Query {
        /// SQL statement
        sql: String,
    },

    /// Workspace configuration (.agents-relay.json)
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },

    /// Test MCP tools from CLI
    Tool {
        /// Tool name (memory_search, memory_list_sessions, etc.)
        name: String,
        /// Query string
        #[arg(long)]
        query: Option<String>,
        /// Date filter
        #[arg(long)]
        date: Option<String>,
        /// Session ID
        #[arg(long)]
        session_id: Option<String>,
        /// Entry ID
        #[arg(long)]
        id: Option<i64>,
        /// Max results
        #[arg(long)]
        limit: Option<i64>,
    },

    /// Emit a compact memory digest for a SessionStart hook.
    /// stdout is injected into Claude's context (bypasses Tool Search deferral).
    SessionHook {
        /// Workspace path (defaults to CWD)
        #[arg(long, env = "AGENTS_RELAY_WORKSPACE")]
        workspace: Option<String>,
        /// Max sessions to summarize
        #[arg(long, default_value = "3")]
        limit: i64,
    },
}

#[derive(Subcommand)]
enum DbAction {
    /// Reset database (delete and recreate)
    Reset {
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show database statistics
    Stats,
    /// Run VACUUM
    Vacuum,
    /// Rebuild FTS index (use if search is broken)
    RebuildFts,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Set a configuration value
    Set {
        /// Config key
        key: String,
        /// Config value
        value: String,
    },
}

#[derive(Subcommand)]
enum WorkspaceAction {
    /// Show workspace config (.agents-relay.json)
    Show,
    /// Enable agents-relay for this workspace
    Enable,
    /// Disable agents-relay for this workspace
    Disable,
    /// Set a workspace config value
    Set {
        /// Config key (enabled, cross_scope)
        key: String,
        /// Config value
        value: String,
    },
}

#[derive(Subcommand)]
enum SyncAction {
    /// Show sync status
    Status,
    /// Reset all sync offsets
    Reset,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = config::Config::db_path();
    let mut conn = db::open(&db_path)?;
    db::init(&conn)?;

    match cli.command {
        Commands::Server => {
            server::run()?;
            return Ok(());
        }

        Commands::Serve { workspace } => {
            mcp::serve(workspace)?;
            return Ok(());
        }

        Commands::Db { action } => match action {
            DbAction::Reset { yes } => {
                // サーバーが稼働中なら中止
                let port = client::read_server_port();
                if client::is_server_alive(port) {
                    anyhow::bail!(
                        "agents-relay server is running on port {port}. Stop it first:\n  pkill -f 'agents-relay.*server'"
                    );
                }
                if !yes {
                    eprint!("This will delete ALL memory data. Are you sure? [y/N] ");
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    if !input.trim().eq_ignore_ascii_case("y") {
                        println!("Aborted.");
                        return Ok(());
                    }
                }
                db::reset(&conn)?;
                println!("Database reset complete.");
            }
            DbAction::Stats => {
                println!("{}", db::stats(&conn)?);
            }
            DbAction::Vacuum => {
                conn.execute_batch("VACUUM")?;
                println!("VACUUM complete.");
            }
            DbAction::RebuildFts => {
                db::rebuild_fts(&conn)?;
                println!("FTS index rebuilt.");
            }
        },

        Commands::Archive { dry } => {
            let cfg = config::Config::load()?;
            let cutoff = chrono::Utc::now()
                - chrono::Duration::days(cfg.retention_days as i64);
            let cutoff_date = cutoff.format("%Y-%m-%d").to_string();

            // stmt の借用を先に解放してから conn.transaction() を呼べるよう
            // dates を Vec に収集してからループに入る（BUG-004対応）
            let dates: Vec<String> = {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT date FROM raw_entries WHERE date < ?1 ORDER BY date",
                )?;
                let collected: Vec<String> = stmt
                    .query_map(rusqlite::params![cutoff_date], |row| row.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                collected
            };

            if dates.is_empty() {
                println!("No entries older than {cutoff_date} to archive.");
                return Ok(());
            }

            let archive_dir = config::resolve_archive_dir(&cfg);

            for date in &dates {
                let parts: Vec<&str> = date.split('-').collect();
                if parts.len() != 3 {
                    continue;
                }
                let dir = archive_dir.join(parts[0]).join(parts[1]);
                let file = dir.join(format!("{}.md", parts[2]));

                if dry {
                    println!("[dry] Would archive {date} -> {}", file.display());
                } else {
                    let md = export::export_date(&conn, date)?;
                    std::fs::create_dir_all(&dir)?;
                    // クラッシュ後の再実行で二重書き出しにならないよう冪等化
                    if !file.exists() {
                        std::fs::write(&file, &md)?;
                    }
                    // ファイル書き出し完了後にトランザクションで削除（BUG-004修正）
                    let tx = conn.transaction()?;
                    tx.execute(
                        "DELETE FROM raw_entries WHERE date = ?1",
                        rusqlite::params![date],
                    )?;
                    tx.commit()?;
                    println!("Archived {date} -> {}", file.display());
                }
            }

            if !dry {
                conn.execute_batch(
                    "DELETE FROM raw_entries_fts;
                     INSERT INTO raw_entries_fts (rowid, content, tool_name, session_id)
                     SELECT id, content, tool_name, session_id FROM raw_entries;",
                )?;
                println!("FTS index rebuilt.");
            }
        }

        Commands::List { date, limit } => {
            let sessions = db::list_sessions(&conn, date.as_deref(), limit, None)?;
            if sessions.is_empty() {
                println!("No sessions found.");
            } else {
                println!(
                    "{:<40} {:<12} {:<12} {:>6}",
                    "SESSION_ID", "DATE", "LAST_TIME", "ENTRIES"
                );
                println!("{}", "-".repeat(74));
                for (sid, _first, last, date, count) in &sessions {
                    let time_part = if last.len() >= 19 { &last[11..19] } else { last };
                    println!("{:<40} {:<12} {:<12} {:>6}", sid, date, time_part, count);
                }
            }
        }

        Commands::Export {
            session_id,
            date,
            from,
            to,
            output,
        } => {
            let md = if let Some(sid) = session_id {
                export::export_session(&conn, &sid)?
            } else if let Some(d) = date {
                export::export_date(&conn, &d)?
            } else if from.is_some() || to.is_some() {
                let f = from.as_deref().unwrap_or("2000-01-01");
                let t = to.as_deref().unwrap_or("2099-12-31");
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT date FROM raw_entries WHERE date >= ?1 AND date <= ?2 ORDER BY date",
                )?;
                let dates: Vec<String> = stmt
                    .query_map(rusqlite::params![f, t], |row| row.get(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                let mut combined = String::new();
                for d in &dates {
                    combined.push_str(&export::export_date(&conn, d)?);
                    combined.push('\n');
                }
                combined
            } else {
                anyhow::bail!("Specify session_id, --date, or --from/--to");
            };

            if let Some(out_path) = output {
                std::fs::create_dir_all(&out_path)?;
                let filename = out_path.join("export.md");
                std::fs::write(&filename, &md)?;
                println!("Exported to {}", filename.display());
            } else {
                print!("{md}");
            }
        }

        Commands::Config { action } => match action {
            ConfigAction::Show => {
                let cfg = config::Config::load()?;
                println!("{}", cfg.show());
            }
            ConfigAction::Set { key, value } => {
                let mut cfg = config::Config::load()?;
                cfg.set(&key, &value)?;
                println!("Set {key} = {value}");
            }
        },

        Commands::Write {
            content,
            r#type,
            session,
        } => {
            let now = chrono::Utc::now();
            let ts = now.to_rfc3339();
            let date = now.format("%Y-%m-%d").to_string();
            let time = now.format("%H:%M:%S").to_string();
            let id = db::insert_entry(
                &conn, &session, &ts, &date, &time, &r#type, None, &content, None, None, "claude",
            )?;
            println!("Inserted entry id={id} type={} session={session}", r#type);
        }

        Commands::Ingest { path } => {
            let count = if path.is_dir() {
                ingest::ingest_dir(&mut conn, &path)?
            } else {
                ingest::ingest_file(&mut conn, &path)?
            };
            println!("Ingested {count} entries.");
        }

        Commands::Sync { action } => match action {
            SyncAction::Status => {
                println!("{}", ingest::sync_status(&conn)?);
            }
            SyncAction::Reset => {
                ingest::sync_reset(&conn)?;
                println!("Sync offsets reset.");
            }
        },

        Commands::Summarize { summary, session_id, tags } => {
            let session = session_id.as_deref().unwrap_or("agent-summary");
            let content = match tags.as_deref() {
                Some(t) if !t.is_empty() => format!("[Tags: {}]\n{}", t, summary),
                _ => summary.clone(),
            };
            let now = chrono::Utc::now();
            let ts = now.to_rfc3339();
            let date = now.format("%Y-%m-%d").to_string();
            let time = now.format("%H:%M:%S").to_string();
            let id = db::insert_entry(
                &conn, session, &ts, &date, &time,
                "summary", Some("skill_hook"), &content,
                None, None, "summarizer",
            )?;
            println!("Recorded summary id={id} session={session}");
        }

        Commands::Workspace { action } => {
            let cwd = std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string());
            let cwd_ref = cwd.as_deref();
            let mut ws_cfg = workspace::WorkspaceConfig::load(cwd_ref);

            match action {
                WorkspaceAction::Show => {
                    let ws = cwd_ref.unwrap_or("(unknown)");
                    println!("{}", ws_cfg.show(ws));
                }
                WorkspaceAction::Enable => {
                    ws_cfg.enabled = Some(true);
                    ws_cfg.save(cwd_ref)?;
                    println!("agents-relay enabled for this workspace.");
                }
                WorkspaceAction::Disable => {
                    ws_cfg.enabled = Some(false);
                    ws_cfg.save(cwd_ref)?;
                    println!("agents-relay disabled for this workspace.");
                }
                WorkspaceAction::Set { key, value } => {
                    ws_cfg.set(&key, &value)?;
                    ws_cfg.save(cwd_ref)?;
                    println!("Set {key} = {value}");
                }
            }
        }

        Commands::Query { sql } => {
            let mut stmt = conn.prepare(&sql)?;
            let col_count = stmt.column_count();
            let col_names: Vec<String> = (0..col_count)
                .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
                .collect();

            println!("{}", col_names.join("\t"));
            println!("{}", "-".repeat(col_names.len() * 16));

            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let vals: Vec<String> = (0..col_count)
                    .map(|i| {
                        row.get::<_, String>(i)
                            .or_else(|_| row.get::<_, i64>(i).map(|v| v.to_string()))
                            .or_else(|_| row.get::<_, f64>(i).map(|v| v.to_string()))
                            .unwrap_or_else(|_| "NULL".to_string())
                    })
                    .collect();
                println!("{}", vals.join("\t"));
            }
        }

        Commands::Tool {
            name,
            query,
            date,
            session_id,
            id,
            limit,
        } => {
            let sync_count = ingest::sync_all(&mut conn)?;
            if sync_count > 0 {
                eprintln!("Synced {sync_count} entries before tool call.");
            }

            match name.as_str() {
                "memory_search" => {
                    let q = query.as_deref().unwrap_or("");
                    let entries = db::search(
                        &conn,
                        q,
                        date.as_deref(),
                        None,
                        None,
                        None,
                        session_id.as_deref(),
                        limit.unwrap_or(20),
                        None, // workspace
                        None, // source
                    )?;
                    for e in &entries {
                        let preview: String = e.content.chars().take(120).collect();
                        println!("[{}] {} | {} | {} | {}: {}", e.id, e.date, e.time, e.source, e.entry_type, preview);
                    }
                    println!("\n{} results.", entries.len());
                }
                "memory_get_entry" => {
                    let entry_id = id.unwrap_or(0);
                    match db::get_entry(&conn, entry_id)? {
                        Some(e) => {
                            println!(
                                "id={} session={} {} {} type={}\n{}",
                                e.id, e.session_id, e.date, e.time, e.entry_type, e.content
                            );
                        }
                        None => println!("Not found."),
                    }
                }
                "memory_get_session" => {
                    let sid = session_id.as_deref().unwrap_or("");
                    let entries =
                        db::get_session_entries(&conn, sid, None, limit.unwrap_or(50))?;
                    for e in &entries {
                        println!("[{}] {} {}: {}", e.id, e.time, e.entry_type, e.content);
                    }
                }
                _ => {
                    println!("Unknown tool: {name}");
                    println!("Available: memory_search, memory_get_entry, memory_get_session");
                    println!("Tip: use 'agents-relay list' for session listing.");
                }
            }
        }

        Commands::SessionHook { workspace, limit } => {
            // sync は失敗してもダイジェスト生成を続行する（hook はセッションを壊さない）
            let _ = ingest::sync_all(&mut conn);
            let ws = workspace.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            });
            let digest = session_hook::build_digest(&conn, ws.as_deref(), limit)?;
            if !digest.is_empty() {
                print!("{digest}");
            }
        }
    }

    Ok(())
}
