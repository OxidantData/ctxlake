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

# The pages these checks guard were consolidated: docs/scaling.md folded into
# docs/storage.md, and docs/runtimes/{claude-code,cursor,hermes}.md into one
# docs/runtimes.md with a `## <runtime>` section each. Only the addressing below
# moved — every assertion further down is unchanged and still made per runtime.
SCALING_PAGE = "storage.md"
RUNTIMES_PAGE = "runtimes.md"
IMPORT_PAGE = "adding-it.md"
RUNTIME_SECTIONS = ("Claude Code", "Cursor", "Hermes")

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


def runtime_section(heading: str) -> str:
    """Slice one runtime's `## <heading>` section out of the merged runtimes page.

    Each runtime used to own a file; they now own a section. This exists purely so the
    per-runtime assertions below keep being made per runtime rather than against the
    whole page, where one runtime's honest wording could satisfy a check for another's.
    """
    text = read(RUNTIMES_PAGE)
    match = re.search(
        rf"^## {re.escape(heading)}\s*$(.*?)(?=^## |\Z)", text, re.M | re.S
    )
    if match is None:
        fail(
            f"{RUNTIMES_PAGE}: no '## {heading}' section — page shape changed, "
            f"update this check"
        )
        return ""
    return match.group(1)


# ---------------------------------------------------------------------------
# Findings 3 & 4: the O(N^2) vs fan-in cost tables (now in docs/storage.md).
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
    scaling = read(SCALING_PAGE)

    naive_row = re.compile(
        r"\|\s*(\d+) agents\s*\|[^|]*\|[^|]*\|\s*"
        r"\$([\d.]+)\s*\+\s*\$([\d.]+)\s*≈\s*\*\*\$([\d.]+)/day\*\*"
    )
    naive_rows = naive_row.findall(scaling)
    if len(naive_rows) != 3:
        fail(
            f"{SCALING_PAGE}: expected 3 naive-discovery table rows, found "
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
                f"{SCALING_PAGE} naive row N={n}: printed total ${printed_total} "
                f"!= sum of its own addends ${a} + ${b} = ${addend_sum:.2f} "
                f"(this is exactly the finding-3 defect)"
            )
        if abs(printed_total - round(recomputed, 2)) > 0.005:
            fail(
                f"{SCALING_PAGE} naive row N={n}: printed total ${printed_total} "
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
            f"{SCALING_PAGE}: expected 3 fan-in table rows, found "
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
                f"{SCALING_PAGE} fan-in row N={n}: printed total ${printed_total} "
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
        fail(f"{SCALING_PAGE}: could not find the 'Nx cheaper at 50 agents' claim")
    else:
        claimed = int(multiplier_match.group(2))
        true_ratio = naive_cost(50) / fanin_cost(50)
        if abs(claimed - true_ratio) > 2:
            fail(
                f"{SCALING_PAGE}: claims {claimed}x cheaper at 50 agents, but "
                f"naive/fan-in from this page's own numbers is "
                f"{true_ratio:.2f}x"
            )


# ---------------------------------------------------------------------------
# Finding 1 (retired): this used to guard the lease object's bootstrap story —
# resource_key(repo, resource) hashed unbounded runtime input, so `ctxlake
# init` could never pre-seed a lease nobody had named yet. The lease
# abstraction itself (holders, TTLs, expiry, stealing) has since been removed
# from ctxlake entirely: every unit of batch work turned out to be idempotent
# by content already (compaction's gen=<hash> directories, extraction's
# create-if-absent claim marker, the snapshot's content-addressed publish),
# so nothing needed a lock and there is no bootstrap story left to guard.
# See docs/how-it-works.md.
# ---------------------------------------------------------------------------


# ---------------------------------------------------------------------------
# Finding 2: runtime docs claimed verification against live installs that
# never happened — ctxlake-hook is an unimplemented scaffold and
# adapters/hermes/ctxlake does not exist. Guard against the false-certainty
# phrasing reappearing, and require the honest not-yet-implemented status to
# stay documented.
# ---------------------------------------------------------------------------


def check_runtime_verification_honesty() -> None:
    """Each runtime section must state the basis for its claims, and that basis differs.

    This check was originally written when ctxlake-hook was a stub and every runtime
    page overclaimed. The hook is implemented now, so the failure mode inverted: the
    pages must no longer say "not yet implemented", and must instead be specific about
    HOW each runtime was verified, because the three were verified very differently:

      claude-code  adapter implemented; fixtures written from the documented field
                   list, not captured from a live run
      cursor       adapter implemented, but a live capture proved four fields wrong;
                   the page must disclose that until the adapter is corrected
      hermes       capabilities read from Hermes's own source; the adapter exists but
                   uses the Python-plugin mechanism rather than shell hooks

    Conflating those is the dishonesty worth guarding against now.
    """
    hook_path = ROOT / "crates" / "ctxlake-hook" / "src" / "main.rs"
    hook_src = hook_path.read_text(encoding="utf-8")
    is_stub = "not yet implemented" in hook_src

    for name in RUNTIME_SECTIONS:
        text = runtime_section(name)
        low = norm(text).lower()

        if is_stub:
            # Back to a scaffold: every page must say so again.
            if "not yet implemented" not in text:
                fail(f"{RUNTIMES_PAGE} § {name}: hook is a stub again but the page does not say so")
            continue

        # The hook is implemented, so a stale scaffold caveat is now the wrong claim.
        if "status: not yet implemented" in low:
            fail(
                f"{RUNTIMES_PAGE} § {name}: still carries a 'not yet implemented' status, but "
                f"{hook_path.relative_to(ROOT)} is implemented — the caveat is now the "
                f"inaccurate statement"
            )

        # Every page must name its verification basis explicitly.
        if "verification basis" not in low:
            fail(
                f"{RUNTIMES_PAGE} § {name}: missing an explicit 'Verification basis' line. The "
                f"three runtimes were verified by different means and the section has "
                f"to say which applies to it."
            )

    # Cursor's known-wrong fields must stay disclosed while they are still wrong.
    cursor_rs = ROOT / "crates" / "ctxlake-hook" / "src" / "adapters" / "cursor.rs"
    if cursor_rs.exists():
        src = cursor_rs.read_text(encoding="utf-8")
        corrected = "workspace_roots" in src and "exitCode" in src
        disclosed = "workspace_roots" in runtime_section("Cursor")
        if not corrected and not disclosed:
            fail(
                f"{RUNTIMES_PAGE} § Cursor: the adapter still reads neither workspace_roots nor "
                "tool_output.exitCode, so Cursor events carry no cwd and no exit code. "
                "Until the adapter is fixed the section must disclose that."
            )


# ---------------------------------------------------------------------------
# Finding 5: the import docs claimed Hermes had no importable history ("Hermes —
# live capture only", "Plugin captures from install onward"). That was false and
# it was published. Hermes keeps a full SQLite state database at
# ~/.hermes/state.db — sessions, every message, structured tool calls, per-session
# token and cost accounting — so it is a high-fidelity import source, not a
# live-only runtime.
#
# The guard is deliberately two-sided, the same shape as
# check_runtime_verification_honesty: the page must not carry the old false claim,
# AND the properties it now promises must still be true of the code. A doc that
# promises a read-only open while the importer opens read-write is the more
# dangerous half, because the promise is about someone else's live database.
# ---------------------------------------------------------------------------

IMPORTER = ROOT / "crates" / "ctxlake-cli" / "src" / "import" / "hermes.rs"

RETIRED_HERMES_IMPORT_CLAIMS = [
    "## Hermes — live capture only",
    "Whether a useful on-disk history exists to backfill depends on your Hermes "
    "installation",
    "Plugin captures from install onward",
]


def check_hermes_import_fidelity() -> None:
    # `import.md` was folded into `adding-it.md` by the docs consolidation; the
    # claims it guards moved with it, so the check follows rather than retires.
    import_md = read(IMPORT_PAGE)
    normalized = norm(import_md)

    for phrase in RETIRED_HERMES_IMPORT_CLAIMS:
        if norm(phrase) in normalized:
            fail(
                f"{IMPORT_PAGE}: contains the retired claim {phrase!r} — Hermes keeps a "
                f"full SQLite state database at ~/.hermes/state.db, so it is an "
                f"importable source (finding 5)"
            )

    # The fidelity table is the summary a reader acts on, so it is checked
    # independently of whatever the prose says.
    row = re.search(r"^\|\s*Hermes\s*\|([^|]*)\|([^|]*)\|([^|]*)\|", import_md, re.M)
    if not row:
        fail(f"{IMPORT_PAGE}: no Hermes row in the fidelity table — table shape changed")
    else:
        source, fidelity, contents = (c.strip() for c in row.groups())
        if "state.db" not in source:
            fail(
                f"{IMPORT_PAGE}: the Hermes fidelity row names its source as {source!r}; "
                f"the history lives in ~/.hermes/state.db"
            )
        if "—" in source or "live capture only" in fidelity.lower():
            fail(
                f"{IMPORT_PAGE}: the Hermes fidelity row still reads {fidelity!r} with "
                f"source {source!r} (finding 5)"
            )
        for expected in ("tool call", "cost"):
            if expected not in contents.lower():
                fail(
                    f"{IMPORT_PAGE}: the Hermes fidelity row does not mention {expected!r}; "
                    f"state.db carries it and the table is what readers act on"
                )

    if not IMPORTER.exists():
        # The importer was removed. The page must stop promising it rather than
        # describing a command nobody can run — the same inversion
        # check_runtime_verification_honesty applies to the hook.
        if "ctxlake import --runtime hermes" in import_md:
            fail(
                f"{IMPORT_PAGE}: promises `ctxlake import --runtime hermes`, but "
                f"{IMPORTER.relative_to(ROOT)} does not exist"
            )
        return

    # Only the production half: the test module names SQLITE_OPEN_READ_WRITE on
    # purpose, in the assertion that it is *absent* from the flags.
    src = IMPORTER.read_text(encoding="utf-8").split("#[cfg(test)]", 1)[0]

    # Promise: the running agent's database is never contended for. This is the
    # one claim on the page that is about someone else's data, so the doc and the
    # code are checked against each other rather than the doc being trusted.
    if "SQLITE_OPEN_READ_ONLY" not in src or "mode=ro&immutable=1" not in src:
        fail(
            "import/hermes.rs no longer opens Hermes's database read-only and "
            "immutable, but adding-it.md and runtimes.md § Hermes both promise it "
            "does — "
            "that promise is about a live agent's own state file"
        )
    if "SQLITE_OPEN_READ_WRITE" in src or "SQLITE_OPEN_CREATE" in src:
        fail(
            "import/hermes.rs asks for a writable or creating open on Hermes's "
            "database; the docs promise read-only"
        )
    for needed, why in (
        ("read-only", "the read-only open"),
        ("immutable=1", "the lock-free open"),
        ("schema_version", "the fail-loud schema gate"),
        ("compacted = 1", "the compaction marker import recovers"),
    ):
        if needed not in import_md:
            fail(f"{IMPORT_PAGE}: no longer documents {why} ({needed!r})")

    # The asymmetry worth stating: live capture has no compaction hook on Hermes,
    # import recovers the marker from the database. Both pages have to agree.
    hermes_page = norm(runtime_section("Hermes")).lower()
    if "ctxlake import" not in hermes_page:
        fail(
            "runtimes.md § Hermes: does not mention `ctxlake import` at all, while "
            "adding-it.md documents a working Hermes importer"
        )
    if "state.db" not in hermes_page:
        fail("runtimes.md § Hermes: does not name ~/.hermes/state.db as the import source")
    # The page still has to be honest that the *hook* surface has no compaction
    # event — import recovering it does not give live capture the marker.
    # Lowercased like the `hermes_page` checks above: the phrase is a heading-cased
    # bullet on the merged page and sentence-cased in prose, and which one it is
    # says nothing about whether the gap is documented.
    if "no `pre_compact` equivalent" not in norm(runtime_section("Hermes")).lower():
        fail(
            "runtimes.md § Hermes: dropped the live-capture compaction gap. Import "
            "recovering the marker from state.db does not give the hook surface one."
        )


def main() -> int:
    check_scaling_arithmetic()
    check_runtime_verification_honesty()
    check_hermes_import_fidelity()

    if failures:
        print(f"FAIL — {len(failures)} regression check(s) failed:\n")
        for f in failures:
            print(f" - {f}")
        return 1

    print("PASS — all docs regression checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
