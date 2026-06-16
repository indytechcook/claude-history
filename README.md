# claude-history (`cch`)

A small Rust CLI to **import, browse, and search your Claude Code session history**.
It reads the raw JSONL transcripts that Claude Code writes under
`~/.claude/projects/**/*.jsonl`, loads them into a **SQLite** database, and lets
you list sessions, search every message, and read full transcripts from the
terminal.

Every record type is captured and **clearly marked** — user/assistant turns,
tool calls and results, thinking blocks, attachments, system notices, titles,
PR links, mode changes, snapshots, and more. The full original JSON for every
line is preserved in the `records.raw_json` column, so nothing is ever lost.

## Build

```sh
cargo build --release
# binary at ./target/release/cch
```

## Quick start

```sh
# Import everything from ~/.claude/projects into ~/.claude/claude-history.db
cch import

# What's in there?
cch stats

# Recent sessions (filter by project path/name)
cch sessions --project sidekiq

# Full-text search across every message
cch search "connection pool timeout"

# Read a whole session transcript (a unique id prefix is enough)
cch show 127956ee --events

# ...or browse and search it all in your browser
cch serve
```

## Commands

| Command | Description |
| --- | --- |
| `cch import` | Scan JSONL files and load them into SQLite. Unchanged files are skipped. |
| `cch sessions` | List sessions, newest first. |
| `cch search <query>` | Full-text search (SQLite FTS5) across all content. |
| `cch show <session>` | Print a full transcript with every type clearly marked. |
| `cch stats` | Counts by record type, content-block type, tools, and tokens. |
| `cch serve` | Launch the web UI to browse and search history in a browser. |

Global flag: `--db <path>` overrides the database location
(default `~/.claude/claude-history.db`).

### `import`
- `--source <dir>` — where the project JSONL files live (default `~/.claude/projects`).
- `--reset` — drop and recreate all tables first.
- `--force` — re-import every file even if its size/mtime are unchanged.

Re-running `import` is incremental: only files that changed since the last run
are re-read, so it's cheap to run repeatedly.

### `search`
- `--type <t>` — restrict to a block type: `text`, `thinking`, `tool_use`, `tool_result`, `image`, `queued`.
- `--role <r>` — restrict to `user` or `assistant`.
- `--session <id>` — restrict to one session (prefix ok).
- `--limit <n>` — max results (default 30).

Plain queries are matched literally, so punctuation just works —
`cch search "vector.yaml"` finds `vector.yaml` even though `.` is FTS5 syntax.
Each space-separated term must appear (implicit AND), and a trailing `*` is a
prefix match (`cch search "vector.y*"`).

For advanced [FTS5 queries](https://www.sqlite.org/fts5.html), use the uppercase
boolean operators, an explicit phrase, or grouping and the input is passed
through untouched: `cch search "sidekiq AND retry"`, `cch search '"exact phrase"'`,
`cch search "metric OR gauge"`.

> Note: Claude Code persists `thinking` blocks with only an encrypted
> signature and an empty body, so their reasoning text is not searchable —
> the blocks themselves are still recorded and counted.

### `show`
- `--events` — also print non-message events (titles, modes, system notices, snapshots, …).
- `--raw` — dump the original JSONL lines instead of the formatted view.

Transcript markers:

| Marker | Meaning |
| --- | --- |
| `▶ USER` | a user turn |
| `◆ ASSISTANT` | an assistant turn |
| `⚙ tool_use` | a tool call (name + input) |
| `⮐ tool_result` | a tool result (`(error)` flagged) |
| `· thinking ·` | a thinking block |
| `⏳ QUEUED` | a message you queued while Claude was working |
| `· <type>` | a non-message event |

Queued messages (anything you typed into the queue while Claude was busy, plus
background task notifications) are captured as searchable `queued` blocks and
shown inline in transcripts — they are not hidden behind `--events`.

### `serve`
Launches a local web UI (a tiny embedded HTTP server, no JS framework):

```sh
cch serve                 # http://127.0.0.1:7777, opens your browser
cch serve --port 9000     # pick a port
cch serve --no-open       # don't auto-open the browser
cch serve --host 0.0.0.0  # bind all interfaces (exposes it on your network)
```

The page has a search box (with block-type / role filters), a session list, and
a transcript pane that renders every type clearly marked — user/assistant turns,
tool calls and results (collapsible, errors flagged), thinking blocks, and an
optional "show events" toggle for titles/modes/system notices. Search snippets
highlight the matched terms; clicking a result opens its session.

It reads the same `~/.claude/claude-history.db`, so run `cch import` first (and
re-run it anytime to refresh).

## Database schema

The `cch` database is plain SQLite — query it directly with `sqlite3` if you like.

- **`sessions`** — one row per session file (titles, project path, git branch, timestamps, counts).
- **`records`** — every JSONL line, with `record_type`, ids, timestamps, and the full `raw_json`.
- **`messages`** — decoded user/assistant turns (role, model, tokens, concatenated text).
- **`content_blocks`** — one row per content block, with `block_type` marking each kind
  (`text`, `thinking`, `tool_use`, `tool_result`, `image`, …) plus tool name/input/result.
- **`events`** — non-message records (titles, modes, PR links, system notices, …) with a readable summary.
- **`search_index`** — an FTS5 full-text index over all content, used by `cch search`.

Example direct query:

```sh
sqlite3 ~/.claude/claude-history.db \
  "SELECT tool_name, COUNT(*) FROM content_blocks
   WHERE block_type='tool_use' GROUP BY tool_name ORDER BY 2 DESC LIMIT 10;"
```
