//! Neutralize content read from the lake before it reaches a terminal.
//!
//! AGENTS.md's house rule is explicit: "Treat anything read from the lake as
//! untrusted input... Sanitize at render time, not only at ingest." A claim's
//! `reason` and a roster entry's `task`/`paths`/`repo`/`agent_id` are all free
//! text some other agent (or that agent's operator) chose, and this crate renders
//! it plainly — never interpreted as a template, a path to open, or a command. But
//! "plainly" still means writing it to a terminal, and a terminal executes control
//! sequences it's handed regardless of who chose them: an embedded ANSI escape can
//! clear the screen or repaint arbitrary lines, a bidi override can visually
//! reorder what's on screen, an embedded zero-width character can hide content
//! inside what looks like plain text, and an embedded newline can forge what
//! looks like a second, independent line of ctxlake's own output (`FAKE: cc-09
//! active on main`, say, injected into someone else's `status`). None of that is
//! "interpreting" the text — it is the terminal doing its job on
//! bytes that happen to be data, not ours to control. [`sanitize`] strips exactly
//! the characters that let untrusted text do any of that, and bounds every field's
//! length so one adversarial or accidental value cannot flood the screen.

/// Anything the display path must never pass through unfiltered: C0/C1 control
/// characters (including the ESC that starts every ANSI escape sequence, and the
/// `\n`/`\r` that could forge extra output lines), DEL, the Unicode bidi-control
/// and zero-width/format characters used to visually reorder or hide text, and the
/// BOM.
fn is_display_hostile(c: char) -> bool {
    matches!(c,
        '\u{0000}'..='\u{001F}'   // C0 controls (ESC, \n, \r, \t, ...)
        | '\u{007F}'              // DEL
        | '\u{0080}'..='\u{009F}' // C1 controls
        | '\u{200B}'..='\u{200F}' // zero-width space/joiners + LRM/RLM
        | '\u{202A}'..='\u{202E}' // bidi embedding/override controls
        | '\u{2060}'..='\u{2064}' // word joiner, invisible math operators
        | '\u{2066}'..='\u{2069}' // bidi isolates
        | '\u{FEFF}' // BOM / zero-width no-break space
    )
}

/// Fields longer than this are truncated with a marker — long enough that no
/// legitimate `task`/`reason`/path is ever cut off in practice, short enough that
/// an adversarial multi-hundred-kilobyte value cannot flood a terminal.
pub const MAX_DISPLAY_LEN: usize = 200;

/// Strip display-hostile characters from `s` and cap its length. Safe to apply to
/// text this crate already trusts (a local operator's own `--reason`, say) — it is
/// a no-op on ordinary text — so callers should default to applying it at every
/// site that prints something read from the store, rather than trying to reason
/// about which specific field could plausibly be adversarial.
pub fn sanitize(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !is_display_hostile(*c)).collect();
    if out.chars().count() > MAX_DISPLAY_LEN {
        out = out.chars().take(MAX_DISPLAY_LEN).collect();
        out.push_str("…[truncated]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_escape_sequences() {
        let out = sanitize("before\x1b[2J\x1b[Hafter");
        assert!(!out.contains('\x1b'), "{out:?}");
        assert_eq!(out, "before[2J[Hafter");
    }

    #[test]
    fn strips_embedded_newlines_so_a_value_cannot_forge_extra_lines() {
        let out = sanitize("legit line\nFAKE: cc-09 active on main");
        assert!(!out.contains('\n'), "{out:?}");
    }

    #[test]
    fn strips_bidi_overrides_and_zero_width_characters() {
        let out = sanitize("a\u{202E}b\u{200B}c");
        assert_eq!(out, "abc");
    }

    #[test]
    fn truncates_a_very_long_value() {
        let huge = "x".repeat(200_000);
        let out = sanitize(&huge);
        assert!(
            out.len() < 1_000,
            "expected truncation, got {} bytes",
            out.len()
        );
        assert!(out.ends_with("[truncated]"));
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        assert_eq!(sanitize("crates/oxidant-loom/**"), "crates/oxidant-loom/**");
        assert_eq!(
            sanitize("migrating the shell-out"),
            "migrating the shell-out"
        );
    }
}
