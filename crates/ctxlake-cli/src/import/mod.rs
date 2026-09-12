//! `ctxlake import` — backfilling the history a runtime already has on disk. See
//! [`docs/import.md`](../../../../docs/import.md).
//!
//! Import is not a second capture pipeline. It builds exactly the same
//! [`ctxlake_core::Envelope`] the live hook builds, runs it through exactly the same
//! [`ctxlake_core::Redactor`], and appends it to exactly the same spool
//! (`ctxlake_hook::spool`) that `ctxlake sync` already drains to the store. That is
//! AGENTS.md invariant 7 taken literally — *"redaction runs before the spool, on every
//! path including import"* — and the only way to honour it that does not end in two
//! implementations of a security control drifting apart. The one field that
//! distinguishes an imported event from a captured one is `Envelope::imported`, which
//! every envelope this module emits carries.
//!
//! ## Structure
//!
//! One submodule per source, each responsible for exactly one thing: turning a
//! runtime's own on-disk format into envelopes, in chronological order, with
//! redaction already applied. This module owns everything that is *not*
//! source-specific — the `--since` cutoff, the dedup ledger, the spool writer, and
//! the report. A new source implements [`EnvelopeSink`]-shaped streaming and nothing
//! else.
//!
//! **Only Hermes is implemented today.** `docs/import.md` describes the per-runtime
//! *fidelity of the data on disk*, which is a property of those runtimes and true
//! regardless; the Claude Code and Cursor readers are not written yet, and this
//! module says so out loud rather than accepting the flag and quietly importing
//! nothing (the exact failure mode AGENTS.md's "how these were found" section is
//! about).
//!
//! ## Chronological order is load-bearing
//!
//! `ctxlake_maint::digest::compute` sorts a session's envelopes by `event_id` and
//! derives duration, turn count and the trailing-failure streak from that order.
//! `event_id` is a monotonic ULID *per generating thread*
//! (`ctxlake_core::envelope::next_event_id`), so envelopes minted in the order the
//! events actually happened sort back into that order — and envelopes minted in any
//! other order are silently wrong in a way no test of a single envelope can see.
//! Every source here must therefore call `Envelope::new` in chronological order.

pub mod hermes;

use std::collections::HashSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use ctxlake_core::envelope::Envelope;
use time::{Duration, OffsetDateTime};

use crate::config::Config;
use crate::hooks::Runtime as HookRuntime;
use crate::paths;

/// What one source's run did. Every field is a count of something that either
/// happened or was deliberately not done — a silent skip with no counter is how an
/// importer reports success while importing nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub runtime: String,
    pub source: String,
    /// Whatever the source's own schema-version marker said, verbatim. Reported
    /// rather than compared against a hardcoded number: this importer was verified
    /// against one live install, and a version string we have never seen is not by
    /// itself evidence of anything. The column check is the real gate.
    pub schema_version: Option<String>,
    pub sessions_seen: usize,
    pub sessions_imported: usize,
    pub sessions_skipped_by_since: usize,
    pub sessions_empty: usize,
    pub events_built: usize,
    pub events_written: usize,
    pub events_deduplicated: usize,
    /// Rows whose `role` this importer has no envelope mapping for. Counted, not
    /// silently dropped — a nonzero value here means a Hermes upgrade started
    /// writing a role we do not carry, which is a thing an operator should see.
    pub rows_skipped_unknown_role: usize,
    pub events_redacted: usize,
    pub events_quarantined: usize,
    /// How many events the dedup ledger knows about after this run. Printed so a
    /// re-import that legitimately does nothing is distinguishable from one that
    /// found nothing to do.
    pub ledger_entries: usize,
}

impl ImportReport {
    pub fn print(&self, dry_run: bool) {
        let verb = if dry_run { "would import" } else { "imported" };
        println!("{}: {verb} from {}", self.runtime, self.source);
        if let Some(v) = &self.schema_version {
            println!("  source schema_version: {v}");
        }
        println!(
            "  sessions: {} seen, {} {verb}, {} outside --since, {} with no messages",
            self.sessions_seen,
            self.sessions_imported,
            self.sessions_skipped_by_since,
            self.sessions_empty
        );
        println!(
            "  events:   {} built, {} {verb}, {} already imported (content-hash dedup)",
            self.events_built, self.events_written, self.events_deduplicated
        );
        println!(
            "  redaction: {} redacted, {} quarantined",
            self.events_redacted, self.events_quarantined
        );
        println!("  ledger:   {} event(s) known", self.ledger_entries);
        if self.rows_skipped_unknown_role > 0 {
            println!(
                "  warning: {} message row(s) had a role this importer does not map — \
                 see docs/import.md",
                self.rows_skipped_unknown_role
            );
        }
    }
}

/// Where a source hands finished, already-redacted envelopes for one session.
///
/// Streaming per session rather than returning one big `Vec` is not a micro-
/// optimization: a real Hermes install holds tens of thousands of messages, and the
/// point of the spool is that nothing has to hold a whole history in memory to move
/// it.
pub trait EnvelopeSink {
    fn accept(&mut self, session_id: &str, envelopes: &[Envelope]) -> Result<()>;
}

/// The persistent record of what import has already emitted.
///
/// **Why a ledger and not "read what's already in the lake".** Reading bronze back to
/// decide what to skip would put the object store on import's read path for every
/// event, and the spool this writes into is drained and *deleted* by `ctxlake sync`
/// the moment an upload succeeds — so there is nothing local left to compare against
/// afterwards either. A tiny append-only file of hashes is the cheapest thing that
/// survives both.
#[derive(Debug)]
pub struct Ledger {
    path: PathBuf,
    seen: HashSet<String>,
}

impl Ledger {
    /// Load the ledger at `path`. A missing file is an empty ledger, not an error —
    /// that is simply the first import on this host.
    pub fn load(path: PathBuf) -> Result<Self> {
        let seen = match fs::read_to_string(&path) {
            Ok(text) => text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, seen })
    }

    pub fn contains(&self, key: &str) -> bool {
        self.seen.contains(key)
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Only the tests call this today. It stays because clippy's
    /// `len_without_is_empty` requires the pair on a public `len`, and because a
    /// length-only API invites `len() == 0` at future call sites.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Append `keys` to the on-disk ledger and remember them.
    ///
    /// Called once per session, after that session's events are on the spool, which
    /// is what makes an interrupted run resumable at session granularity: a crash
    /// mid-session replays that session (and dedups within it on the next pass),
    /// never the whole history.
    pub fn record(&mut self, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening ledger {}", self.path.display()))?;
        let mut buf = String::with_capacity(keys.len() * 80);
        for k in keys {
            buf.push_str(k);
            buf.push('\n');
        }
        f.write_all(buf.as_bytes())
            .with_context(|| format!("appending to ledger {}", self.path.display()))?;
        self.seen.extend(keys.iter().cloned());
        Ok(())
    }
}

/// The idempotency key for one imported event.
///
/// It is a `content_hash` — the same `sha256:` function, over the same post-redaction
/// content — but deliberately **not** the bare `Envelope::content_hash`. Two real,
/// distinct events routinely share content: `ls` run twice, two tool calls that both
/// returned `ok`, two `SessionStart`s with no content at all. Deduping on content
/// alone would silently drop the second one and leave a transcript with holes in it,
/// which is a far worse outcome than the duplicate it prevents.
///
/// So the key hashes the event's *identity*: runtime, session, kind, instant, the
/// source's own row id (`message_id`), the post-redaction content hash, and the tool
/// input hash. Fields are NUL-separated for the reason `hash::resource_key` gives —
/// a plain concatenation makes `("ab","c")` and `("a","bc")` the same key.
pub fn dedup_key(env: &Envelope) -> String {
    let event_type = serde_json::to_string(&env.event_type).unwrap_or_default();
    let input_hash = env
        .tool
        .as_ref()
        .map(|t| t.input_hash.as_str())
        .unwrap_or("");
    ctxlake_core::hash::content_hash(format!(
        "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
        env.runtime.as_str(),
        env.session_id,
        event_type,
        env.emitted_at,
        env.message_id.as_deref().unwrap_or(""),
        env.content_hash,
        input_hash,
    ))
}

/// Writes imported envelopes to the same spool the live hook appends to, skipping
/// anything the ledger has already seen.
pub struct Spooler<'a> {
    spool_root: PathBuf,
    runtime_dir: &'static str,
    ledger: &'a mut Ledger,
    dry_run: bool,
    report: ImportReport,
}

impl<'a> Spooler<'a> {
    pub fn new(
        spool_root: PathBuf,
        runtime: ctxlake_core::Runtime,
        ledger: &'a mut Ledger,
        dry_run: bool,
    ) -> Self {
        Self {
            spool_root,
            runtime_dir: runtime.as_str(),
            ledger,
            dry_run,
            report: ImportReport::default(),
        }
    }

    pub fn into_report(self) -> ImportReport {
        self.report
    }
}

impl EnvelopeSink for Spooler<'_> {
    fn accept(&mut self, session_id: &str, envelopes: &[Envelope]) -> Result<()> {
        let mut fresh: Vec<String> = Vec::new();
        let mut fresh_set: HashSet<String> = HashSet::new();
        self.report.events_built += envelopes.len();

        for env in envelopes {
            let key = dedup_key(env);
            if self.ledger.contains(&key) || fresh_set.contains(&key) {
                self.report.events_deduplicated += 1;
                continue;
            }
            if !self.dry_run {
                let line = env
                    .to_ndjson()
                    .with_context(|| format!("serializing an event of session {session_id}"))?;
                ctxlake_hook::spool::append_event_at(
                    &self.spool_root,
                    self.runtime_dir,
                    session_id,
                    &line,
                )
                .map_err(|e| anyhow!("spooling session {session_id}: {e}"))?;
            }
            match env.redaction.status.as_str() {
                "redacted" => self.report.events_redacted += 1,
                "quarantined" => self.report.events_quarantined += 1,
                _ => {}
            }
            fresh_set.insert(key.clone());
            fresh.push(key);
            self.report.events_written += 1;
        }

        if !fresh.is_empty() {
            self.report.sessions_imported += 1;
            if !self.dry_run {
                // The `.done` sentinel tells `ctxlake sync` this session's file is
                // safe to seal. An imported session is complete by definition —
                // nothing will ever append to it again — so unlike live capture
                // there is no waiting for a session-end event that may never come.
                ctxlake_hook::spool::mark_session_done_at(
                    &self.spool_root,
                    self.runtime_dir,
                    session_id,
                )
                .map_err(|e| anyhow!("marking session {session_id} done: {e}"))?;
                // Recorded only after the events are actually on disk: a ledger
                // entry written before its event would make a crash here look like a
                // completed import and lose the session permanently.
                self.ledger.record(&fresh)?;
            }
        }
        Ok(())
    }
}

/// Parse a `--since` value into the instant it names.
///
/// Accepts a relative window (`90d`, `36h`, `45m`, `30s`) or an absolute point
/// (`2026-01-01`, or full RFC 3339). `now` is a parameter rather than read inside so
/// the relative arithmetic is testable without a clock.
pub fn parse_since(spec: &str, now: OffsetDateTime) -> Result<OffsetDateTime> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("--since needs a value, e.g. 90d or 2026-01-01");
    }

    let (digits, unit) = spec.split_at(spec.len() - 1);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        let n: i64 = digits
            .parse()
            .with_context(|| format!("--since {spec}: window too large"))?;
        let window = match unit {
            "d" => Some(Duration::days(n)),
            "h" => Some(Duration::hours(n)),
            "m" => Some(Duration::minutes(n)),
            "s" => Some(Duration::seconds(n)),
            _ => None,
        };
        if let Some(w) = window {
            return now
                .checked_sub(w)
                .ok_or_else(|| anyhow!("--since {spec}: window reaches before the year 0"));
        }
    }

    // `date` alone is the form people actually type; accept it by completing it to
    // midnight UTC rather than rejecting it with an RFC 3339 lecture.
    let candidate = if spec.len() == "2026-01-01".len() && spec.matches('-').count() == 2 {
        format!("{spec}T00:00:00Z")
    } else {
        spec.to_string()
    };
    OffsetDateTime::parse(&candidate, &time::format_description::well_known::Rfc3339).map_err(
        |_| {
            anyhow!(
                "--since {spec:?} is not a window (90d, 36h, 45m, 30s) or a date \
                 (2026-01-01, or full RFC 3339)"
            )
        },
    )
}

/// The arguments `main.rs` collects for `ctxlake import`.
pub struct ImportRequest {
    pub runtime: Option<HookRuntime>,
    pub all: bool,
    pub since: Option<String>,
    pub dry_run: bool,
    /// Read this file instead of the runtime's default location. Exists for
    /// installs that moved their state and for exercising the importer against a
    /// fixture; it never changes *how* the source is read.
    pub source: Option<PathBuf>,
}

pub fn run(cfg: &Config, req: ImportRequest) -> Result<()> {
    let targets: Vec<HookRuntime> = match (req.runtime, req.all) {
        (Some(_), true) => bail!("pass a runtime or --all, not both"),
        (Some(r), false) => vec![r],
        (None, true) => HookRuntime::ALL.to_vec(),
        (None, false) => bail!("pass a runtime (claude-code, cursor, hermes) or --all"),
    };
    if req.source.is_some() && targets.len() > 1 {
        bail!("--source names one runtime's state, so pass --runtime with it");
    }

    let since = req
        .since
        .as_deref()
        .map(|s| parse_since(s, OffsetDateTime::now_utc()))
        .transpose()?;

    let identity = hermes::ImportIdentity {
        fleet_id: cfg.fleet_id.clone(),
        agent_id: cfg.agent_id.clone(),
        host_id: ctxlake_core::hash::host_id(&hostname()),
    };

    let mut any_error = false;
    for runtime in targets {
        let outcome = match runtime {
            HookRuntime::Hermes => {
                let db = req
                    .source
                    .clone()
                    .unwrap_or_else(paths::hermes_state_db_path);
                import_hermes(&db, &identity, since, req.dry_run)
            }
            // Refused, not silently skipped. `docs/import.md` describes what these
            // runtimes' on-disk history *contains*; no reader for it is written yet,
            // and an importer that accepts the flag and reports zero events is
            // exactly the silent failure AGENTS.md's "how these were found" section
            // is about.
            other => Err(anyhow!(
                "no import source is implemented for {other} yet — only `--runtime hermes` \
                 reads history today (see docs/import.md)"
            )),
        };
        match outcome {
            Ok(report) => report.print(req.dry_run),
            Err(e) => {
                // With `--all` a missing runtime is normal, not a failure: most
                // hosts run one or two of the three.
                if req.all {
                    println!("{runtime}: skipped — {e:#}");
                } else {
                    eprintln!("{runtime}: {e:#}");
                    any_error = true;
                }
            }
        }
    }
    if any_error {
        bail!("import failed — see above");
    }
    Ok(())
}

fn import_hermes(
    db: &Path,
    identity: &hermes::ImportIdentity,
    since: Option<OffsetDateTime>,
    dry_run: bool,
) -> Result<ImportReport> {
    let mut ledger = Ledger::load(paths::import_ledger_path(
        ctxlake_core::Runtime::Hermes.as_str(),
    ))?;
    let mut spooler = Spooler::new(
        paths::spool_root(),
        ctxlake_core::Runtime::Hermes,
        &mut ledger,
        dry_run,
    );
    let source = hermes::read_into(db, identity, since, &mut spooler)?;
    let mut report = spooler.into_report();
    report.runtime = HookRuntime::Hermes.name().to_string();
    report.source = source.source;
    report.schema_version = source.schema_version;
    report.sessions_seen = source.sessions_seen;
    report.sessions_skipped_by_since = source.sessions_skipped_by_since;
    report.sessions_empty = source.sessions_empty;
    report.rows_skipped_unknown_role = source.rows_skipped_unknown_role;
    report.ledger_entries = ledger.len();
    Ok(report)
}

/// Best-effort hostname, hashed before it ever reaches an envelope — mirrors
/// `ctxlake_hook::hostinfo::hostname`, which this crate cannot call directly because
/// that function is private to the hook's own module.
fn hostname() -> String {
    for candidate in [
        std::env::var("HOSTNAME").ok(),
        std::env::var("COMPUTERNAME").ok(),
        fs::read_to_string("/etc/hostname").ok(),
    ]
    .into_iter()
    .flatten()
    {
        let trimmed = candidate.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "unknown-host".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctxlake_core::envelope::{EventType, ToolCall};
    use ctxlake_core::Runtime;

    fn env_at(session: &str, at: &str, content: Option<&str>) -> Envelope {
        let mut e = Envelope::new(
            "myteam",
            "herm-01",
            Runtime::Hermes,
            session,
            EventType::Prompt,
            at,
        );
        e.content = content.map(str::to_string);
        e.content_hash = ctxlake_core::hash::content_hash(content.unwrap_or(""));
        e.imported = true;
        e
    }

    #[test]
    fn parse_since_accepts_relative_windows() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        assert_eq!(
            parse_since("90d", now).unwrap(),
            now - Duration::days(90),
            "90d"
        );
        assert_eq!(parse_since("36h", now).unwrap(), now - Duration::hours(36));
        assert_eq!(
            parse_since("45m", now).unwrap(),
            now - Duration::minutes(45)
        );
        assert_eq!(
            parse_since("30s", now).unwrap(),
            now - Duration::seconds(30)
        );
        assert_eq!(parse_since(" 7d ", now).unwrap(), now - Duration::days(7));
    }

    #[test]
    fn parse_since_accepts_a_bare_date_and_full_rfc3339() {
        let now = OffsetDateTime::now_utc();
        let midnight = parse_since("2026-01-01", now).unwrap();
        assert_eq!(midnight.year(), 2026);
        assert_eq!(midnight.hour(), 0);
        let exact = parse_since("2026-01-01T12:30:00Z", now).unwrap();
        assert_eq!(exact.hour(), 12);
    }

    #[test]
    fn parse_since_rejects_a_value_it_cannot_understand() {
        let now = OffsetDateTime::now_utc();
        for bad in ["", "ninety days", "90x", "d", "2026-13-45"] {
            assert!(
                parse_since(bad, now).is_err(),
                "should have refused {bad:?} rather than guessing a window"
            );
        }
    }

    #[test]
    fn dedup_key_distinguishes_two_identical_messages_in_one_session() {
        // The failure this guards: deduping on `content_hash` alone drops the second
        // of two genuinely distinct events that happen to say the same thing, and
        // the imported transcript quietly has a hole in it.
        let mut a = env_at("s1", "2026-01-01T00:00:00.000Z", Some("ok"));
        a.message_id = Some("11".into());
        let mut b = env_at("s1", "2026-01-01T00:00:05.000Z", Some("ok"));
        b.message_id = Some("12".into());
        assert_eq!(a.content_hash, b.content_hash, "premise: same content");
        assert_ne!(dedup_key(&a), dedup_key(&b));
    }

    #[test]
    fn dedup_key_is_stable_across_two_runs_of_the_same_row() {
        // Two separately-constructed envelopes for the same source row get different
        // `event_id`s (a fresh ULID each time), so the key must not depend on one.
        let mut a = env_at("s1", "2026-01-01T00:00:00.000Z", Some("hello"));
        a.message_id = Some("11".into());
        let mut b = env_at("s1", "2026-01-01T00:00:00.000Z", Some("hello"));
        b.message_id = Some("11".into());
        assert_ne!(a.event_id, b.event_id, "premise: fresh ULIDs");
        assert_eq!(dedup_key(&a), dedup_key(&b));
    }

    #[test]
    fn dedup_key_separates_two_tool_calls_that_differ_only_in_input() {
        let mut a = env_at("s1", "2026-01-01T00:00:00.000Z", None);
        a.event_type = EventType::ToolCall;
        a.message_id = Some("7".into());
        a.tool = Some(ToolCall {
            name: "terminal".into(),
            input_hash: ctxlake_core::hash::content_hash(r#"{"command":"ls"}"#),
            ..Default::default()
        });
        let mut b = a.clone();
        b.tool.as_mut().unwrap().input_hash =
            ctxlake_core::hash::content_hash(r#"{"command":"pwd"}"#);
        assert_ne!(dedup_key(&a), dedup_key(&b));
    }

    #[test]
    fn ledger_round_trips_and_a_missing_file_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("hermes.ledger");
        let mut ledger = Ledger::load(path.clone()).unwrap();
        assert!(ledger.is_empty());
        ledger.record(&["a".to_string(), "b".to_string()]).unwrap();
        assert!(ledger.contains("a"));

        let reloaded = Ledger::load(path).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains("b"));
    }

    #[test]
    fn spooler_writes_one_ndjson_line_per_event_and_marks_the_session_done() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("hermes.ledger");
        let mut ledger = Ledger::load(ledger_path).unwrap();
        let spool = dir.path().join("spool");
        let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, false);

        let events = vec![
            env_at("s1", "2026-01-01T00:00:00.000Z", Some("one")),
            env_at("s1", "2026-01-01T00:00:01.000Z", Some("two")),
        ];
        s.accept("s1", &events).unwrap();
        let report = s.into_report();

        assert_eq!(report.events_written, 2);
        assert_eq!(report.events_deduplicated, 0);
        assert_eq!(report.sessions_imported, 1);
        let file = spool.join("hermes").join("s1.ndjson");
        let text = fs::read_to_string(&file).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(spool.join("hermes").join("s1.done").exists());
    }

    #[test]
    fn re_running_the_same_import_writes_nothing_the_second_time() {
        // The idempotency contract docs/import.md promises, at this layer: the
        // source produces the same envelopes again and every one of them is already
        // in the ledger.
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("hermes.ledger");
        let spool = dir.path().join("spool");
        let events = vec![
            env_at("s1", "2026-01-01T00:00:00.000Z", Some("one")),
            env_at("s1", "2026-01-01T00:00:01.000Z", Some("two")),
        ];

        {
            let mut ledger = Ledger::load(ledger_path.clone()).unwrap();
            let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, false);
            s.accept("s1", &events).unwrap();
        }
        let before = fs::read_to_string(spool.join("hermes").join("s1.ndjson")).unwrap();

        let mut ledger = Ledger::load(ledger_path).unwrap();
        let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, false);
        s.accept("s1", &events).unwrap();
        let report = s.into_report();

        assert_eq!(report.events_written, 0, "a re-import must write nothing");
        assert_eq!(report.events_deduplicated, 2);
        assert_eq!(
            fs::read_to_string(spool.join("hermes").join("s1.ndjson")).unwrap(),
            before,
            "the spool file must be byte-identical after a re-import"
        );
    }

    #[test]
    fn a_dry_run_writes_neither_the_spool_nor_the_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("hermes.ledger");
        let spool = dir.path().join("spool");
        let mut ledger = Ledger::load(ledger_path.clone()).unwrap();
        let mut s = Spooler::new(spool.clone(), Runtime::Hermes, &mut ledger, true);
        s.accept("s1", &[env_at("s1", "2026-01-01T00:00:00.000Z", Some("x"))])
            .unwrap();
        let report = s.into_report();

        assert_eq!(report.events_written, 1, "--dry-run still counts");
        assert!(!spool.exists(), "--dry-run must not touch the spool");
        assert!(!ledger_path.exists(), "--dry-run must not record a ledger");
    }

    #[test]
    fn redaction_status_is_counted_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = Ledger::load(dir.path().join("l")).unwrap();
        let mut s = Spooler::new(dir.path().join("spool"), Runtime::Hermes, &mut ledger, true);
        let mut clean = env_at("s1", "2026-01-01T00:00:00.000Z", Some("a"));
        clean.redaction.status = "clean".into();
        let mut redacted = env_at("s1", "2026-01-01T00:00:01.000Z", Some("b"));
        redacted.redaction.status = "redacted".into();
        let mut quarantined = env_at("s1", "2026-01-01T00:00:02.000Z", Some("c"));
        quarantined.redaction.status = "quarantined".into();
        s.accept("s1", &[clean, redacted, quarantined]).unwrap();
        let report = s.into_report();
        assert_eq!(report.events_redacted, 1);
        assert_eq!(report.events_quarantined, 1);
    }

    #[test]
    fn hostname_is_never_carried_raw_into_an_envelope() {
        // `envelope.rs`'s `host_id` field is explicit that bronze must not carry a
        // hostname. This is the import-side half of that promise.
        let h = hostname();
        let id = ctxlake_core::hash::host_id(&h);
        assert!(id.starts_with("sha256:"));
        assert!(!id.contains(&h) || h.is_empty());
    }
}
