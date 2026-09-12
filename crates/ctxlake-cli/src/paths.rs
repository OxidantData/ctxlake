//! Local filesystem locations `ctxlake` reads and writes.
//!
//! Every function here is a pure computation over environment variables — no
//! `directories`-style crate, matching the no-extra-dependency approach
//! `ctxlake-hook::hostinfo` already takes, and keeping every location injectable in
//! tests. Callers that need a *different* root for a test (a tempdir standing in for
//! `$HOME`) should build the path themselves rather than mutating `HOME` for the
//! process: `std::env::set_var` is global, shared across every test thread in this
//! binary, and any test relying on it would be a coin flip depending on what else is
//! running (see AGENTS.md house rules on flaky tests).

use std::path::PathBuf;

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_CONFIG_HOME`, or `~/.config` per the XDG base directory spec.
pub fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
}

/// Where `ctxlake init` writes `ctxlake.toml` by default, and where every other
/// subcommand looks for it unless `--config` overrides the path.
pub fn default_config_path() -> PathBuf {
    config_home().join("ctxlake").join("ctxlake.toml")
}

/// One fleet's cache slice, resolved by [`ctxlake_core::paths`] so this crate cannot
/// drift from the daemon that writes it or the MCP server that also reads it.
///
/// This used to resolve to `~/.local/share/ctxlake/cache/<fleet_id>/` while every other
/// crate used `~/.ctxlake/cache/`. Both were self-consistent, nothing failed, and
/// `ctxlake status` would have reported "no cache" forever against a cache the daemon
/// was faithfully writing a few directories away.
pub fn cache_dir(fleet_id: &str) -> PathBuf {
    ctxlake_core::paths::fleet_cache_dir(fleet_id)
}

/// The spool root `ctxlake-hook` actually appends to and `ctxlake sync` drains:
/// `$CTXLAKE_SPOOL_DIR`, or `~/.ctxlake/spool` if unset — see
/// `crates/ctxlake-hook/src/spool.rs::spool_root`, which this mirrors exactly.
///
/// This is deliberately **not** `$XDG_DATA_HOME`-based or fleet-scoped, unlike
/// [`cache_dir`] — an earlier version of this function was both, which meant
/// `doctor`'s backlog check watched a directory nothing ever wrote to and reported
/// an empty spool forever. The hook can't scope its own writes by fleet because
/// `fleet_id`/`agent_id` come from the environment and may be unset at capture time
/// (AGENTS.md: "the spool is partitioned by runtime, not fleet/agent") — a path
/// built from a value that might not exist cannot reliably be found again later, so
/// the hook partitions by runtime only, and this must read the exact same root.
pub fn spool_root() -> PathBuf {
    std::env::var_os("CTXLAKE_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".ctxlake").join("spool"))
}

/// User-scope Claude Code settings. `ctxlake install` also supports a project-scope
/// `.claude/settings.json` via `--project`; this is the default target.
pub fn claude_code_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

/// User-scope Cursor hooks config.
pub fn cursor_hooks_path() -> PathBuf {
    home_dir().join(".cursor").join("hooks.json")
}

/// Hermes shell-hook config — see docs/runtimes.md § Hermes.
pub fn hermes_config_path() -> PathBuf {
    home_dir().join(".hermes").join("config.yaml")
}

/// Hermes's own SQLite state database — the source `ctxlake import --runtime hermes`
/// reads (see `import/hermes.rs` and docs/adding-it.md).
///
/// This is a *live* database belonging to another process, which is why the importer
/// opens it read-only and immutable rather than ever writing anywhere near it.
/// Nothing in ctxlake creates this file; a missing one simply means Hermes has never
/// run on this host.
pub fn hermes_state_db_path() -> PathBuf {
    home_dir().join(".hermes").join("state.db")
}

/// Where `ctxlake import` persists its dedup ledger for one runtime.
///
/// Deliberately **not** inside the spool: `ctxlake sync` drains and deletes spool
/// files once they reach the store, so a ledger living there would be erased by a
/// successful upload and the next import would replay every event it had already
/// sent. Bronze is immutable — a duplicated event is permanent — so the ledger has
/// to outlive the thing it guards.
pub fn import_ledger_path(runtime: &str) -> PathBuf {
    home_dir()
        .join(".ctxlake")
        .join("import")
        .join(format!("{runtime}.ledger"))
}

/// Where `ctxlake sync`'s background pidfile lives, scoped per fleet — two fleets
/// running on one host each get their own daemon slot rather than fighting over
/// (or silently sharing) a single pidfile. See `sync_cmd.rs`.
pub fn pid_file(fleet_id: &str) -> PathBuf {
    home_dir()
        .join(".ctxlake")
        .join("run")
        .join(format!("{fleet_id}.pid"))
}

/// Where a backgrounded `ctxlake sync`'s stdout/stderr are redirected — see
/// `sync_cmd.rs`'s daemonization doc for why this isn't a true detached daemon with
/// no controlling output at all.
pub fn sync_log_file(fleet_id: &str) -> PathBuf {
    home_dir()
        .join(".ctxlake")
        .join("run")
        .join(format!("{fleet_id}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_is_under_config_home() {
        let p = default_config_path();
        assert!(p.ends_with("ctxlake/ctxlake.toml"), "got: {}", p.display());
    }

    #[test]
    fn cache_is_scoped_per_fleet() {
        assert_ne!(cache_dir("fleet-a"), cache_dir("fleet-b"));
        assert!(cache_dir("myteam").ends_with("cache/myteam"));
    }

    #[test]
    fn the_import_ledger_outlives_the_spool_it_guards() {
        // Regression guard for the one way this ledger can be silently useless: if
        // it lived under the spool root, `ctxlake sync`'s post-upload cleanup would
        // delete it and the next `ctxlake import` would replay every event into
        // immutable bronze a second time.
        let ledger = import_ledger_path("hermes");
        assert!(
            !ledger.starts_with(spool_root()),
            "the ledger must not live under the spool the daemon deletes: {}",
            ledger.display()
        );
        assert!(ledger.ends_with("import/hermes.ledger"), "{ledger:?}");
        assert_ne!(import_ledger_path("hermes"), import_ledger_path("cursor"));
    }

    #[test]
    fn hermes_state_db_sits_next_to_the_hermes_config() {
        // Both live in Hermes's own home directory; if one moved without the other,
        // `doctor` would report Hermes installed while import found nothing to read.
        let db = hermes_state_db_path();
        assert_eq!(db.parent(), hermes_config_path().parent());
        assert!(db.ends_with(".hermes/state.db"), "{db:?}");
    }

    #[test]
    fn pid_and_log_files_are_scoped_per_fleet() {
        assert_ne!(pid_file("fleet-a"), pid_file("fleet-b"));
        assert_ne!(sync_log_file("fleet-a"), sync_log_file("fleet-b"));
        assert!(pid_file("myteam").ends_with("run/myteam.pid"));
        assert!(sync_log_file("myteam").ends_with("run/myteam.log"));
    }

    #[test]
    fn spool_root_matches_where_ctxlake_hook_actually_writes() {
        // Regression test: this used to be `$XDG_DATA_HOME/ctxlake/spool/<fleet_id>`
        // — a directory `ctxlake-hook` (`crates/ctxlake-hook/src/spool.rs`) never
        // writes a single byte to, since it has no reliable `fleet_id` to scope by.
        // The hook's own default is `~/.ctxlake/spool`, with no `.local/share` and
        // no fleet segment; `doctor`'s backlog check must watch that exact
        // directory or it reports "0 files" against a real, growing backlog forever.
        let root = spool_root();
        assert!(
            root.ends_with(".ctxlake/spool"),
            "must match ctxlake-hook::spool::spool_root's default, got: {}",
            root.display()
        );
        assert!(
            !root.to_string_lossy().contains(".local/share"),
            "the hook never writes under $XDG_DATA_HOME, got: {}",
            root.display()
        );
    }
}
