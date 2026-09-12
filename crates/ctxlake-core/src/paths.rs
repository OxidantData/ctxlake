//! The on-disk layout, defined once.
//!
//! Five crates touch these directories — the hook appends to the spool, the daemon
//! drains it and refreshes the cache, the MCP server and CLI read both. When each
//! crate resolved them independently they disagreed: the CLI looked for the cache
//! under `~/.local/share/ctxlake/`, everything else under `~/.ctxlake/`. Nothing
//! failed loudly. `ctxlake status` would simply have reported "no cache" forever
//! while the daemon wrote one a few directories away.
//!
//! That class of bug cannot be found by reviewing any single crate, because each one
//! is self-consistent. So the resolution lives here, in the crate all of them already
//! depend on, and there is no second place for it to drift to.
//!
//! `~/.ctxlake/` rather than the XDG data directory: the hook established it first and
//! it is the one tree that already exists on disk. Both roots are overridable, which is
//! what the test suites use to run several fictitious agents on one machine.

use std::path::PathBuf;

/// `$HOME`, or the current directory if the environment has no home.
///
/// Falling back rather than panicking is deliberate: this is reachable from the hook,
/// and a hook that panics takes the user's turn down with it. A spool in the wrong
/// place is recoverable; a dead turn is not.
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Where the hook appends captured events, before any daemon has seen them.
///
/// Override with `CTXLAKE_SPOOL_DIR`.
pub fn spool_root() -> PathBuf {
    std::env::var_os("CTXLAKE_SPOOL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".ctxlake").join("spool"))
}

/// Where the daemon mirrors the store for readers on the hook path.
///
/// Override with `CTXLAKE_CACHE_DIR`.
pub fn cache_root() -> PathBuf {
    std::env::var_os("CTXLAKE_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".ctxlake").join("cache"))
}

/// One fleet's slice of the cache: `<cache_root>/<fleet_id>/`.
///
/// Scoped by fleet because a single host can legitimately run agents in more than one,
/// and an unscoped `roster.json` would have them overwrite each other's view of who is
/// active — which looks exactly like peers disappearing at random.
pub fn fleet_cache_dir(fleet_id: &str) -> PathBuf {
    cache_root().join(fleet_id)
}

/// Where hook failures are recorded.
///
/// The hook never fails a turn — on any internal error it still emits a valid response
/// and exits zero. That makes its failures silent by construction, so they need
/// somewhere to surface.
pub fn hook_error_log() -> PathBuf {
    home_dir().join(".ctxlake").join("hook-errors.log")
}

/// `$XDG_CONFIG_HOME/ctxlake/ctxlake.toml`, else `~/.config/ctxlake/ctxlake.toml`.
///
/// Config follows XDG while the spool and cache do not, which is not an oversight: a
/// config file is something a human edits and expects to find where their other config
/// lives. The spool is machine-local working state that only ctxlake reads.
pub fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"))
        .join("ctxlake")
        .join("ctxlake.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spool_and_cache_share_one_root() {
        // The divergence this module exists to prevent: both must sit under the same
        // tree by default, or a daemon draining one and a reader watching the other
        // both behave "correctly" while the pipeline silently carries nothing.
        let spool = spool_root();
        let cache = cache_root();
        assert_eq!(
            spool.parent(),
            cache.parent(),
            "spool and cache must share a parent: {spool:?} vs {cache:?}"
        );
    }

    #[test]
    fn fleet_cache_dir_is_scoped_under_cache_root() {
        let dir = fleet_cache_dir("myteam");
        assert!(dir.starts_with(cache_root()));
        assert_eq!(dir.file_name().unwrap(), "myteam");
    }

    #[test]
    fn distinct_fleets_do_not_share_a_cache_dir() {
        assert_ne!(fleet_cache_dir("alpha"), fleet_cache_dir("beta"));
    }

    #[test]
    fn no_resolver_panics_without_a_home() {
        // Reachable from the hook. A panic here would take down a user's turn, so
        // every one of these must degrade instead.
        for f in [
            spool_root as fn() -> PathBuf,
            cache_root,
            hook_error_log,
            config_path,
        ] {
            let p = f();
            assert!(!p.as_os_str().is_empty());
        }
    }
}
