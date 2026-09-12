//! Snapshot — fold the claim event log into an immutable, content-addressed SQLite
//! artifact. See `docs/memory.md`'s claim model and `docs/architecture.md`'s
//! "serving plane" row for `snapshot/`.
//!
//! **This module does not decide what a claim means.** Promotion (which gate a
//! claim passed, whether it's contested) is the extraction/gate wave's job, not
//! this one's — this module's only responsibility is arithmetic: replay whatever
//! the append-only `claims/events/` log currently holds into one row per
//! `claim_id`, exactly the way the log's own producer defines "current state"
//! (see the next paragraph), and publish that fold as a queryable file. Nothing
//! here promotes, contests, or invents evidence.
//!
//! **The wire format here mirrors the real event log on purpose, without this
//! crate's extraction/gate modules being this module's dependency.** Those modules
//! are a different wave's responsibility (this crate's task brief is explicit:
//! "NOT extraction or gates"), so this module does not `use crate::claims::*` —
//! but the bytes sitting in `claims/events/` are real regardless of which module
//! wrote them, and folding them *correctly* means understanding their actual
//! shape: a `#[serde(tag = "kind")]` enum — `proposed` / `promoted` / `contested`
//! / `retired` / `superseded` — where only `proposed` carries the claim's content
//! (text, type, subject, scope, evidence, an optional embedding) and every other
//! kind carries just `claim_id` plus whatever that transition needs. A fold that
//! assumed every event carried the full claim (this module's own first draft did)
//! would silently blank out `claim`/`subject`/... the moment a `promoted` event —
//! which carries neither — was replayed "last." [`ClaimEventRecord`] and
//! [`apply_event`] below replay it the same way the real fold does: `proposed`
//! inserts-or-merges, every other kind only ever flips `status` on an
//! already-known claim. Parsing stays tolerant (`#[serde(default)]` everywhere,
//! and a record that fails to parse at all is skipped) for the same reason
//! `ctxlake_store::roster::list_intents_directly` is: a future field this build
//! predates must degrade, not break the whole fold.
//!
//! **The claim event log may be empty this wave** — the belief stream lands
//! separately, per the task this module was built against. An empty log must still
//! produce a valid, well-shaped SQLite file with the right schema (a `claims` table,
//! an FTS5 index, an `embedding` column), not an error: the shape has to be right
//! before there is content, so that whatever writes real claims next needs no
//! migration to start landing in it.
//!
//! **Content-addressed, `docs/concepts.md`-style: write the blob, then swap the
//! pointer.** [`publish`] writes `snapshot/<sha256>.sqlite` (skipping the `PUT`
//! entirely if that exact hash is already there — the fold is deterministic, so a
//! maintenance run that finds no new claim events reproduces the same bytes and the
//! same hash) and then CAS-swaps `snapshot/latest.json` to name it, exactly the
//! write-then-swap order `docs/architecture.md`'s "no cross-key atomicity" section
//! says is the safe one: a crash between the two leaves an orphaned blob (harmless)
//! or simply never swaps the pointer (also harmless — readers keep the old
//! snapshot).
//!
//! **The gate is a property of the artifact, not of one crate's function.**
//! `docs/summarization.md`'s shadow mode runs "the whole chain — extraction, gates,
//! snapshot — with agent reads disabled," so `publish` always folds and writes the
//! *full* claim log regardless of mode (`agent_reads_enabled` never skips a row in
//! [`SCHEMA_SQL`]'s `claims` table — that table is this module's arithmetic, and it
//! stays complete for audit and for `ctxlake claims --status ...`). What
//! `agent_reads_enabled` controls is narrower and load-bearing: [`FoldedClaim`]s are
//! marked `visible_to_agents` only when they are `status = "promoted"` **and** the
//! caller says agent reads are enabled, and `claims_fts` — the index an MCP
//! memory-recall tool would actually query into a context window — is populated
//! from *only* those rows. A caller that passes `agent_reads_enabled: false` (the
//! `shadow`/`none` case) still gets a valid, fully-folded snapshot; it just cannot
//! contain a single FTS5-searchable row, by construction, not by a downstream
//! reader remembering to check a flag. This mirrors (without depending on — see the
//! module doc above on why this crate's extraction/gate modules are not this
//! module's dependency) the belief wave's own `claims::claims_visible_to_agents`
//! rule: only `Promoted` claims are ever agent-visible, `Contested`/`Retired` never
//! are regardless of mode, and *how* `agent_reads_enabled` gets computed from
//! `ctxlake.toml`'s `[summarize] mode` is this wave's caller's job, not this
//! module's — today [`crate::run::run`] takes it as a plain `bool` because no
//! config-reading wave has wired `ctxlake.toml` into the maintenance chain yet;
//! that wiring is a follow-up, not a silent "always visible" default smuggled in
//! here.

use std::collections::{BTreeMap, HashSet};

use ctxlake_core::hash;
use object_store::path::Path;
use object_store::{
    Error as OsError, ObjectStore, ObjectStoreExt, PutMode, PutPayload, UpdateVersion,
};
use serde::Deserialize;

use crate::error::MaintError;

/// The schema every published snapshot carries, whether or not `claims` has any
/// rows. `embedding` is a 256-dim f32 BLOB column — populated when a `proposed`
/// event carried one (an evidence-backed claim from extraction, which computes
/// embeddings for the contradiction gate's cosine search), left `NULL` otherwise.
/// Sizing the column now, whether or not every row fills it, is cheap; a schema
/// migration once real snapshots exist to migrate is not.
const SCHEMA_SQL: &str = r#"
CREATE TABLE snapshot_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

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
    -- 1 exactly when status = 'promoted' AND the publishing run was told agent
    -- reads are enabled; 0 otherwise, always — never derived by a downstream
    -- reader re-checking status or mode itself. See the module doc's "the gate
    -- is a property of the artifact" paragraph.
    visible_to_agents INTEGER NOT NULL,
    -- 256-dim f32, little-endian (1024 bytes) when present; NULL when the
    -- proposing call had no embedder wired up. See the module doc.
    embedding         BLOB
);

-- A standalone (not `content=`-synced) FTS5 index: this snapshot is rebuilt whole
-- on every publish rather than updated incrementally, so there is no live table for
-- FTS5's content-sync triggers to track — a plain INSERT ... SELECT after the
-- `claims` table is populated is the entire "index maintenance" this needs.
CREATE VIRTUAL TABLE claims_fts USING fts5(claim_id UNINDEXED, claim, subject);
"#;

/// The literal number of dimensions the `embedding` column is sized for — matches
/// the promotion gate's own `EMBEDDING_DIM` (its cosine-similarity contradiction
/// check operates over the same vectors this column stores). Kept as an
/// independent constant rather than imported, per the module doc: this module
/// does not depend on the gate's crate module, it depends on the wire format both
/// happen to agree on.
pub const EMBEDDING_DIMENSIONS: usize = 256;

/// One event as read from `claims/events/**`. Mirrors the real producer's tagged
/// enum (`kind: "proposed" | "promoted" | "contested" | "retired" | "superseded"`)
/// — see the module doc for why matching this shape, not a simpler guess, is load
/// bearing. `#[serde(default)]` on every field inside each variant is deliberate
/// tolerance for a producer schema this build predates, not an invitation to treat
/// missing data as meaningful; a real writer of these events always sets them.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ClaimEventRecord {
    Proposed(ProposedClaimRecord),
    Promoted {
        claim_id: String,
        #[serde(default)]
        at: String,
        #[serde(default)]
        independent_count: u32,
        #[serde(default)]
        confidence: f64,
    },
    Contested {
        claim_id: String,
        #[serde(default)]
        at: String,
    },
    Retired {
        claim_id: String,
        #[serde(default)]
        at: String,
    },
    Superseded {
        claim_id: String,
        #[serde(default)]
        at: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
struct ProposedClaimRecord {
    claim_id: String,
    #[serde(default)]
    claim: String,
    /// Kept as a plain string, not a re-declared copy of the producer's
    /// `ClaimType` enum: both sides already agree on the wire value (a
    /// `#[serde(rename_all = "snake_case")]` unit-variant enum and a plain
    /// `String` serialize/deserialize identically as a JSON string), so nothing
    /// is gained by this module maintaining its own parallel enum to drift from
    /// theirs.
    #[serde(default)]
    claim_type: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    observed_by: String,
    #[serde(default)]
    observed_at: String,
    /// Kept opaque (not decomposed into named fields) for the same reason: this
    /// module only ever needs to count distinct `session_id`s and re-store the
    /// citations verbatim, never to validate or rewrite one.
    #[serde(default)]
    evidence: Vec<serde_json::Value>,
    #[serde(default)]
    embedding: Option<Vec<f32>>,
}

/// One claim's current (folded) state.
#[derive(Debug, Clone, PartialEq)]
struct FoldedClaim {
    claim_id: String,
    claim: String,
    claim_type: String,
    subject: String,
    scope: String,
    observed_by: String,
    status: String,
    evidence: Vec<serde_json::Value>,
    independent_count: u32,
    confidence: f64,
    updated_at: String,
    embedding: Option<Vec<f32>>,
}

/// Distinct `session_id`s cited — the same quantity the real `ClaimState` exposes
/// as `evidence_session_count()`, computed the same way (a citation count would
/// overcount an agent that quoted one session twice).
fn evidence_session_count(evidence: &[serde_json::Value]) -> u32 {
    let sessions: HashSet<&str> = evidence
        .iter()
        .filter_map(|e| e.get("session_id").and_then(|v| v.as_str()))
        .collect();
    sessions.len() as u32
}

fn merge_evidence(existing: &mut Vec<serde_json::Value>, new: &[serde_json::Value]) {
    for e in new {
        if !existing.contains(e) {
            existing.push(e.clone());
        }
    }
}

/// Replay one event onto the running fold — the same three-shape logic the real
/// producer's own `fold()` uses: `Proposed` inserts a brand-new claim or merges
/// evidence into an existing one; every other kind only ever flips `status` (and,
/// for `Promoted`, sets the counts the gate computed) on a claim that must already
/// exist. An event naming a `claim_id` this fold has never seen `Proposed` is
/// silently ignored — that shouldn't happen from a well-formed log, and there is
/// nothing sensible to construct from a status transition alone.
fn apply_event(folded: &mut BTreeMap<String, FoldedClaim>, record: ClaimEventRecord) {
    match record {
        ClaimEventRecord::Proposed(p) => {
            if p.claim_id.is_empty() {
                return;
            }
            folded
                .entry(p.claim_id.clone())
                .and_modify(|s| {
                    merge_evidence(&mut s.evidence, &p.evidence);
                    if s.embedding.is_none() {
                        s.embedding = p.embedding.clone();
                    }
                })
                .or_insert_with(|| FoldedClaim {
                    claim_id: p.claim_id.clone(),
                    claim: p.claim.clone(),
                    claim_type: p.claim_type.clone(),
                    subject: p.subject.clone(),
                    scope: p.scope.clone(),
                    observed_by: p.observed_by.clone(),
                    status: "candidate".to_string(),
                    evidence: p.evidence.clone(),
                    independent_count: 0,
                    confidence: 0.0,
                    updated_at: p.observed_at.clone(),
                    embedding: p.embedding.clone(),
                });
        }
        ClaimEventRecord::Promoted {
            claim_id,
            at,
            independent_count,
            confidence,
        } => {
            if let Some(s) = folded.get_mut(&claim_id) {
                s.status = "promoted".to_string();
                s.independent_count = independent_count;
                s.confidence = confidence;
                if !at.is_empty() {
                    s.updated_at = at;
                }
            }
        }
        ClaimEventRecord::Contested { claim_id, at } => {
            if let Some(s) = folded.get_mut(&claim_id) {
                s.status = "contested".to_string();
                if !at.is_empty() {
                    s.updated_at = at;
                }
            }
        }
        ClaimEventRecord::Retired { claim_id, at }
        | ClaimEventRecord::Superseded { claim_id, at } => {
            // The real model has no separate "superseded" claim status — both
            // fold to `retired`, matching `claims::fold`'s own
            // `Retired | Superseded => ClaimStatus::Retired` arm exactly.
            if let Some(s) = folded.get_mut(&claim_id) {
                s.status = "retired".to_string();
                if !at.is_empty() {
                    s.updated_at = at;
                }
            }
        }
    }
}

/// Walk every object under `claims/events/`, replay them in append order, and
/// return one folded row per claim.
///
/// "Append order" here is the same ordering the real producer's own
/// `claims::list_events` sorts by: each event's full object key, ascending. That
/// groups by `dt=`/`agent=` before the ULID tail, not by true cross-agent wall
/// time — a faithful match to the existing, already-relied-upon behavior this
/// module mirrors, not an independent "better" ordering this module invented.
async fn fold_claim_events(store: &dyn ObjectStore) -> Result<Vec<FoldedClaim>, MaintError> {
    use futures::StreamExt;
    let prefix = Path::from("claims").join("events");
    let mut stream = store.list(Some(&prefix));
    let mut entries: Vec<(String, ClaimEventRecord)> = Vec::new();

    while let Some(meta) = stream.next().await {
        // Best-effort, matching `ctxlake_store::roster::list_intents_directly`'s
        // own tolerance: a partial write or a future schema this build predates
        // must not take down every other claim's fold.
        let Ok(meta) = meta else { continue };
        let Ok(res) = store.get(&meta.location).await else {
            continue;
        };
        let Ok(bytes) = res.bytes().await else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<ClaimEventRecord>(&bytes) else {
            continue;
        };
        entries.push((meta.location.to_string(), record));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut folded: BTreeMap<String, FoldedClaim> = BTreeMap::new();
    for (_, record) in entries {
        apply_event(&mut folded, record);
    }

    // Deterministic row order: the whole point of a content-addressed publish is
    // that the same logical state always hashes the same, and BTreeMap iteration
    // order for claim_id already sorted this, but sorting explicitly here doesn't
    // depend on that being the map's iteration behavior forever.
    let mut claims: Vec<FoldedClaim> = folded.into_values().collect();
    claims.sort_by(|a, b| a.claim_id.cmp(&b.claim_id));
    Ok(claims)
}

/// 256-dim f32, little-endian — the exact byte shape [`SCHEMA_SQL`]'s `embedding`
/// column comment documents. Stores whatever length is actually given rather than
/// asserting it is exactly [`EMBEDDING_DIMENSIONS`]: a length mismatch is a bug in
/// whatever produced the vector, and refusing to publish an entire snapshot over
/// one claim's malformed embedding would be a much worse failure mode than a BLOB
/// a future reader has to validate before trusting.
fn embedding_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Build the SQLite file's bytes for one fold. A real on-disk temp file, not
/// `Connection::open_in_memory` plus `serialize()`: SQLite's serialize/deserialize
/// pair needs `sqlite3_malloc`-backed buffers and unsafe reconstruction on the read
/// side, where a plain temp file gives the exact same bytes this function needs to
/// publish through ordinary, safe file I/O — this artifact's home is object storage
/// either way, so "file-shaped from the start" costs nothing.
fn build_sqlite_bytes(
    claims: &[FoldedClaim],
    agent_reads_enabled: bool,
) -> Result<Vec<u8>, MaintError> {
    let file =
        tempfile::NamedTempFile::new().map_err(|e| MaintError::Other(format!("tempfile: {e}")))?;
    let path = file.path().to_path_buf();
    {
        let conn = rusqlite::Connection::open(&path)?;
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute(
            "INSERT INTO snapshot_meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        conn.execute(
            "INSERT INTO snapshot_meta (key, value) VALUES ('claim_count', ?1)",
            rusqlite::params![claims.len().to_string()],
        )?;
        conn.execute(
            "INSERT INTO snapshot_meta (key, value) VALUES ('embedding_dimensions', ?1)",
            rusqlite::params![EMBEDDING_DIMENSIONS.to_string()],
        )?;
        // Recorded so a reader can sanity-check the whole artifact's mode at a
        // glance, without scanning every row's `visible_to_agents` — see the
        // module doc's "the gate is a property of the artifact" paragraph.
        conn.execute(
            "INSERT INTO snapshot_meta (key, value) VALUES ('agent_reads_enabled', ?1)",
            rusqlite::params![agent_reads_enabled.to_string()],
        )?;

        {
            let mut stmt = conn.prepare(
                "INSERT INTO claims (claim_id, claim, claim_type, subject, scope, observed_by, \
                 status, evidence_count, independent_count, confidence, evidence_json, updated_at, \
                 visible_to_agents, embedding) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for c in claims {
                let evidence_json = serde_json::to_string(&c.evidence)?;
                let evidence_count = evidence_session_count(&c.evidence);
                let embedding_blob = c.embedding.as_deref().map(embedding_to_blob);
                // Only a claim that actually passed the gate (`status ==
                // "promoted"`) is ever a candidate for agent visibility — a
                // candidate or contested claim is invisible regardless of mode,
                // matching the belief wave's own `claims_visible_to_agents` rule.
                let visible_to_agents = agent_reads_enabled && c.status == "promoted";
                stmt.execute(rusqlite::params![
                    c.claim_id,
                    c.claim,
                    c.claim_type,
                    c.subject,
                    c.scope,
                    c.observed_by,
                    c.status,
                    evidence_count,
                    c.independent_count,
                    c.confidence,
                    evidence_json,
                    c.updated_at,
                    visible_to_agents,
                    embedding_blob,
                ])?;
            }
        }

        // Only rows the gate promoted *and* that this run's mode allows to be
        // read ever enter the search index — this is the one table a
        // memory-recall MCP tool would actually query into a context window, so
        // it is the one table that must be structurally incapable of surfacing a
        // shadow-mode or un-gated claim, not merely conventionally filtered by
        // whoever queries it later. See the module doc.
        conn.execute_batch(
            "INSERT INTO claims_fts (claim_id, claim, subject) \
             SELECT claim_id, claim, subject FROM claims \
             WHERE visible_to_agents = 1 ORDER BY claim_id;",
        )?;
    } // conn dropped and file closed before we read its bytes back.

    std::fs::read(&path).map_err(|e| MaintError::Other(format!("read sqlite bytes: {e}")))
}

/// What one [`publish`] call actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOutcome {
    pub content_hash: String,
    pub claim_count: usize,
    /// Echoes the `agent_reads_enabled` this run was called with — see the module
    /// doc. A different value for logically identical claims produces a different
    /// `content_hash`, which is correct: the mode is part of the published bytes.
    pub agent_reads_enabled: bool,
    /// False when the blob at this hash already existed (a re-run over an unchanged
    /// claim log) — no bytes were re-uploaded.
    pub blob_written: bool,
    /// False when `snapshot/latest.json` already named this exact hash, or when a
    /// concurrent publisher's equally-fresh write won the CAS race first (harmless —
    /// see the module doc's write-then-swap paragraph).
    pub pointer_updated: bool,
}

/// Fold `claims/events/` and publish the result. See the module doc for the full
/// contract, including why this is safe to call with an empty event log, and for
/// what `agent_reads_enabled` does and does not gate.
pub async fn publish(
    store: &dyn ObjectStore,
    agent_reads_enabled: bool,
) -> Result<SnapshotOutcome, MaintError> {
    let claims = fold_claim_events(store).await?;
    let claim_count = claims.len();
    let bytes = build_sqlite_bytes(&claims, agent_reads_enabled)?;
    let full_hash = hash::content_hash(&bytes);
    let content_hash = full_hash.trim_start_matches("sha256:").to_string();

    let blob_path = ctxlake_store::layout::snapshot(&content_hash);
    let blob_written = match store.head(&blob_path).await {
        Ok(_) => false, // this exact content is already published — content-addressed, so nothing to do.
        Err(OsError::NotFound { .. }) => {
            store.put(&blob_path, PutPayload::from(bytes)).await?;
            true
        }
        Err(e) => return Err(e.into()),
    };

    let pointer_path = ctxlake_store::layout::snapshot_latest();
    let new_pointer = serde_json::json!({ "content_hash": content_hash });
    let pointer_payload = || PutPayload::from(serde_json::to_vec(&new_pointer).expect("json"));

    let pointer_updated = match store.get(&pointer_path).await {
        Ok(res) => {
            let version = UpdateVersion {
                e_tag: res.meta.e_tag.clone(),
                version: res.meta.version.clone(),
            };
            let existing: serde_json::Value =
                serde_json::from_slice(&res.bytes().await?).unwrap_or_default();
            if existing.get("content_hash").and_then(|v| v.as_str()) == Some(content_hash.as_str())
            {
                false
            } else {
                match store
                    .put_opts(
                        &pointer_path,
                        pointer_payload(),
                        PutMode::Update(version).into(),
                    )
                    .await
                {
                    Ok(_) => true,
                    // A concurrent publisher's write landed first. Since the fold is
                    // deterministic, "first" here only matters if the two publishers
                    // saw genuinely different claim logs (a real, newer event
                    // arrived between our read and theirs) — either way, there is
                    // nothing to retry: the pointer names *a* valid, freshly-built
                    // snapshot, not a stale one. Same reasoning as
                    // `roster::build`'s `BuildOutcome::Skipped`.
                    Err(OsError::Precondition { .. }) => false,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        // First-ever publish for this fleet: Overwrite, not Create — there is no
        // "someone already holds this" state to protect against for a key with
        // exactly the same disposable-content property `roster::build` documents
        // for its own first publish, and `Create` is the one primitive AGENTS.md
        // invariant 4 forbids depending on (MinIO rejects it outright).
        Err(OsError::NotFound { .. }) => {
            store
                .put_opts(&pointer_path, pointer_payload(), PutMode::Overwrite.into())
                .await?;
            true
        }
        Err(e) => return Err(e.into()),
    };

    Ok(SnapshotOutcome {
        content_hash,
        claim_count,
        agent_reads_enabled,
        blob_written,
        pointer_updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;

    async fn append_claim_event(store: &dyn ObjectStore, path: &str, json: serde_json::Value) {
        store
            .put(
                &Path::from(path),
                PutPayload::from(serde_json::to_vec(&json).unwrap()),
            )
            .await
            .unwrap();
    }

    /// One `proposed` event, in the real producer's exact wire shape
    /// (`#[serde(tag = "kind")]`) — every test fixture below uses this rather than
    /// a hand-rolled JSON shape, so a test passing here is evidence this module
    /// actually understands the real format, not a shape this module's author
    /// merely assumed.
    fn proposed(
        claim_id: &str,
        claim: &str,
        sessions: &[&str],
        embedding: Option<Vec<f32>>,
    ) -> serde_json::Value {
        serde_json::json!({
            "kind": "proposed",
            "claim_id": claim_id,
            "claim": claim,
            "claim_type": "convention",
            "subject": "ctxlake",
            "scope": "agent",
            "observed_by": "cc-01",
            "observed_at": "2026-09-10T10:00:00Z",
            "evidence": sessions.iter().map(|s| serde_json::json!({
                "session_id": s, "message_id": "m1", "excerpt_hash": "sha256:deadbeef"
            })).collect::<Vec<_>>(),
            "embedding": embedding,
        })
    }

    fn promoted(claim_id: &str, independent_count: u32, confidence: f64) -> serde_json::Value {
        serde_json::json!({
            "kind": "promoted",
            "claim_id": claim_id,
            "at": "2026-09-11T10:00:00Z",
            "independent_count": independent_count,
            "confidence": confidence,
        })
    }

    /// Returns the `NamedTempFile` guard alongside the connection — dropping the
    /// guard early unlinks the file out from under any still-open `Connection`,
    /// which SQLite reports as `SQLITE_READONLY_CANTINIT` the moment anything tries
    /// to write again (its directory-entry checks fail against a deleted path).
    /// Callers must keep the returned guard alive for as long as the connection.
    fn open_published(bytes: &[u8]) -> (tempfile::NamedTempFile, rusqlite::Connection) {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), bytes).unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        (file, conn)
    }

    #[tokio::test]
    async fn folding_an_empty_log_produces_a_valid_sqlite_file_with_the_right_shape() {
        let store = InMemory::new();
        let outcome = publish(&store, true).await.unwrap();
        assert_eq!(outcome.claim_count, 0);
        assert!(outcome.blob_written);
        assert!(outcome.pointer_updated);

        let blob = store
            .get(&ctxlake_store::layout::snapshot(&outcome.content_hash))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let (_guard, conn) = open_published(&blob);
        let claim_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(claim_count, 0);
        // The schema itself — table + FTS5 index + embedding column — must exist
        // and be queryable even with zero rows, per the module doc's "the shape has
        // to be right before there is content."
        conn.execute(
            "INSERT INTO claims_fts (claim_id, claim, subject) VALUES ('probe', 'x', 'y')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM claims_fts WHERE claim_id = 'probe'", [])
            .unwrap();
        let embedding_dim: String = conn
            .query_row(
                "SELECT value FROM snapshot_meta WHERE key = 'embedding_dimensions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(embedding_dim, EMBEDDING_DIMENSIONS.to_string());
    }

    #[tokio::test]
    async fn folding_a_log_with_events_produces_the_correct_current_state() {
        let store = InMemory::new();
        // A claim proposed with one piece of evidence, then promoted — the
        // `promoted` event carries no claim text at all (matching the real
        // producer's shape), so the fold must retain claim-1's original content
        // while updating only status/independent_count/confidence.
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
            proposed("claim-1", "this repo uses just, not make", &["s1"], None),
        )
        .await;
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-11/agent=_gate/01B.json",
            promoted("claim-1", 2, 0.75),
        )
        .await;
        // A second, independent proposal for the same claim adds corroborating
        // evidence rather than overwriting the first.
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-03/00Z.json",
            proposed("claim-1", "this repo uses just, not make", &["s2"], None),
        )
        .await;
        // An unrelated, independent claim that only ever got proposed.
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-02/01C.json",
            proposed("claim-2", "staging SSH listens on 2222", &["s3"], None),
        )
        .await;

        let outcome = publish(&store, true).await.unwrap();
        assert_eq!(outcome.claim_count, 2);

        let blob = store
            .get(&ctxlake_store::layout::snapshot(&outcome.content_hash))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let (_guard, conn) = open_published(&blob);

        let (status, claim, evidence_count, independent_count, confidence): (
            String,
            String,
            i64,
            i64,
            f64,
        ) = conn
            .query_row(
                "SELECT status, claim, evidence_count, independent_count, confidence \
                 FROM claims WHERE claim_id = 'claim-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(
            status, "promoted",
            "the promotion event must win the status"
        );
        assert_eq!(
            claim, "this repo uses just, not make",
            "a status-only event must never blank out the claim text the proposal set"
        );
        assert_eq!(
            evidence_count, 2,
            "evidence from both proposals must be merged"
        );
        assert_eq!(independent_count, 2);
        assert!((confidence - 0.75).abs() < 1e-9);

        let (claim2_status, claim2_visible): (String, i64) = conn
            .query_row(
                "SELECT status, visible_to_agents FROM claims WHERE claim_id = 'claim-2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            claim2_status, "candidate",
            "a merely-proposed claim is a candidate"
        );
        assert_eq!(
            claim2_visible, 0,
            "a candidate must never be marked agent-visible, even with reads enabled"
        );

        // The promoted claim must be visible and FTS5-searchable...
        let claim1_visible: i64 = conn
            .query_row(
                "SELECT visible_to_agents FROM claims WHERE claim_id = 'claim-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claim1_visible, 1);
        let just_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claims_fts WHERE claims_fts MATCH 'just'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(just_hits, 1);

        // ...but the merely-candidate claim-2 must not be, even though this run
        // had agent reads enabled: promotion, not mode, is the FTS5 gate for an
        // individual row.
        let ssh_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claims_fts WHERE claims_fts MATCH 'ssh'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            ssh_hits, 0,
            "a candidate claim must never be FTS5-searchable, promoted or not"
        );
    }

    #[tokio::test]
    async fn a_status_only_event_for_an_unknown_claim_is_silently_ignored() {
        // A `promoted`/`contested`/`retired` event can only ever follow a
        // `proposed` one for the same claim_id in a well-formed log. If it
        // somehow doesn't (a torn write, a producer bug), there is nothing
        // sensible to construct from a status transition alone — matching the
        // real fold's own `if let Some(s) = out.get_mut(claim_id)` (no `else`).
        let store = InMemory::new();
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-11/agent=_gate/01B.json",
            promoted("never-proposed", 1, 0.5),
        )
        .await;
        let outcome = publish(&store, true).await.unwrap();
        assert_eq!(outcome.claim_count, 0);
    }

    #[tokio::test]
    async fn retired_and_superseded_both_fold_to_the_retired_status() {
        let store = InMemory::new();
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
            proposed("claim-1", "x", &["s1"], None),
        )
        .await;
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-11/agent=_gate/01B.json",
            serde_json::json!({"kind": "superseded", "claim_id": "claim-1", "at": "2026-09-11T00:00:00Z", "by": "claim-9"}),
        )
        .await;
        let outcome = publish(&store, true).await.unwrap();
        let blob = store
            .get(&ctxlake_store::layout::snapshot(&outcome.content_hash))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let (_guard, conn) = open_published(&blob);
        let status: String = conn
            .query_row(
                "SELECT status FROM claims WHERE claim_id = 'claim-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "retired");
    }

    #[tokio::test]
    async fn an_embedding_on_the_proposal_is_stored_as_a_little_endian_blob() {
        let store = InMemory::new();
        let vector: Vec<f32> = (0..EMBEDDING_DIMENSIONS)
            .map(|i| i as f32 * 0.001)
            .collect();
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
            proposed("claim-1", "x", &["s1"], Some(vector.clone())),
        )
        .await;
        let outcome = publish(&store, true).await.unwrap();
        let blob = store
            .get(&ctxlake_store::layout::snapshot(&outcome.content_hash))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let (_guard, conn) = open_published(&blob);
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT embedding FROM claims WHERE claim_id = 'claim-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored.len(), EMBEDDING_DIMENSIONS * 4);
        let decoded: Vec<f32> = stored
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, vector);
    }

    #[tokio::test]
    async fn the_same_input_twice_produces_the_same_content_hash() {
        let store_a = InMemory::new();
        let store_b = InMemory::new();
        for store in [&store_a, &store_b] {
            append_claim_event(
                store,
                "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
                proposed("claim-1", "same claim, same evidence", &["s1"], None),
            )
            .await;
        }
        let outcome_a = publish(&store_a, true).await.unwrap();
        let outcome_b = publish(&store_b, true).await.unwrap();
        assert_eq!(
            outcome_a.content_hash, outcome_b.content_hash,
            "identical logical claim state must hash identically — this is a \
             correctness property of content-addressing, not an optimization"
        );
    }

    #[tokio::test]
    async fn republishing_an_unchanged_log_does_not_rewrite_the_blob_or_the_pointer() {
        let store = InMemory::new();
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
            proposed("claim-1", "x", &[], None),
        )
        .await;
        let first = publish(&store, true).await.unwrap();
        assert!(first.blob_written);
        assert!(first.pointer_updated);

        let second = publish(&store, true).await.unwrap();
        assert_eq!(second.content_hash, first.content_hash);
        assert!(
            !second.blob_written,
            "identical content must not be re-uploaded"
        );
        assert!(
            !second.pointer_updated,
            "the pointer already names this exact hash; a redundant CAS write buys nothing"
        );
    }

    #[tokio::test]
    async fn a_malformed_claim_event_is_skipped_not_fatal() {
        let store = InMemory::new();
        store
            .put(
                &Path::from("claims/events/dt=2026-09-10/agent=cc-01/broken.json"),
                PutPayload::from_static(b"{not valid json"),
            )
            .await
            .unwrap();
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
            proposed("claim-1", "x", &[], None),
        )
        .await;

        let outcome = publish(&store, true).await.unwrap();
        assert_eq!(
            outcome.claim_count, 1,
            "the one well-formed event must still fold"
        );
    }

    /// The regression for the review finding: publishing with agent reads
    /// disabled must still run the full fold (arithmetic doesn't skip in shadow
    /// mode, per `docs/summarization.md`), but the artifact it produces must be
    /// structurally incapable of serving that promoted claim to an agent — no
    /// FTS5 hit, no `visible_to_agents` row — rather than merely trusting every
    /// future reader to remember to check the mode themselves.
    #[tokio::test]
    async fn shadow_mode_still_folds_but_publishes_nothing_agent_queryable() {
        let store = InMemory::new();
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-10/agent=cc-01/01A.json",
            proposed("claim-1", "this repo uses just, not make", &["s1"], None),
        )
        .await;
        append_claim_event(
            &store,
            "claims/events/dt=2026-09-11/agent=_gate/01B.json",
            promoted("claim-1", 2, 0.9),
        )
        .await;

        let outcome = publish(&store, false).await.unwrap();
        assert_eq!(
            outcome.claim_count, 1,
            "the fold itself must not be gated by agent_reads_enabled"
        );
        assert!(!outcome.agent_reads_enabled);

        let blob = store
            .get(&ctxlake_store::layout::snapshot(&outcome.content_hash))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let (_guard, conn) = open_published(&blob);

        // The full audit row survives — this is not an empty snapshot.
        let (status, visible): (String, i64) = conn
            .query_row(
                "SELECT status, visible_to_agents FROM claims WHERE claim_id = 'claim-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "promoted");
        assert_eq!(
            visible, 0,
            "a promoted claim must not be marked agent-visible when reads are disabled"
        );

        // ...but it must be structurally absent from the servable, queryable
        // index — this is the one table an MCP memory-recall tool would query
        // into a context window.
        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM claims_fts WHERE claims_fts MATCH 'just'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            hits, 0,
            "a shadow-mode snapshot must publish zero FTS5-discoverable claims, \
             even ones that were fully promoted"
        );

        let meta_flag: String = conn
            .query_row(
                "SELECT value FROM snapshot_meta WHERE key = 'agent_reads_enabled'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(meta_flag, "false");
    }
}
