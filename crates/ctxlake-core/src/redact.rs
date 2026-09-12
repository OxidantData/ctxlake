//! Redaction — runs before anything is written to the spool.
//!
//! Bronze is immutable. A leaked key in bronze is permanent, so scrubbing happens in
//! the hook process before the line is written, never in a later pass. This applies
//! equally to `ctxlake import`: historical transcripts are the likeliest place an
//! un-redacted `cat .env` is already sitting.
//!
//! Three layers, cheapest first:
//!
//! 1. **Literal prefixes** — `sk-`, `AKIA`, `ghp_`, PEM headers, `Authorization:`.
//!    Matched with Aho-Corasick in one pass over the input.
//! 2. **Entropy** — long base64/hex runs in tool *output*, which is where secrets
//!    actually leak (a `cat .env`, a `printenv`, a curl echoing its headers).
//! 3. **Path denylist** — a read of `~/.aws/credentials` or `.env` has its *result*
//!    dropped entirely; the call itself is still recorded.
//!
//! Anything that trips a rule is quarantined rather than silently dropped, so the
//! failure is visible. We keep a hash of the redacted span so dedup and counting still
//! work without holding the value.

use aho_corasick::AhoCorasick;

/// Literal markers that indicate a credential. Order matters only for reporting.
const SECRET_MARKERS: &[(&str, &str)] = &[
    ("sk-", "anthropic_or_openai_key"),
    ("sk_live_", "stripe_live_key"),
    ("sk_test_", "stripe_test_key"),
    ("AKIA", "aws_access_key_id"),
    ("ASIA", "aws_session_key_id"),
    ("ghp_", "github_pat"),
    ("gho_", "github_oauth"),
    ("ghs_", "github_server_token"),
    ("github_pat_", "github_fine_grained_pat"),
    ("xoxb-", "slack_bot_token"),
    ("xoxp-", "slack_user_token"),
    ("glpat-", "gitlab_pat"),
    ("AIza", "google_api_key"),
    ("-----BEGIN", "pem_block"),
    ("eyJhbGciOi", "jwt"),
    ("Authorization:", "authorization_header"),
    ("authorization:", "authorization_header"),
    ("aws_secret_access_key", "aws_secret_kv"),
    ("AWS_SECRET_ACCESS_KEY", "aws_secret_kv"),
    ("ANTHROPIC_API_KEY", "anthropic_key_kv"),
    ("PRIVATE KEY", "private_key"),
];

/// Path fragments whose *read results* are dropped wholesale.
const DENY_PATHS: &[&str] = &[
    "/.aws/credentials",
    "/.aws/config",
    "/.ssh/id_",
    "/.hermes/.env",
    "/.netrc",
    "/.npmrc",
    "/.pypirc",
    "/.docker/config.json",
    "/.kube/config",
    "/secrets/",
    "/vault/",
];

/// Minimum run length before the entropy check considers a token suspicious.
const ENTROPY_MIN_LEN: usize = 32;
/// Shannon entropy per character, above which a long alphanumeric run is treated as
/// a secret. Base64-encoded random data sits near 6.0; English prose sits near 4.0;
/// a hex digest sits near 4.0 but is caught by its length and charset instead.
const ENTROPY_THRESHOLD: f64 = 4.5;
/// Above this, scan only a prefix. A 10 MB tool result should not cost 10 MB of
/// scanning on a path budgeted at 5ms.
const MAX_SCAN_BYTES: usize = 256 * 1024;

/// What redaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactionOutcome {
    /// Nothing matched; content passes through unchanged.
    Clean,
    /// Spans were replaced. Content is safe to store in bronze.
    Redacted { rules: Vec<String> },
    /// The whole value is withheld and routed to `quarantine/`.
    Quarantined { rules: Vec<String> },
}

impl RedactionOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            RedactionOutcome::Clean => "clean",
            RedactionOutcome::Redacted { .. } => "redacted",
            RedactionOutcome::Quarantined { .. } => "quarantined",
        }
    }
    pub fn rules(&self) -> &[String] {
        match self {
            RedactionOutcome::Clean => &[],
            RedactionOutcome::Redacted { rules } | RedactionOutcome::Quarantined { rules } => rules,
        }
    }
    pub fn is_clean(&self) -> bool {
        matches!(self, RedactionOutcome::Clean)
    }
}

/// Scrubs secrets out of captured content. Construct once and reuse; building the
/// automaton is the expensive part and the hook budget is 5ms.
pub struct Redactor {
    markers: AhoCorasick,
    marker_rules: Vec<String>,
    deny: AhoCorasick,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    pub fn new() -> Self {
        let patterns: Vec<&str> = SECRET_MARKERS.iter().map(|(p, _)| *p).collect();
        let marker_rules = SECRET_MARKERS
            .iter()
            .map(|(_, r)| (*r).to_string())
            .collect();
        Self {
            markers: AhoCorasick::new(patterns).expect("static secret markers are valid"),
            marker_rules,
            deny: AhoCorasick::new(DENY_PATHS).expect("static deny paths are valid"),
        }
    }

    /// True if reading this path should have its result withheld entirely.
    pub fn is_denied_path(&self, path: &str) -> bool {
        self.deny.is_match(path)
    }

    /// Scrub `input`, returning the outcome and the text safe to persist.
    ///
    /// `is_tool_output` enables the entropy heuristic. Prose triggers false positives
    /// far more readily than command output does, so prompts and assistant messages
    /// get literal matching only.
    pub fn scrub(&self, input: &str, is_tool_output: bool) -> (RedactionOutcome, String) {
        let scan_len = input.len().min(MAX_SCAN_BYTES);
        let scan = &input[..floor_char_boundary(input, scan_len)];

        let mut rules: Vec<String> = Vec::new();
        for m in self.markers.find_iter(scan) {
            let rule = &self.marker_rules[m.pattern().as_usize()];
            if !rules.iter().any(|r| r == rule) {
                rules.push(rule.clone());
            }
        }

        let mut out = if rules.is_empty() {
            input.to_string()
        } else {
            // A credential marker means the surrounding value is untrustworthy, so we
            // withhold the whole thing rather than trying to find the value's extent.
            // Guessing where a secret ends is how redactors leak.
            return (
                RedactionOutcome::Quarantined { rules },
                format!(
                    "[ctxlake: withheld, {} bytes, {}]",
                    input.len(),
                    crate::hash::content_hash(input)
                ),
            );
        };

        if is_tool_output {
            let (n, replaced) = redact_high_entropy_runs(&out);
            if n > 0 {
                out = replaced;
                rules.push("high_entropy_run".to_string());
                return (RedactionOutcome::Redacted { rules }, out);
            }
        }

        (RedactionOutcome::Clean, out)
    }
}

/// Replace long, high-entropy alphanumeric runs with a placeholder.
fn redact_high_entropy_runs(s: &str) -> (usize, String) {
    let mut out = String::with_capacity(s.len());
    let mut count = 0usize;
    let mut run = String::new();

    let flush = |run: &mut String, out: &mut String, count: &mut usize| {
        if run.len() >= ENTROPY_MIN_LEN && shannon_entropy(run) >= ENTROPY_THRESHOLD {
            out.push_str("[ctxlake:redacted-secret]");
            *count += 1;
        } else {
            out.push_str(run);
        }
        run.clear();
    };

    for ch in s.chars() {
        if is_secret_charset(ch) {
            run.push(ch);
        } else {
            flush(&mut run, &mut out, &mut count);
            out.push(ch);
        }
    }
    flush(&mut run, &mut out, &mut count);
    (count, out)
}

/// Characters that appear in base64/base64url/hex-encoded secrets.
fn is_secret_charset(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' || c == '-' || c == '_'
}

fn shannon_entropy(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    let bytes = s.as_bytes();
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// `str::floor_char_boundary` is still unstable, so we do it by hand rather than
/// risk slicing through a multi-byte character and panicking inside a hook.
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_prose_passes_through_unchanged() {
        let r = Redactor::new();
        let text = "I refactored the Glue catalog to stop shelling out to the AWS CLI.";
        let (outcome, out) = r.scrub(text, true);
        assert_eq!(outcome, RedactionOutcome::Clean);
        assert_eq!(out, text);
    }

    #[test]
    fn quarantines_known_key_prefixes() {
        let r = Redactor::new();
        for probe in [
            "export ANTHROPIC_API_KEY=sk-ant-api03-abcdef",
            "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
            "token: ghp_16CharactersHereAndMore",
            "-----BEGIN RSA PRIVATE KEY-----",
            "Authorization: Bearer abc.def.ghi",
        ] {
            let (outcome, out) = r.scrub(probe, true);
            assert!(
                matches!(outcome, RedactionOutcome::Quarantined { .. }),
                "should quarantine: {probe}"
            );
            assert!(!out.contains("AKIA") && !out.contains("sk-ant") && !out.contains("ghp_"));
            assert!(out.contains("withheld"), "must say why: {out}");
        }
    }

    #[test]
    fn quarantine_keeps_a_hash_so_dedup_still_works() {
        let r = Redactor::new();
        let (_, a) = r.scrub("AKIAIOSFODNN7EXAMPLE", true);
        let (_, b) = r.scrub("AKIAIOSFODNN7EXAMPLE", true);
        let (_, c) = r.scrub("AKIAIOSFODNN7DIFFERENT", true);
        assert_eq!(a, b, "same secret must produce the same placeholder");
        assert_ne!(a, c, "different secrets must be distinguishable");
    }

    #[test]
    fn entropy_catches_an_unprefixed_secret_in_tool_output() {
        let r = Redactor::new();
        let leaked = "DB_PASS=k3Jx9QfZ2mNpR7vTbY4wL8sH1dG6cV0aE5uI3oP9zXrK";
        let (outcome, out) = r.scrub(leaked, true);
        assert!(
            matches!(outcome, RedactionOutcome::Redacted { .. }),
            "entropy should fire, got {outcome:?}"
        );
        assert!(out.contains("[ctxlake:redacted-secret]"), "got: {out}");
    }

    #[test]
    fn entropy_is_not_applied_to_prose() {
        // Prose false-positives are costly: they silently destroy the content that
        // makes a briefing useful. Tool output is where secrets actually leak.
        let r = Redactor::new();
        let prose = "Considered supercalifragilisticexpialidociousandthensome as a name";
        let (outcome, _) = r.scrub(prose, false);
        assert_eq!(outcome, RedactionOutcome::Clean);
    }

    #[test]
    fn ordinary_long_identifiers_survive() {
        // A git sha, a ULID, and a Rust path are all long-ish. Redacting them would
        // gut the episodic layer, which is built out of exactly these.
        let r = Redactor::new();
        for benign in [
            "commit a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
            "crates/oxidant-loom/src/catalog_bridge.rs",
            "running cargo test -p oxidant-connect --all-features",
        ] {
            let (outcome, out) = r.scrub(benign, true);
            assert_eq!(outcome, RedactionOutcome::Clean, "over-redacted: {benign}");
            assert_eq!(out, benign);
        }
    }

    #[test]
    fn denied_paths_are_recognized() {
        let r = Redactor::new();
        assert!(r.is_denied_path("/home/alice/.aws/credentials"));
        assert!(r.is_denied_path("/home/x/.ssh/id_ed25519"));
        assert!(r.is_denied_path("/etc/secrets/token"));
        assert!(!r.is_denied_path("/home/alice/projects/ctxlake/README.md"));
    }

    #[test]
    fn multibyte_input_at_the_scan_cap_does_not_panic() {
        // The scan cap slices the input. Slicing through a multi-byte character
        // panics, and a panic inside a hook takes the agent's turn down with it.
        let r = Redactor::new();
        let big = "é".repeat(MAX_SCAN_BYTES);
        let (outcome, _) = r.scrub(&big, true);
        assert!(outcome.is_clean());
    }

    #[test]
    fn empty_input_is_clean() {
        let r = Redactor::new();
        assert!(r.scrub("", true).0.is_clean());
    }
}
