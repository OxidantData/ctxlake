//! Read the published claim snapshot — the real belief store `memory_search`,
//! `memory_timeline`, and the briefing's fleet-context block all read from.
//!
//! **What this file is not allowed to be a way around.** AGENTS.md invariant 1
//! says the object store is never on this process's path, in either direction.
//! `ctxlake-maint`'s `snapshot::publish` writes `snapshot/<hash>.sqlite` to the
//! *object store*; this module never opens that. It only ever opens
//! `<cache_root>/<fleet_id>/snapshot.bin`, the byte-for-byte local mirror
//! `ctxlake-sync`'s `cache::refresh_snapshot` already produces (see that module's
//! doc: "this module treats the blob as opaque bytes and mirrors it
//! byte-for-byte"). Reading it is ordinary synchronous file I/O — the same
//! discipline `fleet.rs` and the old JSON-cache version of this module already
//! used, just against a SQLite file instead of a `.json` one.
//!
//! **The schema here is not imported, and that is deliberate — matching
//! `ctxlake-maint::snapshot`'s own module doc for the identical reason.** This
//! crate cannot depend on `ctxlake-maint` (it pulls `object_store` and `tokio`
//! transitively, which would reintroduce exactly the network boundary invariant 1
//! forbids for this process). So this module hand-mirrors the real
//! `SCHEMA_SQL`'s column names and types (`claims`, `claims_fts`, `visible_to_agents`,
//! a little-endian f32 `embedding` BLOB) rather than sharing a type. The two sides
//! agreeing is a wire-format contract, verified by `crates/ctxlake-maint/src/
//! snapshot.rs`'s own tests on the write side and this module's tests on the read
//! side — the same "match the real shape, don't guess at a simpler one" discipline
//! AGENTS.md's "hard-won facts" section applies to every runtime adapter in this
//! codebase.
//!
//! **Shadow mode is not this module's decision to make, and it structurally can't
//! be.** `visible_to_agents` is baked into the artifact at publish time by
//! `ctxlake-maint::snapshot::publish` — 1 exactly when a claim is `promoted` *and*
//! the publishing run was told agent reads are enabled, 0 in every other case
//! (candidate, contested, retired, or a promoted claim published under shadow
//! mode). Every query in this module filters on `visible_to_agents = 1`, and nothing
//! here has a way to set that column or override the filter — a fresh install (no
//! `snapshot.bin` at all) and a live-but-shadow-mode install (a `snapshot.bin` full
//! of claims, every one with `visible_to_agents = 0`) read as the identical "zero
//! claims" outcome, for the identical structural reason. Flipping a fleet from
//! shadow to live is entirely `ctxlake.toml`'s `[summarize] mode` and an operator's
//! decision — this module has no config of its own to flip.
//!
//! **The vector search is real cosine, over a real embedding column, but today's
//! embeddings are a documented stand-in.** `docs/memory.md`'s Tier 2 extraction
//! (`crates/ctxlake-maint/src/extract.rs`) has no `embed()` call on its `Provider`
//! trait yet — every claim proposed anywhere in this codebase today carries
//! `embedding: None`. Requiring a real semantic embedder before `memory_search`
//! could do anything beyond FTS5 would mean shipping no vector search at all until
//! a future wave adds one, and then re-deriving the exact brute-force-cosine
//! machinery this task calls for. Instead, [`hash_embedding`] is a deterministic,
//! dependency-free bag-of-words fingerprint (a hashing trick, the same family of
//! technique `gate.rs`'s own `lexical_overlap` is honest about being "a
//! deliberately minimal stand-in, not a search engine") used as the query vector
//! *and* as the fallback for any claim whose stored `embedding` is `NULL` — which,
//! today, is every claim, so both sides of every comparison sit in the same
//! well-defined space. The moment a real embedder starts populating the column,
//! this module already prefers the stored vector over the fallback for that row
//! (see [`ClaimRow::vector`]); the honest limitation, stated once here rather than
//! silently: a stored real embedding compared against a hash-derived query vector
//! would not be meaningfully comparable, and nothing forces the two to agree until
//! `memory_search` itself grows a matching query embedder. That is future work,
//! not a gap this module papers over.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Matches `ctxlake_maint::snapshot::EMBEDDING_DIMENSIONS` — kept as an
/// independent constant for the same reason that module keeps its own copy
/// instead of importing anything: the two sides agree on the wire format, not on
/// a shared Rust type. See the module doc.
pub const EMBEDDING_DIM: usize = 256;

/// One row of the `claims` table, filtered to `visible_to_agents = 1` by every
/// query in this module — see the module doc's shadow-mode paragraph. This is the
/// belief-layer equivalent of `memory::ClaimRecord`, with the additional fields
/// (`scope`, `claim_type`, embedding) [`super::memory`]'s search/ranking logic
/// needs that the render-only `ClaimRecord` does not.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimRow {
    pub claim_id: String,
    pub claim: String,
    pub claim_type: String,
    pub subject: String,
    pub scope: String,
    pub observed_by: String,
    pub status: String,
    pub independent_count: u32,
    pub confidence: f64,
    pub updated_at: String,
    pub embedding: Option<Vec<f32>>,
}

impl ClaimRow {
    /// The vector this row compares against a query with: its own stored
    /// embedding when the publisher had a real one, or the same deterministic
    /// fallback the query itself is embedded with otherwise. See the module
    /// doc's vector-search paragraph for why this is the honest choice today.
    fn vector(&self) -> Vec<f32> {
        self.embedding
            .clone()
            .unwrap_or_else(|| hash_embedding(&self.claim))
    }
}

fn snapshot_path(cache_root: &Path, fleet_id: &str) -> PathBuf {
    cache_root.join(fleet_id).join("snapshot.bin")
}

/// Open the local snapshot mirror read-only. `None` covers every reason this
/// might not work — no file yet (nothing has synced), a torn write caught
/// mid-copy, a file from some other, unrelated format — and every one of those is
/// the same honest "not enabled" outcome to a caller, never a panic and never a
/// guess at partial content. Read-only is not just hygiene: this process must
/// never be the thing that mutates the one artifact `ctxlake-maint` and
/// `ctxlake-sync` already have a single-writer story for.
pub fn open(cache_root: &Path, fleet_id: &str) -> Option<Connection> {
    let path = snapshot_path(cache_root, fleet_id);
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags).ok()?;
    // A cheap sanity probe: confirm the `claims` table this module's every other
    // query assumes actually exists, so a file that happens to open (any SQLite
    // file will) but isn't this schema fails here, once, rather than as a
    // confusing per-query error deeper in.
    conn.query_row("SELECT COUNT(*) FROM claims", [], |r| r.get::<_, i64>(0))
        .ok()?;
    Some(conn)
}

fn decode_embedding(blob: &[u8]) -> Option<Vec<f32>> {
    if blob.is_empty() || !blob.len().is_multiple_of(4) {
        return None;
    }
    Some(
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().expect("chunks_exact(4)")))
            .collect(),
    )
}

fn row_to_claim(row: &rusqlite::Row) -> rusqlite::Result<ClaimRow> {
    let embedding_blob: Option<Vec<u8>> = row.get("embedding")?;
    Ok(ClaimRow {
        claim_id: row.get("claim_id")?,
        claim: row.get("claim")?,
        claim_type: row.get("claim_type")?,
        subject: row.get("subject")?,
        scope: row.get("scope")?,
        observed_by: row.get("observed_by")?,
        status: row.get("status")?,
        independent_count: row.get("independent_count")?,
        confidence: row.get("confidence")?,
        updated_at: row.get("updated_at")?,
        embedding: embedding_blob.and_then(|b| decode_embedding(&b)),
    })
}

/// Every `visible_to_agents = 1` row, optionally narrowed by an exact-match
/// `subject`/`claim_type`/`scope` filter (`None` skips that filter). This is the
/// full agent-visible universe a call is allowed to rank over — capped at a
/// generous ceiling so one pathological snapshot can't make a single tool call
/// scan an unbounded table; `ctxlake-maint`'s own sizing comment (5k claims * 256
/// dims * 4 bytes = 5MB) is the scale this whole design is built for, so a cap an
/// order of magnitude above that is a safety rail, not a real limit in practice.
const MAX_SCANNED_ROWS: usize = 20_000;

/// Crate-visible (not just `search`'s private helper) so `memory::briefing_claims`
/// can rank confidence over the *entire* visible universe — see that function's
/// doc for why going through `search`'s `k+1`-truncated, claim_id-tie-broken
/// output instead would silently brief from the oldest claims rather than the
/// strongest ones on any fleet past `MAX_ROW_LIMIT` visible claims.
pub(crate) fn fetch_visible(
    conn: &Connection,
    subject: Option<&str>,
    claim_type: Option<&str>,
    scope: Option<&str>,
) -> Vec<ClaimRow> {
    let sql = "SELECT claim_id, claim, claim_type, subject, scope, observed_by, status, \
               independent_count, confidence, updated_at, embedding \
               FROM claims \
               WHERE visible_to_agents = 1 \
               AND (?1 IS NULL OR subject = ?1) \
               AND (?2 IS NULL OR claim_type = ?2) \
               AND (?3 IS NULL OR scope = ?3) \
               LIMIT ?4";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(
        rusqlite::params![subject, claim_type, scope, MAX_SCANNED_ROWS as i64],
        row_to_claim,
    );
    let Ok(rows) = rows else { return Vec::new() };
    rows.filter_map(Result::ok).collect()
}

/// FTS5 treats each whitespace-split token as its own quoted phrase term, joined
/// with `OR` for recall — see the module doc's precedent
/// (`gate.rs::lexical_overlap`) for why a hand-rolled, honestly-partial ranker
/// beats silently mis-parsing a caller's query as FTS5 query syntax. Quoting every
/// token (doubling any embedded `"`) makes hyphens, colons, and other FTS5
/// operator characters inert literal text instead of a syntax error or an
/// unintended `NOT`/`NEAR` operator a caller never meant to invoke.
fn fts_match_expr(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Claim ids matching `query` via FTS5, best match first, among rows already
/// narrowed by `subject`/`claim_type`/`scope`. `claims_fts` only ever contains
/// `visible_to_agents = 1` rows (see `ctxlake-maint::snapshot`'s own module doc:
/// "only rows the gate promoted *and* that this run's mode allows to be read ever
/// enter the search index"), so this join can never surface a shadow-mode or
/// unpromoted claim even if this function's own `WHERE` clause had a bug in it —
/// the artifact itself is the enforcement, not this query.
///
/// `bm25()` takes the FTS5 table's declared name, `claims_fts` — NOT the `f`
/// alias this query joins it under. SQLite resolves `bm25()`'s argument by
/// looking up the schema object by that literal identifier, not by resolving it
/// as a table reference the way an ordinary column would be; `bm25(f)` fails
/// `prepare()` with "no such column: f" (confirmed against this crate's bundled
/// SQLite), which — because `prepare()`'s `Err` is deliberately swallowed into
/// an empty result just below, matching the "fail closed, not open" discipline
/// AGENTS.md asks for everywhere claims are read — silently disabled the entire
/// lexical half of `search` rather than surfacing as a startup error. Prefer the
/// real name here over the alias precisely because that failure mode is silent.
fn fts_matches(
    conn: &Connection,
    query: &str,
    subject: Option<&str>,
    claim_type: Option<&str>,
    scope: Option<&str>,
) -> Vec<String> {
    let Some(expr) = fts_match_expr(query) else {
        return Vec::new();
    };
    let sql = "SELECT c.claim_id FROM claims_fts f \
               JOIN claims c ON c.claim_id = f.claim_id \
               WHERE f.claims_fts MATCH ?1 \
               AND (?2 IS NULL OR c.subject = ?2) \
               AND (?3 IS NULL OR c.claim_type = ?3) \
               AND (?4 IS NULL OR c.scope = ?4) \
               ORDER BY bm25(claims_fts) \
               LIMIT ?5";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let ids = stmt.query_map(
        rusqlite::params![expr, subject, claim_type, scope, MAX_SCANNED_ROWS as i64],
        |r| r.get::<_, String>(0),
    );
    let Ok(ids) = ids else { return Vec::new() };
    ids.filter_map(Result::ok).collect()
}

/// Cosine similarity between two equal-length vectors — 0.0, never NaN or a
/// panic, when either side is all zeros (an empty query, or a claim whose
/// fallback embedding degenerated to nothing). Unequal lengths (which should
/// never happen: every vector here is [`EMBEDDING_DIM`]) also return 0.0 rather
/// than panicking, since a length mismatch is a bug worth a silently-unranked row,
/// not a crashed tool call.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// A deterministic, dependency-free "hashing trick" bag-of-words embedding — see
/// the module doc for why this exists instead of a real semantic embedder.
/// Lowercased whitespace tokens are each hashed (`DefaultHasher`, already in
/// `std`) into one of [`EMBEDDING_DIM`] buckets and accumulated, then the whole
/// vector is L2-normalized so [`cosine`] behaves the same way it would over a
/// real embedding. Two texts sharing more words land closer together; this is
/// deliberately a lexical proxy, not a semantic one.
pub fn hash_embedding(text: &str) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut buckets = vec![0.0f32; EMBEDDING_DIM];
    for word in text.to_lowercase().split_whitespace() {
        let mut hasher = DefaultHasher::new();
        word.hash(&mut hasher);
        let bucket = (hasher.finish() as usize) % EMBEDDING_DIM;
        buckets[bucket] += 1.0;
    }
    let norm: f32 = buckets.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for b in &mut buckets {
            *b /= norm;
        }
    }
    buckets
}

/// `memory_search`'s ranking: reciprocal-rank FTS5 score plus brute-force cosine,
/// summed — a simple, well-known hybrid-retrieval blend (see e.g. reciprocal rank
/// fusion) chosen over trying to reconcile FTS5's `bm25()` sign/scale with cosine's
/// `[-1, 1]` range. Highest combined score first, ties broken by `claim_id` for a
/// deterministic order across runs.
///
/// Returns up to `k + 1` rows, not `k` — the one extra row is a truncation
/// sentinel `memory.rs`'s `search` uses to set `truncated: true` honestly (a
/// caller reading exactly `k` rows back can never tell "there were exactly `k`
/// matches" from "there were more and this got cut") and then trims itself; `k`
/// is clamped by the caller (see `memory.rs`'s `MAX_ROW_LIMIT`) before it ever
/// reaches here.
pub fn search(
    conn: &Connection,
    query: &str,
    subject: Option<&str>,
    claim_type: Option<&str>,
    scope: Option<&str>,
    k: usize,
) -> Vec<ClaimRow> {
    let universe = fetch_visible(conn, subject, claim_type, scope);
    if universe.is_empty() {
        return Vec::new();
    }
    // A blank query carries no ranking signal at all — `fts_match_expr` returns
    // `None` for it and `hash_embedding` returns the zero vector, which cosine
    // correctly scores 0.0 against everything (see `cosine`'s doc). That is a
    // real "no relevance signal" answer, not "nothing matches": a caller passing
    // `subject`/`claim_type`/`scope` with no `query` (this is exactly what
    // `briefing_claims` does) means "list what you already narrowed down to,"
    // not "and also require some text to have matched." So relevance-filtering
    // only applies once there is a query to be relevant *to*.
    let has_query = !query.trim().is_empty();
    let lexical_hits = fts_matches(conn, query, subject, claim_type, scope);
    // Reciprocal rank: the first FTS hit gets 1.0, the second 0.5, and so on — a
    // claim never matched at all gets 0.0 from this term.
    let lexical_score = |claim_id: &str| -> f32 {
        lexical_hits
            .iter()
            .position(|id| id == claim_id)
            .map(|pos| 1.0 / (pos as f32 + 1.0))
            .unwrap_or(0.0)
    };
    let query_vector = hash_embedding(query);

    let mut scored: Vec<(f32, &ClaimRow)> = universe
        .iter()
        .map(|row| {
            let score = lexical_score(&row.claim_id) + cosine(&query_vector, &row.vector());
            (score, row)
        })
        .collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then_with(|| a.1.claim_id.cmp(&b.1.claim_id))
    });
    scored
        .into_iter()
        .filter(|(score, _)| !has_query || *score > 0.0)
        .take(k + 1)
        .map(|(_, row)| row.clone())
        .collect()
}

/// `memory_timeline`'s read: `outcome` claims about `subject`, oldest first (the
/// narrative order "what has this fleet actually tried" implies), optionally
/// bounded to `since` (an RFC3339 timestamp; string comparison is correct because
/// every `updated_at` this module ever writes or reads is RFC3339, which sorts
/// lexicographically in time order). Returns up to `limit + 1` rows — see
/// [`search`]'s doc for why the extra row is a truncation sentinel, not a bug.
pub fn timeline(
    conn: &Connection,
    subject: &str,
    since: Option<&str>,
    limit: usize,
) -> Vec<ClaimRow> {
    let sql = "SELECT claim_id, claim, claim_type, subject, scope, observed_by, status, \
               independent_count, confidence, updated_at, embedding \
               FROM claims \
               WHERE visible_to_agents = 1 \
               AND claim_type = 'outcome' \
               AND subject = ?1 \
               AND (?2 IS NULL OR updated_at >= ?2) \
               ORDER BY updated_at ASC \
               LIMIT ?3";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map(
        rusqlite::params![subject, since, (limit + 1) as i64],
        row_to_claim,
    );
    let Ok(rows) = rows else { return Vec::new() };
    rows.filter_map(Result::ok).collect()
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    //! Build a fixture `snapshot.bin` with the exact schema
    //! `ctxlake_maint::snapshot`'s `SCHEMA_SQL` publishes, without depending on
    //! that crate (see the module doc for why) — this is the local mirror of that
    //! module's own test helpers, kept honest by both sides' tests exercising the
    //! same literal column set.
    //!
    //! Gated behind either this crate's own `#[cfg(test)]` or the `test-support`
    //! feature — the latter is what lets `ctxlake-cli`'s briefing tests (a
    //! different crate; a plain `#[cfg(test)]` item is invisible outside the
    //! crate that declares it) build the exact same fixture shape rather than
    //! hand-rolling a second, driftable copy of `SCHEMA_SQL`.
    //! `ctxlake-cli/Cargo.toml` enables the feature only under
    //! `[dev-dependencies]`, so it never reaches a release binary.
    use super::*;

    pub const SCHEMA_SQL: &str = r#"
        CREATE TABLE claims (
            claim_id          TEXT PRIMARY KEY,
            claim             TEXT NOT NULL,
            claim_type        TEXT NOT NULL,
            subject           TEXT NOT NULL,
            scope             TEXT NOT NULL,
            observed_by       TEXT NOT NULL,
            status            TEXT NOT NULL,
            evidence_count    INTEGER NOT NULL,
            independent_count INTEGER NOT NULL,
            confidence        REAL NOT NULL,
            evidence_json     TEXT NOT NULL,
            updated_at        TEXT NOT NULL,
            visible_to_agents INTEGER NOT NULL,
            embedding         BLOB
        );
        CREATE VIRTUAL TABLE claims_fts USING fts5(claim_id UNINDEXED, claim, subject);
    "#;

    #[derive(Clone)]
    pub struct FixtureClaim {
        pub claim_id: &'static str,
        pub claim: &'static str,
        pub claim_type: &'static str,
        pub subject: &'static str,
        pub scope: &'static str,
        pub observed_by: &'static str,
        pub status: &'static str,
        pub independent_count: u32,
        pub confidence: f64,
        pub updated_at: &'static str,
        pub visible_to_agents: bool,
        pub embedding: Option<Vec<f32>>,
    }

    impl FixtureClaim {
        pub fn promoted(
            claim_id: &'static str,
            claim: &'static str,
            claim_type: &'static str,
            subject: &'static str,
        ) -> Self {
            Self {
                claim_id,
                claim,
                claim_type,
                subject,
                scope: "fleet",
                observed_by: "cc-01",
                status: "promoted",
                independent_count: 2,
                confidence: 0.8,
                updated_at: "2026-09-09T00:00:00Z",
                visible_to_agents: true,
                embedding: None,
            }
        }
    }

    fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
        let mut out = Vec::with_capacity(v.len() * 4);
        for f in v {
            out.extend_from_slice(&f.to_le_bytes());
        }
        out
    }

    /// Write a fixture snapshot to `path`, matching the real publisher's schema
    /// and its `visible_to_agents = 1 AND status = 'promoted'` FTS5 population
    /// rule exactly (see `ctxlake_maint::snapshot::build_sqlite_bytes`) — a
    /// fixture that populated `claims_fts` differently would validate nothing
    /// about the real artifact.
    pub fn write_snapshot(path: &std::path::Path, claims: &[FixtureClaim]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        // Content-addressed publishing means the real artifact is never
        // reopened and reused — `ctxlake-maint::snapshot::publish` always builds
        // a fresh temp file. A test that "republishes" to the same fixture path
        // (simulating an operator flipping shadow -> live) must start from a
        // clean file too, or the second `CREATE TABLE` collides with the first.
        let _ = std::fs::remove_file(path);
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        for c in claims {
            conn.execute(
                "INSERT INTO claims (claim_id, claim, claim_type, subject, scope, observed_by, \
                 status, evidence_count, independent_count, confidence, evidence_json, \
                 updated_at, visible_to_agents, embedding) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                rusqlite::params![
                    c.claim_id,
                    c.claim,
                    c.claim_type,
                    c.subject,
                    c.scope,
                    c.observed_by,
                    c.status,
                    1,
                    c.independent_count,
                    c.confidence,
                    "[]",
                    c.updated_at,
                    c.visible_to_agents as i64,
                    c.embedding.as_deref().map(embedding_to_blob),
                ],
            )
            .unwrap();
            if c.visible_to_agents && c.status == "promoted" {
                conn.execute(
                    "INSERT INTO claims_fts (claim_id, claim, subject) VALUES (?1, ?2, ?3)",
                    rusqlite::params![c.claim_id, c.claim, c.subject],
                )
                .unwrap();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{write_snapshot, FixtureClaim};
    use super::*;

    #[test]
    fn open_returns_none_when_no_snapshot_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(open(dir.path(), "oxidant").is_none());
    }

    #[test]
    fn open_returns_none_for_a_file_that_is_not_this_schema() {
        let dir = tempfile::tempdir().unwrap();
        let fleet_dir = dir.path().join("oxidant");
        std::fs::create_dir_all(&fleet_dir).unwrap();
        std::fs::write(fleet_dir.join("snapshot.bin"), b"not a sqlite file at all").unwrap();
        assert!(open(dir.path(), "oxidant").is_none());
    }

    #[test]
    fn open_succeeds_against_a_real_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        write_snapshot(&path, &[]);
        assert!(open(dir.path(), "oxidant").is_some());
    }

    /// The gate-preserving property this whole module exists to uphold: a claim
    /// published with `visible_to_agents = 0` (shadow mode, or simply not
    /// promoted) is invisible to every read this module offers, from the exact
    /// same on-disk file a live-mode claim would be visible from.
    #[test]
    fn shadow_mode_claims_are_invisible_to_search_and_fetch_from_the_same_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let mut shadowed = FixtureClaim::promoted(
            "c1",
            "cargo test needs RUSTFLAGS set first",
            "convention",
            "ci",
        );
        shadowed.visible_to_agents = false; // published while `[summarize] mode = "shadow"`
        write_snapshot(&path, &[shadowed]);

        let conn = open(dir.path(), "oxidant").unwrap();
        assert!(fetch_visible(&conn, None, None, None).is_empty());
        assert!(search(&conn, "cargo test", None, None, None, 8).is_empty());
        assert!(timeline(&conn, "ci", None, 8).is_empty());
    }

    #[test]
    fn live_mode_search_finds_a_promoted_claim_the_same_shadow_claim_would_have_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        write_snapshot(
            &path,
            &[FixtureClaim::promoted(
                "c1",
                "cargo test needs RUSTFLAGS set first",
                "convention",
                "ci",
            )],
        );
        let conn = open(dir.path(), "oxidant").unwrap();
        let hits = search(&conn, "RUSTFLAGS", None, None, None, 8);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].claim_id, "c1");
    }

    #[test]
    fn a_merely_candidate_claim_never_surfaces_even_though_it_is_in_the_claims_table() {
        // A row can exist in `claims` (kept for `ctxlake claims --status candidate`)
        // without ever entering `claims_fts` or counting as visible — the exact
        // shape a real snapshot produces for an unpromoted candidate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let mut candidate = FixtureClaim::promoted(
            "c2",
            "staging SSH listens on 2222",
            "environment",
            "staging",
        );
        candidate.status = "candidate";
        candidate.visible_to_agents = false;
        write_snapshot(&path, &[candidate]);
        let conn = open(dir.path(), "oxidant").unwrap();
        assert!(search(&conn, "staging", None, None, None, 8).is_empty());
    }

    #[test]
    fn subject_and_claim_type_filters_narrow_the_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        write_snapshot(
            &path,
            &[
                FixtureClaim::promoted("c1", "uses just not make", "convention", "tooling"),
                FixtureClaim::promoted("c2", "staging SSH on 2222", "environment", "staging"),
            ],
        );
        let conn = open(dir.path(), "oxidant").unwrap();
        let all = search(&conn, "", Some("tooling"), None, None, 8);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].claim_id, "c1");

        let by_type = search(&conn, "", None, Some("environment"), None, 8);
        assert_eq!(by_type.len(), 1);
        assert_eq!(by_type[0].claim_id, "c2");
    }

    #[test]
    fn hash_embedding_is_deterministic_and_shares_more_similarity_for_shared_words() {
        let a = hash_embedding("cargo test needs rustflags");
        let b = hash_embedding("cargo test needs rustflags");
        assert_eq!(a, b, "same text must embed identically every time");

        let close = hash_embedding("cargo test needs rustflags set");
        let far = hash_embedding("the weather today is sunny and warm");
        assert!(
            cosine(&a, &close) > cosine(&a, &far),
            "shared vocabulary must cosine closer than disjoint vocabulary"
        );
    }

    #[test]
    fn cosine_of_zero_vector_is_zero_not_nan() {
        let zero = vec![0.0f32; EMBEDDING_DIM];
        let other = hash_embedding("anything");
        assert_eq!(cosine(&zero, &other), 0.0);
    }

    /// The brute-force cosine half of the ranking actually contributes: a claim
    /// whose text shares no tokens with the query but was stored with an
    /// embedding identical to the query's must still be found, purely via cosine
    /// — this is only possible once real embeddings exist, but the machinery must
    /// already be exact today so it needs no changes once they do.
    #[test]
    fn cosine_alone_can_surface_a_claim_fts_would_never_lexically_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let query_vec = hash_embedding("flaky reattachable exec test under colima");
        let mut with_embedding = FixtureClaim::promoted(
            "c1",
            "unrelated wording entirely, no shared tokens here",
            "hypothesis",
            "ci",
        );
        with_embedding.embedding = Some(query_vec.clone());
        write_snapshot(&path, &[with_embedding]);
        let conn = open(dir.path(), "oxidant").unwrap();
        let hits = search(
            &conn,
            "flaky reattachable exec test under colima",
            None,
            None,
            None,
            8,
        );
        assert_eq!(hits.len(), 1, "cosine alone must surface the row: {hits:?}");
        assert_eq!(hits[0].claim_id, "c1");
    }

    /// The mirror of the cosine-alone test above: a claim only findable through
    /// FTS5, with the `claim` text sharing zero tokens with the query so cosine
    /// (which falls back to `hash_embedding(claim)` with no stored embedding)
    /// scores exactly 0.0. Only `subject` — which `claims_fts` also indexes —
    /// carries the query's words, so this can only pass if the FTS5 `MATCH`
    /// actually runs. This is the regression test for the `bm25(f)` vs.
    /// `bm25(claims_fts)` bug: before that fix, `conn.prepare()` failed on the
    /// bad alias, `fts_matches` swallowed the error into `Vec::new()`, and this
    /// claim was invisible to `search` no matter what the query said.
    #[test]
    fn lexical_alone_can_surface_a_claim_cosine_would_never_find() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let claim = FixtureClaim::promoted(
            "c1",
            "unrelated wording entirely, no shared tokens here",
            "hypothesis",
            "distinctive network glitch signature",
        );
        write_snapshot(&path, &[claim]);
        let conn = open(dir.path(), "oxidant").unwrap();

        // Sanity check this test's own premise: cosine over the claim text must
        // score exactly 0.0 against this query, or a pass here would prove
        // nothing about the lexical path.
        let query_vec = hash_embedding("distinctive network glitch signature");
        let claim_vec = hash_embedding("unrelated wording entirely, no shared tokens here");
        assert_eq!(
            cosine(&query_vec, &claim_vec),
            0.0,
            "test premise broken: cosine must contribute nothing here"
        );

        let hits = search(
            &conn,
            "distinctive network glitch signature",
            None,
            None,
            None,
            8,
        );
        assert_eq!(hits.len(), 1, "FTS5 alone must surface the row: {hits:?}");
        assert_eq!(hits[0].claim_id, "c1");
    }

    #[test]
    fn timeline_returns_only_outcome_claims_about_the_subject_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let mut c1 = FixtureClaim::promoted("c1", "migration failed at abc123", "outcome", "glue");
        c1.updated_at = "2026-09-10T00:00:00Z";
        let mut c2 = FixtureClaim::promoted("c2", "migration passed at def456", "outcome", "glue");
        c2.updated_at = "2026-09-11T00:00:00Z";
        let hypothesis =
            FixtureClaim::promoted("c3", "the flake is a colima artifact", "hypothesis", "glue");
        write_snapshot(&path, &[c2.clone(), c1.clone(), hypothesis]);
        let conn = open(dir.path(), "oxidant").unwrap();
        let entries = timeline(&conn, "glue", None, 10);
        assert_eq!(entries.len(), 2, "only outcome claims: {entries:?}");
        assert_eq!(entries[0].claim_id, "c1", "oldest first");
        assert_eq!(entries[1].claim_id, "c2");
    }

    #[test]
    fn timeline_since_filters_out_earlier_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let mut c1 = FixtureClaim::promoted("c1", "attempt one failed", "outcome", "glue");
        c1.updated_at = "2026-09-01T00:00:00Z";
        let mut c2 = FixtureClaim::promoted("c2", "attempt two passed", "outcome", "glue");
        c2.updated_at = "2026-09-10T00:00:00Z";
        write_snapshot(&path, &[c1, c2]);
        let conn = open(dir.path(), "oxidant").unwrap();
        let entries = timeline(&conn, "glue", Some("2026-09-05T00:00:00Z"), 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].claim_id, "c2");
    }

    #[test]
    fn search_result_is_capped_at_k() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oxidant").join("snapshot.bin");
        let claims: Vec<FixtureClaim> = (0..5)
            .map(|i| {
                let id: &'static str = Box::leak(format!("c{i}").into_boxed_str());
                FixtureClaim::promoted(id, "cargo test needs rustflags", "convention", "ci")
            })
            .collect();
        write_snapshot(&path, &claims);
        let conn = open(dir.path(), "oxidant").unwrap();
        // k=2 with 5 matching rows: exactly k+1 come back (the truncation
        // sentinel `memory.rs`'s `search` trims — see this function's doc).
        let hits = search(&conn, "cargo test", None, None, None, 2);
        assert_eq!(hits.len(), 3);
    }
}
