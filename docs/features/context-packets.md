# Context Packets

> Normative reference: [spec §4](../spec.md#4-context-packets--retrieval).
> Related briefs: [typed memory model](typed-memory-model.md),
> [CLI & MCP](cli-and-mcp.md), and
> [behavioral control plane](behavioral-control-plane.md).

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

The packet hash is a receipt, not an access capability. Explanation returns
packet bodies only to the same project and owning agent while every task or
exact work-item anchor in the packet remains active for the requesting
session. Rebinding a task or changing work focus makes the old anchored packet
unexplainable even though its immutable bytes remain stored.

The packet also carries typed `FeedPosition { kind, id, position }` values for
the persisted focus's named dense project, root-work, and active run-execution
feeds. Shared work memories retain their subject work id but are selected by
verified root identity; private work scratch remains exact-item. The hash
reproduces content; feed positions order later peer changes. `engram context delta --feed <id> --since
<position>` returns shared changes without rebuilding the packet. A runtime
notification can act as a doorbell, but Engram's named feeds are authoritative.

Under behavioral control, packet and delta delivery are durable protocol
records rather than caller convention. Each `ContextDelivery` has its own
dense per-session `DeliveryPosition { session_id, position }` and binds one or
more exact `FeedRange { kind, id, from_position, to_position,
observed_head_position }` sources, `has_more` state, a content digest, and a
delivery token. A grant binds the current source `FeedPosition` vector
separately from the session delivery position. The host may advance only by
acknowledging the next exact delivery and its contiguous source ranges with
compare-and-swap. Checkpoint atomically promotes that delivery position and
the resulting source-feed progress vector; it cannot skip, jump beyond a feed
head, or start an ordinary turn with required pages outstanding. The
acknowledgement asserts processing by the host/agent—it cannot prove semantic
comprehension.

The normal pre-turn decision inlines any required bounded packet/delta and
activates the turn only after the host confirms those exact tokens at
`turn_begin`; it does not send the agent away to call `memory_delta`. Events
carry `blocking`, `advisory`, or `informational` admission impact in their
named dense feeds, so an unrelated feed cannot create phantom lag.
Host-reported context
compaction invalidates packet delivery and forces the pinned tier to be
injected again.

Even when the task feed is already at its confirmed head, a focused session
receives a zero-length, context-only delivery whose packet binds the current
work-feed head vector plus two non-content revision fences: one for
project-visible memory and one scoped to the packet's project and owning
agent. The private fence exposes no memory hash or body. `turn_begin`
transactionally re-reads the persisted focus, project/root/run heads, and both
revision fences. A focus change, intervening work/project-memory event, or new
owner-private memory invalidates the unbegun grant with `delta_required`;
another agent's private capture does not. Reevaluation rebuilds and injects
the current context before execution. Work-feed deltas themselves remain on
the six-operation work protocol in V1.

A project policy epoch changes when control policy or mediation changes; a
work admission epoch changes when applicable pinned policy, root membership,
work revision/claim, or run lifecycle changes. Packet hashes reproduce
content, dense feed positions order changes, the two scoped epochs revoke
grants, the claim fence revokes stale responsibility, and resource-lease
fences revoke stale mutation authority; none substitutes for another.

Every packet ends with proposed and stale item counts so review pressure is
visible during normal work rather than hidden behind a separate queue command.
