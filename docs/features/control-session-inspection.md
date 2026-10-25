# Read-only control session inspection

> Normative reference: [spec §8](../spec.md#8-interfaces).
> Related: [host readiness](host-readiness.md), [host checklist](../host-checklist.md),
> [behavioral control](behavioral-control-plane.md), [CLI](cli-and-mcp.md),
> [turn gate assessment](turn-gate-assessment.md).

An operator or host may inspect stale local checkpoint handles with:

```text
engram --project-file FILE --home HOME control-session-inspect --target-session-id SESSION --retained-grant-id GRANT --json
```

This opens only an existing current-format store read-only. It does not start
a control connection, bind a session, expire a grant, checkpoint, repair,
change policy or reconcile anything. It is not `SessionStatus`: that host
operation may expire grants, and its missing-binding refusal can also mean
a project mismatch. Neither that refusal nor a stopped host session proves
that no durable grant remains.

## Receipt and admission

Exit 0 emits one JSON object with these required fields:

| Field | Meaning |
| --- | --- |
| `schema_version` | `1` |
| `scope` | `"control_session_inspect"` |
| `mutation_enabled` | `false` |
| `project_id`, `database` | Selected project marker and canonical absolute database path |
| `session_id`, `retained_grant_id` | Exact queried selectors |
| `host_path_policy` | `stored`, `resolved` descriptions and `status:"matched"` |
| `session_present` | Any physical session row with this id, regardless of project |
| `session_grants_present` | Any grant row for this session, regardless of state |
| `retained_grant_present` | Any grant row with this id anywhere in the selected store |
| `details` | Versioned diagnostic facts described below; never absence clearance or authorization |

Build fields are informational, not authentication. No routing token or stored
payload is returned. Selectors must be nonblank, control-free and at most 64
UTF-8 bytes. The three existence checks use the current schema's indexes in
one read snapshot, with schema, policy and path admission rechecked within
that snapshot. Orphan grants, grants for another session and malformed grant
payloads still count as present. This does not perform a full history audit.
Unchecked data includes, but is not limited to, connection-token rows in
`control_connections`; these may remain for this session even when all three
booleans are false. The receipt does not establish that every
record for the id is absent, nor authorize recycling the id or overwriting
a connection token.
Ordinary schema/policy admission still has its normal cost; it is not a
constant-time promise. SQLite busy waits use the existing five-second timeout;
hosts must also impose their own end-to-end process deadline. For that
deadline's sake the inspection's open does not start again after SQLite's
locking-protocol refusal, as ordinary opens do once: it returns the refusal,
classified as a busy store (see [the SQLite store](sqlite-store.md)).

The command requires both a stored and resolved matching host-path policy.
It never binds a missing one. Without the global `--host-path-policy` override,
the ordinary project-root probe resolves identity; it looks the project file up
under the opposite case and writes nothing. That probe and SQLite
shared-memory coordination are not database/WAL content mutations.
A read-only SQLite open may create an empty WAL and its shared-memory sidecar;
the absence of those files is not promised.

**All three booleans false is evidence of absence in that snapshot only.**
Exit 0 with any true boolean does not admit absence-based reconciliation.
The host must compare the exact project, canonical database, selectors,
scope/version and path policy, retain its reset/quiescence fence, establish
non-running eligibility, and recheck the same old connection and persisted
store identity plus session/token/grant ownership before committing its local
reconciliation. A copied or replaced store cannot be authenticated by this
receipt. No snapshot fences later writes or grants permission to clear state.

## Stored diagnostic facts

The additive `details` object has its own `schema_version:1`. The outer version,
scope and three presence booleans retain their existing meaning. A consumer
that needs only absence evidence may ignore the additional object.

- `session` contains `status` (`found`, `not_found` or `withheld`), nullable
  `project_match`, `phase` and `phase_status`. A session in the selected project
  exposes its recognized stored phase: `ready`, `sync_required`, `turn_open`
  or `exited`. An unknown value is null with `phase_status:"unrecognized"`;
  arbitrary stored strings are never echoed. A foreign session has withheld
  details and `project_match:false`.
- `session_begun_grant_present` is a stored `state='begun'` row's presence for
  the selected-project session, independent of its phase. It is null when
  the session is missing or withheld. It neither validates that grant nor
  grants authority; `turn_open` alone does not prove a turn was begun.
- `retained_grant` contains `status`, nullable `project_match`, `state`,
  `state_status`, `session_id`, `session_relation` (`target` or `other`),
  `owner_session_present`, `matches_current_task`, `issued_at`, `expires_at`,
  `begun_at`, `completed_at` and `timestamps_status`.

Grant attribution follows its task anchor, not its owner's current project:
an old grant remains in its original project after a session rebind. Only a
grant attributed to the selected project exposes state, times and a validated
owner session id. Foreign grants have `status:"withheld"` and
`project_match:false`; an orphan anchor has withheld details and a null match.
Withheld or missing details are null, not default state or timestamp values.
The top-level presence booleans still count those rows. Invalid owner ids are
not echoed. Relationship fields describe stored rows, not credential validity.

Recognized grant states are `issued`, `begun`, `completed`, `expired` and
`superseded`. A successful checkpoint stores `completed`; `checkpointed` and
`consumed` are not alternative stored states. Unknown state is null with
`state_status:"unrecognized"`. Timestamps are RFC 3339 UTC values from stored
millisecond columns. Nullable begin/completion times remain null when absent;
invalid values are null with `timestamps_status:"invalid"`. State/time shape
disagreement is `inconsistent`; otherwise it is `valid`. Timestamp ordering
is not assumed, and an unrecognized state cannot establish shape coherence.
There is no separate consumption or expiration-transition timestamp.

In particular, inspection distinguishes `begun`, stored `expired`, and still
`issued` with an elapsed `expires_at` deadline. It never changes the last case
to expired. These facts do not prove that any particular historical request
was replayed, that expiry came from a particular attempt, or that a checkpoint
will pass its other scope/schema/fence checks. No predicted refusal or replay
outcome is returned. Routing tokens, connection credentials, actors, intents,
idempotency keys, stored bodies and checkpoint directives are not disclosed.

The facts are read inside the same admitted snapshot as presence. The
diagnostic reads have a fixed 100,000 SQLite VM-instruction budget because
checking for any begun grant can scan a session's history. Exhaustion refuses
the whole inspection, with no presence booleans or partial details. This is
not a wall-clock deadline or a bound on filesystem stalls; normal admission
and the host's end-to-end deadline remain separate.

## Refusals

Missing stores, incompatible schemas, unresolved/unbound/mismatched path
policy, invalid policy, busy locks and uncertain reads refuse. An admitted
invocation emits a scoped version-1 receipt with
`code:"control_session_inspection_refused"`, `mutation_enabled:false` and a
human-readable `reason` retaining error causes, then exits nonzero. The reason
is diagnostic text, not a machine-readable retry or repair instruction. This
scope does not define per-cause retry categories; every refusal stops the
current absence-based reconciliation attempt. Identity fields may be omitted
or null on resolution failure. **Presence booleans are omitted, never defaulted false.**
Invalid CLI syntax/selectors use clap exit 2 and may have no JSON receipt.
Other session-admission surfaces use length-only validation; historical blank
or control-character ids therefore may exist but cannot be inspected through
this command. This is a selector limitation, not a transient read failure:
retrying the same invalid selector cannot establish absence. Do not normalize
the id or clear its state to bypass the refusal. Nonblank whitespace-padded
ids without control characters are admitted and queried exactly, without trim.
Missing fields, unknown versions/scopes, malformed output, timeout and any
nonzero exit provide no absence evidence. Never fall back to a writable
opener, full audit, `SessionStatus`, or interpreting stderr as clearance.
