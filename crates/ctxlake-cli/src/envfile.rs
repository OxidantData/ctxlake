//! Loading `<config_home>/ctxlake/env` into the process environment.
//!
//! See [`crate::paths::env_file`] for why this file exists at all. In short: a
//! supervised daemon inherits neither your shell's `export`s nor your `.zshrc`, so a
//! provider key that `ctxlake doctor` resolved in a terminal is simply absent when the
//! daemon runs — and there is nowhere else to put it that is not either a config file
//! (invariant 10) or a world-readable unit file.
//!
//! Three rules, each of which exists because the alternative is a silent failure or a
//! leaked secret:
//!
//! - **Owner-only or refused.** A file holding an API key that is group- or
//!   world-readable is not protected by being outside `ctxlake.toml`. This refuses to
//!   read it and says so, rather than loading it and leaving the user believing the
//!   secret is handled.
//! - **Never overrides what is already set.** An operator who exported a key for one
//!   run means that one. Silently preferring a file on disk over an explicit
//!   `FOO=bar ctxlake maint` would be the kind of surprise that takes an afternoon.
//! - **Never logged, never echoed.** [`load`] reports how many variables it set and
//!   their *names*, never a value — the same rule `doctor` follows.

use std::collections::BTreeSet;
use std::path::Path;

/// What [`load`] did, for a caller that wants to report it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    /// Names set into the environment by this call. Never values.
    pub set: BTreeSet<String>,
    /// Names present in the file but already set in the environment, so left alone.
    pub skipped: BTreeSet<String>,
    /// Why the file was not read at all, when it exists but could not be used.
    pub refused: Option<String>,
}

/// Load the default env file, if it exists.
pub fn load_default() -> Loaded {
    load(&crate::paths::env_file())
}

/// Load `path` into the process environment.
///
/// A missing file is the normal case — most users configure no model at all, and
/// `claude-cli` and `ollama` need no key — so it is not an error and produces an
/// empty [`Loaded`].
pub fn load(path: &Path) -> Loaded {
    let mut out = Loaded::default();
    let Ok(meta) = std::fs::metadata(path) else {
        return out;
    };
    if let Some(why) = permission_problem(&meta) {
        out.refused = Some(why);
        return out;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        out.refused = Some(format!("{} could not be read", path.display()));
        return out;
    };
    for (name, value) in parse(&text) {
        if std::env::var_os(&name).is_some() {
            out.skipped.insert(name);
            continue;
        }
        // SAFETY: called once at startup, before any thread that reads the
        // environment is spawned — the same discipline `crates/oxidant-cli` uses for
        // its own pre-runtime env setup, and the reason `load_default` is called from
        // `main` rather than from inside a command.
        unsafe { std::env::set_var(&name, value) };
        out.set.insert(name);
    }
    out
}

/// `None` when the file's mode is safe for a secret.
///
/// Only the owner may read it. This is the same bar `ssh` holds private keys to, and
/// for the same reason: a file that any local process can read is not a place a
/// credential is stored, it is a place a credential is published.
#[cfg(unix)]
fn permission_problem(meta: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Some(format!(
            "refusing to read it: mode {mode:04o} lets other users read a file holding \
             credentials. Run `chmod 600` on it"
        ));
    }
    None
}

#[cfg(not(unix))]
fn permission_problem(_meta: &std::fs::Metadata) -> Option<String> {
    None
}

/// The variable names this file would provide, if it is readable and safely permissioned.
///
/// Used by `ctxlake doctor` to answer a question `std::env::var` cannot: not "is this
/// key set *here*", but "will it be set for the *daemon*". Those differ for every key
/// that only exists because a shell exported it, which is most of them.
///
/// Returns names only — never values, and never a hint of one.
pub fn names(path: &Path) -> BTreeSet<String> {
    let Ok(meta) = std::fs::metadata(path) else {
        return BTreeSet::new();
    };
    if permission_problem(&meta).is_some() {
        return BTreeSet::new();
    }
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text).into_iter().map(|(n, _)| n).collect(),
        Err(_) => BTreeSet::new(),
    }
}

/// `NAME=value` pairs, in file order.
///
/// Deliberately a small, boring subset rather than shell syntax: `KEY=value`, one per
/// line, `#` comments, optional `export ` prefix, optional surrounding quotes. It is
/// not a shell and must not pretend to be one — a file that looks like it supports
/// `$(...)` or variable interpolation but silently does not is worse than one that
/// obviously does neither.
fn parse(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        out.push((name.to_string(), value.to_string()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_a_person_actually_writes() {
        let got = parse(
            "# a comment\n\
             \n\
             OPENROUTER_API_KEY=sk-abc\n\
             export ANTHROPIC_API_KEY=\"sk-def\"\n\
               GEMINI_API_KEY='sk-ghi'  \n\
             not a pair\n\
             BAD-NAME=1\n",
        );
        assert_eq!(
            got,
            vec![
                ("OPENROUTER_API_KEY".to_string(), "sk-abc".to_string()),
                ("ANTHROPIC_API_KEY".to_string(), "sk-def".to_string()),
                ("GEMINI_API_KEY".to_string(), "sk-ghi".to_string()),
            ]
        );
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives_intact() {
        // Base64 and JWT-shaped credentials routinely contain `=`. Splitting on the
        // last one, or on all of them, silently truncates the key — and the failure
        // surfaces as a 401 that says nothing about this file.
        let got = parse("K=abc=def==\n");
        assert_eq!(got, vec![("K".to_string(), "abc=def==".to_string())]);
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(&dir.path().join("nope")), Loaded::default());
    }

    #[cfg(unix)]
    #[test]
    fn names_reports_what_the_daemon_will_see_and_nothing_from_an_unsafe_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("env");
        std::fs::write(&f, "OPENROUTER_API_KEY=sk-abc\n# note\n").unwrap();

        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(names(&f).contains("OPENROUTER_API_KEY"));

        // A file that `load` refuses must not be reported as providing anything
        // either, or `doctor` would tell an operator the daemon is covered when the
        // daemon is about to refuse the same file.
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(names(&f).is_empty(), "an unsafe file provides nothing");

        assert!(names(&dir.path().join("absent")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_file_is_refused_rather_than_loaded() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("env");
        std::fs::write(&f, "CTXLAKE_TEST_WORLD_READABLE=x\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();

        let got = load(&f);
        assert!(got.refused.is_some(), "must refuse: {got:?}");
        assert!(got.set.is_empty(), "must not load anything: {got:?}");
        assert!(
            std::env::var_os("CTXLAKE_TEST_WORLD_READABLE").is_none(),
            "a refused file must not reach the environment"
        );
        let why = got.refused.unwrap();
        assert!(why.contains("chmod 600"), "say the fix: {why}");
        assert!(!why.contains('x'), "never echo a value: {why}");
    }

    #[cfg(unix)]
    #[test]
    fn an_owner_only_file_loads_without_overriding_what_is_already_set() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("env");
        std::fs::write(
            &f,
            "CTXLAKE_TEST_ENVFILE_FRESH=loaded\nCTXLAKE_TEST_ENVFILE_PRESET=from_file\n",
        )
        .unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();

        // SAFETY: these two names are unique to this test; nothing else reads them.
        unsafe { std::env::set_var("CTXLAKE_TEST_ENVFILE_PRESET", "from_shell") };

        let got = load(&f);
        assert!(got.refused.is_none(), "{got:?}");
        assert!(got.set.contains("CTXLAKE_TEST_ENVFILE_FRESH"), "{got:?}");
        assert!(
            got.skipped.contains("CTXLAKE_TEST_ENVFILE_PRESET"),
            "{got:?}"
        );
        assert_eq!(
            std::env::var("CTXLAKE_TEST_ENVFILE_FRESH").unwrap(),
            "loaded"
        );
        assert_eq!(
            std::env::var("CTXLAKE_TEST_ENVFILE_PRESET").unwrap(),
            "from_shell",
            "an explicit export must win over a file on disk"
        );

        unsafe { std::env::remove_var("CTXLAKE_TEST_ENVFILE_FRESH") };
        unsafe { std::env::remove_var("CTXLAKE_TEST_ENVFILE_PRESET") };
    }
}
