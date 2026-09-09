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
| Tasks | 16 across the whole plan, not per level |
| Explicit prerequisite edges | 128 across the whole plan |
| Hierarchy depth | 4, with roots at depth 0 |
| Task keys and idempotency key | 1–64 ASCII bytes; start with a letter or digit, then use letters, digits, `.`, `_`, or `-` |
| Serialized typed `plan` | 64 KiB, including default/optional fields and JSON escaping |
| Acceptance entries and labels | 64 each per task |
| Initial notes | 16 across the whole plan; existing note-body bounds also apply |
| Protocol result | 12 KiB; no partial mapping |

The size limit applies after decoding to the typed plan, not to the raw file
or its whitespace. The existing CLI JSON reader is unchanged; it does not
provide a bounded raw-file read for this command.

After an uncertain response, retry the same input with the same project,
actor, session, source skill, and idempotency key. Engram returns the original
mapping,
including after a restart between the graph commit and protocol receipt.
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
