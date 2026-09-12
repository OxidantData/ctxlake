# Managed by ctxlake. A pure-stdlib ULID generator.
#
# Hermes plugins run inside the host's own Python interpreter with no way to add a
# dependency to it (a `pip install` isn't ours to make into someone else's Hermes
# install) — 26 lines of Crockford base32 is simpler than negotiating one in. This
# intentionally makes no claim about what `ctxlake-core`'s Rust side does with its own
# `ulid` crate: that is a fact about a different file in a different language, and a
# comment here asserting it can silently go stale the next time that file changes
# without this one changing too. What matters here is only what this function itself
# guarantees.
#
# Monotonicity matters more for this generator than for `ctxlake-hook`'s: Hermes runs
# in-process and mints `event_id`s back-to-back for the same session (a `pre_tool_call`
# immediately followed by its `post_tool_call`, for instance), so two ids routinely
# land in the same millisecond. Bronze sorts by key with no secondary index
# (envelope.rs's module docs), so a plain `epoch-ms + random` ULID — whose two calls in
# the same millisecond differ only in random bits — would let those events sort
# arbitrarily instead of in call order.
from __future__ import annotations

import os
import threading
import time

_CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

_MAX_RANDOM = (1 << 80) - 1

_lock = threading.Lock()
_last_ms = -1
_last_random = 0


def new_ulid() -> str:
    """A 26-character Crockford-base32 ULID: 48 bits of epoch milliseconds followed
    by 80 bits of randomness.

    Two calls in the same millisecond get the *same* random start incremented by one,
    not two independent random draws — so consecutive calls from this process always
    compare greater than the one before, matching call order. A fresh millisecond
    reseeds the random bits from scratch (there is no ordering requirement across
    milliseconds; the timestamp already provides it). If the random component would
    overflow its 80 bits — on the order of 2^80 ids inside one millisecond — this
    falls back to a fresh random draw for that same millisecond rather than raising;
    the next millisecond restores strict ordering on its own, so a rare, practically
    unreachable fallback is better than a generator that can fail on a path that runs
    inside the user's agent.
    """
    ms = time.time_ns() // 1_000_000
    with _lock:
        global _last_ms, _last_random
        if ms == _last_ms and _last_random < _MAX_RANDOM:
            _last_random += 1
            rand_bytes = _last_random.to_bytes(10, "big")
        else:
            rand_bytes = os.urandom(10)
            _last_ms = ms
            _last_random = int.from_bytes(rand_bytes, "big")
    ts_bytes = ms.to_bytes(6, "big")
    return _encode(ts_bytes + rand_bytes)


def _encode(data: bytes) -> str:
    # 16 bytes = 128 bits -> 26 Crockford-base32 characters (130 bits, top 2 unused).
    value = int.from_bytes(data, "big")
    chars = []
    for i in range(26):
        shift = 5 * (25 - i)
        chars.append(_CROCKFORD[(value >> shift) & 0x1F])
    return "".join(chars)
