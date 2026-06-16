mod db;
mod fts;
mod import;
mod query;
mod serve;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Import, browse, and search your Claude Code session history.
#[derive(Parser)]
#[command(name = "cch", version, about, long_about = None)]
struct Cli {
    /// Path to the SQLite database (default: ~/.claude/claude-history.db)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import JSONL history files into the database.
    Import {
        /// Directory of Claude Code projects (default: ~/.claude/projects)
        #[arg(long)]
        source: Option<PathBuf>,
        /// Re-import every file even if unchanged.
        #[arg(long)]
        force: bool,
        /// Drop and recreate all tables before importing.
        #[arg(long)]
        reset: bool,
    },
    /// List sessions, most recent first.
    Sessions {
        /// Filter by project path/name substring.
        #[arg(long)]
        project: Option<String>,
        /// Maximum number of sessions to show.
        #[arg(long, default_value_t = 40)]
        limit: i64,
    },
    /// Full-text search across all message content.
    Search {
        /// FTS5 query (e.g. `sidekiq AND retry`).
        query: String,
        /// Restrict to a block type: text, thinking, tool_use, tool_result, image.
        #[arg(long = "type")]
        block_type: Option<String>,
        /// Restrict to a role: user or assistant.
        #[arg(long)]
        role: Option<String>,
        /// Restrict to a session id (prefix ok).
        #[arg(long)]
        session: Option<String>,
        /// Maximum results.
        #[arg(long, default_value_t = 30)]
        limit: i64,
    },
    /// Print a full session transcript (every type clearly marked).
    Show {
        /// Session id (a unique prefix is enough).
        session: String,
        /// Also print non-message events (titles, modes, system notices, ...).
        #[arg(long)]
        events: bool,
        /// Dump the raw JSONL lines instead of formatting.
        #[arg(long)]
        raw: bool,
    },
    /// Show counts by record type, block type, tools, and tokens.
    Stats,
    /// Launch the web UI to browse and search history in a browser.
    Serve {
        /// Port to listen on.
        #[arg(long, default_value_t = 7777)]
        port: u16,
        /// Host/interface to bind.
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Do not auto-open the browser.
        #[arg(long)]
        no_open: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(db::default_db_path);

    match cli.command {
        Command::Import {
            source,
            force,
            reset,
        } => {
            let mut conn = db::open(&db_path)?;
            if reset {
                db::reset_schema(&conn)?;
            } else {
                db::init_schema(&conn)?;
            }
            let source = source.unwrap_or_else(db::default_source_dir);
            println!("Importing from {} ...", source.display());
            let stats = import::import_dir(&mut conn, &source, force)?;
            println!(
                "Done. {} file(s) scanned, {} imported, {} unchanged. {} records, {} content blocks.",
                stats.files_scanned,
                stats.files_imported,
                stats.files_skipped,
                stats.records,
                stats.blocks
            );
            println!("Database: {}", db_path.display());
        }
        Command::Sessions { project, limit } => {
            let conn = db::open(&db_path)?;
            db::init_schema(&conn)?;
            query::sessions(&conn, project.as_deref(), limit)?;
        }
        Command::Search {
            query,
            block_type,
            role,
            session,
            limit,
        } => {
            let conn = db::open(&db_path)?;
            db::init_schema(&conn)?;
            query::search(
                &conn,
                &query,
                block_type.as_deref(),
                role.as_deref(),
                session.as_deref(),
                limit,
            )?;
        }
        Command::Show {
            session,
            events,
            raw,
        } => {
            let conn = db::open(&db_path)?;
            db::init_schema(&conn)?;
            query::show(&conn, &session, events, raw)?;
        }
        Command::Stats => {
            let conn = db::open(&db_path)?;
            db::init_schema(&conn)?;
            query::stats(&conn)?;
        }
        Command::Serve {
            port,
            host,
            no_open,
        } => {
            serve::serve(&db_path, &host, port, !no_open)?;
        }
    }
    Ok(())
}
