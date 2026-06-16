use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

pub struct ImportStats {
    pub files_scanned: usize,
    pub files_imported: usize,
    pub files_skipped: usize,
    pub records: usize,
    pub blocks: usize,
}

/// Walk `source` for *.jsonl files and import each into the database.
/// Files whose size+mtime are unchanged since the last import are skipped
/// (unless `force` is set).
pub fn import_dir(conn: &mut Connection, source: &Path, force: bool) -> Result<ImportStats> {
    let mut stats = ImportStats {
        files_scanned: 0,
        files_imported: 0,
        files_skipped: 0,
        records: 0,
        blocks: 0,
    };

    for entry in WalkDir::new(source).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        stats.files_scanned += 1;

        let meta = std::fs::metadata(path)?;
        let size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let path_str = path.to_string_lossy().to_string();
        if !force && file_unchanged(conn, &path_str, size, mtime)? {
            stats.files_skipped += 1;
            continue;
        }

        let (recs, blocks) = import_file(conn, path, size, mtime)
            .with_context(|| format!("importing {}", path.display()))?;
        stats.files_imported += 1;
        stats.records += recs;
        stats.blocks += blocks;
    }

    Ok(stats)
}

fn file_unchanged(conn: &Connection, path: &str, size: i64, mtime: i64) -> Result<bool> {
    let found: Option<(i64, i64)> = conn
        .query_row(
            "SELECT size, mtime FROM files WHERE path = ?1",
            params![path],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    Ok(found == Some((size, mtime)))
}

/// Import a single JSONL file inside one transaction.
fn import_file(conn: &mut Connection, path: &Path, size: i64, mtime: i64) -> Result<(usize, usize)> {
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_dir = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let project_path = decode_project_dir(&project_dir);
    let path_str = path.to_string_lossy().to_string();

    let tx = conn.transaction()?;

    // Clear any prior import of this session (handles re-imports cleanly).
    // Delete child rows before the parent `records` they reference so the
    // foreign-key constraints hold during the delete.
    //
    // search_index is an FTS5 table; its `session_id` is UNINDEXED, so deleting
    // by it would scan the whole index. Instead we delete by rowid (which we set
    // equal to content_blocks.id) using the indexed lookup on content_blocks.
    tx.execute(
        "DELETE FROM search_index
         WHERE rowid IN (SELECT id FROM content_blocks WHERE session_id = ?1)",
        params![session_id],
    )?;
    tx.execute(
        "DELETE FROM content_blocks WHERE session_id = ?1",
        params![session_id],
    )?;
    tx.execute("DELETE FROM messages WHERE session_id = ?1", params![session_id])?;
    tx.execute("DELETE FROM events WHERE session_id = ?1", params![session_id])?;
    tx.execute("DELETE FROM records WHERE session_id = ?1", params![session_id])?;

    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);

    let mut record_count = 0usize;
    let mut block_count = 0usize;
    let mut message_count = 0usize;
    let mut first_ts: Option<String> = None;
    let mut last_ts: Option<String> = None;
    let mut custom_title: Option<String> = None;
    let mut ai_title: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut version: Option<String> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let seq = (idx + 1) as i64;
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue, // skip malformed lines but keep going
        };

        let rtype = v
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown")
            .to_string();
        let uuid = str_field(&v, "uuid");
        let parent_uuid = str_field(&v, "parentUuid");
        let timestamp = str_field(&v, "timestamp");
        let is_sidechain = bool_field(&v, "isSidechain");
        let is_meta = bool_field(&v, "isMeta");
        let user_type = str_field(&v, "userType");
        let rcwd = str_field(&v, "cwd");
        let rbranch = str_field(&v, "gitBranch");
        let rversion = str_field(&v, "version");

        if let Some(ts) = &timestamp {
            if first_ts.is_none() {
                first_ts = Some(ts.clone());
            }
            last_ts = Some(ts.clone());
        }
        if cwd.is_none() {
            cwd = rcwd.clone();
        }
        if git_branch.is_none() {
            git_branch = rbranch.clone();
        }
        if version.is_none() {
            version = rversion.clone();
        }

        tx.execute(
            "INSERT INTO records
                (session_id, seq, record_type, uuid, parent_uuid, timestamp,
                 is_sidechain, is_meta, user_type, cwd, git_branch, version, raw_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                session_id,
                seq,
                rtype,
                uuid,
                parent_uuid,
                timestamp,
                is_sidechain,
                is_meta,
                user_type,
                rcwd,
                rbranch,
                rversion,
                line,
            ],
        )?;
        let record_id = tx.last_insert_rowid();
        record_count += 1;

        match rtype.as_str() {
            "user" | "assistant" => {
                message_count += 1;
                block_count += import_message(
                    &tx,
                    record_id,
                    &session_id,
                    &rtype,
                    uuid.as_deref(),
                    parent_uuid.as_deref(),
                    timestamp.as_deref(),
                    &v,
                )?;
            }
            other => {
                // Capture useful titles/branch for the session row.
                match other {
                    "custom-title" => custom_title = str_field(&v, "customTitle"),
                    "ai-title" => ai_title = str_field(&v, "aiTitle"),
                    _ => {}
                }
                let summary = summarize_event(other, &v);
                tx.execute(
                    "INSERT INTO events (session_id, seq, event_type, timestamp, summary, raw_json)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                    params![session_id, seq, other, timestamp, summary, line],
                )?;

                // Queued messages (enqueue/popAll) carry real conversation text
                // the user typed while Claude was busy. Store the full content as
                // a searchable `queued` block so it shows up in results.
                if other == "queue-operation" {
                    if let Some(content) = v.get("content") {
                        let text = flatten_content(content);
                        if !text.trim().is_empty() {
                            let op = v
                                .get("operation")
                                .and_then(|x| x.as_str())
                                .unwrap_or("enqueue");
                            block_count += insert_queued_block(
                                &tx,
                                record_id,
                                &session_id,
                                uuid.as_deref(),
                                timestamp.as_deref(),
                                op,
                                &text,
                            )?;
                        }
                    }
                }
            }
        }
    }

    // Upsert the session row.
    tx.execute(
        "INSERT INTO sessions
            (session_id, project_dir, project_path, file_path, custom_title, ai_title,
             git_branch, cwd, version, first_timestamp, last_timestamp,
             record_count, message_count)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(session_id) DO UPDATE SET
            project_dir=excluded.project_dir,
            project_path=excluded.project_path,
            file_path=excluded.file_path,
            custom_title=COALESCE(excluded.custom_title, sessions.custom_title),
            ai_title=COALESCE(excluded.ai_title, sessions.ai_title),
            git_branch=excluded.git_branch,
            cwd=excluded.cwd,
            version=excluded.version,
            first_timestamp=excluded.first_timestamp,
            last_timestamp=excluded.last_timestamp,
            record_count=excluded.record_count,
            message_count=excluded.message_count",
        params![
            session_id,
            project_dir,
            project_path,
            path_str,
            custom_title,
            ai_title,
            git_branch,
            cwd,
            version,
            first_ts,
            last_ts,
            record_count as i64,
            message_count as i64,
        ],
    )?;

    tx.execute(
        "INSERT INTO files (path, session_id, size, mtime, imported_at)
         VALUES (?1,?2,?3,?4, datetime('now'))
         ON CONFLICT(path) DO UPDATE SET
            session_id=excluded.session_id, size=excluded.size,
            mtime=excluded.mtime, imported_at=excluded.imported_at",
        params![path_str, session_id, size, mtime],
    )?;

    tx.commit()?;
    Ok((record_count, block_count))
}

#[allow(clippy::too_many_arguments)]
fn import_message(
    tx: &rusqlite::Transaction,
    record_id: i64,
    session_id: &str,
    rtype: &str,
    uuid: Option<&str>,
    parent_uuid: Option<&str>,
    timestamp: Option<&str>,
    v: &Value,
) -> Result<usize> {
    let msg = v.get("message");
    let role = msg
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        .unwrap_or(rtype)
        .to_string();
    let model = msg.and_then(|m| m.get("model")).and_then(|x| x.as_str());
    let message_id = msg.and_then(|m| m.get("id")).and_then(|x| x.as_str());
    let stop_reason = msg
        .and_then(|m| m.get("stop_reason"))
        .and_then(|x| x.as_str());
    let usage = msg.and_then(|m| m.get("usage"));
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|x| x.as_i64());
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(|x| x.as_i64());

    let content = msg.and_then(|m| m.get("content"));
    let mut text_parts: Vec<String> = Vec::new();
    let mut blocks: Vec<Block> = Vec::new();

    match content {
        Some(Value::String(s)) => {
            text_parts.push(s.clone());
            blocks.push(Block::text(s.clone()));
        }
        Some(Value::Array(arr)) => {
            for item in arr {
                let b = parse_block(item);
                if let Some(t) = &b.text {
                    if b.block_type == "text" {
                        text_parts.push(t.clone());
                    }
                }
                blocks.push(b);
            }
        }
        _ => {}
    }

    let text_content = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    tx.execute(
        "INSERT INTO messages
            (record_id, session_id, uuid, parent_uuid, role, model, message_id,
             stop_reason, timestamp, input_tokens, output_tokens, text_content)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            record_id,
            session_id,
            uuid,
            parent_uuid,
            role,
            model,
            message_id,
            stop_reason,
            timestamp,
            input_tokens,
            output_tokens,
            text_content,
        ],
    )?;

    let count = blocks.len();
    for (i, b) in blocks.into_iter().enumerate() {
        tx.execute(
            "INSERT INTO content_blocks
                (record_id, session_id, message_uuid, seq, block_type, role,
                 text, tool_name, tool_use_id, tool_input, is_error, tool_result, timestamp)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                record_id,
                session_id,
                uuid,
                i as i64,
                b.block_type,
                role,
                b.text,
                b.tool_name,
                b.tool_use_id,
                b.tool_input,
                b.is_error,
                b.tool_result,
                timestamp,
            ],
        )?;
        let block_id = tx.last_insert_rowid();

        // Build the searchable body from whatever text this block carries.
        let mut body = String::new();
        if let Some(t) = &b.text {
            body.push_str(t);
        }
        if let Some(n) = &b.tool_name {
            body.push(' ');
            body.push_str(n);
        }
        if let Some(t) = &b.tool_input {
            body.push(' ');
            body.push_str(t);
        }
        if let Some(t) = &b.tool_result {
            body.push(' ');
            body.push_str(t);
        }
        if !body.trim().is_empty() {
            // rowid == block_id lets re-imports delete FTS rows by rowid cheaply.
            tx.execute(
                "INSERT INTO search_index (rowid, body, block_id, session_id, block_type, role)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![block_id, cap_body(&body), block_id, session_id, b.block_type, role],
            )?;
        }
    }

    Ok(count)
}

/// Store a queued message as a `queued` content block plus a search-index row.
/// The operation (enqueue/popAll) is kept in `tool_name` for display; only the
/// message text is indexed for search.
fn insert_queued_block(
    tx: &rusqlite::Transaction,
    record_id: i64,
    session_id: &str,
    message_uuid: Option<&str>,
    timestamp: Option<&str>,
    operation: &str,
    text: &str,
) -> Result<usize> {
    tx.execute(
        "INSERT INTO content_blocks
            (record_id, session_id, message_uuid, seq, block_type, role,
             text, tool_name, tool_use_id, tool_input, is_error, tool_result, timestamp)
         VALUES (?1,?2,?3,0,'queued',NULL,?4,?5,NULL,NULL,NULL,NULL,?6)",
        params![record_id, session_id, message_uuid, text, operation, timestamp],
    )?;
    let block_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO search_index (rowid, body, block_id, session_id, block_type, role)
         VALUES (?1,?2,?3,?4,'queued',NULL)",
        params![block_id, cap_body(text), block_id, session_id],
    )?;
    Ok(1)
}

#[derive(Default)]
struct Block {
    block_type: String,
    text: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    tool_input: Option<String>,
    is_error: Option<bool>,
    tool_result: Option<String>,
}

impl Block {
    fn text(s: String) -> Self {
        Block {
            block_type: "text".to_string(),
            text: Some(s),
            ..Default::default()
        }
    }
}

fn parse_block(item: &Value) -> Block {
    let bt = item
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown")
        .to_string();
    let mut b = Block {
        block_type: bt.clone(),
        ..Default::default()
    };
    match bt.as_str() {
        "text" => b.text = item.get("text").and_then(|x| x.as_str()).map(String::from),
        "thinking" => {
            b.text = item
                .get("thinking")
                .and_then(|x| x.as_str())
                .map(String::from)
        }
        "tool_use" => {
            b.tool_name = item.get("name").and_then(|x| x.as_str()).map(String::from);
            b.tool_use_id = item.get("id").and_then(|x| x.as_str()).map(String::from);
            b.tool_input = item.get("input").map(compact_json);
        }
        "tool_result" => {
            b.tool_use_id = item
                .get("tool_use_id")
                .and_then(|x| x.as_str())
                .map(String::from);
            b.is_error = item.get("is_error").and_then(|x| x.as_bool());
            b.tool_result = item.get("content").map(flatten_content);
        }
        "image" => {
            b.text = Some("[image]".to_string());
        }
        _ => {
            // Preserve anything unexpected as JSON so it is never lost.
            b.text = Some(compact_json(item));
        }
    }
    b
}

/// Flatten a tool_result `content` (string, or array of blocks) into plain text.
fn flatten_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .map(|item| match item.get("type").and_then(|t| t.as_str()) {
                Some("text") => item
                    .get("text")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                Some("image") => "[image]".to_string(),
                _ => compact_json(item),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => compact_json(other),
    }
}

fn summarize_event(event_type: &str, v: &Value) -> Option<String> {
    let s = match event_type {
        "system" => {
            let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("");
            let content = v
                .get("content")
                .map(flatten_content)
                .unwrap_or_default();
            format!("[{}] {}", sub, truncate(&content, 200))
        }
        "custom-title" => v
            .get("customTitle")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        "ai-title" => v
            .get("aiTitle")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        "last-prompt" => v
            .get("lastPrompt")
            .and_then(|x| x.as_str())
            .map(|s| truncate(s, 200))
            .unwrap_or_default(),
        "pr-link" => format!(
            "{} #{} {}",
            v.get("prRepository").and_then(|x| x.as_str()).unwrap_or(""),
            v.get("prNumber")
                .map(|x| x.to_string())
                .unwrap_or_default(),
            v.get("prUrl").and_then(|x| x.as_str()).unwrap_or("")
        ),
        "mode" => v.get("mode").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        "permission-mode" => v
            .get("permissionMode")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        "agent-name" => v
            .get("agentName")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        "queue-operation" => format!(
            "{}: {}",
            v.get("operation").and_then(|x| x.as_str()).unwrap_or(""),
            v.get("content")
                .map(flatten_content)
                .map(|s| truncate(&s, 160))
                .unwrap_or_default()
        ),
        "attachment" => {
            let a = v.get("attachment");
            let t = a
                .and_then(|x| x.get("type"))
                .and_then(|x| x.as_str())
                .unwrap_or("attachment");
            t.to_string()
        }
        "file-history-snapshot" => v
            .get("messageId")
            .and_then(|x| x.as_str())
            .map(|m| format!("snapshot for {}", m))
            .unwrap_or_else(|| "snapshot".to_string()),
        "bridge-session" => v
            .get("bridgeSessionId")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        _ => return None,
    };
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// `-Users-indy-projects-foo` -> `/Users/indy/projects/foo`
fn decode_project_dir(dir: &str) -> String {
    if dir.is_empty() {
        return String::new();
    }
    dir.replace('-', "/")
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(String::from)
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(|x| x.as_bool())
}

fn compact_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Cap the text that goes into the full-text index. The full content is still
/// stored in `content_blocks`; this only bounds index size so a few huge
/// machine-generated payloads (e.g. task-notification dumps) don't bloat it.
fn cap_body(s: &str) -> String {
    const MAX: usize = 16_384;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{}…", t)
    }
}
