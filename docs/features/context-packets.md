# Context Packets

> Normative reference: [spec §4](../spec.md#4-context-packets--retrieval).
> Related briefs: [typed memory model](typed-memory-model.md),
> [CLI & MCP](cli-and-mcp.md).

A context packet is the unit of memory delivery: the block an agent receives
at session start or on request. Packet construction is a first-class core API
used identically by the CLI and the MCP server, so delivery semantics cannot
drift between interfaces.

## Three rungs, one budget

| Rung | Content | Default budget | On overflow |
| --- | --- | --- | --- |
| 1 — pinned | Active pinned memories, verbatim | 4 KiB | **Fail closed** |
| 2 — index | Titles only (`id · kind · title`) for everything index-delivered in scope | 8 KiB | Ranked eviction + omission manifest |
| 3 — on demand | `show` / `history` / `search` (FTS5) | — | — |

Whole-packet hard cap: 12 KiB (≈3k tokens). All budgets are per-deployment
configuration, tuned against the [evaluation harness](../spec.md#10-evaluation--telemetry).

## Fail closed

The pinned tier is never silently truncated **and never self-contradictory**.
Packet construction fails before the agent acts when the pinned tier cannot
fit its budget, or when an unresolved contradiction stands between two
applicable hard/firm pinned records — delivering both would ask the model to
improvise policy precedence. The error names the records needing merge,
demotion, or resolution. A dropped convention is a nuisance; a dropped or
ambiguous "never do X" is an incident.

## The index tier

Titles-only listing solves "agents don't know what they don't know": fifty
memories cost fifty lines, and the agent knows exactly what it can pull via
rung 3. Anything evicted appears in an **omission manifest** with counts and
reasons — absence is always visible.

## Ranking

Non-pinned candidates rank by: scope proximity → exact identifier match →
FTS relevance → authority → confidence → `last_verified`/validity → prior
usefulness → byte cost. Deliberately **no recency boost** for constraints and
decisions: an old, recently-verified decision outranks a new, unverified one.

## Reproducibility

Every packet has a content hash; `engram context explain <packet-id>` shows
exactly what was included, omitted, and why. Every packet item carries its
retrieval reason and evidence pointers, so an agent can cite — and a human
can audit — the chain from context back to source.

The packet also carries the monotonic task event cursor observed during
construction. The hash reproduces content; the cursor orders later peer
changes. `engram context delta --since <cursor>` returns task-shared changes
without rebuilding the packet. A runtime notification can act as a doorbell,
but the durable Engram feed is authoritative.

Every packet ends with proposed and stale item counts so review pressure is
visible during normal work rather than hidden behind a separate queue command.
