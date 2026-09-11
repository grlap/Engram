# Atomic work plans

Use one `work core propose` call to create a complete local plan. The plan can
contain several new roots, nested children, and prerequisites. Engram creates
all tasks and edges in one SQLite transaction, or creates none of them.

This is a host/operator input to the existing work protocol. It adds no agent
word or MCP tool. The existing `root` and `decompose` variants keep their
behavior. See [local work](local-work-system.md) and the
[CLI contract](cli-and-mcp.md).

## Submit a plan

Save this JSON as `plan.json`:

```json
{
  "kind": "plan",
  "plan": {
    "idempotency_key": "release-plan-1",
    "tasks": [
      {
        "key": "release",
        "title": "Deliver the release",
        "outcome": "The release is ready",
        "acceptance": ["Required parts are complete"]
      },
      {
        "key": "package",
        "parent_key": "release",
        "title": "Build the package",
        "outcome": "A tested package is available",
        "acceptance": ["Package tests pass"]
      },
      {
        "key": "tests",
        "parent_key": "package",
        "title": "Test the package",
        "outcome": "Test results are recorded",
        "acceptance": ["All required tests pass"]
      }
    ],
    "prerequisites": []
  }
}
```

Run it with a stable actor and session:

```sh
engram work --actor-id planner --session-id planning-session core propose --input @plan.json
```

No preview/apply pair is required. The response has `kind: "plan"` and a
`tasks` array. Every input key appears once, in input order, with its generated
`work_id`, `short_ref`, and admission `revision`. The mapping is complete;
Engram never removes rows to fit the response. It checks the actual serialized
protocol result before committing the graph.

## Relationships and defaults

`parent_key` names a task in this payload. Tasks can appear in any order. All
roots are new: a plan cannot attach children to an existing parent, revise an
existing item, or use ambient focus. Omit `--work-ref`; supplying it refuses
the plan. Admission preserves focus and does not claim any new task.

Children are required unless `requirement` is `"optional"`. An optional root
is invalid. Title, outcome, and an acceptance array are explicit inputs. An
empty acceptance array stays empty; Engram does not invent a criterion.
Task options also include `kind`, `priority`, `labels`, `assigned_to`,
`deferred_until`, `external_ref`, and `notes`. Root priority defaults to 1;
children inherit their parent's priority when omitted. Child labels include
the parent's labels, as in ordinary decomposition. Initial notes are
attributed non-holder observations, not execution evidence.

A prerequisite entry has `work_key` and a `prerequisite` object:

```json
{"work_key": "tests", "prerequisite": {"kind": "local", "value": "fixture"}}
```

Here `fixture` must be another task key in the payload. To require an existing
item, use `kind: "existing"` and its short ref or full UUID as `value`.
Only the dependent task is new; the existing prerequisite is not changed.
Both namespaces are explicit, so a local key cannot hide an existing ref.
Existing prerequisites must be open in the same project. Already completed
prerequisites refuse as already satisfied. Duplicate edges, unresolved refs,
ancestor prerequisites, and cycles refuse the whole plan. Cycle checks include
implicit required-child relationships.

## Bounds and retries

| Input | Limit |
| --- | --- |
| Tasks | 256 across the whole forest, including roots |
| Open descendants | 255 per root, across all levels (at most 256 tasks in a single new tree) |
| Explicit prerequisite edges | 1024 across the whole plan |
| Hierarchy depth | 4, with roots at depth 0 |
| Task keys and idempotency key | 1–64 ASCII bytes; start with a letter or digit, then use letters, digits, `.`, `_`, or `-` |
| Serialized typed `plan` | 1 MiB, including default/optional fields and JSON escaping |
| Raw CLI `core propose --input` | 2 MiB for the entire JSON input, including whitespace, any file BOM, and the outer envelope; checked before decoding. One leading UTF-8 BOM is accepted only for `@file`. |
| Acceptance entries and labels | 64 each per task |
| Initial notes | 16 across the whole plan; existing note-body bounds also apply |
| Compact JSON plan result | 64 KiB; no partial mapping (CLI pretty-print whitespace is additional) |

These are host/operator admission limits, not compact agent-response limits.
Ordinary agent responses retain their 12 KiB ceiling, and ordinary decomposition
retains its 16-child per-call limit. A complete `plan` can create more than 16
direct children, but never exceeds the shared per-root descendant or depth
limits. All 256 tasks can belong to one root, either as direct children or
across several levels. The 255-open-descendant limit also applies to ordinary
planning and graph restoration; completed, cancelled and superseded descendants
do not consume it. Choose separate roots for genuinely separate outcomes.

The limits bound input memory, generated work, and the complete result. They
are not transaction timeouts. The 256-task limit leaves room for maximum-width
keys and identities within the separate 64 KiB result budget. The 1024-edge
limit caps canonical edge writes and their feed entries. A regression fixture
checks a two-tree envelope against a pre-set limit of 20,000 canonical decodes.
Flat and nested single-root fixtures use a tighter limit of 7,000, calibrated
after measuring 6,430 and 6,861 decodes. That guard rejects an additional decode
for each of the 1,024 edges even in the cheaper fixture. Time is diagnostic, not a
pass condition or a latency promise for every host and payload. A large plan
holds SQLite's writer lock until admission finishes, so other writes can wait.

The typed-input bound and raw CLI bound are separate checks. The raw bound also
applies to the `root` and `decompose` variants of `core propose`. File reads stop
after the limit plus one byte; whitespace cannot force an unbounded read. The
plan-result bound covers the serialized `kind` and complete task mapping.
Both the conservative preflight result and the actual in-transaction result
are checked against this same operator budget. An exact retry uses it too.

Payload cycle validation visits tasks and edges without a transitive-closure
matrix. Admission runs one full-project cycle check over the final old/new
graph before committing, rather than scanning the project after each task or
edge. It still detects unrelated existing cycles and rolls back the whole
plan on failure. Refusing an unrelated existing cycle pays the full plan
admission cost under the writer lock before the final scan rolls back its writes.
This removes repeated project scans; it does not make total
admission cost independent of project size. The final root-size check runs
once per new root, not once per decomposed parent. Depth and canonical
transition checks still run within the writer transaction. For new dependants,
the writer carries a transaction-local relation basis through each edge
addition, then verifies every final edge against canonical history before
commit. It does not re-decode every growing prefix of the same edge set.
Existing prerequisites retain their admission checks. Ordinary single-item
updates do not use this plan-local state.

There is no multi-call atomic mode. A refused plan leaves no partial graph.
Splitting a plan into multiple calls does not extend its transaction or allow
later `plan` calls to attach to a previously created root.

After an uncertain response, retry the same input with the same project,
actor, session, source skill, and idempotency key. Engram returns the original
mapping, including after a restart between the graph commit and protocol receipt.
Changing the input under that key refuses. A different key or session is a
new admission, not recovery. Inspect existing work before starting a new one.
Retry time and changed host actor context do not change intent or rewrite the
attribution recorded by the first admission. Storage checks this same intent
even when its core receipt predates the service's protocol attempt.

The graph, initial observations, and core replay result commit together.
Session registration and protocol-attempt bookkeeping are separate: a refused
attempt can retain these audit rows, but not a partial plan. No live claim,
external lifecycle, or publication authority is created. External references
are planning labels, not automatic tracker synchronization or immutable
[source intake](source-intake.md). Actor context is asserted, not authenticated;
the development redactor provides no secret-filtering guarantee.
