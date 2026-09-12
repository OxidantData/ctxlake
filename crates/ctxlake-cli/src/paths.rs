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

/// `$XDG_DATA_HOME`, or `~/.local/share` — where the spool and cache live
/// (docs/architecture.md's component table).
pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".local").join("share"))
}

/// Where `ctxlake init` writes `ctxlake.toml` by default, and where every other
/// subcommand looks for it unless `--config` overrides the path.
pub fn default_config_path() -> PathBuf {
    config_home().join("ctxlake").join("ctxlake.toml")
}

/// `~/.local/share/ctxlake/cache/<fleet_id>/` — refreshed by `ctxlake sync`'s
/// store-to-cache leg. `ctxlake-hook` and `ctxlake-mcp` read it; nothing in this
/// crate writes to it, only reads, since populating it is the daemon's job.
pub fn cache_dir(fleet_id: &str) -> PathBuf {
    data_home().join("ctxlake").join("cache").join(fleet_id)
}

/// `~/.local/share/ctxlake/spool/<fleet_id>/` — appended to by `ctxlake-hook`,
/// drained by `ctxlake sync`. `doctor`'s backlog check only ever reads this.
pub fn spool_dir(fleet_id: &str) -> PathBuf {
    data_home().join("ctxlake").join("spool").join(fleet_id)
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

/// Hermes shell-hook config — see docs/runtimes/hermes.md.
pub fn hermes_config_path() -> PathBuf {
    home_dir().join(".hermes").join("config.yaml")
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
    fn cache_and_spool_are_scoped_per_fleet() {
        assert_ne!(cache_dir("fleet-a"), cache_dir("fleet-b"));
        assert_ne!(spool_dir("fleet-a"), spool_dir("fleet-b"));
        assert!(cache_dir("myteam").ends_with("cache/myteam"));
        assert!(spool_dir("myteam").ends_with("spool/myteam"));
    }
}
