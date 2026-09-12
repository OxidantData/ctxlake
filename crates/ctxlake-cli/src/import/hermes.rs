//! Hermes import — `~/.hermes/state.db`.
//!
//! Hermes keeps a full SQLite state database: every session, every message (user,
//! assistant and tool), the structured `tool_calls` an assistant asked for, per-model
//! token and cost accounting, and the session titles Hermes generates for its own UI.
//! That makes it a **high**-fidelity import source, comparable to Claude Code's JSONL
//! transcript — not the "live capture only" runtime `docs/adding-it.md` used to claim it
//! was. See that page for the corrected fidelity table.
//!
//! ## Opening someone else's live database
//!
//! This is the load-bearing part of this module. `state.db` belongs to a *running*
//! agent. Opening it read-write, or in any mode that takes a lock, means ctxlake and
//! Hermes contending for the same file — and the process that loses is the user's
//! agent, mid-turn. So the connection is opened through the URI form
//! `file:<path>?mode=ro&immutable=1` with [`OpenFlags::SQLITE_OPEN_READ_ONLY`]:
//!
//! - `mode=ro` and `SQLITE_OPEN_READ_ONLY` make a write impossible rather than merely
//!   unintended. The flags are asserted by a test, not just the behaviour, because
//!   "it happened not to write anything this run" is not the property that matters.
//! - `immutable=1` tells SQLite the file cannot change underneath it, which is what
//!   actually removes the locking: no shared lock, no `-shm` file, nothing for Hermes
//!   to contend on.
//!
//! `immutable=1` has one honest cost, stated here rather than discovered later: if
//! Hermes is in WAL mode with frames not yet checkpointed into the main database
//! file, an immutable read does not see them. Import therefore reads a slightly older
//! view of a *live* session than `sqlite3` would. That is the right trade — the
//! alternative is taking a lock on a running agent's database — and it is harmless
//! here because import is idempotent (see [`super::dedup_key`]): the next run picks
//! up whatever the previous one could not see.
//!
//! ## The schema can move, and it must fail loudly when it does
//!
//! Everything below was verified against **one** live install's schema. Hermes ships
//! a `schema_version` table precisely because that shape is not frozen. A future
//! version that renames `tool_calls`, or moves message text out of `content`, would
//! make a lenient importer succeed, report zero events, and look exactly like "you
//! have no history" — the silent-failure shape AGENTS.md's "how these were found"
//! section exists to rule out. So [`verify_schema`] requires every column this module
//! maps, by name, and fails with all the missing ones listed and the observed
//! `schema_version` quoted back. The version number itself is *reported*, never
//! compared against a constant: we have seen one install, and a version we have never
//! seen is not by itself evidence of breakage.
//!
//! ## What is deliberately not mapped
//!
//! - **`reasoning_tokens`.** [`Usage`] has no field for it, and folding it into
//!   `output_tokens` would silently change what that field means for one runtime.
//!   `envelope.rs`'s versioning rule is explicit: add a field, never reinterpret one.
//! - **Per-model attribution of individual messages.** `session_model_usage` is
//!   per-(session, model); `messages` carries no model column. Attributing a message
//!   to a model from the usage rows' `first_seen`/`last_seen` windows would be a
//!   guess, and bronze is immutable. Usage is therefore summed onto the session's one
//!   `SessionEnd` envelope, which is also the only placement that does not double
//!   count in `ctxlake_maint::digest::compute` (it sums `usage` across every envelope
//!   in the session).
//! - **`effect_disposition`, `finish_reason`, `active`, `observed`.** No verified
//!   meaning, so no mapping. Rows are imported regardless of `active`/`observed`:
//!   bronze records what happened, and filtering history on a flag we have not
//!   verified is how an import quietly loses half a transcript.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use ctxlake_core::envelope::{Envelope, EventType, ToolCall, Usage};
use ctxlake_core::redact::Redactor;
use ctxlake_core::{hash, Runtime};
use ctxlake_hook::adapters::common::{
    scrub_field, truncate, withhold_if_denied_path, RedactionAcc,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use time::OffsetDateTime;

use super::EnvelopeSink;

/// Columns of `messages` this importer maps. Every one is required: each carries a
/// piece of the fidelity `docs/adding-it.md` now promises for Hermes, and losing any of
/// them silently is worse than refusing to run.
const REQUIRED_MESSAGE_COLUMNS: &[&str] = &[
    "id",
    "session_id",
    "role",
    "content",
    "tool_call_id",
    "tool_calls",
    "tool_name",
    "timestamp",
    "compacted",
];

/// The `sessions` primary key, whichever of these it is called. Accepting both
/// spellings costs one lookup and avoids a hard failure over a cosmetic rename.
const SESSION_KEY_CANDIDATES: &[&str] = &["id", "session_id"];

/// Columns read from `session_model_usage` when the table exists. Individually
/// optional — a missing one reads as zero — because token accounting is a bonus on
/// top of the transcript, not the thing import exists for.
const USAGE_COLUMNS: &[&str] = &[
    "model",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "estimated_cost_usd",
    "actual_cost_usd",
];

/// `?` and `#` terminate a SQLite URI's path; `%` starts an escape. Everything else
/// (including `/` and spaces) is legal in the path component and left alone so the
/// URI still reads as the path it is.
const URI_PATH_ESCAPES: &AsciiSet = &CONTROLS.add(b'?').add(b'#').add(b'%');

/// Fleet/agent/host identity stamped onto every imported envelope. `host_id` is
/// already hashed by the caller — `envelope.rs` is explicit that bronze never carries
/// a raw hostname.
#[derive(Debug, Clone)]
pub struct ImportIdentity {
    pub fleet_id: String,
    pub agent_id: String,
    pub host_id: String,
}

/// Counts the *source* owns, as opposed to the ones the spooler owns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceStats {
    pub source: String,
    pub schema_version: Option<String>,
    pub sessions_seen: usize,
    pub sessions_skipped_by_since: usize,
    pub sessions_empty: usize,
    pub rows_skipped_unknown_role: usize,
}

/// The flags every connection to a Hermes database is opened with.
///
/// Exposed (and asserted in tests) rather than inlined at the call site: "we open it
/// read-only" is a safety claim about someone else's live data, and a claim like that
/// should be checkable without inferring it from the absence of a write.
pub fn open_flags() -> OpenFlags {
    // READ_ONLY: a write is impossible, not merely unintended.
    // URI: required for the `?mode=ro&immutable=1` query string to be parsed at all
    //      — without it SQLite treats the whole URI as a literal filename.
    // NO_MUTEX: this connection is used from one thread; matches
    //      `ctxlake_mcp::snapshot::open`'s identical choice for the same reason.
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// The URI form used to open `path`. See the module doc for why `immutable=1`.
pub fn read_only_uri(path: &Path) -> String {
    format!(
        "file:{}?mode=ro&immutable=1",
        utf8_percent_encode(&path.to_string_lossy(), URI_PATH_ESCAPES)
    )
}

/// Open a Hermes state database read-only and lock-free.
pub fn open(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!(
            "no Hermes state database at {} — Hermes stores its history there; pass \
             --source if yours lives elsewhere",
            path.display()
        );
    }
    Connection::open_with_flags(read_only_uri(path), open_flags())
        .with_context(|| format!("opening {} read-only", path.display()))
}

/// What [`verify_schema`] found.
#[derive(Debug, Clone)]
pub struct Schema {
    pub schema_version: Option<String>,
    pub session_key_column: String,
    pub session_columns: BTreeSet<String>,
    pub usage_columns: BTreeSet<String>,
}

/// Confirm the database has the shape this importer maps, or fail saying exactly
/// what is missing. See the module doc on why this is a hard gate.
pub fn verify_schema(conn: &Connection) -> Result<Schema> {
    let version = schema_version(conn);
    let version_note = match &version {
        Some(v) => format!(" (schema_version {v})"),
        None => " (no schema_version table)".to_string(),
    };

    let message_columns = table_columns(conn, "messages")?;
    if message_columns.is_empty() {
        bail!(
            "this database has no `messages` table{version_note} — it is not a Hermes \
             state database, or Hermes's schema has moved"
        );
    }
    let missing: Vec<&str> = REQUIRED_MESSAGE_COLUMNS
        .iter()
        .copied()
        .filter(|c| !message_columns.contains(*c))
        .collect();
    if !missing.is_empty() {
        bail!(
            "Hermes's `messages` table is missing {}{version_note}; this importer was \
             verified against a schema that has them, and importing without them would \
             silently drop history rather than fail. See docs/adding-it.md.",
            missing.join(", ")
        );
    }

    let session_columns = table_columns(conn, "sessions")?;
    if session_columns.is_empty() {
        bail!("this database has no `sessions` table{version_note}");
    }
    let session_key_column = SESSION_KEY_CANDIDATES
        .iter()
        .find(|c| session_columns.contains(**c))
        .map(|c| (*c).to_string())
        .ok_or_else(|| {
            anyhow!(
                "Hermes's `sessions` table has no `id` or `session_id` column{version_note} \
                 — nothing to join `messages.session_id` against"
            )
        })?;

    // Usage is a bonus, so its absence is not fatal — but a `session_model_usage`
    // table with no `session_id` cannot be joined, and reading it anyway would
    // attribute one session's cost to all of them.
    let mut usage_columns = table_columns(conn, "session_model_usage")?;
    if !usage_columns.contains("session_id") {
        usage_columns.clear();
    }

    Ok(Schema {
        schema_version: version,
        session_key_column,
        session_columns,
        usage_columns,
    })
}

/// Column names of `table`, or an empty set if it does not exist.
///
/// `PRAGMA table_info` cannot take a bound parameter in a normal statement, so this
/// goes through rusqlite's `pragma` helper, which binds the table name properly
/// instead of interpolating it.
fn table_columns(conn: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut cols = BTreeSet::new();
    conn.pragma(None, "table_info", table, |row| {
        let name: String = row.get("name")?;
        cols.insert(name);
        Ok(())
    })
    .with_context(|| format!("reading the schema of `{table}`"))?;
    Ok(cols)
}

/// Whatever the `schema_version` table says, as a string, best-effort.
///
/// Reported, never enforced: we have seen exactly one install's value, so a number
/// this code does not recognize is not evidence of anything. The column check in
/// [`verify_schema`] is the real contract; this just makes the error message
/// actionable for whoever has to look at a changed Hermes.
fn schema_version(conn: &Connection) -> Option<String> {
    let cols = table_columns(conn, "schema_version").ok()?;
    // The column name is chosen from what the PRAGMA reported, never from user
    // input, so interpolating it into the query introduces nothing.
    let col = ["version", "schema_version", "value"]
        .into_iter()
        .find(|c| cols.contains(*c))
        .map(str::to_string)
        .or_else(|| cols.iter().next().cloned())?;
    let sql = format!("SELECT {col} FROM schema_version ORDER BY 1 DESC LIMIT 1");
    let value: rusqlite::types::Value = conn.query_row(&sql, [], |r| r.get(0)).ok()?;
    let rendered = match value {
        rusqlite::types::Value::Integer(i) => i.to_string(),
        rusqlite::types::Value::Real(f) => f.to_string(),
        rusqlite::types::Value::Text(t) => t,
        _ => return None,
    };
    (!rendered.is_empty()).then_some(rendered)
}

#[derive(Debug, Clone)]
struct SessionRow {
    id: String,
    title: Option<String>,
}

#[derive(Debug, Clone)]
struct MessageRow {
    id: i64,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    timestamp: Option<f64>,
    compacted: bool,
}

#[derive(Debug, Clone, Default)]
struct UsageRow {
    model: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    /// `actual_cost_usd` when Hermes has it, else `estimated_cost_usd`. Which one it
    /// was is not carried: [`Usage`] has one cost field, and quietly widening its
    /// meaning per runtime is the reinterpretation `envelope.rs` forbids.
    cost_usd: Option<f64>,
}

/// Read every session from `db` and hand its envelopes to `sink`, oldest session
/// first.
pub fn read_into(
    db: &Path,
    identity: &ImportIdentity,
    since: Option<OffsetDateTime>,
    sink: &mut dyn EnvelopeSink,
) -> Result<SourceStats> {
    let conn = open(db)?;
    let schema = verify_schema(&conn)?;
    let redactor = Redactor::new();

    let mut stats = SourceStats {
        source: db.display().to_string(),
        schema_version: schema.schema_version.clone(),
        ..Default::default()
    };

    let cutoff = since.map(|t| t.unix_timestamp_nanos() as f64 / 1e9);

    for session in read_sessions(&conn, &schema)? {
        stats.sessions_seen += 1;
        let rows = read_messages(&conn, &session.id)?;
        let Some(timeline) = Timeline::of(&rows) else {
            // No row in this session carries a usable timestamp, so there is no
            // instant to stamp an envelope with. Counted rather than dropped
            // silently — see `ImportReport`'s doc.
            stats.sessions_empty += 1;
            continue;
        };
        if let Some(cutoff) = cutoff {
            // `--since` selects whole sessions by last activity, never individual
            // messages: half a session would produce a digest whose duration, turn
            // count and friction signals are arithmetic over a truncated transcript,
            // which is worse than not importing it.
            if timeline.last < cutoff {
                stats.sessions_skipped_by_since += 1;
                continue;
            }
        }
        let usage = read_usage(&conn, &schema, &session.id)?;
        let built = build_session(
            identity, &redactor, &session, &rows, &timeline, &usage, &mut stats,
        );
        sink.accept(&session.id, &built)?;
    }
    Ok(stats)
}

fn read_sessions(conn: &Connection, schema: &Schema) -> Result<Vec<SessionRow>> {
    // Both interpolations come from the schema this process just read, never from
    // user input: `session_key_column` is one of two literals and `title` is a
    // presence check.
    let key = &schema.session_key_column;
    let title = if schema.session_columns.contains("title") {
        "title"
    } else {
        "NULL"
    };
    let sql = format!("SELECT {key}, {title} FROM sessions ORDER BY {key}");
    let mut stmt = conn.prepare(&sql).context("preparing the sessions query")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(SessionRow {
                id: r.get::<_, String>(0)?,
                title: r.get::<_, Option<String>>(1)?,
            })
        })
        .context("reading sessions")?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading a session row")?);
    }
    Ok(out)
}

fn read_messages(conn: &Connection, session_id: &str) -> Result<Vec<MessageRow>> {
    // NULL timestamps sort first in SQLite; `id` as the tiebreak keeps insertion
    // order for them, which is the only ordering left when there is no clock.
    let mut stmt = conn
        .prepare(
            "SELECT id, role, content, tool_call_id, tool_calls, tool_name, timestamp, compacted \
             FROM messages WHERE session_id = ?1 ORDER BY timestamp ASC, id ASC",
        )
        .context("preparing the messages query")?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok(MessageRow {
                id: r.get::<_, i64>(0)?,
                role: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                content: r.get::<_, Option<String>>(2)?,
                tool_call_id: r.get::<_, Option<String>>(3)?,
                tool_calls: r.get::<_, Option<String>>(4)?,
                tool_name: r.get::<_, Option<String>>(5)?,
                timestamp: r.get::<_, Option<f64>>(6)?,
                compacted: r.get::<_, Option<i64>>(7)?.unwrap_or(0) != 0,
            })
        })
        .with_context(|| format!("reading messages of session {session_id}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading a message row")?);
    }
    Ok(out)
}

fn read_usage(conn: &Connection, schema: &Schema, session_id: &str) -> Result<Vec<UsageRow>> {
    if schema.usage_columns.is_empty() {
        return Ok(Vec::new());
    }
    // A column the installed schema does not have is selected as NULL rather than
    // dropping the query, so a partial `session_model_usage` still yields the
    // numbers it does have.
    let select: Vec<String> = USAGE_COLUMNS
        .iter()
        .map(|c| {
            if schema.usage_columns.contains(*c) {
                (*c).to_string()
            } else {
                format!("NULL AS {c}")
            }
        })
        .collect();
    let sql = format!(
        "SELECT {} FROM session_model_usage WHERE session_id = ?1",
        select.join(", ")
    );
    let mut stmt = conn.prepare(&sql).context("preparing the usage query")?;
    let rows = stmt
        .query_map([session_id], |r| {
            let cost: Option<f64> = match r.get::<_, Option<f64>>(6)? {
                Some(actual) => Some(actual),
                None => r.get::<_, Option<f64>>(5)?,
            };
            Ok(UsageRow {
                model: r.get::<_, Option<String>>(0)?,
                input_tokens: non_negative(r.get::<_, Option<i64>>(1)?),
                output_tokens: non_negative(r.get::<_, Option<i64>>(2)?),
                cache_read_tokens: non_negative(r.get::<_, Option<i64>>(3)?),
                cache_write_tokens: non_negative(r.get::<_, Option<i64>>(4)?),
                cost_usd: cost,
            })
        })
        .with_context(|| format!("reading usage of session {session_id}"))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.context("reading a usage row")?);
    }
    Ok(out)
}

fn non_negative(v: Option<i64>) -> u64 {
    v.unwrap_or(0).max(0) as u64
}

/// Every message's resolved instant, plus the session's first and last.
struct Timeline {
    /// One RFC 3339 stamp per row of the input slice, same order, same length.
    stamps: Vec<String>,
    /// The session's first and last instants, RFC 3339-formatted — what the
    /// synthesized `session_start`/`session_end` envelopes are stamped with.
    first_stamp: String,
    last_stamp: String,
    /// The last instant as the raw epoch float, for the `--since` comparison.
    last: f64,
}

impl Timeline {
    /// `None` when no row in the session carries a usable timestamp at all.
    ///
    /// A row with a NULL or non-finite timestamp inherits the previous row's instant
    /// (or the session's first, if it comes before any). Dropping such rows instead
    /// would lose real messages; inventing a clock for them would be worse. Carrying
    /// the neighbouring instant keeps them in the right place in the transcript and
    /// is honest about the resolution being "somewhere around here".
    fn of(rows: &[MessageRow]) -> Option<Self> {
        let first_finite = rows
            .iter()
            .find_map(|r| r.timestamp.filter(|t| usable(*t)))?;
        let mut current = first_finite;
        let mut last = first_finite;
        let mut stamps = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(t) = row.timestamp.filter(|t| usable(*t)) {
                current = t;
                last = t;
            }
            stamps.push(epoch_to_rfc3339(current)?);
        }
        let first_stamp = stamps.first()?.clone();
        let last_stamp = stamps.last()?.clone();
        Some(Self {
            stamps,
            first_stamp,
            last_stamp,
            last,
        })
    }
}

/// Timestamps outside this range are treated as absent rather than converted.
/// `1e12` seconds is the year 33658 — anything beyond it is corrupt, not a date.
fn usable(t: f64) -> bool {
    t.is_finite() && t.abs() < 1e12
}

/// A Hermes `timestamp` (unix epoch **seconds, as a float**) as RFC 3339.
///
/// Fixed three-decimal milliseconds, matching `ctxlake_hook::clock::now_rfc3339`
/// exactly, so a session's imported events and its later live-captured ones sort and
/// parse identically. `time`'s own RFC 3339 formatter trims trailing subsecond zeros,
/// which would make two equally-valid stamps look different in bronze.
pub fn epoch_to_rfc3339(ts: f64) -> Option<String> {
    if !usable(ts) {
        return None;
    }
    // Seconds and fraction are converted separately, and that is not fussiness:
    // `ts * 1e9` for a present-day timestamp is ~1.76e18, far past f64's 2^53 exact
    // integer range, so the product lands a nanosecond or two off and a stamp that
    // should read `.250Z` comes out `.249Z`. Both halves are individually exact —
    // the epoch seconds are ~1.7e9, the scaled fraction is < 1e9 — so splitting the
    // conversion in two keeps the whole thing exact.
    let secs = ts.floor();
    let mut nanos_part = ((ts - secs) * 1_000_000_000.0).round() as i128;
    let mut whole = secs as i128;
    if nanos_part >= 1_000_000_000 {
        // The fraction rounded up to a whole second; carry rather than emit an
        // out-of-range nanosecond.
        whole += 1;
        nanos_part -= 1_000_000_000;
    }
    let dt = OffsetDateTime::from_unix_timestamp_nanos(whole * 1_000_000_000 + nanos_part).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.millisecond(),
    ))
}

/// One entry of a `tool_calls` JSON array, in whichever shape the row carries.
#[derive(Debug, Clone, Default, PartialEq)]
struct ToolCallSpec {
    id: Option<String>,
    name: Option<String>,
    /// Compact JSON text of the call's arguments.
    input: Option<String>,
}

/// Parse a `tool_calls` cell.
///
/// Two shapes are accepted because two are in the wild and Hermes proxies several
/// providers: OpenAI's `{"id", "function": {"name", "arguments": "<json string>"}}`
/// and Anthropic's `{"id", "name", "input": {...}}`. Anything else parses to an empty
/// list — an unrecognized shape leaves `tool.input` absent, which is visible, rather
/// than storing a guess.
fn parse_tool_calls(raw: &str) -> Vec<ToolCallSpec> {
    let Ok(v) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    match &v {
        Value::Array(items) => items.iter().map(spec_from_value).collect(),
        Value::Object(_) => vec![spec_from_value(&v)],
        _ => Vec::new(),
    }
}

fn spec_from_value(v: &Value) -> ToolCallSpec {
    let function = v.get("function");
    let id = ["id", "tool_call_id", "call_id"]
        .into_iter()
        .find_map(|k| v.get(k).and_then(Value::as_str))
        .map(str::to_string);
    let name = ["name", "tool_name"]
        .into_iter()
        .find_map(|k| v.get(k).and_then(Value::as_str))
        .or_else(|| function.and_then(|f| f.get("name")).and_then(Value::as_str))
        .map(str::to_string);
    let input = function
        .and_then(|f| f.get("arguments"))
        .or_else(|| v.get("arguments"))
        .or_else(|| v.get("input"))
        .or_else(|| v.get("args"))
        .and_then(stringify);
    ToolCallSpec { id, name, input }
}

/// A JSON string stays verbatim (OpenAI's `arguments` is *already* JSON text, and
/// re-encoding it would store `"{\"command\":...}"`); anything else is serialized
/// compactly. Mirrors `ctxlake_hook::adapters::common::get_stringified`.
fn stringify(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

/// The path a tool call touched, when its input names one — the key to
/// `Redactor::is_denied_path`. Same key list the live adapters use.
fn file_path_of(input: Option<&str>) -> Option<String> {
    let v: Value = serde_json::from_str(input?).ok()?;
    ["file_path", "path", "filePath", "file"]
        .into_iter()
        .find_map(|k| v.get(k).and_then(Value::as_str))
        .map(str::to_string)
}

/// Build one session's envelopes, in chronological order.
///
/// Order matters beyond tidiness: `Envelope::new` mints a monotonic ULID per call, so
/// the order these are constructed in *is* the order `ctxlake_maint::digest::compute`
/// will replay them in. See the module doc of [`super`].
fn build_session(
    identity: &ImportIdentity,
    redactor: &Redactor,
    session: &SessionRow,
    rows: &[MessageRow],
    timeline: &Timeline,
    usage: &[UsageRow],
    stats: &mut SourceStats,
) -> Vec<Envelope> {
    let mut out = Vec::with_capacity(rows.len() + 3);

    // 1. session_start, at the first message's instant, carrying the title Hermes
    //    already generated — a digest gets a headline without an LLM call, exactly
    //    the way Claude Code's `ai-title` record gives one.
    let mut start = new_envelope(
        identity,
        &session.id,
        EventType::SessionStart,
        &timeline.first_stamp,
    );
    let mut acc = RedactionAcc::default();
    // Prose, so literal markers only: the entropy heuristic false-positives on
    // titles the same way it does on any short human phrase.
    start.content = scrub_field(redactor, &mut acc, session.title.clone(), false);
    finish(&mut start, acc);
    out.push(start);

    // 2. the messages, with a compaction marker closing each run of compacted rows.
    let call_index = index_tool_calls(rows);
    for (i, row) in rows.iter().enumerate() {
        let at = &timeline.stamps[i];
        match row.role.as_str() {
            "user" => out.push(message_envelope(
                identity,
                redactor,
                &session.id,
                row,
                at,
                EventType::Prompt,
                "user",
            )),
            "assistant" => out.push(message_envelope(
                identity,
                redactor,
                &session.id,
                row,
                at,
                EventType::Assistant,
                "assistant",
            )),
            "tool" => out.push(tool_envelope(
                identity,
                redactor,
                &session.id,
                row,
                at,
                &call_index,
            )),
            _ => stats.rows_skipped_unknown_role += 1,
        }

        let run_ends = row.compacted && rows.get(i + 1).map(|n| !n.compacted).unwrap_or(true);
        if run_ends {
            // Hermes has no compaction *hook* — `docs/runtimes.md § Hermes`'s
            // compaction gap is real for live capture — but the database records the
            // fact after the event. `compacted = 1` means the message was compacted
            // AWAY (Hermes sets `active = 0, compacted = 1` together — its own tests
            // assert this, and across a real 17,329-message database all 2,835 such
            // rows had `active = 0`). The summary message carries a separate
            // `_compressed_summary` flag. One marker per contiguous run of `compacted = 1`
            // rows, stamped at the run's last message: that instant is where the
            // discarded region ends, which is the thing a reader wants to know. One
            // per row would report a 500-message compaction as 500 compactions.
            let mut compact = new_envelope(identity, &session.id, EventType::Compact, at);
            // No content, exactly like a live `PreCompact` envelope
            // (`adapters::claude_code`): the event is the marker.
            compact.message_id = Some(row.id.to_string());
            finish(&mut compact, RedactionAcc::default());
            out.push(compact);
        }
    }

    // 3. session_end, at the last message's instant, carrying the session's summed
    //    token and cost accounting. See the module doc for why it is summed here and
    //    not spread across the assistant messages.
    let mut end = new_envelope(
        identity,
        &session.id,
        EventType::SessionEnd,
        &timeline.last_stamp,
    );
    end.usage = aggregate_usage(usage);
    finish(&mut end, RedactionAcc::default());
    out.push(end);

    out
}

/// `tool_call_id` -> the assistant row that asked for it, and the arguments it asked
/// with. A `role='tool'` row carries the *result*; the input lives on the assistant
/// message that requested the call, which is the only place to recover it from.
fn index_tool_calls(rows: &[MessageRow]) -> HashMap<String, (i64, ToolCallSpec)> {
    let mut index = HashMap::new();
    for row in rows {
        let Some(raw) = &row.tool_calls else { continue };
        for spec in parse_tool_calls(raw) {
            if let Some(id) = spec.id.clone() {
                index.insert(id, (row.id, spec));
            }
        }
    }
    index
}

fn new_envelope(
    identity: &ImportIdentity,
    session_id: &str,
    event_type: EventType,
    at: &str,
) -> Envelope {
    let mut env = Envelope::new(
        identity.fleet_id.clone(),
        identity.agent_id.clone(),
        Runtime::Hermes,
        session_id,
        event_type,
        at,
    );
    env.host_id = identity.host_id.clone();
    // The one field that distinguishes this from a live capture, so a backfilled
    // session is never mistaken for an active one in the roster.
    env.imported = true;
    env
}

/// Fold the accumulated redaction outcome in and re-hash the content.
///
/// `content_hash` must reflect what actually landed in bronze, never the
/// pre-redaction value — the same discipline every live adapter follows, and the
/// reason dedup still works on a quarantined event.
fn finish(env: &mut Envelope, acc: RedactionAcc) {
    env.redaction = acc.into_redaction();
    env.content_hash = hash::content_hash(env.content.as_deref().unwrap_or(""));
}

fn message_envelope(
    identity: &ImportIdentity,
    redactor: &Redactor,
    session_id: &str,
    row: &MessageRow,
    at: &str,
    event_type: EventType,
    role: &str,
) -> Envelope {
    let mut env = new_envelope(identity, session_id, event_type, at);
    let mut acc = RedactionAcc::default();
    env.role = Some(role.to_string());
    // `messages.id` is the only guaranteed-unique identity a row has, and it is what
    // makes a re-import exactly idempotent without ever collapsing two distinct
    // messages that happen to say the same thing (see `super::dedup_key`).
    env.message_id = Some(row.id.to_string());
    // Prose: literal markers only, no entropy pass. See `redact.rs` on why prose
    // false-positives are costly.
    env.content = scrub_field(redactor, &mut acc, row.content.clone(), false);
    finish(&mut env, acc);
    env
}

fn tool_envelope(
    identity: &ImportIdentity,
    redactor: &Redactor,
    session_id: &str,
    row: &MessageRow,
    at: &str,
    call_index: &HashMap<String, (i64, ToolCallSpec)>,
) -> Envelope {
    let mut env = new_envelope(identity, session_id, EventType::ToolCall, at);
    let mut acc = RedactionAcc::default();
    env.message_id = Some(row.id.to_string());
    // `role` stays unset, matching `adapters::hermes`: a live tool event carries the
    // call in `tool`, not a role string, and an imported one must look the same.

    // The requesting assistant message, when the call id resolves — a real parent
    // link, not a guess, and the only piece of Hermes's message DAG recoverable here.
    let matched = row
        .tool_call_id
        .as_deref()
        .and_then(|id| call_index.get(id));
    if let Some((parent_id, _)) = matched {
        env.parent_message_id = Some(parent_id.to_string());
    }

    // The row's own `tool_calls`, if it has one, wins over the index: a row that
    // carries its own call spec is describing itself.
    let own_spec = row
        .tool_calls
        .as_deref()
        .map(parse_tool_calls)
        .and_then(|specs| {
            match row.tool_call_id.as_deref() {
                Some(id) => specs.into_iter().find(|s| s.id.as_deref() == Some(id)),
                // A single unlabelled call on the row itself is unambiguous.
                None => specs.into_iter().next(),
            }
        });
    let spec = own_spec.or_else(|| matched.map(|(_, s)| s.clone()));

    let name = row
        .tool_name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| spec.as_ref().and_then(|s| s.name.clone()))
        .unwrap_or_else(|| "unknown".to_string());

    let raw_input = spec.as_ref().and_then(|s| s.input.clone()).map(|s| {
        // Bound before hashing, exactly like the live adapters: `input_hash` has to
        // be a hash of the value that is actually stored.
        truncate(&s)
    });
    let input_hash = hash::content_hash(raw_input.as_deref().unwrap_or(""));
    let path = file_path_of(raw_input.as_deref());

    // Both directions go through the denylist. A *write* to `.aws/credentials`
    // carries the secret in the input, not the result; skipping that direction is a
    // silent bypass of AGENTS.md invariant 7. Same reasoning as
    // `adapters::hermes::build_tool_call`.
    let input = withhold_if_denied_path(redactor, &mut acc, path.as_deref(), raw_input);
    let input = scrub_field(redactor, &mut acc, input, true);

    let raw_result = row.content.clone();
    let raw_result = withhold_if_denied_path(redactor, &mut acc, path.as_deref(), raw_result);
    let result = scrub_field(redactor, &mut acc, raw_result, true);

    env.tool = Some(ToolCall {
        name,
        input,
        input_hash,
        result,
        paths: path.into_iter().collect(),
        ..Default::default()
    });
    finish(&mut env, acc);
    env
}

/// Sum `session_model_usage` across models. `model` is named only when the session
/// used exactly one — naming one of several would be a lie about where the tokens
/// went, and [`Usage`] has room for one name.
fn aggregate_usage(rows: &[UsageRow]) -> Option<Usage> {
    if rows.is_empty() {
        return None;
    }
    let mut total = Usage::default();
    let mut cost = None::<f64>;
    let mut models: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        total.input_tokens += row.input_tokens;
        total.output_tokens += row.output_tokens;
        total.cache_read_tokens += row.cache_read_tokens;
        total.cache_write_tokens += row.cache_write_tokens;
        if let Some(c) = row.cost_usd {
            cost = Some(cost.unwrap_or(0.0) + c);
        }
        if let Some(m) = row.model.as_ref().filter(|m| !m.is_empty()) {
            models.insert(m.clone());
        }
    }
    total.cost_usd = cost;
    if models.len() == 1 {
        total.model = models.into_iter().next();
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::{dedup_key, EnvelopeSink, Ledger, Spooler};
    use std::path::PathBuf;

    /// Collects what a source produced, so envelope construction can be asserted
    /// field by field without a spool or a ledger in the way.
    #[derive(Default)]
    struct Collector {
        sessions: Vec<(String, Vec<Envelope>)>,
    }

    impl EnvelopeSink for Collector {
        fn accept(&mut self, session_id: &str, envelopes: &[Envelope]) -> anyhow::Result<()> {
            self.sessions
                .push((session_id.to_string(), envelopes.to_vec()));
            Ok(())
        }
    }

    impl Collector {
        fn session(&self, id: &str) -> &[Envelope] {
            &self
                .sessions
                .iter()
                .find(|(s, _)| s.as_str() == id)
                .unwrap_or_else(|| panic!("no session {id} in {:?}", self.ids()))
                .1
        }
        fn ids(&self) -> Vec<&str> {
            self.sessions.iter().map(|(s, _)| s.as_str()).collect()
        }
    }

    fn identity() -> ImportIdentity {
        ImportIdentity {
            fleet_id: "myteam".into(),
            agent_id: "herm-01".into(),
            host_id: hash::host_id("alice-laptop"),
        }
    }

    /// The real `~/.hermes/state.db` schema, reproduced column-for-column from a
    /// live install's `PRAGMA table_info` output.
    ///
    /// **Never point a test at a real Hermes database.** It belongs to a running
    /// agent, it contains that person's transcripts, and a test that depends on one
    /// passes or fails according to what somebody typed last week.
    const FIXTURE_SCHEMA: &str = r#"
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        CREATE TABLE sessions (
            id TEXT PRIMARY KEY,
            title TEXT,
            title_source TEXT,
            last_activity_at REAL,
            last_activity_description TEXT,
            api_call_count INTEGER DEFAULT 0,
            handoff_state TEXT,
            profile_name TEXT,
            archived INTEGER DEFAULT 0,
            pinned INTEGER DEFAULT 0,
            hidden INTEGER DEFAULT 0,
            tool_names TEXT,
            estimated_cost_usd REAL,
            actual_cost_usd REAL
        );
        CREATE TABLE messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            role TEXT,
            content TEXT,
            tool_call_id TEXT,
            tool_calls TEXT,
            tool_name TEXT,
            effect_disposition TEXT,
            timestamp REAL,
            token_count INTEGER,
            finish_reason TEXT,
            reasoning TEXT,
            reasoning_content TEXT,
            reasoning_details TEXT,
            codex_reasoning_items TEXT,
            codex_message_items TEXT,
            platform_message_id TEXT,
            observed INTEGER DEFAULT 0,
            _compressed_summary INTEGER DEFAULT 0,
            active INTEGER DEFAULT 1,
            compacted INTEGER DEFAULT 0,
            api_content TEXT,
            display_kind TEXT,
            display_metadata TEXT,
            display_identity TEXT,
            display_order TEXT
        );
        CREATE TABLE session_model_usage (
            session_id TEXT NOT NULL,
            model TEXT NOT NULL,
            api_call_count INTEGER DEFAULT 0,
            input_tokens INTEGER DEFAULT 0,
            output_tokens INTEGER DEFAULT 0,
            cache_read_tokens INTEGER DEFAULT 0,
            cache_write_tokens INTEGER DEFAULT 0,
            reasoning_tokens INTEGER DEFAULT 0,
            estimated_cost_usd REAL,
            actual_cost_usd REAL,
            cost_status TEXT,
            cost_source TEXT,
            first_seen REAL,
            last_seen REAL,
            PRIMARY KEY (session_id, model)
        );
        CREATE TABLE system_prompts (id INTEGER PRIMARY KEY, session_id TEXT, content TEXT);
        -- The real database carries FTS5 indexes over message content. The importer
        -- never reads them; the fixture keeps one so "it tripped over the shadow
        -- tables" cannot be the reason a future change breaks.
        CREATE VIRTUAL TABLE messages_fts USING fts5(content);
    "#;

    struct Fixture {
        _dir: tempfile::TempDir,
        db: PathBuf,
    }

    impl Fixture {
        /// A database with the real schema and a handful of sessions:
        ///
        /// - `sess-alpha`: a user prompt, an assistant message with a `tool_calls`
        ///   array, the matching tool result, a second prompt, a compacted row, and
        ///   a usage row.
        /// - `sess-beta`: one prompt containing a credential.
        /// - `sess-empty`: a session with no messages at all.
        fn build() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let db = dir.path().join("state.db");
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(FIXTURE_SCHEMA).unwrap();
            conn.execute("INSERT INTO schema_version (version) VALUES (7)", [])
                .unwrap();
            conn.execute_batch(
                r#"
                INSERT INTO sessions (id, title, title_source, last_activity_at)
                VALUES ('sess-alpha', 'Port the Glue catalog off the CLI', 'llm', 1763000123.5),
                       ('sess-beta',  'Debugging a deploy',                'llm', 1763000200.0),
                       ('sess-empty', 'Never said anything',               'llm', 1763000000.0);

                INSERT INTO messages
                    (id, session_id, role, content, tool_call_id, tool_calls, tool_name,
                     timestamp, token_count, compacted)
                VALUES
                    (1, 'sess-alpha', 'user', 'run the tests please', NULL, NULL, NULL,
                     1763000100.25, 5, 0),
                    (2, 'sess-alpha', 'assistant', 'Running them now.', NULL,
                     '[{"id":"call_1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"cargo test\"}"}}]',
                     NULL, 1763000110.5, 9, 0),
                    (3, 'sess-alpha', 'tool', 'test result: ok. 41 passed', 'call_1', NULL,
                     'terminal', 1763000120.75, 12, 0),
                    (4, 'sess-alpha', 'user', 'nice, compact and carry on', NULL, NULL, NULL,
                     1763000121.0, 6, 1),
                    (5, 'sess-alpha', 'assistant', 'Context compacted.', NULL, NULL, NULL,
                     1763000123.5, 4, 0),
                    (6, 'sess-beta', 'user', 'here is my key AKIAIOSFODNN7EXAMPLE, use it',
                     NULL, NULL, NULL, 1763000200.0, 11, 0);

                INSERT INTO session_model_usage
                    (session_id, model, api_call_count, input_tokens, output_tokens,
                     cache_read_tokens, cache_write_tokens, reasoning_tokens,
                     estimated_cost_usd, actual_cost_usd, cost_status, cost_source,
                     first_seen, last_seen)
                VALUES ('sess-alpha', 'claude-opus-4', 3, 1200, 340, 800, 120, 44,
                        0.0201, 0.0187, 'actual', 'api', 1763000100.25, 1763000123.5);
                "#,
            )
            .unwrap();
            drop(conn);
            Self { _dir: dir, db }
        }

        fn import(&self) -> (SourceStats, Collector) {
            self.import_since(None)
        }

        fn import_since(&self, since: Option<OffsetDateTime>) -> (SourceStats, Collector) {
            let mut c = Collector::default();
            let stats = read_into(&self.db, &identity(), since, &mut c).unwrap();
            (stats, c)
        }
    }

    // --- opening the database -------------------------------------------------

    #[test]
    fn the_database_is_opened_read_only() {
        // Asserting the *flags*, not just that no write happened: Hermes's state.db
        // belongs to a running agent, and "this code path didn't happen to write"
        // is not the same property as "a write is impossible".
        let flags = open_flags();
        assert!(
            flags.contains(OpenFlags::SQLITE_OPEN_READ_ONLY),
            "must be read-only"
        );
        assert!(
            flags.contains(OpenFlags::SQLITE_OPEN_URI),
            "the ?mode=ro&immutable=1 query string is inert without URI parsing"
        );
        assert!(
            !flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE),
            "read-write would contend with the running agent for its own database"
        );
        assert!(
            !flags.contains(OpenFlags::SQLITE_OPEN_CREATE),
            "import must never bring a Hermes database into existence"
        );
    }

    #[test]
    fn the_open_uri_asks_for_read_only_and_immutable() {
        let uri = read_only_uri(Path::new("/home/alice/.hermes/state.db"));
        assert_eq!(uri, "file:/home/alice/.hermes/state.db?mode=ro&immutable=1");
    }

    #[test]
    fn a_path_with_uri_punctuation_is_escaped_not_truncated() {
        // An unescaped `?` ends the path component, so a database in a directory
        // containing one would silently be opened as some other file.
        let uri = read_only_uri(Path::new("/home/alice/what? dir/state.db"));
        assert!(
            uri.starts_with("file:/home/alice/what%3F dir/state.db?"),
            "{uri}"
        );
        assert!(uri.ends_with("?mode=ro&immutable=1"), "{uri}");
    }

    #[test]
    fn an_opened_database_refuses_a_write() {
        // The behavioural half of `the_database_is_opened_read_only`.
        let fx = Fixture::build();
        let conn = open(&fx.db).unwrap();
        let err = conn
            .execute("DELETE FROM messages", [])
            .expect_err("a read-only connection must refuse a write");
        assert!(
            err.to_string().to_lowercase().contains("read"),
            "expected a read-only error, got: {err}"
        );
    }

    #[test]
    fn opening_takes_no_lock_file_beside_the_database() {
        // `immutable=1`'s actual purpose: no `-shm`/`-wal` companion appears, so
        // there is nothing for the running agent to contend on.
        let fx = Fixture::build();
        let conn = open(&fx.db).unwrap();
        let _: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert!(!fx.db.with_extension("db-shm").exists());
        assert!(!fx.db.with_extension("db-wal").exists());
    }

    #[test]
    fn a_missing_database_is_a_clear_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let err = open(&dir.path().join("state.db")).unwrap_err();
        assert!(
            err.to_string().contains("no Hermes state database"),
            "{err}"
        );
    }

    // --- the schema gate ------------------------------------------------------

    #[test]
    fn the_schema_version_is_reported() {
        let fx = Fixture::build();
        let (stats, _) = fx.import();
        assert_eq!(stats.schema_version.as_deref(), Some("7"));
    }

    #[test]
    fn a_renamed_column_fails_loudly_instead_of_importing_nothing() {
        // The regression this whole gate exists for: a future Hermes that renames
        // `tool_calls` must not produce a successful, empty-looking import.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn.execute_batch("ALTER TABLE messages RENAME COLUMN tool_calls TO tool_invocations;")
            .unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (99)", [])
            .unwrap();
        drop(conn);

        let mut c = Collector::default();
        let err = read_into(&db, &identity(), None, &mut c).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("tool_calls"),
            "must name the missing column: {msg}"
        );
        assert!(msg.contains("99"), "must quote the observed version: {msg}");
        assert!(
            c.sessions.is_empty(),
            "nothing may be imported on a failed gate"
        );
    }

    #[test]
    fn a_database_that_is_not_hermes_at_all_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("not-hermes.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE notes (id INTEGER, body TEXT);")
            .unwrap();
        drop(conn);
        let mut c = Collector::default();
        let err = read_into(&db, &identity(), None, &mut c).unwrap_err();
        assert!(err.to_string().contains("no `messages` table"), "{err}");
    }

    #[test]
    fn a_missing_usage_table_degrades_rather_than_failing() {
        // Token accounting is a bonus on top of the transcript; losing it must not
        // cost the transcript.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn.execute_batch(
            "DROP TABLE session_model_usage;
             INSERT INTO sessions (id, title) VALUES ('s1', 'a title');
             INSERT INTO messages (id, session_id, role, content, timestamp)
             VALUES (1, 's1', 'user', 'hello', 1763000100.0);",
        )
        .unwrap();
        drop(conn);

        let mut c = Collector::default();
        read_into(&db, &identity(), None, &mut c).unwrap();
        let events = c.session("s1");
        assert_eq!(events.len(), 3, "start + prompt + end");
        assert!(events.last().unwrap().usage.is_none());
    }

    // --- the mapping ----------------------------------------------------------

    #[test]
    fn a_session_becomes_start_messages_and_end_in_chronological_order() {
        let fx = Fixture::build();
        let (stats, c) = fx.import();
        assert_eq!(stats.sessions_seen, 3);

        let events = c.session("sess-alpha");
        let kinds: Vec<EventType> = events.iter().map(|e| e.event_type).collect();
        assert_eq!(
            kinds,
            vec![
                EventType::SessionStart,
                EventType::Prompt,
                EventType::Assistant,
                EventType::ToolCall,
                EventType::Prompt,
                EventType::Compact,
                EventType::Assistant,
                EventType::SessionEnd,
            ]
        );

        // event_id order must equal chronological order — `digest::compute` sorts by
        // it and derives duration and turn count from that order.
        let ids: Vec<&str> = events.iter().map(|e| e.event_id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(
            ids, sorted,
            "envelopes must be minted in chronological order"
        );

        let stamps: Vec<&str> = events.iter().map(|e| e.emitted_at.as_str()).collect();
        let mut sorted_stamps = stamps.clone();
        sorted_stamps.sort_unstable();
        assert_eq!(stamps, sorted_stamps);
    }

    #[test]
    fn every_imported_envelope_is_flagged_imported_and_carries_the_fleet_identity() {
        let fx = Fixture::build();
        let (_, c) = fx.import();
        for env in c.session("sess-alpha") {
            assert!(
                env.imported,
                "{:?} was not flagged imported",
                env.event_type
            );
            assert_eq!(env.runtime, Runtime::Hermes);
            assert_eq!(env.fleet_id, "myteam");
            assert_eq!(env.agent_id, "herm-01");
            assert_eq!(env.host_id, hash::host_id("alice-laptop"));
            assert_eq!(env.session_id, "sess-alpha");
        }
    }

    #[test]
    fn the_session_title_rides_in_on_session_start() {
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let start = &c.session("sess-alpha")[0];
        assert_eq!(start.event_type, EventType::SessionStart);
        assert_eq!(
            start.content.as_deref(),
            Some("Port the Glue catalog off the CLI")
        );
        assert_eq!(
            start.content_hash,
            hash::content_hash("Port the Glue catalog off the CLI"),
            "content_hash must be the hash of what actually landed"
        );
    }

    #[test]
    fn a_user_row_becomes_a_prompt_and_an_assistant_row_an_assistant_message() {
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let events = c.session("sess-alpha");

        let prompt = &events[1];
        assert_eq!(prompt.event_type, EventType::Prompt);
        assert_eq!(prompt.role.as_deref(), Some("user"));
        assert_eq!(prompt.content.as_deref(), Some("run the tests please"));
        assert_eq!(prompt.message_id.as_deref(), Some("1"));

        let assistant = &events[2];
        assert_eq!(assistant.event_type, EventType::Assistant);
        assert_eq!(assistant.role.as_deref(), Some("assistant"));
        assert_eq!(assistant.content.as_deref(), Some("Running them now."));
        assert_eq!(assistant.message_id.as_deref(), Some("2"));
    }

    #[test]
    fn a_tool_row_carries_name_result_and_the_input_from_the_requesting_message() {
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let tool_event = &c.session("sess-alpha")[3];
        assert_eq!(tool_event.event_type, EventType::ToolCall);
        let tool = tool_event.tool.as_ref().expect("a tool call");

        assert_eq!(tool.name, "terminal");
        assert_eq!(tool.result.as_deref(), Some("test result: ok. 41 passed"));
        // `tool_calls` lives on the assistant row that asked for the call, not on
        // the result row — resolving it through `tool_call_id` is the only way to
        // recover the input at all.
        assert_eq!(tool.input.as_deref(), Some(r#"{"command":"cargo test"}"#));
        assert_eq!(
            tool.input_hash,
            hash::content_hash(r#"{"command":"cargo test"}"#)
        );
        assert_eq!(
            tool_event.parent_message_id.as_deref(),
            Some("2"),
            "the tool result's parent is the assistant message that requested it"
        );
        // Matches live capture: a tool event carries the call, not a role string.
        assert!(tool_event.role.is_none());
    }

    #[test]
    fn an_anthropic_shaped_tool_call_is_understood_too() {
        // Hermes proxies several providers, so `tool_calls` arrives in more than one
        // shape. A parser that only knows OpenAI's silently loses every tool input
        // from the others.
        let specs =
            parse_tool_calls(r#"[{"id":"toolu_1","name":"read_file","input":{"path":"a.rs"}}]"#);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id.as_deref(), Some("toolu_1"));
        assert_eq!(specs[0].name.as_deref(), Some("read_file"));
        assert_eq!(specs[0].input.as_deref(), Some(r#"{"path":"a.rs"}"#));
    }

    #[test]
    fn an_unparseable_tool_calls_cell_leaves_the_input_absent_rather_than_guessing() {
        assert!(parse_tool_calls("not json at all").is_empty());
        assert!(parse_tool_calls("[1, 2, 3]")[0].input.is_none());
    }

    #[test]
    fn a_compacted_row_yields_a_compact_event() {
        // Hermes has no compaction *hook* — live capture cannot see this at all —
        // but the database records it, so import can. This is the asymmetry
        // docs/adding-it.md now states.
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let compacts: Vec<&Envelope> = c
            .session("sess-alpha")
            .iter()
            .filter(|e| e.event_type == EventType::Compact)
            .collect();
        assert_eq!(compacts.len(), 1, "one marker for the one compacted run");
        assert_eq!(compacts[0].message_id.as_deref(), Some("4"));
        assert_eq!(compacts[0].emitted_at, "2025-11-13T02:15:21.000Z");
        assert!(
            compacts[0].content.is_none(),
            "a compaction marker has no content"
        );
    }

    #[test]
    fn a_run_of_compacted_rows_yields_one_marker_not_one_per_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, title) VALUES ('s1', 't');
             INSERT INTO messages (id, session_id, role, content, timestamp, compacted) VALUES
                (1, 's1', 'user',      'a', 1763000100.0, 1),
                (2, 's1', 'assistant', 'b', 1763000101.0, 1),
                (3, 's1', 'user',      'c', 1763000102.0, 1),
                (4, 's1', 'assistant', 'd', 1763000103.0, 0),
                (5, 's1', 'user',      'e', 1763000104.0, 1);",
        )
        .unwrap();
        drop(conn);

        let mut c = Collector::default();
        read_into(&db, &identity(), None, &mut c).unwrap();
        let compacts: Vec<&Envelope> = c
            .session("s1")
            .iter()
            .filter(|e| e.event_type == EventType::Compact)
            .collect();
        assert_eq!(
            compacts.len(),
            2,
            "two runs of compacted rows, not five markers"
        );
        assert_eq!(
            compacts[0].message_id.as_deref(),
            Some("3"),
            "run ends at row 3"
        );
        assert_eq!(compacts[1].message_id.as_deref(), Some("5"));
    }

    #[test]
    fn a_float_timestamp_converts_to_rfc3339() {
        // The exact shape ctxlake_hook::clock::now_rfc3339 emits, so imported and
        // captured events in the same session are indistinguishable to a parser.
        assert_eq!(
            epoch_to_rfc3339(1_700_000_000.0).unwrap(),
            "2023-11-14T22:13:20.000Z"
        );
        assert_eq!(
            epoch_to_rfc3339(1_700_000_000.5).unwrap(),
            "2023-11-14T22:13:20.500Z"
        );
        assert_eq!(
            epoch_to_rfc3339(1_763_000_100.25).unwrap(),
            "2025-11-13T02:15:00.250Z"
        );
        // A leap day, the thing a hand-rolled calendar gets wrong first.
        assert_eq!(
            epoch_to_rfc3339(1_709_208_000.0).unwrap(),
            "2024-02-29T12:00:00.000Z"
        );
        // Parses back with the same reader `digest.rs` uses.
        assert!(OffsetDateTime::parse(
            &epoch_to_rfc3339(1_763_000_100.25).unwrap(),
            &time::format_description::well_known::Rfc3339
        )
        .is_ok());
    }

    #[test]
    fn a_nonsense_timestamp_is_refused_rather_than_wrapping_to_a_fake_date() {
        assert!(epoch_to_rfc3339(f64::NAN).is_none());
        assert!(epoch_to_rfc3339(f64::INFINITY).is_none());
        assert!(epoch_to_rfc3339(1e30).is_none());
    }

    #[test]
    fn the_first_and_last_message_bound_the_session() {
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let events = c.session("sess-alpha");
        assert_eq!(events[0].emitted_at, "2025-11-13T02:15:00.250Z");
        assert_eq!(
            events.last().unwrap().emitted_at,
            "2025-11-13T02:15:23.500Z"
        );
    }

    #[test]
    fn token_and_cost_accounting_lands_on_session_end() {
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let end = c.session("sess-alpha").last().unwrap();
        assert_eq!(end.event_type, EventType::SessionEnd);
        let usage = end.usage.as_ref().expect("usage from session_model_usage");
        assert_eq!(usage.input_tokens, 1200);
        assert_eq!(usage.output_tokens, 340);
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.cache_write_tokens, 120);
        assert_eq!(usage.model.as_deref(), Some("claude-opus-4"));
        // `actual_cost_usd` wins over the estimate when Hermes has it.
        assert_eq!(usage.cost_usd, Some(0.0187));

        // And exactly one envelope in the session carries usage, or
        // `digest::compute` — which sums across every envelope — double counts.
        let carrying = c
            .session("sess-alpha")
            .iter()
            .filter(|e| e.usage.is_some())
            .count();
        assert_eq!(carrying, 1);
    }

    #[test]
    fn usage_falls_back_to_the_estimate_and_sums_across_models() {
        let rows = vec![
            UsageRow {
                model: Some("a".into()),
                input_tokens: 10,
                output_tokens: 1,
                cost_usd: Some(0.5),
                ..Default::default()
            },
            UsageRow {
                model: Some("b".into()),
                input_tokens: 20,
                output_tokens: 2,
                cost_usd: Some(0.25),
                ..Default::default()
            },
        ];
        let u = aggregate_usage(&rows).unwrap();
        assert_eq!(u.input_tokens, 30);
        assert_eq!(u.output_tokens, 3);
        assert_eq!(u.cost_usd, Some(0.75));
        assert!(
            u.model.is_none(),
            "naming one of two models would misattribute the tokens"
        );
        assert!(aggregate_usage(&[]).is_none());
    }

    #[test]
    fn a_session_with_no_messages_is_counted_not_imported() {
        let fx = Fixture::build();
        let (stats, c) = fx.import();
        assert_eq!(stats.sessions_empty, 1);
        assert!(!c.ids().contains(&"sess-empty"));
    }

    #[test]
    fn a_row_with_an_unmapped_role_is_counted_not_silently_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO sessions (id, title) VALUES ('s1', 't');
             INSERT INTO messages (id, session_id, role, content, timestamp) VALUES
                (1, 's1', 'user',   'hi',       1763000100.0),
                (2, 's1', 'system', 'you are…', 1763000101.0);",
        )
        .unwrap();
        drop(conn);
        let mut c = Collector::default();
        let stats = read_into(&db, &identity(), None, &mut c).unwrap();
        assert_eq!(stats.rows_skipped_unknown_role, 1);
        assert_eq!(c.session("s1").len(), 3, "start + the one prompt + end");
    }

    // --- --since --------------------------------------------------------------

    #[test]
    fn since_selects_whole_sessions_by_last_activity() {
        let fx = Fixture::build();
        // Between sess-alpha's last message (…123.5) and sess-beta's (…200.0).
        let cutoff = OffsetDateTime::from_unix_timestamp(1_763_000_150).unwrap();
        let (stats, c) = fx.import_since(Some(cutoff));
        assert_eq!(stats.sessions_skipped_by_since, 1);
        assert!(c.ids().contains(&"sess-beta"));
        assert!(!c.ids().contains(&"sess-alpha"));
    }

    #[test]
    fn since_before_everything_imports_everything() {
        let fx = Fixture::build();
        let cutoff = OffsetDateTime::from_unix_timestamp(1).unwrap();
        let (stats, c) = fx.import_since(Some(cutoff));
        assert_eq!(stats.sessions_skipped_by_since, 0);
        assert_eq!(c.sessions.len(), 2, "the two non-empty sessions");
    }

    // --- redaction ------------------------------------------------------------

    #[test]
    fn a_credential_in_an_imported_message_is_quarantined_before_it_reaches_bronze() {
        // AGENTS.md invariant 7: import is not a fast path around the scrubber, and
        // a historical transcript is the likeliest place an un-redacted key already
        // sits. Bronze is immutable, so this has to happen here or never.
        let fx = Fixture::build();
        let (_, c) = fx.import();
        let prompt = c
            .session("sess-beta")
            .iter()
            .find(|e| e.event_type == EventType::Prompt)
            .expect("the prompt");
        let content = prompt.content.as_deref().unwrap();
        assert!(
            !content.contains("AKIAIOSFODNN7EXAMPLE"),
            "the credential survived import: {content}"
        );
        assert_eq!(prompt.redaction.status, "quarantined");
        assert!(prompt
            .redaction
            .rules_fired
            .contains(&"aws_access_key_id".to_string()));
        assert_eq!(
            prompt.content_hash,
            hash::content_hash(content),
            "content_hash must hash the redacted value, not the original"
        );
    }

    #[test]
    fn a_secret_in_a_tool_result_is_scrubbed_too() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        conn.execute_batch(
            r#"INSERT INTO sessions (id, title) VALUES ('s1', 't');
               INSERT INTO messages (id, session_id, role, content, tool_call_id, tool_calls, tool_name, timestamp) VALUES
                 (1, 's1', 'assistant', 'checking', NULL,
                  '[{"id":"c1","function":{"name":"terminal","arguments":"{\"command\":\"cat .env\"}"}}]',
                  NULL, 1763000100.0),
                 (2, 's1', 'tool', 'ANTHROPIC_API_KEY=sk-ant-api03-abcdefghij', 'c1', NULL,
                  'terminal', 1763000101.0);"#,
        )
        .unwrap();
        drop(conn);
        let mut c = Collector::default();
        read_into(&db, &identity(), None, &mut c).unwrap();
        let tool_event = c
            .session("s1")
            .iter()
            .find(|e| e.event_type == EventType::ToolCall)
            .unwrap();
        let result = tool_event.tool.as_ref().unwrap().result.as_deref().unwrap();
        assert!(!result.contains("sk-ant"), "leaked: {result}");
        assert_eq!(tool_event.redaction.status, "quarantined");
    }

    #[test]
    fn a_write_to_a_denylisted_path_does_not_leak_through_the_tool_input() {
        let secret = "qV3kRt8zLmNp0XyW7bHfJ2sD4gUe6AcZ1oIl5TnB";
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(FIXTURE_SCHEMA).unwrap();
        let tool_calls = serde_json::json!([{
            "id": "c1",
            "name": "write_file",
            "input": {
                "file_path": "/home/alice/.aws/credentials",
                "content": format!("[default]\naws_access_key={secret}")
            }
        }])
        .to_string();
        conn.execute("INSERT INTO sessions (id, title) VALUES ('s1', 't')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, tool_calls, timestamp) \
             VALUES (1, 's1', 'assistant', 'writing', ?1, 1763000100.0)",
            [&tool_calls],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, session_id, role, content, tool_call_id, tool_name, timestamp) \
             VALUES (2, 's1', 'tool', 'ok', 'c1', 'write_file', 1763000101.0)",
            [],
        )
        .unwrap();
        drop(conn);

        let mut c = Collector::default();
        read_into(&db, &identity(), None, &mut c).unwrap();
        let tool_event = c
            .session("s1")
            .iter()
            .find(|e| e.event_type == EventType::ToolCall)
            .unwrap();
        let tool = tool_event.tool.as_ref().unwrap();
        assert_eq!(tool_event.redaction.status, "quarantined");
        assert!(
            !tool.input.as_deref().unwrap().contains(secret),
            "the secret leaked through tool.input: {:?}",
            tool.input
        );
        assert_eq!(tool.paths, vec!["/home/alice/.aws/credentials".to_string()]);
    }

    // --- idempotency, end to end ---------------------------------------------

    #[test]
    fn re_importing_the_same_database_writes_nothing_the_second_time() {
        let fx = Fixture::build();
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("hermes.ledger");
        let spool = dir.path().join("spool");

        let first = {
            let mut ledger = Ledger::load(ledger_path.clone()).unwrap();
            let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, false);
            read_into(&fx.db, &identity(), None, &mut s).unwrap();
            s.into_report()
        };
        assert!(first.events_written > 0);
        let alpha = spool.join("hermes").join("sess-alpha.ndjson");
        let before = std::fs::read_to_string(&alpha).unwrap();

        let second = {
            let mut ledger = Ledger::load(ledger_path).unwrap();
            let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, false);
            read_into(&fx.db, &identity(), None, &mut s).unwrap();
            s.into_report()
        };
        assert_eq!(second.events_written, 0, "a second import must add nothing");
        assert_eq!(second.events_deduplicated, first.events_written);
        assert_eq!(
            std::fs::read_to_string(&alpha).unwrap(),
            before,
            "the spool file must be byte-identical after a re-import"
        );
    }

    #[test]
    fn two_reads_of_the_same_row_produce_the_same_dedup_key() {
        // The property the ledger depends on: nothing in an envelope's identity may
        // vary between runs (the ULID `event_id` does, which is why it is excluded).
        let fx = Fixture::build();
        let (_, a) = fx.import();
        let (_, b) = fx.import();
        let keys_a: Vec<String> = a.session("sess-alpha").iter().map(dedup_key).collect();
        let keys_b: Vec<String> = b.session("sess-alpha").iter().map(dedup_key).collect();
        assert_eq!(keys_a, keys_b);
        let unique: BTreeSet<&String> = keys_a.iter().collect();
        assert_eq!(
            unique.len(),
            keys_a.len(),
            "keys must not collide in-session"
        );
    }

    #[test]
    fn the_spooled_lines_parse_back_into_envelopes() {
        // What `ctxlake sync` will actually read. A line that does not round-trip is
        // a session the daemon silently cannot upload.
        let fx = Fixture::build();
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path().join("l")).unwrap();
        let spool = dir.path().join("spool");
        let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, false);
        read_into(&fx.db, &identity(), None, &mut s).unwrap();

        let text = std::fs::read_to_string(spool.join("hermes").join("sess-alpha.ndjson")).unwrap();
        assert_eq!(text.lines().count(), 8);
        for line in text.lines() {
            let env: Envelope = serde_json::from_str(line).expect("a valid envelope line");
            assert!(env.imported);
            assert_eq!(env.runtime, Runtime::Hermes);
        }
    }
}
