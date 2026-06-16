use anyhow::Result;
use rusqlite::{params, Connection};

fn truncate(s: &str, max: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let t: String = one_line.chars().take(max).collect();
        format!("{}…", t)
    }
}

fn short_time(ts: &str) -> String {
    // ISO 8601 -> "YYYY-MM-DD HH:MM"
    ts.replace('T', " ").chars().take(16).collect()
}

/// List sessions, most-recent first.
pub fn sessions(
    conn: &Connection,
    project: Option<&str>,
    limit: i64,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT session_id, COALESCE(cwd, project_path), custom_title, ai_title, git_branch,
                last_timestamp, message_count
         FROM sessions",
    );
    let mut wheres = Vec::new();
    if project.is_some() {
        wheres.push("(cwd LIKE ?1 OR project_path LIKE ?1 OR project_dir LIKE ?1)");
    }
    if !wheres.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
    }
    sql.push_str(" ORDER BY last_timestamp DESC LIMIT ?2");

    let like = project.map(|p| format!("%{}%", p)).unwrap_or_default();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![like, limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;

    let mut n = 0;
    for row in rows {
        let (sid, proj, custom, ai, branch, ts, msgs) = row?;
        let title = custom
            .filter(|s| !s.is_empty())
            .or(ai)
            .unwrap_or_else(|| "(untitled)".to_string());
        let when = ts.map(|t| short_time(&t)).unwrap_or_default();
        println!(
            "{}  {:>4} msgs  {}",
            when,
            msgs,
            truncate(&title, 70)
        );
        println!(
            "    {}  [{}]{}",
            &sid[..sid.len().min(8)],
            proj.unwrap_or_default(),
            branch
                .filter(|b| !b.is_empty())
                .map(|b| format!("  @{}", b))
                .unwrap_or_default()
        );
        n += 1;
    }
    if n == 0 {
        println!("No sessions found. Run `cch import` first.");
    } else {
        println!("\n{} session(s).", n);
    }
    Ok(())
}

/// Full-text search across all content blocks.
pub fn search(
    conn: &Connection,
    query: &str,
    block_type: Option<&str>,
    role: Option<&str>,
    session: Option<&str>,
    limit: i64,
) -> Result<()> {
    let mut sql = String::from(
        "SELECT s.block_id, s.session_id, s.block_type, s.role,
                snippet(search_index, 0, '«', '»', '…', 48) AS snip,
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
        println!("Empty search.");
        return Ok(());
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![match_expr, bt, rl, se, limit],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        },
    )?;

    let mut n = 0;
    for row in rows {
        let (_bid, sid, btype, role, snip, ts) = row?;
        let when = ts.map(|t| short_time(&t)).unwrap_or_default();
        println!(
            "{}  {:<11} {:<6} {}",
            when,
            format!("[{}]", btype),
            role.unwrap_or_default(),
            &sid[..sid.len().min(8)]
        );
        println!("    {}", truncate(&snip, 500));
        n += 1;
    }
    if n == 0 {
        println!("No matches for: {}", query);
    } else {
        println!("\n{} match(es).", n);
    }
    Ok(())
}

/// Print a full session transcript with every record clearly marked.
pub fn show(
    conn: &Connection,
    session: &str,
    include_events: bool,
    raw: bool,
) -> Result<()> {
    // Resolve a possibly-partial session id.
    let sid: String = match conn
        .query_row(
            "SELECT session_id FROM sessions WHERE session_id = ?1 OR session_id LIKE ?2
             ORDER BY session_id LIMIT 1",
            params![session, format!("{}%", session)],
            |r| r.get(0),
        )
        .ok()
    {
        Some(s) => s,
        None => {
            println!("No session matching '{}'.", session);
            return Ok(());
        }
    };

    if !raw {
    if let Some((title, proj, branch)) = conn
        .query_row(
            "SELECT COALESCE(custom_title, ai_title, '(untitled)'), COALESCE(cwd, project_path), git_branch
             FROM sessions WHERE session_id = ?1",
            params![sid],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()
    {
        println!("═══ {} ═══", title);
        println!("session {}  [{}]{}", sid, proj.unwrap_or_default(),
            branch.filter(|b| !b.is_empty()).map(|b| format!("  @{}", b)).unwrap_or_default());
        println!();
    }
    }

    let mut stmt = conn.prepare(
        "SELECT seq, record_type, timestamp, raw_json FROM records
         WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![sid], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;

    for row in rows {
        let (seq, rtype, ts, raw_json) = row?;
        let when = ts.as_deref().map(short_time).unwrap_or_default();

        if raw {
            println!("{}", raw_json);
            continue;
        }

        match rtype.as_str() {
            "user" | "assistant" => {
                print_message(conn, &sid, seq, &rtype, &when)?;
            }
            "queue-operation" => {
                // Queued messages are real content: always show them. The
                // bookkeeping ops (dequeue/remove) have no block and fall back
                // to an event line.
                if !print_queued(conn, &sid, seq, &when)? && include_events {
                    print_event(conn, &sid, seq, "queue-operation", &when)?;
                }
            }
            other => {
                if include_events {
                    print_event(conn, &sid, seq, other, &when)?;
                }
            }
        }
    }
    Ok(())
}

fn print_message(
    conn: &Connection,
    sid: &str,
    seq: i64,
    rtype: &str,
    when: &str,
) -> Result<()> {
    let marker = match rtype {
        "user" => "▶ USER",
        "assistant" => "◆ ASSISTANT",
        _ => rtype,
    };

    // The record id for this seq.
    let record_id: i64 = conn.query_row(
        "SELECT id FROM records WHERE session_id = ?1 AND seq = ?2",
        params![sid, seq],
        |r| r.get(0),
    )?;

    let model: Option<String> = conn
        .query_row(
            "SELECT model FROM messages WHERE record_id = ?1",
            params![record_id],
            |r| r.get(0),
        )
        .ok()
        .flatten();

    println!(
        "{}  {}{}",
        marker,
        when,
        model.map(|m| format!("  ({})", m)).unwrap_or_default()
    );

    let mut stmt = conn.prepare(
        "SELECT block_type, text, tool_name, tool_input, is_error, tool_result
         FROM content_blocks WHERE record_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map(params![record_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<bool>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;

    for row in rows {
        let (btype, text, tool_name, tool_input, is_error, tool_result) = row?;
        match btype.as_str() {
            "text" => {
                if let Some(t) = text {
                    println!("{}", indent(&t));
                }
            }
            "thinking" => {
                println!("  · thinking ·");
                if let Some(t) = text {
                    println!("{}", indent(&truncate(&t, 4000)));
                }
            }
            "tool_use" => {
                println!(
                    "  ⚙ tool_use: {} {}",
                    tool_name.unwrap_or_default(),
                    truncate(&tool_input.unwrap_or_default(), 300)
                );
            }
            "tool_result" => {
                let err = if is_error == Some(true) { " (error)" } else { "" };
                println!("  ⮐ tool_result{}:", err);
                if let Some(t) = tool_result {
                    println!("{}", indent(&truncate(&t, 2000)));
                }
            }
            other => {
                println!("  [{}] {}", other, truncate(&text.unwrap_or_default(), 300));
            }
        }
    }
    println!();
    Ok(())
}

/// Render a queued message if this record has one. Returns true if it did.
fn print_queued(conn: &Connection, sid: &str, seq: i64, when: &str) -> Result<bool> {
    let record_id: i64 = conn.query_row(
        "SELECT id FROM records WHERE session_id = ?1 AND seq = ?2",
        params![sid, seq],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT tool_name, text FROM content_blocks
         WHERE record_id = ?1 AND block_type = 'queued' ORDER BY seq",
    )?;
    let rows: Vec<(Option<String>, Option<String>)> = stmt
        .query_map(params![record_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .filter_map(|x| x.ok())
        .collect();
    if rows.is_empty() {
        return Ok(false);
    }
    for (op, text) in rows {
        println!("⏳ QUEUED ({})  {}", op.unwrap_or_default(), when);
        if let Some(t) = text {
            println!("{}", indent(&truncate(&t, 4000)));
        }
        println!();
    }
    Ok(true)
}

fn print_event(
    conn: &Connection,
    sid: &str,
    seq: i64,
    etype: &str,
    when: &str,
) -> Result<()> {
    let summary: Option<String> = conn
        .query_row(
            "SELECT summary FROM events WHERE session_id = ?1 AND seq = ?2",
            params![sid, seq],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    println!(
        "· {} {}  {}",
        etype,
        when,
        truncate(&summary.unwrap_or_default(), 160)
    );
    Ok(())
}

fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("    {}", l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Counts of record types, block types, and overall totals.
pub fn stats(conn: &Connection) -> Result<()> {
    let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let records: i64 = conn.query_row("SELECT COUNT(*) FROM records", [], |r| r.get(0))?;
    let blocks: i64 = conn.query_row("SELECT COUNT(*) FROM content_blocks", [], |r| r.get(0))?;
    println!("Sessions: {}", sessions);
    println!("Records:  {}", records);
    println!("Blocks:   {}", blocks);

    println!("\nRecord types:");
    let mut stmt = conn.prepare(
        "SELECT record_type, COUNT(*) FROM records GROUP BY record_type ORDER BY COUNT(*) DESC",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (t, c) = row?;
        println!("  {:<24} {:>8}", t, c);
    }

    println!("\nContent block types (clearly marked):");
    let mut stmt = conn.prepare(
        "SELECT block_type, COUNT(*) FROM content_blocks GROUP BY block_type ORDER BY COUNT(*) DESC",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (t, c) = row?;
        println!("  {:<24} {:>8}", t, c);
    }

    println!("\nTop tools used:");
    let mut stmt = conn.prepare(
        "SELECT tool_name, COUNT(*) FROM content_blocks
         WHERE block_type='tool_use' AND tool_name IS NOT NULL
         GROUP BY tool_name ORDER BY COUNT(*) DESC LIMIT 15",
    )?;
    for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
        let (t, c) = row?;
        println!("  {:<24} {:>8}", t, c);
    }

    let tokens: Option<i64> = conn
        .query_row(
            "SELECT SUM(input_tokens) + SUM(output_tokens) FROM messages",
            [],
            |r| r.get(0),
        )
        .ok()
        .flatten();
    if let Some(tok) = tokens {
        println!("\nTotal tokens (in+out): {}", tok);
    }
    Ok(())
}
