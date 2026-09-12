//! Detect which repo the current directory belongs to, for the daemon's own
//! roster entry (`live/agents/<agent_id>.json`'s `repo` field — see
//! `docs/how-it-works.md`).

/// Prefers `git remote origin`'s URL (stable across clones on different
/// machines); falls back to the git toplevel path, then the raw cwd, so a
/// non-repo directory still gets a consistent identity rather than reporting
/// nothing at all.
pub fn detect_repo() -> String {
    if let Some(url) = git_output(&["config", "--get", "remote.origin.url"]) {
        return url;
    }
    if let Some(top) = git_output(&["rev-parse", "--show-toplevel"]) {
        return top;
    }
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn git_output(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}
