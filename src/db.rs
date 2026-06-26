use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Default location for the SQLite database.
pub fn default_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("claude-history.db")
}

/// Default location of the Claude Code project history JSONL files.
pub fn default_source_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects")
}

pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

/// Create all tables and indexes. Idempotent.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    migrate(conn)?;
    Ok(())
}

/// Small in-place migrations for databases created by older versions.
fn migrate(conn: &Connection) -> Result<()> {
    // `parent_session_id` links a subagent transcript (`agent-*.jsonl`, every
    // record `isSidechain=true`) back to the main-thread session that spawned
    // it. Older DBs lack the column; add it and backfill from the preserved
    // `records.raw_json` (each subagent record carries the parent's id in its
    // own `sessionId` field, which differs from the agent file's id).
    if !has_column(conn, "sessions", "parent_session_id")? {
        conn.execute_batch("ALTER TABLE sessions ADD COLUMN parent_session_id TEXT;")?;
        conn.execute_batch(
            r#"
            UPDATE sessions
            SET parent_session_id = (
                SELECT json_extract(r.raw_json, '$.sessionId')
                FROM records r
                WHERE r.session_id = sessions.session_id
                  AND json_extract(r.raw_json, '$.sessionId') IS NOT NULL
                  AND json_extract(r.raw_json, '$.sessionId') <> sessions.session_id
                LIMIT 1
            )
            WHERE parent_session_id IS NULL;
            "#,
        )?;
    }
    // Created here (not in SCHEMA) so it runs only once the column is guaranteed
    // to exist on both fresh and migrated databases.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_session_id);",
    )?;
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Drop everything so an import starts fresh.
pub fn reset_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP TABLE IF EXISTS search_index;
        DROP TABLE IF EXISTS content_blocks;
        DROP TABLE IF EXISTS messages;
        DROP TABLE IF EXISTS events;
        DROP TABLE IF EXISTS records;
        DROP TABLE IF EXISTS sessions;
        DROP TABLE IF EXISTS files;
        "#,
    )?;
    init_schema(conn)?;
    Ok(())
}

const SCHEMA: &str = r#"
-- One row per imported JSONL file, used to skip unchanged files on re-import.
CREATE TABLE IF NOT EXISTS files (
    path        TEXT PRIMARY KEY,
    session_id  TEXT,
    size        INTEGER,
    mtime       INTEGER,
    imported_at TEXT
);

-- One row per session (one JSONL file == one session).
CREATE TABLE IF NOT EXISTS sessions (
    session_id      TEXT PRIMARY KEY,
    project_dir     TEXT,   -- encoded directory name under ~/.claude/projects
    project_path    TEXT,   -- decoded filesystem path of the project
    file_path       TEXT,
    custom_title    TEXT,
    ai_title        TEXT,
    git_branch      TEXT,
    cwd             TEXT,
    version         TEXT,
    first_timestamp TEXT,
    last_timestamp  TEXT,
    record_count    INTEGER DEFAULT 0,
    message_count   INTEGER DEFAULT 0,
    parent_session_id TEXT  -- set for subagent (agent-*) files: id of the spawning session
);

-- Every JSONL line is captured here, regardless of type. The full original
-- line is preserved in raw_json so nothing is ever lost.
CREATE TABLE IF NOT EXISTS records (
    id           INTEGER PRIMARY KEY,
    session_id   TEXT NOT NULL,
    seq          INTEGER NOT NULL,   -- line number within the file (1-based)
    record_type  TEXT NOT NULL,      -- assistant, user, system, attachment, ...
    uuid         TEXT,
    parent_uuid  TEXT,
    timestamp    TEXT,
    is_sidechain INTEGER,
    is_meta      INTEGER,
    user_type    TEXT,
    cwd          TEXT,
    git_branch   TEXT,
    version      TEXT,
    raw_json     TEXT NOT NULL
);

-- Decoded user/assistant turns.
CREATE TABLE IF NOT EXISTS messages (
    record_id     INTEGER PRIMARY KEY,
    session_id    TEXT NOT NULL,
    uuid          TEXT,
    parent_uuid   TEXT,
    role          TEXT,    -- user | assistant
    model         TEXT,
    message_id    TEXT,
    stop_reason   TEXT,
    timestamp     TEXT,
    input_tokens  INTEGER,
    output_tokens INTEGER,
    text_content  TEXT,    -- concatenated text blocks, for quick reading
    FOREIGN KEY (record_id) REFERENCES records(id)
);

-- One row per content block inside a message. block_type clearly marks what
-- kind of content it is: text, thinking, tool_use, tool_result, image, ...
CREATE TABLE IF NOT EXISTS content_blocks (
    id           INTEGER PRIMARY KEY,
    record_id    INTEGER NOT NULL,
    session_id   TEXT NOT NULL,
    message_uuid TEXT,
    seq          INTEGER NOT NULL,  -- position within the message
    block_type   TEXT NOT NULL,     -- text | thinking | tool_use | tool_result | image | ...
    role         TEXT,
    text         TEXT,              -- text / thinking content
    tool_name    TEXT,              -- tool_use
    tool_use_id  TEXT,              -- tool_use / tool_result
    tool_input   TEXT,              -- tool_use input, as JSON
    is_error     INTEGER,           -- tool_result error flag
    tool_result  TEXT,              -- tool_result content, flattened to text
    timestamp    TEXT,
    FOREIGN KEY (record_id) REFERENCES records(id)
);

-- Non-message events (titles, modes, pr links, system notices, snapshots, ...).
-- Each is clearly marked by event_type and given a human-readable summary.
CREATE TABLE IF NOT EXISTS events (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    event_type  TEXT NOT NULL,
    timestamp   TEXT,
    summary     TEXT,
    raw_json    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_records_session  ON records(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_records_type     ON records(record_type);
CREATE INDEX IF NOT EXISTS idx_blocks_session   ON content_blocks(session_id, seq);
-- Index the foreign-key column so deleting a session's `records` (e.g. on
-- re-import) doesn't full-scan content_blocks for each FK check.
CREATE INDEX IF NOT EXISTS idx_blocks_record    ON content_blocks(record_id);
CREATE INDEX IF NOT EXISTS idx_blocks_type      ON content_blocks(block_type);
CREATE INDEX IF NOT EXISTS idx_blocks_tool      ON content_blocks(tool_name);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
CREATE INDEX IF NOT EXISTS idx_messages_role    ON messages(role);
CREATE INDEX IF NOT EXISTS idx_events_session   ON events(session_id, seq);
CREATE INDEX IF NOT EXISTS idx_events_type      ON events(event_type);

-- Full-text search across all content. Standalone FTS5 table populated during
-- import; metadata columns are UNINDEXED so they can be filtered cheaply.
CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(
    body,
    block_id   UNINDEXED,
    session_id UNINDEXED,
    block_type UNINDEXED,
    role       UNINDEXED,
    tokenize = 'porter unicode61'
);
"#;
