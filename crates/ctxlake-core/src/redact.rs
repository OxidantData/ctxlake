//! Redaction — runs before anything is written to the spool.
//!
//! Bronze is immutable. A leaked key in bronze is permanent, so scrubbing happens in
//! the hook process before the line is written, never in a later pass. This applies
//! equally to `ctxlake import`: historical transcripts are the likeliest place an
//! un-redacted `cat .env` is already sitting.
//!
//! Four layers, cheapest first. The first three run on **every** path — a credential
//! pasted into a prompt is as permanent in bronze as one a command printed:
//!
//! 1. **Literal markers** — `sk-`, `AKIA`, `ghp_`, `gsk_`, PEM headers, `Bearer `,
//!    webhook hosts, and the env-var names that appear next to secrets. One
//!    Aho-Corasick pass. A marker means a credential is *near* but says nothing about
//!    where it ends, so the whole value is quarantined: guessing a secret's extent is
//!    how redactors leak.
//! 2. **Structural scanners** — `scheme://user:password@host` URIs, Luhn-valid card
//!    numbers, dashed US SSNs. These know their own extent exactly, so they replace
//!    just the matched span and leave the surrounding output readable. Dropping a
//!    5,000-line log because one line held a card number trains people to switch
//!    redaction off, which protects nothing.
//! 3. **Path denylist** — a read of `~/.aws/credentials`, any `.env`, a `.pem`, a
//!    `terraform.tfstate` has its *result* dropped entirely; the call is still recorded.
//! 4. **Entropy** — long base64/hex runs, in tool *output* only. Prose trips this far
//!    more readily than command output does, so prompts get the other three layers.
//!
//! Anything that trips a rule is reported rather than silently dropped, so the failure
//! is visible. We keep a hash of a quarantined value so dedup and counting still work
//! without holding it.
//!
//! **Layers 1–3 were audited against real inputs rather than trusted.** Before that
//! audit, a `.env` read was not denied at all (this doc claimed it was), and a
//! Postgres URL with an inline password, a Slack webhook, a Groq/HuggingFace/npm
//! token, a card number and an SSN all reached the spool untouched. The lesson worth
//! keeping: a redactor's coverage is whatever its tests actually exercise, never what
//! its module doc says.

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
    ("ya29.", "google_oauth_token"),
    ("-----BEGIN", "pem_block"),
    ("eyJhbGciOi", "jwt"),
    ("Authorization:", "authorization_header"),
    ("authorization:", "authorization_header"),
    ("Bearer ", "bearer_token"),
    ("aws_secret_access_key", "aws_secret_kv"),
    ("AWS_SECRET_ACCESS_KEY", "aws_secret_kv"),
    ("ANTHROPIC_API_KEY", "anthropic_key_kv"),
    ("PRIVATE KEY", "private_key"),
    // Added after a live audit found each of these reaching the spool untouched.
    ("gsk_", "groq_key"),
    ("hf_", "huggingface_token"),
    ("npm_", "npm_token"),
    ("xai-", "xai_key"),
    ("dop_v1_", "digitalocean_token"),
    ("shpat_", "shopify_access_token"),
    ("shpss_", "shopify_shared_secret"),
    ("SG.", "sendgrid_key"),
    ("glrt-", "gitlab_runner_token"),
    ("xoxa-", "slack_app_token"),
    ("xoxr-", "slack_refresh_token"),
    ("xapp-", "slack_app_level_token"),
    ("hooks.slack.com/services/", "slack_webhook"),
    ("discord.com/api/webhooks/", "discord_webhook"),
    ("rk_live_", "stripe_restricted_key"),
    ("PGPASSWORD", "postgres_password_kv"),
    ("MYSQL_PWD", "mysql_password_kv"),
    ("GOOGLE_APPLICATION_CREDENTIALS", "gcp_adc_kv"),
];

/// Path fragments whose *read results* are dropped wholesale.
const DENY_PATHS: &[&str] = &[
    "/.aws/credentials",
    "/.aws/config",
    "/.ssh/id_",
    "/.netrc",
    "/.npmrc",
    "/.pypirc",
    "/.docker/config.json",
    "/.kube/config",
    "/secrets/",
    "/vault/",
    // `.env` in any directory, not just Hermes's. This list previously carried
    // `/.hermes/.env` alone while this module's own doc claimed "a read of
    // ~/.aws/credentials or .env has its result dropped entirely" — so the single
    // most common secret file in existence went to the spool in full, and the
    // documentation said otherwise. Found by auditing the behaviour rather than
    // the prose.
    ".env",
    "/.git-credentials",
    "/.config/gh/hosts.yml",
    "/.config/gcloud/",
    "/.azure/",
    "/.gnupg/",
    "/terraform.tfstate",
    ".pem",
    ".key",
    ".p12",
    ".pfx",
    ".jks",
    "/id_rsa",
    "/id_ed25519",
    "/id_ecdsa",
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

        // Structural scanners run on every path, not just tool output. A credential
        // pasted into a prompt ("here's the db url: postgres://admin:hunter2@...") is
        // as permanent in bronze as one a command printed, and the entropy heuristic
        // that guards tool output is deliberately not applied to prose.
        for (n, replaced, rule) in [
            {
                let (n, r) = redact_uri_credentials(&out);
                (n, r, "uri_credentials")
            },
            {
                let (n, r) = redact_card_numbers(&out);
                (n, r, "card_number")
            },
            {
                let (n, r) = redact_ssns(&out);
                (n, r, "ssn")
            },
        ] {
            if n > 0 {
                out = replaced;
                rules.push(rule.to_string());
            }
        }

        if is_tool_output {
            let (n, replaced) = redact_high_entropy_runs(&out);
            if n > 0 {
                out = replaced;
                rules.push("high_entropy_run".to_string());
            }
        }

        if rules.is_empty() {
            (RedactionOutcome::Clean, out)
        } else {
            (RedactionOutcome::Redacted { rules }, out)
        }
    }
}

// ---------------------------------------------------------------------------
// Structural scanners — for secrets with no literal marker to match on.
//
// These differ from the marker layer in an important way: a marker tells you a
// credential is *near*, but not where it ends, which is why a marker quarantines the
// whole value ("guessing where a secret ends is how redactors leak"). A structural
// match knows its own extent exactly — the password in a URI ends at the `@`, a card
// number ends when the digits do — so it replaces just that span and leaves the rest
// of the output readable. Dropping a 5,000-line log because one line held a card
// number trains people to turn redaction off.
// ---------------------------------------------------------------------------

/// Replace the password in `scheme://user:password@host` style URIs.
///
/// Database URLs are the most common way a credential reaches a transcript without
/// looking like one: `postgres://admin:hunter2@db/prod` contains no marker, no
/// high-entropy run, and nothing a literal scan can find. Verified absent before this
/// existed — it went to the spool in full.
fn redact_uri_credentials(s: &str) -> (usize, String) {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    let mut n = 0;
    while let Some(scheme_end) = rest.find("://") {
        let after = scheme_end + 3;
        // The authority runs to the next '/', '?', whitespace or quote.
        let auth_end = rest[after..]
            .find(|c: char| c == '/' || c == '?' || c == '"' || c == '\'' || c.is_whitespace())
            .map(|i| after + i)
            .unwrap_or(rest.len());
        let authority = &rest[after..auth_end];
        // A password only exists when there is a colon before the '@'.
        match (authority.find('@'), authority.find(':')) {
            (Some(at), Some(colon)) if colon < at => {
                out.push_str(&rest[..after + colon + 1]);
                out.push_str("[ctxlake:redacted]");
                out.push_str(&rest[after + at..auth_end]);
                n += 1;
            }
            _ => out.push_str(&rest[..auth_end]),
        }
        rest = &rest[auth_end..];
    }
    out.push_str(rest);
    (n, out)
}

/// Luhn check — what makes card detection precise enough to act on.
///
/// Without it, "any 16 digits" flags order numbers, build ids and timestamps. With
/// it, roughly 9 in 10 random digit runs are rejected, so a hit is worth redacting
/// rather than worth arguing about.
fn passes_luhn(digits: &[u8]) -> bool {
    if digits.len() < 13 || digits.len() > 19 {
        return false;
    }
    let mut sum = 0u32;
    for (i, d) in digits.iter().rev().enumerate() {
        let mut v = u32::from(*d);
        if i % 2 == 1 {
            v *= 2;
            if v > 9 {
                v -= 9;
            }
        }
        sum += v;
    }
    sum.is_multiple_of(10)
}

/// Replace Luhn-valid card numbers, with or without separators.
fn redact_card_numbers(s: &str) -> (usize, String) {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut n = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            // Multi-byte safe: only ASCII digits start a run, so any other byte is
            // copied through as part of its own character.
            let ch = s[i..].chars().next().expect("index is on a char boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        // A run of digits possibly separated by single spaces or dashes.
        let start = i;
        let mut digits = Vec::new();
        let mut j = i;
        while j < bytes.len() {
            if bytes[j].is_ascii_digit() {
                digits.push(bytes[j] - b'0');
                j += 1;
            } else if (bytes[j] == b' ' || bytes[j] == b'-')
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
                && !digits.is_empty()
            {
                j += 1;
            } else {
                break;
            }
        }
        if passes_luhn(&digits) {
            out.push_str("[ctxlake:redacted-card]");
            n += 1;
        } else {
            out.push_str(&s[start..j]);
        }
        i = j;
    }
    (n, out)
}

/// Replace US social security numbers in their canonical `123-45-6789` form.
///
/// Only the dashed form: bare nine-digit runs are far more often an id, a zip+4 or a
/// phone number, and redacting those makes transcripts useless without protecting
/// anything. Area group `000`, `666` and `9xx` are never issued, so they are left
/// alone as a cheap false-positive guard.
fn redact_ssns(s: &str) -> (usize, String) {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        let fits = i + 11 <= b.len()
            && b[i..i + 3].iter().all(u8::is_ascii_digit)
            && b[i + 3] == b'-'
            && b[i + 4..i + 6].iter().all(u8::is_ascii_digit)
            && b[i + 6] == b'-'
            && b[i + 7..i + 11].iter().all(u8::is_ascii_digit)
            && (i == 0 || !b[i - 1].is_ascii_digit())
            && (i + 11 == b.len() || !b[i + 11].is_ascii_digit());
        if fits {
            let area = &s[i..i + 3];
            if area != "000" && area != "666" && !area.starts_with('9') {
                out.push_str("[ctxlake:redacted-ssn]");
                i += 11;
                n += 1;
                continue;
            }
        }
        let ch = s[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    (n, out)
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

    // ---- structural scanners: added after an audit found each of these reaching
    // the spool untouched on the prompt path ----

    #[test]
    fn a_password_in_a_database_url_is_replaced_but_the_url_survives() {
        let r = Redactor::new();
        let (o, out) = r.scrub(
            "DATABASE_URL=postgres://admin:hunter2@db.internal:5432/prod",
            false,
        );
        assert!(matches!(o, RedactionOutcome::Redacted { .. }), "{o:?}");
        assert!(!out.contains("hunter2"), "the password must be gone: {out}");
        assert!(
            out.contains("db.internal:5432/prod"),
            "the rest must survive: {out}"
        );
        assert!(out.contains("admin"), "the user is not the secret: {out}");
    }

    #[test]
    fn a_url_with_no_password_is_left_alone() {
        // `postgres://db.internal:5432/prod` has a colon in the authority and no
        // credential. Treating every colon as a password would redact every port.
        let r = Redactor::new();
        let (o, _) = r.scrub("listening on postgres://db.internal:5432/prod", false);
        assert!(matches!(o, RedactionOutcome::Clean), "{o:?}");
    }

    #[test]
    fn a_luhn_valid_card_is_redacted_and_the_line_survives() {
        let r = Redactor::new();
        let (o, out) = r.scrub("paid with 4111 1111 1111 1111 on tuesday", false);
        assert!(matches!(o, RedactionOutcome::Redacted { .. }), "{o:?}");
        assert!(!out.contains("4111"), "{out}");
        assert!(out.contains("on tuesday"), "context must survive: {out}");
    }

    #[test]
    fn long_digit_runs_that_are_not_cards_are_left_alone() {
        // Without the Luhn check this layer would flag order numbers, epoch
        // milliseconds and build ids — and a redactor that mangles ordinary output
        // gets switched off, which protects nothing.
        let r = Redactor::new();
        for s in [
            "order 1234567812345678 shipped",
            "epoch 1757700000000 ms",
            "commit a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
        ] {
            let (o, _) = r.scrub(s, false);
            assert!(matches!(o, RedactionOutcome::Clean), "{s:?} -> {o:?}");
        }
    }

    #[test]
    fn a_dashed_ssn_is_redacted_but_a_phone_number_is_not() {
        let r = Redactor::new();
        let (o, out) = r.scrub("employee ssn 123-45-6789 filed", false);
        assert!(matches!(o, RedactionOutcome::Redacted { .. }), "{o:?}");
        assert!(!out.contains("123-45-6789"), "{out}");

        for s in [
            "call 415-555-0134 tomorrow",
            "mail to 94105-1234",
            "id 900-45-6789",
        ] {
            let (o, _) = r.scrub(s, false);
            assert!(matches!(o, RedactionOutcome::Clean), "{s:?} -> {o:?}");
        }
    }

    #[test]
    fn dotenv_is_denied_in_any_directory() {
        // This list once held `/.hermes/.env` alone while this module's doc claimed
        // `.env` reads were dropped entirely — so the most common secret file there
        // is went to the spool in full, and the documentation said otherwise.
        let r = Redactor::new();
        for p in [
            "/home/alice/project/.env",
            "/home/alice/project/.env.local",
            "/srv/app/.env.production",
        ] {
            assert!(r.is_denied_path(p), "{p} must be denied");
        }
    }

    #[test]
    fn the_deny_list_covers_the_other_common_credential_files() {
        let r = Redactor::new();
        for p in [
            "/home/alice/.git-credentials",
            "/home/alice/certs/server.key",
            "/home/alice/certs/client.p12",
            "/home/alice/terraform.tfstate",
            "/home/alice/.config/gcloud/application_default_credentials.json",
            "/home/alice/.gnupg/secring.gpg",
            "/home/alice/.ssh/id_ed25519",
        ] {
            assert!(r.is_denied_path(p), "{p} must be denied");
        }
        assert!(
            !r.is_denied_path("/home/alice/project/src/main.rs"),
            "ordinary source files must not be denied"
        );
    }

    #[test]
    fn newer_provider_tokens_are_recognised() {
        let r = Redactor::new();
        for (s, why) in [
            ("GROQ_API_KEY=gsk_aBcDeFgHiJkLmNoPqRsTuVwXyZ01", "groq"),
            ("hf_aBcDeFgHiJkLmNoPqRsTuVwXyZ01234567", "huggingface"),
            ("_authToken=npm_aBcDeFgHiJkLmNoPqRs", "npm"),
            (
                "https://hooks.slack.com/services/T00/B00/XXXX",
                "slack webhook",
            ),
            ("ya29.a0AfB_byC-secret", "google oauth"),
            ("curl -H 'Bearer abcdefghijklmnop'", "bearer"),
            ("PGPASSWORD=correcthorse", "postgres password"),
        ] {
            let (o, out) = r.scrub(s, false);
            assert!(
                matches!(o, RedactionOutcome::Quarantined { .. }),
                "{why} must quarantine, got {o:?}"
            );
            assert!(out.contains("withheld"), "{why}: {out}");
        }
    }

    #[test]
    fn ordinary_prose_and_output_stay_clean() {
        // The cost of over-redacting is a tool nobody leaves switched on.
        let r = Redactor::new();
        for s in [
            "the test suite passes locally but CI keeps failing",
            "https://github.com/OxidantData/ctxlake/releases",
            "bumped 1.2.3 to 1.2.4",
            "test result: ok. 42 passed; 0 failed",
        ] {
            let (o, _) = r.scrub(s, false);
            assert!(matches!(o, RedactionOutcome::Clean), "{s:?} -> {o:?}");
        }
    }

    #[test]
    fn a_multibyte_character_next_to_a_scanner_hit_does_not_panic() {
        // Every scanner walks bytes; slicing one mid-character panics inside a hook,
        // on a session start. Transcripts are routinely non-ASCII.
        let r = Redactor::new();
        for s in [
            "café 4111 1111 1111 1111 ☕",
            "naïve 123-45-6789 ✓",
            "postgres://user:pw@hôte/db — ok",
        ] {
            let _ = r.scrub(s, true);
        }
    }

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
