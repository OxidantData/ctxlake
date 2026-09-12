# Managed by ctxlake. Redaction, mirroring crates/ctxlake-core/src/redact.rs.
#
# AGENTS.md invariant 7: redaction runs before the spool, on every capture path — that
# includes this one. `ctxlake-hook` gets this for free by depending on `ctxlake-core`;
# a Hermes plugin runs inside someone else's Python process with no dependency on
# that crate (see _ulid.py's docstring for the same constraint), so the marker list,
# deny-path list, and entropy thresholds below are hand-kept in sync with redact.rs
# rather than shared. If you change one, change the other — there is no test that can
# catch drift across a language boundary, so a future wave's job is to either
# generate both from one source or accept the duplication with eyes open.
from __future__ import annotations

import hashlib
import math
from dataclasses import dataclass, field

# Order matches redact.rs's SECRET_MARKERS; only the order of `dict` insertion here
# is relied on, not any semantics.
SECRET_MARKERS: list[tuple[str, str]] = [
    ("sk-", "anthropic_or_openai_key"),
    ("sk_live_", "stripe_live_key"),
    ("sk_test_", "stripe_test_key"),
    ("AKIA", "aws_access_key_id"),
    ("ASIA", "aws_session_key_id"),
    ("ghp_", "github_pat"),
    ("gho_", "github_oauth"),
    ("ghs_", "github_server_token"),
    ("github_pat_", "github_fine_grained_pat"),
    ("xoxb-", "slack_bot_token"),
    ("xoxp-", "slack_user_token"),
    ("glpat-", "gitlab_pat"),
    ("AIza", "google_api_key"),
    ("-----BEGIN", "pem_block"),
    ("eyJhbGciOi", "jwt"),
    ("Authorization:", "authorization_header"),
    ("authorization:", "authorization_header"),
    ("aws_secret_access_key", "aws_secret_kv"),
    ("AWS_SECRET_ACCESS_KEY", "aws_secret_kv"),
    ("ANTHROPIC_API_KEY", "anthropic_key_kv"),
    ("PRIVATE KEY", "private_key"),
]

DENY_PATHS: list[str] = [
    "/.aws/credentials",
    "/.aws/config",
    "/.ssh/id_",
    "/.hermes/.env",
    "/.netrc",
    "/.npmrc",
    "/.pypirc",
    "/.docker/config.json",
    "/.kube/config",
    "/secrets/",
    "/vault/",
]

ENTROPY_MIN_LEN = 32
ENTROPY_THRESHOLD = 4.5
MAX_SCAN_BYTES = 256 * 1024
_SECRET_CHARSET = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/=-_")


def content_hash(s: str) -> str:
    """`sha256:<hex>`, identical format and encoding to `ctxlake_core::hash::content_hash`
    (both are plain SHA-256 over the UTF-8 bytes) — cross-runtime dedup depends on the
    two runtimes hashing the same string to the same value."""
    return "sha256:" + hashlib.sha256(s.encode("utf-8", errors="surrogatepass")).hexdigest()


def is_denied_path(path: str) -> bool:
    return any(p in path for p in DENY_PATHS)


@dataclass
class Outcome:
    status: str  # "clean" | "redacted" | "quarantined"
    rules: list[str] = field(default_factory=list)


def scrub(text: str, is_tool_output: bool) -> tuple[Outcome, str]:
    """Scrub `text`, returning the outcome and the text safe to persist. Mirrors
    `Redactor::scrub` in redact.rs: literal markers first (quarantine withholds the
    whole value, since guessing where a secret ends is how redactors leak), then an
    entropy pass over tool *output* only — prompts and assistant prose trip the
    entropy heuristic far more often than command output does.
    """
    scan = text[: _floor_char_boundary(text, MAX_SCAN_BYTES)]

    rules: list[str] = []
    for marker, rule in SECRET_MARKERS:
        if marker in scan and rule not in rules:
            rules.append(rule)

    if rules:
        return (
            Outcome("quarantined", rules),
            f"[ctxlake: withheld, {len(text.encode('utf-8'))} bytes, {content_hash(text)}]",
        )

    if is_tool_output:
        count, replaced = _redact_high_entropy_runs(text)
        if count > 0:
            return Outcome("redacted", ["high_entropy_run"]), replaced

    return Outcome("clean"), text


def withhold_if_denied_path(path: str | None, result: str | None) -> tuple[str | None, bool]:
    """Mirrors `adapters::common::withhold_if_denied_path` on the Rust side: a read of
    a denied path has its *result* withheld even if the content doesn't happen to
    match a marker; the fact that the call touched this path is still worth keeping."""
    if path and is_denied_path(path):
        return f"[ctxlake: withheld, denied path {path}]", True
    return result, False


def _redact_high_entropy_runs(s: str) -> tuple[int, str]:
    out: list[str] = []
    run: list[str] = []
    count = 0

    def flush() -> None:
        nonlocal count
        if len(run) >= ENTROPY_MIN_LEN and _shannon_entropy(run) >= ENTROPY_THRESHOLD:
            out.append("[ctxlake:redacted-secret]")
            count += 1
        else:
            out.extend(run)
        run.clear()

    for ch in s:
        if ch in _SECRET_CHARSET:
            run.append(ch)
        else:
            flush()
            out.append(ch)
    flush()
    return count, "".join(out)


def _shannon_entropy(chars: list[str]) -> float:
    counts: dict[str, int] = {}
    for c in chars:
        counts[c] = counts.get(c, 0) + 1
    length = float(len(chars))
    return -sum((c / length) * math.log2(c / length) for c in counts.values())


def _floor_char_boundary(s: str, i: int) -> int:
    # Python `str` is already a sequence of code points, not bytes — there is no
    # UTF-8 boundary to straddle the way `redact.rs` has to guard against when
    # slicing a `&str`'s byte buffer. A plain length cap is the faithful port.
    return min(i, len(s))
