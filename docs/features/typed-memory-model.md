# Typed Memory Model

> Normative reference: [spec §2](../spec.md#2-data-model). Related briefs:
> [historical context packet format](context-packets.md), [write policy & review](write-policy-and-review.md).

A memory in Engram is not a string — it is a stable identity plus an
append-only chain of immutable versions under minted ids, classified along
three orthogonal axes.

## The three axes

| Axis | Values | Drives |
| --- | --- | --- |
| `kind` | `constraint` · `decision` · `convention` · `fact` · `preference` · `episode` | What species of claim this is |
| `authority` | `hard` · `firm` · `soft` | Write policy and delivery defaults |
| `delivery` | `pinned` · `index` · `on_demand` · `suppressed` | Stored delivery classification |

Historical generic capture derived delivery from kind × authority. These
labels remain in serialized records; they do not promise automatic injection
or a packet builder. Live keyed project Episodes are explicitly listed and
read with `memories`.

`decision` is deliberately first-class: a decision stays valid until
superseded and carries unusually important provenance.

## Versions, not edits

Changing a memory asserts a new version naming its parent version(s). History
is never mutated, and no write silently wins over another.

The current agent MCP surface does not expose general-purpose version, merge,
or resolution mutation. Its constrained `remember`/`forget` exception creates attributed
project-scoped Episodes under permanent safe keys. `remember --revise` appends
a linear attributed revision under the same key; optional expected revision
checks refuse stale writes, and omitted checks explicitly report the replaced
and new revisions. Earlier versions remain discoverable until terminal
`forget`, which retires the key and all its reads without erasing local history.
This constrained episode operation is not general-purpose merge or conflict
resolution. Engram does not pretend keyword or model inference can safely
discover semantic conflicts, and it has no contradiction or conflict-marking
operation.

## Metadata that used to live in prose

Every version carries as first-class fields what flat memory stores force
users to hand-encode in text: provenance chain (asserted-by / relayed-by /
derived-from), actor and assurance, confidence, sensitivity, validity window
(`valid_from`/`valid_until`), review deadline (`review_by`), evidence refs,
external refs, and tags. The `title` is separate from the `body`. Historical
classification and delivery-override reasons remain readable with their stored
versions. Generic task capture is retired; work findings use `note`, and keyed
project Episodes use `remember`.

## Derived status

`proposed`, `active`, `stale`, `expired`, `retracted`, `tombstoned` — all
derived from the object graph (versions + events), never stored as a mutable
column. See [spec §2.4](../spec.md#24-version-schema) for the full target
schema and status table. Live work-memory retrieval selects only
`proposed`, `active`, and `stale` heads; `expired`,
`retracted`, and `tombstoned` facts remain canonical history but are not
retrieval candidates.

## Scope

A memory attaches to one visibility scope. `agent` is private scratch, `task`
identifies historical shared task records, `work` is shared working memory,
and `project` identifies project knowledge. Live work-memory retrieval selects
shared records by verified root identity and private scratch by exact item and
owning actor. Keyed project Episodes use the separate `remember`, `memories`,
and `forget` surface.

Historical task, work, private, and project rows share `memory_version` and
`memory_assertion_event` kinds. Readers, doctor, projection repair, and graph
restore distinguish their scopes and keyed identities; retiring task capture
never retires project memory by object kind. No task capture/search or pinned
packet-delivery surface remains. Broader `org`/`global` and cross-host tiers
remain [roadmap](../roadmap.md) design.
