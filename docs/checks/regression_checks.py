#!/usr/bin/env python3
"""Regression checks for an adversarial review of wave1/docs.

Why this exists, and why it is a script instead of a Rust test: these findings
are about *prose*, not code — no crate under crates/ owns docs/, and this
branch's scope is docs/ only, so a guard for a docs bug has to live in docs/
too. It is deliberately dependency-free (stdlib only) so `python3
docs/checks/regression_checks.py` from the repo root is the whole contract;
nothing here is wired into `cargo test` because there is no docs-testing
harness in this workspace to wire it into.

Each check below reproduces the exact defect an adversarial review found,
independent of whatever the docs currently say, then verifies the shipped
docs against that independent computation/assertion. Run this against the
pre-fix commit and every arithmetic check fails; run it here and all pass —
that is the red/green this script exists to prove, since diffing markdown by
eye is exactly how the original errors survived review once already.
"""

import re
from pathlib import Path

DOCS = Path(__file__).resolve().parent.parent
ROOT = DOCS.parent

failures: list[str] = []


def fail(msg: str) -> None:
    failures.append(msg)


def read(relpath: str) -> str:
    return (DOCS / relpath).read_text(encoding="utf-8")


def norm(s: str) -> str:
    """Collapse whitespace so a phrase that wraps across markdown lines still
    matches a substring search the same way a reader's eye would find it."""
    return re.sub(r"\s+", " ", s)


def money(s: str) -> float:
    return float(s.replace("$", "").replace(",", ""))


# ---------------------------------------------------------------------------
# Findings 3 & 4: docs/scaling.md's O(N^2) vs fan-in cost tables.
#
# The review's complaint was precise: the printed totals didn't equal the sum
# of their own displayed addends (finding 3), and the fan-in formula silently
# dropped the merged-snapshot PUT its own prose says happens every cycle
# (finding 4). Both are re-derived here from the page's own stated inputs
# (17,280 cycles/day, the price list) rather than trusted from the prose, so
# a future edit that reintroduces either mistake fails this even if the two
# numbers it changes still happen to agree with *each other*.
# ---------------------------------------------------------------------------

CYCLES_PER_DAY = 17_280
LIST_PRICE = 0.005 / 1000
PUT_PRICE = 0.005 / 1000
GET_PRICE = 0.0004 / 1000


def naive_cost(n: int) -> float:
    lists = n * CYCLES_PER_DAY
    gets = n * (n - 1) * CYCLES_PER_DAY
    return lists * LIST_PRICE + gets * GET_PRICE


def fanin_cost(n: int) -> float:
    lists = CYCLES_PER_DAY  # one LIST/cycle to enumerate live/agents/
    puts = CYCLES_PER_DAY  # one PUT/cycle for the merged snapshot (finding 4)
    gets = 2 * n * CYCLES_PER_DAY  # N to build it, N for peers to read it
    return lists * LIST_PRICE + puts * PUT_PRICE + gets * GET_PRICE


def check_scaling_arithmetic() -> None:
    scaling = read("scaling.md")

    naive_row = re.compile(
        r"\|\s*(\d+) agents\s*\|[^|]*\|[^|]*\|\s*"
        r"\$([\d.]+)\s*\+\s*\$([\d.]+)\s*≈\s*\*\*\$([\d.]+)/day\*\*"
    )
    naive_rows = naive_row.findall(scaling)
    if len(naive_rows) != 3:
        fail(
            f"scaling.md: expected 3 naive-discovery table rows, found "
            f"{len(naive_rows)} — table shape changed, update this check"
        )
    for n_str, a, b, total_str in naive_rows:
        n = int(n_str)
        printed_total = money(total_str)
        addend_sum = money(a) + money(b)
        recomputed = naive_cost(n)
        # The bug this guards: printed_total was $0.03-$0.35 higher than the
        # sum of the two addends printed right next to it. Tolerance is 1.5
        # cents, not 0.5, because rounding two addends to cents *before*
        # summing them can legitimately land a cent away from rounding the
        # precise total directly (e.g. 1.728 -> $1.73, 2.6266 -> $2.63, but
        # their exact sum 4.3546 rounds to $4.35, not $4.36) — that's a
        # rounding artifact, not the finding-3 defect, which was 3-35x larger.
        if abs(printed_total - round(addend_sum, 2)) > 0.015:
            fail(
                f"scaling.md naive row N={n}: printed total ${printed_total} "
                f"!= sum of its own addends ${a} + ${b} = ${addend_sum:.2f} "
                f"(this is exactly the finding-3 defect)"
            )
        if abs(printed_total - round(recomputed, 2)) > 0.005:
            fail(
                f"scaling.md naive row N={n}: printed total ${printed_total} "
                f"!= independently recomputed ${recomputed:.4f} from "
                f"{CYCLES_PER_DAY} cycles/day at the page's own prices"
            )

    fanin_row = re.compile(
        r"\|\s*(\d+) agents\s*\|\s*\$[\d.]+\s*\+\s*\d+\s*×\s*\$[\d.]+\s*≈\s*"
        r"\*\*\$([\d.]+)/day\*\*"
    )
    fanin_rows = fanin_row.findall(scaling)
    if len(fanin_rows) != 3:
        fail(
            f"scaling.md: expected 3 fan-in table rows, found "
            f"{len(fanin_rows)} — table shape changed, update this check"
        )
    for n_str, total_str in fanin_rows:
        n = int(n_str)
        printed_total = money(total_str)
        recomputed = fanin_cost(n)
        # The bug this guards: the printed formula omitted the merged-
        # snapshot PUT entirely, undercounting cost by up to 55%.
        if abs(printed_total - round(recomputed, 2)) > 0.005:
            fail(
                f"scaling.md fan-in row N={n}: printed total ${printed_total} "
                f"!= independently recomputed ${recomputed:.4f}, which "
                f"includes the once-per-cycle merged-snapshot PUT the prose "
                f"says happens (this is exactly the finding-4 defect if the "
                f"gap equals a whole $0.0864/day PUT term)"
            )

    # The "Nx cheaper" headline is derived from the two totals above, so it
    # has to move when they do rather than being left as a stale multiplier.
    multiplier_match = re.search(
        r"\*\*(~?)(\d+)x cheaper at 50 agents\*\*", scaling
    )
    if not multiplier_match:
        fail("scaling.md: could not find the 'Nx cheaper at 50 agents' claim")
    else:
        claimed = int(multiplier_match.group(2))
        true_ratio = naive_cost(50) / fanin_cost(50)
        if abs(claimed - true_ratio) > 2:
            fail(
                f"scaling.md: claims {claimed}x cheaper at 50 agents, but "
                f"naive/fan-in from this page's own numbers is "
                f"{true_ratio:.2f}x"
            )


# ---------------------------------------------------------------------------
# Finding 1: lease (and roster) bootstrap cannot be "seeded at ctxlake init"
# because resource_key(repo, resource) hashes unbounded, arbitrary runtime
# input (crates/ctxlake-core/src/hash.rs) — init cannot enumerate a claim
# nobody has typed yet. Guard against the impossible claim reappearing, and
# require the actual (lazy, unconditional-PUT) bootstrap mechanism to be
# documented in its place.
# ---------------------------------------------------------------------------

IMPOSSIBLE_BOOTSTRAP_PHRASES = [
    "seeds every lease object a fleet might need",
    "every lease object is seeded to `free` at init time",
    "after first creation by `ctxlake init`",
    "seeded at ctxlake init (never put-if-absent",
]

REQUIRED_BOOTSTRAP_PHRASES = {
    "coordination.md": ["unconditional `PUT`", "rules out pre-seeding"],
    "storage.md": ["unconditional `PUT`", "cannot pre-seed a resource"],
    "architecture.md": ["lazily created on first claim"],
}


def check_lease_bootstrap_honesty() -> None:
    for name in ("coordination.md", "storage.md", "architecture.md"):
        text = norm(read(name))
        for phrase in IMPOSSIBLE_BOOTSTRAP_PHRASES:
            if norm(phrase) in text:
                fail(
                    f"{name}: contains the impossible bootstrap claim "
                    f"{phrase!r} — resource_key() hashes unbounded runtime "
                    f"input, so ctxlake init cannot pre-seed it (finding 1)"
                )
        for phrase in REQUIRED_BOOTSTRAP_PHRASES.get(name, []):
            if norm(phrase) not in text:
                fail(
                    f"{name}: missing the corrected bootstrap explanation "
                    f"{phrase!r} — the lazy unconditional-PUT mechanism "
                    f"should be documented here"
                )


# ---------------------------------------------------------------------------
# Finding 2: runtime docs claimed verification against live installs that
# never happened — ctxlake-hook is an unimplemented scaffold and
# adapters/hermes/ctxlake does not exist. Guard against the false-certainty
# phrasing reappearing, and require the honest not-yet-implemented status to
# stay documented.
# ---------------------------------------------------------------------------

FALSE_VERIFICATION_PHRASES = {
    "claude-code.md": [
        "every row in this doc is verified against a real install",
        "this is the reference implementation",
    ],
    "hermes.md": [
        "implemented and verified",
        "verified against a live install",
        "capture-complete\" is a real, verified claim",
    ],
}

REQUIRED_HONESTY_PHRASES = {
    "claude-code.md": ["not yet implemented", "crates/ctxlake-hook/src/main.rs"],
    "hermes.md": ["not yet implemented", "crates/ctxlake-hook/src/main.rs"],
}


def check_runtime_verification_honesty() -> None:
    # Guard the premise itself: if ctxlake-hook ever stops being a stub, this
    # whole finding is moot and these docs may legitimately claim
    # verification again — fail loudly rather than silently going stale.
    hook_path = ROOT / "crates" / "ctxlake-hook" / "src" / "main.rs"
    hook_src = hook_path.read_text(encoding="utf-8")
    if "not yet implemented" not in hook_src:
        fail(
            "crates/ctxlake-hook/src/main.rs no longer says 'not yet "
            "implemented' — the runtime docs' honesty caveats in "
            "docs/runtimes/ were written assuming it's still a scaffold; "
            "re-review whether they can now claim real verification"
        )

    for name, phrases in FALSE_VERIFICATION_PHRASES.items():
        text = norm(read(f"runtimes/{name}")).lower()
        for phrase in phrases:
            if norm(phrase).lower() in text:
                fail(
                    f"runtimes/{name}: contains the overclaim {phrase!r} — "
                    f"nothing in {hook_path.relative_to(ROOT)} or "
                    f"adapters/ is implemented yet (finding 2)"
                )
    for name, phrases in REQUIRED_HONESTY_PHRASES.items():
        text = read(f"runtimes/{name}")
        for phrase in phrases:
            if phrase not in text:
                fail(
                    f"runtimes/{name}: missing the honest not-yet-"
                    f"implemented status pointing at {phrase!r}"
                )


def main() -> int:
    check_scaling_arithmetic()
    check_lease_bootstrap_honesty()
    check_runtime_verification_honesty()

    if failures:
        print(f"FAIL — {len(failures)} regression check(s) failed:\n")
        for f in failures:
            print(f" - {f}")
        return 1

    print("PASS — all docs regression checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
