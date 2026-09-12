# Managed by ctxlake. A pure-stdlib ULID generator.
#
# `ctxlake-core` uses the `ulid` crate for `event_id` (envelope.rs). Hermes plugins
# run inside the host's own Python interpreter with no way to add a dependency to
# it — this crate's "only the standard library" rule (see the wave-1 task brief) is
# the same reasoning as `ctxlake-hook`'s frozen dependency list, applied to a runtime
# where a `pip install` isn't ours to make. 26 lines of Crockford base32 is simpler
# than negotiating a new dependency into someone else's Hermes install.
from __future__ import annotations

import os
import time

_CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"


def new_ulid() -> str:
    """A 26-character Crockford-base32 ULID: 48 bits of epoch milliseconds followed
    by 80 bits of randomness. Not guaranteed monotonic within the same millisecond —
    the Rust `ulid` crate isn't either without its monotonic generator, which
    `ctxlake-core` does not use (see envelope.rs), so this matches what it actually
    guarantees rather than a stronger property nobody promised.
    """
    ms = time.time_ns() // 1_000_000
    ts_bytes = ms.to_bytes(6, "big")
    rand_bytes = os.urandom(10)
    return _encode(ts_bytes + rand_bytes)


def _encode(data: bytes) -> str:
    # 16 bytes = 128 bits -> 26 Crockford-base32 characters (130 bits, top 2 unused).
    value = int.from_bytes(data, "big")
    chars = []
    for i in range(26):
        shift = 5 * (25 - i)
        chars.append(_CROCKFORD[(value >> shift) & 0x1F])
    return "".join(chars)
