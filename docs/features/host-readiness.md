# Host readiness and full audit

> Normative reference: [spec §8](../spec.md#8-interfaces).
> Related: [CLI and MCP](cli-and-mcp.md), [host checklist](../host-checklist.md),
> [SQLite admission](sqlite-store.md), [security and trust](security-and-trust.md).

Host Verify/Save can call:

```bash
engram --home HOME --project-file FILE readiness --json
```

This operator command opens an **existing store read-only**, runs strict
current-schema and control-policy/authority admission, compares the resolved
host-path policy, and returns the selected project/store and current control
policy. It never initializes, repairs, persists a path binding, changes policy,
creates a session or authorizes a turn. SQLite may create its read-coordination
sidecar, but the command does not write database or WAL bytes.
Without `--host-path-policy` or `ENGRAM_HOST_PATH_POLICY`, the CLI probes the
project root by creating, reading and removing a uniquely named temporary file.
That probe can be visible to file watchers and can fail on a read-only checkout;
the store read-only guarantee does not mean zero project-root file I/O.

It does not run the work-history audit, reconstruct roots or enumerate historical
sessions/grants. Policy-chain admission remains required, so this is not a
constant-time promise. Full Audit is a separate explicit host action using
`engram doctor --json`; its existing health contract is unchanged. A host must
not call the full audit implicitly to save settings, report readiness as full
health, or erase a previous audit result when it reads readiness.
Readiness does not emit Full Audit's no-op-redactor and control-limitation
stderr warnings. `doctor --json` retains those disclosures: the development
redactor provides no secret or PII protection, and action gating, organizational
authority mediation and action-outcome tracking remain unavailable. A quiet
readiness stderr is not evidence of these protections. Hosts still compare the
reported required assurance with their actual mediation; no V1 host may claim
unsupported action gating.

## Version 1 receipt

Text mode exposes the same facts for human diagnosis; its layout is unversioned.
Human-readable lines escape unsafe terminal scalars; JSON preserves source values.
Hosts consume the versioned `--json` receipt, not text formatting or stderr prose.

Success is exit 0 and one JSON object on stdout with these required fields:

| Field | Meaning |
| --- | --- |
| `schema_version` | `1`, the receipt schema, not a database migration version |
| `scope` | `"readiness"` |
| `ready` | `true` |
| `full_audit` | `"not_run"` |
| `mutation_enabled` | `false` for this probe, not a policy for other commands |
| `project_id` | Nonempty id read from the selected project marker |
| `database` | Canonical absolute selected database path, as in doctor |
| `work_schema_version` | Current admitted work schema marker |
| `host_path_policy` | Object with nullable `stored`, nullable `resolved`, and `status` |
| `control` | Same fields and enum spellings as `control-policy show` |
| `build`, `build_fingerprint` | Existing doctor build diagnostics, never authentication or admission tokens |

The receipt never contains `healthy`. Diagnostic build fields can be unavailable
as documented in [build identity](cli-and-mcp.md#build-identity-and-doctor-refusals);
schema admission does not depend on those fingerprints. Example of the scoped
status fields (the actual receipt also includes the identity and policy above):

```json
{
  "schema_version": 1,
  "scope": "readiness",
  "ready": true,
  "full_audit": "not_run",
  "mutation_enabled": false
}
```

`control` contains `schema_version`, opaque record ids `policy` and
`obligation_rules`, numeric `epoch`, `required_assurance`, `supported_effects`,
and `acceptance_evaluation`. It does not include session/grant counts.
The host compares the required assurance with the mediation it actually provides;
a successful probe does not attest that the host mediates every turn.

Host-path strings use the same descriptions as doctor; do not parse them as an
authority token. `status` is `matched` when stored and resolved policies agree,
`unresolved` when the opener cannot resolve one, or `unbound` when it resolved
one but none is stored. A present mismatch refuses. Existing refusal for
path-bearing state without a stored binding remains. Unresolved reads do not
gain resolved path identity and unbound reads do not persist a binding. The global
`--host-path-policy` override is supported; otherwise the existing filesystem
probe resolves the project root's identity.
Omitting the CLI override requests that probe; it does not skip resolution.
For the CLI, `resolved:null` / `status:"unresolved"` means that probe failed.
The library also permits a caller to pass no identity deliberately. The status
does not encode the probe's detailed error; stderr supplies that diagnostic.
In either case the stored policy has not been compared with a resolved identity.
`ready:true` admits the scoped store read, not readiness for path-bearing control requests. Hosts must
inspect this status when their intended operation requires resolved identity.

Project identity uses normal explicit project-file/home routing; it is not
authentication of the owner of an arbitrary copied database. The host retains
binary/path trust, expected store identity, configuration/reset race fences,
grant revocation/rebinding and persistence checks. This read is an observation,
not a lease against subsequent policy or store changes.

## Refusals

For stale checkpoint handles, use the separate
[read-only session inspection](control-session-inspection.md) receipt.
Readiness itself supplies no session or grant absence evidence.

An admitted invocation that refuses emits one JSON object on stdout and exits 1.
Required fields: `schema_version:1`, `scope:"readiness"`, `ready:false`,
`full_audit:"not_run"`, `mutation_enabled:false`, `project_id`, `database`,
`code`, `phase`, `reason`, `remedy`, `build`, `build_fingerprint`.
Identities may be null when resolution failed. A refusal path is diagnostic only;
never bind host identity from it. `control` and `healthy` are absent.
For store-admission refusals, `detail` preserves the borrowed doctor diagnostic
(without its `healthy` field), including a projection-repair `scope` array.
Readiness supplies its refusal `reason` and any missing `remedy` in that detail.
Only the documented routing fields are promoted to the top level; `scope` there
always remains `"readiness"`. Extra diagnostic fields cannot replace the envelope.
Consumers use the top-level envelope for readiness posture and build diagnostics;
copies inside `detail` are borrowed diagnostic context and must not override it.

Codes are `project_resolution_failed`, `home_required`, `store_not_initialized`,
`projection_repair_required`, `different_build_schema`, `corrupt_store`, and
`store_open_refused`. The last also carries `kind` (`busy`, `permission`, `io`,
or `path_policy`), preserving existing error classification. Policy/authority
binding failures use `corrupt_store`, not operational contention. `phase` names
`resolve`, `open`, or `path`. `reason` and `remedy` are human-readable; consumers
must not match their prose. Additional diagnostic fields may be ignored.
`open` includes schema, policy and host-path-policy mismatch refusals; `path`
specifically means that canonical database identity could not be obtained after
the store read. It does not mean a host-path-policy mismatch.

`store_open_refused` with `kind:"busy"` can be transient. Hosts may offer a
later user-triggered or bounded retry, but must keep the current result not-ready.
Missing stores, incompatible schemas and corrupt state require the stated remedy,
not a retry loop. No retry may silently initialize, repair or run Full Audit.

Read-only admission cannot perform SQLite recovery that requires writes. If
SQLite returns `SQLITE_READONLY_RECOVERY` or `SQLITE_READONLY_CANTINIT`, the
existing classification is `store_open_refused` / `permission`; this is not
proof of an ordinary filesystem-permission problem. Check the detailed SQLite
reason and sidecar access. With operator approval, a separate writable opener
such as Full Audit (`doctor --json`) can attempt the required recovery. Readiness
never retries writable or silently runs that audit, and recovery is not guaranteed.

Invalid CLI syntax still uses clap stderr and exit 2, potentially without JSON.
An old binary lacking this command, an unsupported receipt version, malformed
output, timeout or nonzero exit never means ready. Hosts report unsupported
readiness rather than falling back to `control-policy show` or the full audit.
Warnings on stderr do not override a valid exit-0 readiness receipt.

## Evidence boundary

The readiness regression deliberately changes an unrelated work projection:
readiness still passes but full doctor fails. This is the intended distinction,
not recovery or a claim that the work history is healthy. Other regressions pin
missing-store no-creation, policy/scalar disagreement, incompatible schema,
projection-repair refusal, no path binding, path mismatch and structured
project/home refusals.
