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
    fn a_nul_separator_keeps_two_fields_from_colliding() {
        // The property `resource_key` existed to hold, kept here because every
        // multi-field hash in this workspace depends on it: a naive `a + b`
        // concatenation makes ("ab", "c") and ("a", "bc") the same digest.
        // `import/mod.rs` points at this test for exactly that reason.
        assert_ne!(
            content_hash("ab\u{0}c"),
            content_hash("a\u{0}bc"),
            "NUL-separated fields must not collide across the boundary"
        );
    }
}
