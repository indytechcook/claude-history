use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use tiny_http::{Header, Response, Server};

/// Start the web UI server. Blocks until the process is killed.
pub fn serve(db_path: &Path, host: &str, port: u16, open: bool) -> Result<()> {
    let conn = crate::db::open(db_path)?;
    crate::db::init_schema(&conn)?;

    let addr = format!("{}:{}", host, port);
    let server = Server::http(&addr)
        .map_err(|e| anyhow::anyhow!("failed to bind {}: {}", addr, e))?;

    let url = format!("http://{}:{}", host, port);
    println!("claude-history UI serving at {}", url);
    println!("database: {}", db_path.display());
    println!("press Ctrl-C to stop.");

    if open {
        let _ = std::process::Command::new("open").arg(&url).spawn();
    }

    // Single-user local tool: handle requests sequentially on one connection.
    for request in server.incoming_requests() {
        let raw_url = request.url().to_string();
        let (path, query) = split_url(&raw_url);
        let params = parse_query(query);

        let response = match path.as_str() {
            "/" | "/index.html" => html_response(INDEX_HTML),
            "/api/sessions" => json_response(api_sessions(&conn, &params)),
            "/api/subagents" => json_response(match get_str(&params, "parent") {
                Some(parent) => api_subagents(&conn, parent),
                None => Ok(json!({ "subagents": [] })),
            }),
            "/api/search" => json_response(api_search(&conn, &params)),
            "/api/stats" => json_response(api_stats(&conn)),
            p if p.starts_with("/api/session/") => {
                let id = p.trim_start_matches("/api/session/");
                json_response(api_session(&conn, id))
            }
            _ => Response::from_string("not found")
                .with_status_code(404)
                .boxed(),
        };

        let _ = request.respond(response);
    }
    Ok(())
}

// ---------- response helpers ----------

fn html_response(body: &str) -> tiny_http::ResponseBox {
    Response::from_string(body)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        .boxed()
}

fn json_response(value: Result<Value>) -> tiny_http::ResponseBox {
    match value {
        Ok(v) => {
            let body = serde_json::to_string(&v).unwrap_or_else(|_| "null".to_string());
            response_with_json(body, 200)
        }
        Err(e) => {
            let body = json!({ "error": e.to_string() }).to_string();
            response_with_json(body, 500)
        }
    }
}

fn response_with_json(body: String, status: u16) -> tiny_http::ResponseBox {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json; charset=utf-8"))
        .boxed()
}

fn header(k: &str, v: &str) -> Header {
    Header::from_bytes(k.as_bytes(), v.as_bytes()).unwrap()
}

// ---------- url / query parsing ----------

fn split_url(url: &str) -> (String, &str) {
    match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q),
        None => (url.to_string(), ""),
    }
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(urldecode(k), urldecode(v));
    }
    map
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = hex(bytes[i + 1]);
                let lo = hex(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                } else {
                    out.push(bytes[i]);
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn get_i64(p: &HashMap<String, String>, key: &str, default: i64) -> i64 {
    p.get(key).and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn get_str<'a>(p: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    p.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty())
}

// ---------- API handlers ----------

fn api_sessions(conn: &Connection, p: &HashMap<String, String>) -> Result<Value> {
    let project = get_str(p, "project");
    let limit = get_i64(p, "limit", 100).clamp(1, 1000);

    // Only top-level (non-subagent) sessions appear in the main list; each
    // carries a count of the subagent transcripts it spawned, which the UI
    // lazy-loads and nests underneath via `/api/subagents`.
    let mut sql = String::from(
        "SELECT session_id, COALESCE(cwd, project_path), custom_title, ai_title,
                git_branch, last_timestamp, message_count,
                (SELECT COUNT(*) FROM sessions c WHERE c.parent_session_id = sessions.session_id)
         FROM sessions
         WHERE parent_session_id IS NULL",
    );
    if project.is_some() {
        sql.push_str(" AND (cwd LIKE ?1 OR project_path LIKE ?1 OR project_dir LIKE ?1)");
    }
    sql.push_str(" ORDER BY last_timestamp DESC LIMIT ?2");

    let like = project.map(|p| format!("%{}%", p)).unwrap_or_default();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![like, limit], |r| {
        let custom: Option<String> = r.get(2)?;
        let ai: Option<String> = r.get(3)?;
        let title = custom
            .filter(|s| !s.is_empty())
            .or(ai)
            .unwrap_or_else(|| "(untitled)".to_string());
        Ok(json!({
            "session_id": r.get::<_, String>(0)?,
            "project": r.get::<_, Option<String>>(1)?,
            "title": title,
            "branch": r.get::<_, Option<String>>(4)?,
            "last": r.get::<_, Option<String>>(5)?,
            "messages": r.get::<_, i64>(6)?,
            "subagent_count": r.get::<_, i64>(7)?,
        }))
    })?;
    let items: Vec<Value> = rows.filter_map(|x| x.ok()).collect();
    Ok(json!({ "sessions": items }))
}

/// Subagent transcripts spawned by a given parent session. Titled by their
/// first user message — the Task prompt the parent handed the agent — since
/// subagent files never emit a custom/ai title of their own.
fn api_subagents(conn: &Connection, parent: &str) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT s.session_id, s.last_timestamp, s.message_count,
                (SELECT m.text_content FROM messages m
                  WHERE m.session_id = s.session_id AND m.role = 'user'
                  ORDER BY m.timestamp LIMIT 1)
         FROM sessions s
         WHERE s.parent_session_id = ?1
         ORDER BY s.first_timestamp ASC",
    )?;
    let rows = stmt.query_map(params![parent], |r| {
        let prompt: Option<String> = r.get(3)?;
        let title = prompt
            .map(|p| p.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(subagent)".to_string());
        Ok(json!({
            "session_id": r.get::<_, String>(0)?,
            "title": title,
            "last": r.get::<_, Option<String>>(1)?,
            "messages": r.get::<_, i64>(2)?,
        }))
    })?;
    let items: Vec<Value> = rows.filter_map(|x| x.ok()).collect();
    Ok(json!({ "subagents": items }))
}

fn api_search(conn: &Connection, p: &HashMap<String, String>) -> Result<Value> {
    let query = match get_str(p, "q") {
        Some(q) => q,
        None => return Ok(json!({ "results": [] })),
    };
    let block_type = get_str(p, "type");
    let role = get_str(p, "role");
    let session = get_str(p, "session");
    let limit = get_i64(p, "limit", 50).clamp(1, 500);

    let mut sql = String::from(
        "SELECT s.block_id, s.session_id, s.block_type, s.role,
                snippet(search_index, 0, '\u{2039}', '\u{203a}', '\u{2026}', 48) AS snip,
                cb.timestamp
         FROM search_index s
         JOIN content_blocks cb ON cb.id = s.block_id
         WHERE search_index MATCH ?1",
    );
    if block_type.is_some() {
        sql.push_str(" AND s.block_type = ?2");
    }
    if role.is_some() {
        sql.push_str(" AND s.role = ?3");
    }
    if session.is_some() {
        sql.push_str(" AND s.session_id LIKE ?4");
    }
    sql.push_str(" ORDER BY rank LIMIT ?5");

    let bt = block_type.unwrap_or("");
    let rl = role.unwrap_or("");
    let se = session.map(|s| format!("{}%", s)).unwrap_or_default();
    let match_expr = crate::fts::build_match(query);
    if match_expr.is_empty() {
        return Ok(json!({ "results": [] }));
    }

    // FTS5 MATCH throws on syntax errors; surface a clean message instead.
    let mut stmt = conn.prepare(&sql)?;
    let mapped = stmt.query_map(params![match_expr, bt, rl, se, limit], |r| {
        Ok(json!({
            "block_id": r.get::<_, i64>(0)?,
            "session_id": r.get::<_, String>(1)?,
            "block_type": r.get::<_, String>(2)?,
            "role": r.get::<_, Option<String>>(3)?,
            "snippet": r.get::<_, String>(4)?,
            "timestamp": r.get::<_, Option<String>>(5)?,
        }))
    });
    let items: Vec<Value> = match mapped {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(e) => return Ok(json!({ "results": [], "error": e.to_string() })),
    };
    Ok(json!({ "results": items }))
}

fn api_session(conn: &Connection, id: &str) -> Result<Value> {
    let sid: String = match conn
        .query_row(
            "SELECT session_id FROM sessions WHERE session_id = ?1 OR session_id LIKE ?2
             ORDER BY session_id LIMIT 1",
            params![id, format!("{}%", id)],
            |r| r.get(0),
        )
        .ok()
    {
        Some(s) => s,
        None => return Ok(json!({ "error": "session not found" })),
    };

    let meta = conn
        .query_row(
            "SELECT COALESCE(custom_title, ai_title, '(untitled)'),
                    COALESCE(cwd, project_path), git_branch, version,
                    first_timestamp, last_timestamp, message_count
             FROM sessions WHERE session_id = ?1",
            params![sid],
            |r| {
                Ok(json!({
                    "session_id": sid,
                    "title": r.get::<_, String>(0)?,
                    "project": r.get::<_, Option<String>>(1)?,
                    "branch": r.get::<_, Option<String>>(2)?,
                    "version": r.get::<_, Option<String>>(3)?,
                    "first": r.get::<_, Option<String>>(4)?,
                    "last": r.get::<_, Option<String>>(5)?,
                    "messages": r.get::<_, i64>(6)?,
                }))
            },
        )
        .unwrap_or(json!({ "session_id": sid }));

    // Walk every record in order so all types are represented.
    let mut stmt = conn.prepare(
        "SELECT id, seq, record_type, timestamp FROM records
         WHERE session_id = ?1 ORDER BY seq",
    )?;
    let recs: Vec<(i64, i64, String, Option<String>)> = stmt
        .query_map(params![sid], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .filter_map(|x| x.ok())
        .collect();

    let mut items: Vec<Value> = Vec::with_capacity(recs.len());
    for (record_id, seq, rtype, ts) in recs {
        match rtype.as_str() {
            "user" | "assistant" => {
                items.push(message_json(conn, record_id, &rtype, ts.as_deref())?);
            }
            "queue-operation" => {
                // Render the queued message text if present, else a plain event.
                match queued_json(conn, record_id, ts.as_deref())? {
                    Some(q) => items.push(q),
                    None => {
                        let summary: Option<String> = conn
                            .query_row(
                                "SELECT summary FROM events WHERE session_id = ?1 AND seq = ?2",
                                params![sid, seq],
                                |r| r.get(0),
                            )
                            .ok()
                            .flatten();
                        items.push(json!({
                            "kind": "event",
                            "event_type": "queue-operation",
                            "timestamp": ts,
                            "summary": summary,
                        }));
                    }
                }
            }
            other => {
                let summary: Option<String> = conn
                    .query_row(
                        "SELECT summary FROM events WHERE session_id = ?1 AND seq = ?2",
                        params![sid, seq],
                        |r| r.get(0),
                    )
                    .ok()
                    .flatten();
                items.push(json!({
                    "kind": "event",
                    "event_type": other,
                    "timestamp": ts,
                    "summary": summary,
                }));
            }
        }
    }

    Ok(json!({ "session": meta, "items": items }))
}

/// Build a `queued` item from a queue-operation record, or None if it carries
/// no queued text (dequeue/remove bookkeeping).
fn queued_json(conn: &Connection, record_id: i64, ts: Option<&str>) -> Result<Option<Value>> {
    let mut stmt = conn.prepare(
        "SELECT tool_name, text FROM content_blocks
         WHERE record_id = ?1 AND block_type = 'queued' ORDER BY seq",
    )?;
    let rows: Vec<(Option<String>, Option<String>)> = stmt
        .query_map(params![record_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|x| x.ok())
        .collect();
    let Some((op, text)) = rows.into_iter().next() else {
        return Ok(None);
    };
    Ok(Some(json!({
        "kind": "queued",
        "operation": op,
        "timestamp": ts,
        "text": text,
    })))
}

fn message_json(
    conn: &Connection,
    record_id: i64,
    rtype: &str,
    ts: Option<&str>,
) -> Result<Value> {
    let model: Option<String> = conn
        .query_row(
            "SELECT model FROM messages WHERE record_id = ?1",
            params![record_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    let mut stmt = conn.prepare(
        "SELECT block_type, text, tool_name, tool_input, is_error, tool_result
         FROM content_blocks WHERE record_id = ?1 ORDER BY seq",
    )?;
    let blocks: Vec<Value> = stmt
        .query_map(params![record_id], |r| {
            Ok(json!({
                "block_type": r.get::<_, String>(0)?,
                "text": r.get::<_, Option<String>>(1)?,
                "tool_name": r.get::<_, Option<String>>(2)?,
                "tool_input": r.get::<_, Option<String>>(3)?,
                "is_error": r.get::<_, Option<bool>>(4)?,
                "tool_result": r.get::<_, Option<String>>(5)?,
            }))
        })?
        .filter_map(|x| x.ok())
        .collect();

    Ok(json!({
        "kind": "message",
        "role": rtype,
        "model": model,
        "timestamp": ts,
        "blocks": blocks,
    }))
}

fn api_stats(conn: &Connection) -> Result<Value> {
    let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let records: i64 = conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    let blocks: i64 = conn.query_row("SELECT COUNT(*) FROM content_blocks", [], |r| r.get(0))?;

    let group = |sql: &str| -> Result<Vec<Value>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(json!({ "name": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            })?
            .filter_map(|x| x.ok())
            .collect();
        Ok(rows)
    };

    Ok(json!({
        "sessions": sessions,
        "records": records,
        "blocks": blocks,
        "record_types": group(
            "SELECT record_type, COUNT(*) FROM records GROUP BY record_type ORDER BY 2 DESC"
        )?,
        "block_types": group(
            "SELECT block_type, COUNT(*) FROM content_blocks GROUP BY block_type ORDER BY 2 DESC"
        )?,
        "tools": group(
            "SELECT tool_name, COUNT(*) FROM content_blocks
             WHERE block_type='tool_use' AND tool_name IS NOT NULL
             GROUP BY tool_name ORDER BY 2 DESC LIMIT 20"
        )?,
    }))
}

const INDEX_HTML: &str = include_str!("ui.html");
