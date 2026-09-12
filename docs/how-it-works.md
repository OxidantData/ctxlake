# How it works

Three things move data, and only one of them is automatic.

<svg viewBox="0 0 720 236" role="img" aria-label="Hooks write to a local spool; the sync daemon ships it to object storage and refreshes a local cache; the hook reads that cache. Maintenance derives digests and memories." style="width:100%;height:auto">
  <defs>
    <marker id="a" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto">
      <path d="M0 0 L8 4 L0 8 z" fill="var(--oxidant-text-muted)"/>
    </marker>
    <style>
      .b { fill: var(--oxidant-surface); stroke: var(--oxidant-border-strong); stroke-width: 1; rx: 6 }
      .t { fill: var(--oxidant-text); font: 500 13px var(--oxidant-font-ui) }
      .s { fill: var(--oxidant-text-muted); font: 400 11px var(--oxidant-font-ui) }
      .l { stroke: var(--oxidant-text-muted); stroke-width: 1.25; fill: none; marker-end: url(#a) }
      .dash { stroke-dasharray: 4 3 }
    </style>
  </defs>

  <text x="8" y="20" class="s">YOUR MACHINE</text>
  <rect class="b" x="8" y="30" width="130" height="52"/>
  <text x="24" y="52" class="t">agent</text>
  <text x="24" y="68" class="s">any of three</text>

  <rect class="b" x="8" y="128" width="130" height="52"/>
  <text x="24" y="150" class="t">spool</text>
  <text x="24" y="166" class="s">~/.ctxlake/spool</text>

  <rect class="b" x="196" y="128" width="130" height="52"/>
  <text x="212" y="150" class="t">cache</text>
  <text x="212" y="166" class="s">~/.ctxlake/cache</text>

  <text x="392" y="20" class="s">OBJECT STORAGE</text>
  <rect class="b" x="392" y="30" width="150" height="52"/>
  <text x="408" y="52" class="t">sessions/</text>
  <text x="408" y="68" class="s">what happened</text>

  <rect class="b" x="392" y="102" width="150" height="52"/>
  <text x="408" y="124" class="t">live/</text>
  <text x="408" y="140" class="s">who is active now</text>

  <rect class="b" x="392" y="174" width="150" height="52"/>
  <text x="408" y="196" class="t">snapshot/</text>
  <text x="408" y="212" class="s">what is believed</text>

  <rect class="b" x="586" y="102" width="126" height="52"/>
  <text x="602" y="124" class="t">maint</text>
  <text x="602" y="140" class="s">you schedule</text>

  <path class="l" d="M73 82 V122"/>
  <text x="80" y="108" class="s">hook · 5ms</text>

  <path class="l" d="M138 148 H190"/>
  <text x="126" y="200" class="s">daemon: ships up, pulls down</text>
  <path class="l" d="M326 145 H386"/>
  <path class="l dash" d="M386 165 H330"/>

  <path class="l" d="M542 128 H580"/>
  <path class="l dash" d="M580 145 H546"/>
  <text x="712" y="176" class="s" text-anchor="end">derives digests + memories</text>

  <path class="l dash" d="M208 128 V90 H140"/>
  <text x="216" y="86" class="s">briefing, on session start</text>
</svg>

| Stage | Moved by | Automatic? |
|---|---|---|
| Agent event → local spool | **hook** | **Yes** — `ctxlake install` wires it |
| spool ⇄ object storage | **`ctxlake sync`** daemon | No — `ctxlake sync install` wires it |
| storage → digests, memories | the same daemon, every 5 min | **Yes** — once the daemon runs |

**The hook never touches the network**, in either direction. It appends one line and
exits; its dependency tree contains no HTTP client and no object store, and CI fails if
that changes. So a lake outage costs you nothing at capture time — the spool grows, and
the daemon drains it when the store comes back.

## Three planes

Each area of the bucket has exactly one write pattern. That is what makes the whole
thing safe without coordination.

| Plane | Holds | Written | By |
|---|---|---|---|
| `live/` | who is active now | compare-and-swap | each agent, to its own key |
| `sessions/` | what happened | append-only, never rewritten | each agent, once per record |
| `snapshot/` | what is believed | immutable publish + pointer swap | `ctxlake maint` |

Getting `live/` wrong for five seconds is a stale presence indicator. Getting
`sessions/` wrong loses history permanently. They cannot share a write discipline
without one paying for the other's guarantees.

## Nothing needs a lock

Several machines can run `ctxlake maint` at the same time. No coordination, no leader,
no lease — because each unit of work is idempotent by content:

- **Compaction** writes into a directory named for the hash of its input set. Same
  input, same directory, same bytes. Different input, different directory.
- **Extraction** claims each session once with a create-if-absent marker, so exactly one
  host processes it.
- **The snapshot** is content-addressed and published by swapping a pointer.

Two hosts doing the same work produce the same result; two hosts doing different work
never touch the same key.

## Presence

Each agent writes one small object describing itself — repo, branch, what it is doing —
and refreshes it about once a minute with a five-minute expiry. An agent that stops
writing simply ages out.

The daemon merges those into a single roster so every other machine reads **one** object
instead of one per agent. At fifty agents that is the difference between roughly \$23 and
\$650 a month in request charges.

## One schema, three runtimes

Claude Code, Cursor and Hermes have different hook mechanisms and different field names.
Each adapter normalises into one envelope, and everything downstream reads only that.

> Every field name was captured from a live run rather than taken from documentation.
> Three of the three runtimes turned out to send something other than what their docs
> describe, and every mismatch failed *silently* — see [runtimes.md](runtimes.md).

## Redaction happens before anything is written

Secrets are scrubbed in the hook process, before the line reaches the spool — not later,
and not in the daemon. Object storage is append-only here, so a leaked key would be
permanent. The same scrubber runs on imported history, which is the likeliest place an
old `cat .env` is already sitting.

## One prompt, end to end

<svg viewBox="0 0 720 322" role="img" aria-label="A prompt becomes memory in three bands. Capture: you type a prompt, the hook redacts it and appends it to the local spool in under five milliseconds without touching the network, and the sync daemon ships it to immutable session storage. Maintenance, which flows right to left: compaction, then a Tier 0 structural digest, then Tier 2 claim extraction, then the four promotion gates. Only extraction needs a model, and the whole band is idempotent by content so any number of hosts may run it at once. Serving: the folded snapshot is pulled into the local cache as a briefing, and the hook injects it at the next session start." style="width:100%;height:auto">
  <defs>
    <marker id="pa" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto">
      <path d="M0 0 L8 4 L0 8 z" fill="var(--oxidant-text-muted)"/>
    </marker>
    <style>
      .pb { fill: var(--oxidant-surface); stroke: var(--oxidant-border-strong); stroke-width: 1; rx: 6 }
      .pt { fill: var(--oxidant-text); font: 500 13px var(--oxidant-font-ui) }
      .ps { fill: var(--oxidant-text-muted); font: 400 11px var(--oxidant-font-ui) }
      .pm { fill: var(--oxidant-text-muted); font: 400 10.5px var(--oxidant-font-mono, monospace) }
      .pl { stroke: var(--oxidant-text-muted); stroke-width: 1.25; fill: none; marker-end: url(#pa) }
    </style>
  </defs>

  <rect class="pb" x="8" y="40" width="170" height="46"/>
  <text x="22" y="61" class="pt">your prompt</text>
  <text x="22" y="77" class="ps">or a tool call</text>

  <path class="pl" d="M178 63 H271"/>
  <text x="184" y="56" class="ps">hook · ≤5ms</text>

  <rect class="pb" x="275" y="40" width="170" height="46"/>
  <text x="289" y="61" class="pt">local spool</text>
  <text x="289" y="77" class="pm">redacted already</text>

  <path class="pl" d="M445 63 H538"/>
  <text x="451" y="56" class="ps">daemon</text>

  <rect class="pb" x="552" y="40" width="150" height="46"/>
  <text x="566" y="61" class="pt">sessions/</text>
  <text x="566" y="77" class="pm">immutable</text>

  <path class="pl" d="M627 86 V136"/>

  <text x="8" y="128" class="ps">MAINTENANCE — idempotent by content: any host, any number of them, at once</text>

  <rect class="pb" x="552" y="140" width="150" height="46"/>
  <text x="566" y="161" class="pt">compact</text>
  <text x="566" y="177" class="pm">gen=&lt;hash&gt;/</text>

  <path class="pl" d="M552 163 H526"/>

  <rect class="pb" x="372" y="140" width="150" height="46"/>
  <text x="386" y="161" class="pt">Tier 0 digest</text>
  <text x="386" y="177" class="pm">digest.json</text>

  <path class="pl" d="M372 163 H344"/>

  <rect class="pb" x="190" y="140" width="150" height="46"/>
  <text x="204" y="161" class="pt">Tier 2 extract</text>
  <text x="204" y="177" class="pm">proposed</text>

  <path class="pl" d="M190 163 H162"/>

  <rect class="pb" x="8" y="140" width="150" height="46"/>
  <text x="22" y="161" class="pt">four gates</text>
  <text x="22" y="177" class="pm">promoted | contested</text>

  <text x="190" y="208" class="ps">the only step that needs a model</text>

  <path class="pl" d="M83 186 V262"/>

  <rect class="pb" x="8" y="266" width="150" height="46"/>
  <text x="22" y="287" class="pt">snapshot/</text>
  <text x="22" y="303" class="pm">&lt;hash&gt;.sqlite</text>

  <path class="pl" d="M158 289 H188"/>

  <rect class="pb" x="202" y="266" width="180" height="46"/>
  <text x="216" y="287" class="pt">local cache</text>
  <text x="216" y="303" class="pm">briefing.json</text>

  <path class="pl" d="M382 289 H422"/>
  <text x="436" y="283" class="ps">the hook injects it at</text>
  <text x="436" y="299" class="ps">the next session start</text>
</svg>

## Next steps

- [getting-started.md](getting-started.md) — install and first briefing
- [runtimes.md](runtimes.md) — per-runtime behaviour and honest gaps
- [architecture.md](architecture.md) — every component, for when you are debugging one
