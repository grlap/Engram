# Typed Memory Model

> Normative reference: [spec §2](../spec.md#2-data-model). Related briefs:
> [context packets](context-packets.md), [write policy & review](write-policy-and-review.md).

A memory in Engram is not a string — it is a stable identity plus an
append-only chain of immutable, content-addressed versions, classified along
three orthogonal axes.

## The three axes

| Axis | Values | Drives |
| --- | --- | --- |
| `kind` | `constraint` · `decision` · `convention` · `fact` · `preference` · `episode` | What species of claim this is |
| `authority` | `hard` · `firm` · `soft` | Write policy and delivery defaults |
| `delivery` | `pinned` · `index` · `on_demand` · `suppressed` | How it reaches an agent's context |

Delivery is **derived by default** from kind × authority (hard/firm
constraints pin; decisions pin only when hard; episodes are on-demand only)
and can be overridden per memory with a recorded reason. The default mapping
keeps the operator's mental model simple — "constraints are always in
context" — without collapsing the axes into one enum.

`decision` is deliberately first-class: a decision stays valid until
superseded and carries unusually important provenance.

## Versions, not edits

Changing a memory asserts a new version naming its parent version(s). History
is never mutated; a memory with multiple unsuperseded heads is **contested**
and stays visibly so until an attributed resolution version cites all
conflicting parents. Conflicts between different memories get explicit
`contradicts` edges — never last-writer-wins.

Engram represents that edge as a canonical contradiction event carrying two
shared version hashes plus an attributed reason. The current agent MCP surface
does not expose general-purpose version, contradiction, merge, or resolution
mutation. Its constrained `remember`/`forget` exception creates attributed
project-scoped Episodes under permanent safe keys and can only append a
terminal tombstone. Engram does not pretend keyword or model inference can
safely discover every semantic conflict. The edge makes both records visibly
contested, and an applicable firm/hard pinned pair stops packet construction.
Explicit contradiction, merge, and resolution operations remain host/operator
work outside the current agent surface.

## Metadata that used to live in prose

Every version carries as first-class fields what flat memory stores force
users to hand-encode in text: provenance chain (asserted-by / relayed-by /
derived-from), actor and assurance, confidence, sensitivity, validity window
(`valid_from`/`valid_until`), review deadline (`review_by`), evidence refs,
external refs, and tags. The `title` is separate from the `body` — one line
that powers the cheap index tier in [context packets](context-packets.md).
Low-friction capture also retains its classification basis and any delivery
override reason, so a receipt or authorized storage inspection can explain
exactly why prose became a particular typed record.

## Derived status

`proposed`, `active`, `contested`, `stale`, `expired`, `retracted`,
`tombstoned` — all derived from the object graph (versions + events), never
stored as a mutable column. See [spec §2.4](../spec.md#24-version-schema) for
the full schema and status table. Generic search and context assembly expose
only `proposed`, `active`, `contested`, and `stale` heads; `expired`,
`retracted`, and `tombstoned` facts remain canonical history but are not
retrieval candidates.

## Scope

A memory attaches to one visibility scope. In V1, `agent` is private scratch,
`task` is the default shared working set for participants, and `project` is
reviewed knowledge that outlives one task. A caller on an active task receives
applicable project + task + its own agent records. Broader `org`/`global` and
cross-host tiers activate with a shared backend later
([roadmap](../roadmap.md)). Pinned constraints from every applicable scope are
always delivered — scopes never shadow silently.
