//! `ctxlake sessions` — inspecting Tier 0, the structural half of memory.
//!
//! **Why this exists.** Every other tier had a way in and this one did not. `ctxlake
//! claims` reads the belief layer; nothing at all read the session digests, even though
//! they are the half that needs no model, cannot hallucinate, and feeds the "recent
//! sessions" block of every briefing. Grooming memory without being able to see what a
//! session actually recorded means grooming the conclusions without the evidence.
//!
//! Reads the same local snapshot the agent reads, deliberately. What this prints is
//! what a peer would be told — not a privileged view of the lake — so "why did an agent
//! see that" and "what does `ctxlake sessions` say" cannot drift apart.

use anyhow::Result;

use crate::config::Config;

/// Rows shown by the list view before it says there are more.
const DEFAULT_LIMIT: usize = 20;

/// One session as the snapshot records it.
struct Row {
    session_id: String,
    agent_id: String,
    repo: Option<String>,
    branch: Option<String>,
    ended_at: Option<String>,
    duration_ms: Option<i64>,
    turn_count: i64,
    outcome: String,
    summary: String,
    files: Vec<String>,
    commands: Vec<ctxlake_maint::digest::CommandRun>,
    friction: Vec<ctxlake_maint::digest::Friction>,
    input_tokens: i64,
    output_tokens: i64,
    cost_usd: f64,
}

/// Decode one of the digest's JSON columns into the type `ctxlake-maint` wrote it
/// from. Deliberately typed rather than `Value`: `Friction` already carries the exact
/// briefing phrasing in `headline()`, and re-deriving that here would be a second
/// renderer to keep in step with the first.
///
/// A column this reader cannot parse yields an empty list rather than an error — a
/// digest written by a newer ctxlake must not make `sessions` unusable on an older one.
fn parse_arr<T: serde::de::DeserializeOwned>(raw: &str) -> Vec<T> {
    serde_json::from_str::<Vec<T>>(raw).unwrap_or_default()
}

fn open(cfg: &Config) -> Option<rusqlite::Connection> {
    ctxlake_mcp::snapshot::open(&ctxlake_core::paths::cache_root(), &cfg.fleet_id)
}

fn load(conn: &rusqlite::Connection, one: Option<&str>) -> Result<Vec<Row>> {
    let sql = "SELECT session_id, agent_id, repo, branch, ended_at, duration_ms, turn_count, \
               outcome, summary, files_json, commands_json, friction_json, input_tokens, \
               output_tokens, cost_usd FROM sessions \
               WHERE (?1 IS NULL OR session_id LIKE ?1 || '%') \
               ORDER BY ended_at DESC";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(rusqlite::params![one], |r| {
            Ok(Row {
                session_id: r.get(0)?,
                agent_id: r.get(1)?,
                repo: r.get(2)?,
                branch: r.get(3)?,
                ended_at: r.get(4)?,
                duration_ms: r.get(5)?,
                turn_count: r.get(6)?,
                outcome: r.get(7)?,
                summary: r.get(8)?,
                files: parse_arr::<String>(&r.get::<_, String>(9)?),
                commands: parse_arr(&r.get::<_, String>(10)?),
                friction: parse_arr(&r.get::<_, String>(11)?),
                input_tokens: r.get(12)?,
                output_tokens: r.get(13)?,
                cost_usd: r.get(14)?,
            })
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    Ok(rows)
}

/// `ctxlake sessions [<id-prefix>]`.
pub async fn run(cfg: &Config, session: Option<&str>, limit: Option<usize>) -> Result<()> {
    let Some(conn) = open(cfg) else {
        println!(
            "no snapshot synced locally yet for fleet {}.\n\
             Run `ctxlake maint --once`, or wait for the sync daemon's next cache refresh.",
            cfg.fleet_id
        );
        return Ok(());
    };
    let rows = load(&conn, session)?;
    if rows.is_empty() {
        match session {
            Some(s) => println!("no session matching {s:?} in this fleet's snapshot."),
            None => println!("no sessions in this fleet's snapshot yet."),
        }
        return Ok(());
    }
    // A single match is shown in full, whether or not an exact id was given: asking for
    // one session and getting a one-line summary of it would be the wrong answer.
    if rows.len() == 1 {
        return detail(&conn, &rows[0]);
    }
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    println!("{} session(s) in fleet {}\n", rows.len(), cfg.fleet_id);
    for r in rows.iter().take(limit) {
        let when = r.ended_at.as_deref().unwrap_or("?");
        let short = &r.session_id[..8.min(r.session_id.len())];
        println!(
            "  {short}  {:<16} {:<28} {}",
            r.agent_id,
            r.repo.as_deref().unwrap_or("?"),
            &when[..19.min(when.len())]
        );
        println!("            {}", r.summary);
    }
    if rows.len() > limit {
        println!("\n  ... {} more. `--limit N` for more.", rows.len() - limit);
    }
    println!("\n  `ctxlake sessions <id-prefix>` for one session in full.");
    Ok(())
}

/// Everything Tier 0 recorded for one session.
fn detail(conn: &rusqlite::Connection, r: &Row) -> Result<()> {
    println!("session   {}", r.session_id);
    println!("agent     {}", r.agent_id);
    println!("repo      {}", r.repo.as_deref().unwrap_or("(unknown)"));
    if let Some(b) = &r.branch {
        println!("branch    {b}");
    }
    println!("ended     {}", r.ended_at.as_deref().unwrap_or("?"));
    if let Some(ms) = r.duration_ms {
        println!("duration  {}m {}s", ms / 60_000, (ms % 60_000) / 1000);
    }
    println!("turns     {}", r.turn_count);
    println!("outcome   {}", r.outcome);
    if r.input_tokens + r.output_tokens > 0 {
        println!(
            "tokens    {} in / {} out{}",
            r.input_tokens,
            r.output_tokens,
            if r.cost_usd > 0.0 {
                format!("  (${:.4})", r.cost_usd)
            } else {
                String::new()
            }
        );
    }

    // Friction first: `docs/memory.md` calls it the most useful line in a briefing, and
    // it is the reason to open a session rather than read its one-line summary.
    if !r.friction.is_empty() {
        println!("\nfriction");
        for f in &r.friction {
            println!("  {}", one_line(&f.headline()));
        }
    }
    if !r.files.is_empty() {
        println!("\nfiles touched ({})", r.files.len());
        for f in &r.files {
            println!("  {f}");
        }
    }
    if !r.commands.is_empty() {
        println!("\ncommands ({})", r.commands.len());
        for c in &r.commands {
            // `?` is not cosmetic: a session captured before transcript enrichment has
            // no exit codes at all, and showing those as "ok" would invent a success.
            let mark = match c.exit_code {
                Some(0) => " ok ",
                Some(_) => "FAIL",
                None => "  ? ",
            };
            println!("  [{mark}] {}", one_line(&c.command));
        }
    }

    // The join that makes this a grooming tool rather than a log viewer: from the raw
    // evidence to what the fleet concluded from it, and back. `evidence_json` is the
    // claim's own record of which sessions it rests on, so this asks the claim, not a
    // heuristic.
    match claims_citing(conn, &r.session_id) {
        Ok(claims) if !claims.is_empty() => {
            println!("\nclaims resting on this session ({})", claims.len());
            for (id, ty, status, text) in &claims {
                println!(
                    "  {} [{ty}/{status}] {}",
                    &id[..crate::claims::ID_DISPLAY_LEN.min(id.len())],
                    one_line(text)
                );
            }
            println!("\n  Wrong? `ctxlake claims --retire <id> --reason \"...\"`.");
        }
        _ => println!(
            "\nno claim cites this session — Tier 0 recorded it, Tier 2 concluded nothing from it."
        ),
    }
    Ok(())
}

/// Promoted or contested claims whose evidence names this session.
///
/// Matches on the session id inside `evidence_json` rather than parsing every row:
/// ids are ULID/UUID-shaped and long enough that a substring hit is the claim, and
/// the alternative is decoding the evidence of every claim in the fleet to find a
/// handful.
fn claims_citing(
    conn: &rusqlite::Connection,
    session_id: &str,
) -> Result<Vec<(String, String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT claim_id, claim_type, status, claim FROM claims \
         WHERE evidence_json LIKE '%' || ?1 || '%' ORDER BY status, claim_id",
    )?;
    let rows = stmt
        .query_map([session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(std::result::Result::ok)
        .collect();
    Ok(rows)
}

/// Collapse to one line and bound it — command text is agent-authored and can be a
/// whole heredoc, which would bury the rest of the report.
fn one_line(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= 140 {
        return flat.to_string();
    }
    format!("{}…", flat.chars().take(139).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_mcp::snapshot::test_support::{write_snapshot_with, FixtureClaim, FixtureSession};

    /// Build a snapshot at the path `open()` would look in, and open it the same way.
    fn fixture(sessions: &[FixtureSession], claims: &[FixtureClaim]) -> rusqlite::Connection {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("myteam").join("snapshot.bin");
        write_snapshot_with(&path, claims, sessions);
        let conn = rusqlite::Connection::open(&path).unwrap();
        // The tempdir must outlive the connection; SQLite holds the file open, and on
        // macOS dropping the dir first has the connection reading a deleted inode.
        std::mem::forget(dir);
        conn
    }

    fn session(id: &'static str) -> FixtureSession {
        FixtureSession {
            files_json: r#"["src/main.rs","deploy/tls.md"]"#,
            commands_json: r#"[{"tool":"Bash","command":"cargo test","exit_code":1,"is_test":true},
                               {"tool":"Bash","command":"cargo test","exit_code":0,"is_test":true}]"#,
            friction_json: r#"[{"kind":"repeated_failure","command":"cargo test","count":2}]"#,
            ..FixtureSession::new(id, "fixed the TLS default")
        }
    }

    #[test]
    fn a_prefix_finds_the_session_it_names() {
        let conn = fixture(&[session("abc12345-0000-0000-0000-000000000000")], &[]);
        let rows = load(&conn, Some("abc123")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "fixed the TLS default");
    }

    /// The columns are JSON text in SQLite; if this reader decoded them into the wrong
    /// shape the command would silently print an empty session for a busy one — which
    /// is exactly how `tool_result` stayed broken for a month.
    #[test]
    fn the_digests_json_columns_decode_into_what_maint_wrote() {
        let conn = fixture(&[session("abc12345-0000-0000-0000-000000000000")], &[]);
        let r = &load(&conn, None).unwrap()[0];
        assert_eq!(r.files, vec!["src/main.rs", "deploy/tls.md"]);
        assert_eq!(r.commands.len(), 2);
        assert_eq!(r.commands[0].exit_code, Some(1));
        assert_eq!(r.commands[1].exit_code, Some(0));
        assert_eq!(r.friction.len(), 1);
        assert_eq!(r.friction[0].headline(), "`cargo test` failed 2 times");
    }

    /// A digest written by a newer ctxlake must not make `sessions` unusable here.
    #[test]
    fn an_unreadable_column_costs_that_column_and_nothing_else() {
        let conn = fixture(
            &[FixtureSession {
                commands_json: r#"[{"shape":"from a future version"}]"#,
                ..session("abc12345-0000-0000-0000-000000000000")
            }],
            &[],
        );
        let r = &load(&conn, None).unwrap()[0];
        assert!(r.commands.is_empty(), "undecodable column yields nothing");
        assert_eq!(r.files.len(), 2, "the other columns still decode");
    }

    /// The join from raw evidence to what the fleet concluded — the reason this is a
    /// grooming tool and not a log viewer.
    #[test]
    fn a_session_reports_the_claims_that_rest_on_it() {
        let sid = "abc12345-0000-0000-0000-000000000000";
        let conn = fixture(
            &[session(sid)],
            &[
                FixtureClaim {
                    evidence_json: r#"[{"session_id":"abc12345-0000-0000-0000-000000000000"}]"#,
                    ..FixtureClaim::promoted("01AAA", "tls defaults to off", "convention", "tls")
                },
                FixtureClaim {
                    evidence_json: r#"[{"session_id":"99999999-0000-0000-0000-000000000000"}]"#,
                    ..FixtureClaim::promoted("01BBB", "unrelated", "convention", "other")
                },
            ],
        );
        let found = claims_citing(&conn, sid).unwrap();
        assert_eq!(found.len(), 1, "only the claim citing this session");
        assert_eq!(found[0].0, "01AAA");
    }

    #[test]
    fn a_session_no_claim_cites_reports_none_rather_than_all() {
        let conn = fixture(
            &[session("abc12345-0000-0000-0000-000000000000")],
            &[FixtureClaim::promoted(
                "01BBB",
                "unrelated",
                "convention",
                "other",
            )],
        );
        let found = claims_citing(&conn, "abc12345-0000-0000-0000-000000000000").unwrap();
        assert!(found.is_empty());
    }

    /// Command text is agent-authored and can be a whole heredoc.
    #[test]
    fn a_long_command_is_bounded_and_a_short_one_is_untouched() {
        assert_eq!(one_line("cargo test"), "cargo test");
        assert_eq!(one_line("a\nb\tc"), "a b c");
        let long = "x".repeat(500);
        let out = one_line(&long);
        assert_eq!(out.chars().count(), 140, "139 chars plus the ellipsis");
        assert!(out.ends_with('…'));
    }
}
