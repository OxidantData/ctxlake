//! Local identity for the envelope: fleet/agent id and a hashed host id.
//!
//! Wave 1 ships no config file — `ctxlake.toml` lands with the CLI/install wave — so
//! these come from environment variables the installer sets into the hook's command
//! environment, the same pattern the existing Cursor/Claude Code hook shims already
//! use for passing per-install context (see `~/.orca/agent-hooks/*`). An unset var
//! falls back to a visible placeholder rather than failing the hook: a mis-attributed
//! event is fixable later by whoever reads bronze; a dropped event is not.

const DEFAULT_FLEET_ID: &str = "unconfigured-fleet";
const DEFAULT_AGENT_ID: &str = "unconfigured-agent";

pub fn fleet_id() -> String {
    resolve_env("CTXLAKE_FLEET_ID", DEFAULT_FLEET_ID)
}

pub fn agent_id() -> String {
    resolve_env("CTXLAKE_AGENT_ID", DEFAULT_AGENT_ID)
}

fn resolve_env(key: &str, default: &str) -> String {
    resolve(std::env::var(key).ok().as_deref(), default)
}

/// Pure fallback logic, split out so it can be unit tested without mutating process
/// environment — `cargo test` runs tests in one process, and two tests racing to
/// set/unset the same env var is a real flake, not a hypothetical one.
fn resolve(value: Option<&str>, default: &str) -> String {
    match value {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => default.to_string(),
    }
}

/// Best-effort hostname, without adding a dependency. `HOSTNAME` is rarely exported
/// by interactive shells on macOS, so this is a known gap — see `docs/runtimes/` —
/// until a later wave either links a tiny libc shim or has the installer stamp a
/// stable machine id itself instead of relying on the hostname at all.
pub fn hostname() -> String {
    pick_hostname(
        std::env::var("HOSTNAME").ok(),
        std::env::var("COMPUTERNAME").ok(),
        std::fs::read_to_string("/etc/hostname").ok(),
    )
}

fn pick_hostname(
    env_hostname: Option<String>,
    env_computername: Option<String>,
    etc_hostname: Option<String>,
) -> String {
    for h in [env_hostname, env_computername, etc_hostname]
        .into_iter()
        .flatten()
    {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    "unknown-host".to_string()
}

/// Hashed, never the raw hostname — the envelope's `host_id` field is explicit that
/// bronze must not carry it (see `envelope.rs`).
pub fn host_id() -> String {
    ctxlake_core::hash::host_id(&hostname())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_falls_back_on_missing_or_empty() {
        assert_eq!(resolve(None, "default"), "default");
        assert_eq!(resolve(Some(""), "default"), "default");
        assert_eq!(resolve(Some("set"), "default"), "set");
    }

    #[test]
    fn pick_hostname_prefers_hostname_env_then_computername_then_etc() {
        assert_eq!(
            pick_hostname(Some("h".into()), Some("c".into()), Some("e".into())),
            "h"
        );
        assert_eq!(pick_hostname(None, Some("c".into()), Some("e".into())), "c");
        assert_eq!(pick_hostname(None, None, Some("e".into())), "e");
        assert_eq!(pick_hostname(None, None, None), "unknown-host");
    }

    #[test]
    fn pick_hostname_skips_blank_candidates() {
        // /etc/hostname commonly ends in a trailing newline; a blank env var should
        // not win over a real value further down the chain.
        assert_eq!(
            pick_hostname(Some("  \n".into()), None, Some("real\n".into())),
            "real"
        );
    }

    #[test]
    fn host_id_is_a_stable_hash_not_the_raw_hostname() {
        let h = hostname();
        let id = host_id();
        assert!(id.starts_with("sha256:"), "got: {id}");
        assert_ne!(id, h, "host_id must not be the plain hostname");
        assert_eq!(id, host_id(), "same host must hash the same every call");
        assert_eq!(id, ctxlake_core::hash::host_id(&h));
    }
}
