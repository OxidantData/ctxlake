//! Asserts the Rust redactor against the shared cross-language corpus.
//!
//! The Hermes adapter runs in-process in Python and cannot call this redactor, so the
//! logic exists twice. `tests/redaction-corpus.json` is the contract both halves answer
//! to; `adapters/hermes/tests/` runs the same file. Drift in either direction becomes a
//! test failure instead of a silent hole in one runtime's capture.

use ctxlake_core::{RedactionOutcome, Redactor};
use std::path::PathBuf;

fn corpus() -> serde_json::Value {
    // CARGO_MANIFEST_DIR is crates/ctxlake-core; the corpus is shared, so it lives at
    // the workspace root where the Python suite can reach it too.
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/redaction-corpus.json")
        .canonicalize()
        .expect("shared corpus must exist at tests/redaction-corpus.json");
    let raw = std::fs::read_to_string(&path).expect("corpus readable");
    serde_json::from_str(&raw).expect("corpus is valid JSON")
}

#[test]
fn every_corpus_vector_matches() {
    let c = corpus();
    let r = Redactor::new();
    let vectors = c["vectors"].as_array().expect("vectors array");
    assert!(!vectors.is_empty(), "corpus must not be empty");

    let mut failures = Vec::new();
    for v in vectors {
        let name = v["name"].as_str().unwrap();
        let input = v["input"].as_str().unwrap();
        let is_tool_output = v["is_tool_output"].as_bool().unwrap();
        let expect = v["expect"].as_str().unwrap();

        let (outcome, out) = r.scrub(input, is_tool_output);
        let got = outcome.status();
        if got != expect {
            failures.push(format!("  {name}: expected {expect}, got {got}"));
            continue;
        }

        // Beyond the label: a non-clean outcome must not leak the original bytes.
        if !matches!(outcome, RedactionOutcome::Clean) && !input.is_empty() {
            if let RedactionOutcome::Quarantined { .. } = outcome {
                assert!(
                    !out.contains(input),
                    "{name}: quarantined output still contains the original value"
                );
            }
        }
    }
    assert!(
        failures.is_empty(),
        "corpus mismatches ({} of {}):\n{}",
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
}

#[test]
fn corpus_covers_every_outcome() {
    // A corpus that only exercises one branch would pass against a redactor that
    // always returns that branch. Guard the guard.
    let c = corpus();
    let expects: Vec<&str> = c["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["expect"].as_str().unwrap())
        .collect();
    for outcome in ["clean", "redacted", "quarantined"] {
        assert!(
            expects.contains(&outcome),
            "corpus must exercise the {outcome} outcome"
        );
    }
}

#[test]
fn corpus_has_the_entropy_channel_pair() {
    // The same bytes must be redacted as tool output and left alone as prose. An
    // implementation that applies entropy everywhere passes one and fails the other,
    // so the pair has to be present for the corpus to catch it.
    let c = corpus();
    let v = c["vectors"].as_array().unwrap();
    let paired: Vec<&serde_json::Value> = v
        .iter()
        .filter(|x| x["input"].as_str().unwrap().contains("DB_PASS"))
        .collect();
    assert_eq!(paired.len(), 2, "expected both halves of the channel pair");
    let channels: Vec<bool> = paired
        .iter()
        .map(|x| x["is_tool_output"].as_bool().unwrap())
        .collect();
    assert!(
        channels.contains(&true) && channels.contains(&false),
        "the pair must differ only in is_tool_output"
    );
}

#[test]
fn denied_and_allowed_paths_match() {
    let c = corpus();
    let r = Redactor::new();
    for p in c["denied_paths"]["denied"].as_array().unwrap() {
        let p = p.as_str().unwrap();
        assert!(r.is_denied_path(p), "must be denied: {p}");
    }
    for p in c["denied_paths"]["allowed"].as_array().unwrap() {
        let p = p.as_str().unwrap();
        assert!(!r.is_denied_path(p), "must NOT be denied: {p}");
    }
}
