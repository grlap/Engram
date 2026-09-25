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
hosts must also impose their own end-to-end process deadline.

The command requires both a stored and resolved matching host-path policy.
It never binds a missing one. Without the global `--host-path-policy` override,
the ordinary project-root temporary-file probe resolves identity. That probe
and SQLite shared-memory coordination are not database/WAL content mutations.
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
