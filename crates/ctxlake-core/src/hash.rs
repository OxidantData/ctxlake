//! Content hashing — the basis of dedup and idempotent replay.

use sha2::{Digest, Sha256};

/// `sha256:<hex>` over the given bytes.
///
/// Used for `content_hash` and `tool.input_hash`. Cross-runtime dedup matters more
/// than it looks: the same `CLAUDE.md` or `AGENTS.md` is read into every session, and
/// without dedup it dominates storage and skews every frequency count.
pub fn content_hash(s: impl AsRef<[u8]>) -> String {
    let mut h = Sha256::new();
    h.update(s.as_ref());
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// Stable pseudonymous id for a host. We never store a hostname.
pub fn host_id(hostname: &str) -> String {
    content_hash(hostname)
}

/// Key for a lease on a repo-relative path. Deterministic, so two agents derive the
/// same key for the same resource without coordinating.
pub fn resource_key(repo: &str, resource: &str) -> String {
    let h = content_hash(format!("{repo}\u{0}{resource}"));
    h.trim_start_matches("sha256:").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_prefixed() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert!(content_hash("abc").starts_with("sha256:"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
    }

    #[test]
    fn resource_key_is_collision_resistant_across_field_boundary() {
        // A naive `repo + resource` concatenation would make ("ab", "c") and
        // ("a", "bc") the same lease — two unrelated resources sharing a lock.
        assert_ne!(resource_key("ab", "c"), resource_key("a", "bc"));
    }

    #[test]
    fn resource_key_has_no_prefix_and_is_path_safe() {
        let k = resource_key("github.com/OxidantData/ctxlake", "crates/**");
        assert!(!k.contains(':'), "lease keys become object keys: {k}");
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
