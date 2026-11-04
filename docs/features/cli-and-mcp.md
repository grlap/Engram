# CLI & MCP Surface

> Normative reference: [spec §8](../spec.md#8-interfaces). Related briefs:
> [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [atomic work plans](atomic-work-plan.md),
> [local tasks & reports](local-tasks-and-reports.md), and
> [Execution Observability](execution-observability.md).

One core library owns object storage, scope authorization, work binding,
ordered deltas, and keyed project-memory operations. The CLI and MCP server
are thin faces over it; transport code does not redefine memory policy. The
agent sees fourteen words; every host and operator control lives under
[Host integration](#host-integration).

This index keeps the original section links. Follow a topic link for the complete contract.

## History and evidence receipt facts

Continue in [History and evidence receipt facts](cli-agent-usage.md#history-and-evidence-receipt-facts).

## Using Engram as an agent

Continue in [Using Engram as an agent](cli-agent-usage.md#using-engram-as-an-agent).

## Host integration

Continue in [Host integration](cli-host-and-operator.md#host-integration).

### Build identity and doctor refusals

Continue in [Build identity and doctor refusals](cli-host-and-operator.md#build-identity-and-doctor-refusals).

### Host and operator CLI

Continue in [Host and operator CLI](cli-host-and-operator.md#host-and-operator-cli).

#### Deadlines and cancelling a push

Continue in [Deadlines and cancelling a push](cli-host-and-operator.md#deadlines-and-cancelling-a-push).

#### The `backup status --json` receipt

Continue in [The backup status --json receipt](cli-host-and-operator.md#the-backup-status---json-receipt).

The field table stays here because the receipt contract test reads this path. The complete operator contract is linked above.

<!-- backup-status-receipt:begin -->

| Path | Type | Meaning |
| --- | --- | --- |
| `schema_version` | integer, always `1` | The receipt's schema. |
| `durability` | object | The mode with what backs it; a host never shows `mode` without `off_host`. |
| `durability.mode` | string, one of: `local`, `local_backed_up` | `local_backed_up` when at least one kind qualifies under the freshness rule. |
| `durability.off_host` | array | One entry per qualifying kind; empty with `local`. |
| `durability.off_host[].kind` | string, one of: `store` | The qualifying kind. |
| `durability.off_host[].off_host` | string, one of: `off-host asserted; not verified` | How far the copy is known to leave the machine. |
| `durability.off_host[].restores` | string, one of: `the same store at the copy's cut` | What the kind's copy restores. |
| `store_cut` | object or null | The store's current cut; null when it could not be read. |
| `store_cut.work_feed` | integer | The store's project work-feed head. |
| `store_cut.project_memory` | integer | The store's project-memory change position. |
| `store_cut_unavailable` | object or null | Why the store's cut could not be read; null when it was. |
| `store_cut_unavailable.code` | string | The store refusal's typed code, such as `store_not_initialized`. |
| `store_cut_unavailable.message` | string | The refusal's text. |
| `running_build` | string or null | The build fingerprint of the build reporting; null when it cannot name itself. |
| `as_of` | string, an RFC 3339 time | When the status was evaluated, read after the records. |
| `kinds` | array | One entry per copy kind. |
| `kinds[].kind` | string, one of: `store` | The copy kind. |
| `kinds[].qualifies` | boolean | Whether the kind qualifies under the freshness rule. |
| `kinds[].reason` | string or null, one of: `backup_record_unreadable`, `backup_not_configured`, `backup_never_confirmed`, `backup_target_changed`, `backup_other_format`, `backup_clock_invalid`, `backup_copy_missing`, `backup_confirmation_expired`, `backup_stale` | The first reason it does not qualify; null when it qualifies. |
| `kinds[].unreadable` | string or null | For a record this build cannot use, its path and why. |
| `kinds[].target` | object or null | The configured target and its evidence; null when none is configured or its records cannot be used. |
| `kinds[].target.adapter` | string, one of: `directory` | The adapter. |
| `kinds[].target.location` | string | Where the copies go: for a directory target, its absolute path. |
| `kinds[].target.identity` | string | The target's identity, derived from its configuration. |
| `kinds[].target.window_hours` | integer | Hours a copy keeps the kind qualified. |
| `kinds[].target.off_host` | string, one of: `off-host asserted; not verified` | The target's off-host assurance. |
| `kinds[].target.disclosure_authorized` | object | The operator's statement authorizing the disclosure. |
| `kinds[].target.disclosure_authorized.by` | string | Who made it, as asserted. |
| `kinds[].target.disclosure_authorized.at` | string, an RFC 3339 time | When. |
| `kinds[].target.off_host_asserted` | object or null | The operator's statement that the target leaves this machine. |
| `kinds[].target.off_host_asserted.by` | string | Who made it, as asserted. |
| `kinds[].target.off_host_asserted.at` | string, an RFC 3339 time | When. |
| `kinds[].target.copy` | object or null | The newest receipt's copy; null before the first one. |
| `kinds[].target.copy.copy` | string | The copy's name at the target. |
| `kinds[].target.copy.acknowledgement` | string, one of: `read_back` | How the target acknowledged it. |
| `kinds[].target.copy.target_identity` | string | The identity the receipt was issued for. |
| `kinds[].target.copy.for_earlier_target` | boolean | Whether that is an earlier identity, not the configured target's. |
| `kinds[].target.copy.received_at` | string, an RFC 3339 time | When the receipt was recorded. |
| `kinds[].target.copy.capture_started_at` | string, an RFC 3339 time | When the copy's capture began. |
| `kinds[].target.copy.capture_age_seconds` | integer | Seconds from the capture's start to `as_of`. |
| `kinds[].target.copy.observed_equal_at` | string or null | When the store's content was last observed in this copy. |
| `kinds[].target.copy.cut` | object | The copy's cut. |
| `kinds[].target.copy.cut.work_feed` | integer | Its work-feed head. |
| `kinds[].target.copy.cut.project_memory` | integer | Its project-memory change position. |
| `kinds[].target.copy.store_moved` | object or null | How far the store has moved past the copy's cut; null when the store's cut could not be read. |
| `kinds[].target.copy.store_moved.work_feed` | integer | Work-feed positions since; negative when the store is behind the copy. |
| `kinds[].target.copy.store_moved.project_memory` | integer | Memory positions since; negative when the store is behind the copy. |
| `kinds[].target.copy.last_confirmation` | string or null | When the target last confirmed holding the copy. |
| `kinds[].target.copy.missing` | object or null | A recorded finding that the target no longer holds the copy. |
| `kinds[].target.copy.missing.at` | string, an RFC 3339 time | When it was found. |
| `kinds[].target.copy.missing.reason` | string | What was found. |
| `kinds[].target.copy.checking_build` | string or null | The build that captured and checked the copy, only when it is not the running build. |
| `kinds[].target.copy.checking_build_unknown` | boolean | Whether the copy's manifest names no checking build. |
| `kinds[].target.pending` | object or null | An attempt recorded before its put and not yet resolved. |
| `kinds[].target.pending.copy` | string | The attempt's copy. |
| `kinds[].target.pending.capture_started_at` | string, an RFC 3339 time | When its capture began. |
| `kinds[].target.pending.target_identity` | string | The identity it was recorded for. |
| `kinds[].target.pending.for_earlier_target` | boolean | Whether that is an earlier identity. |
| `kinds[].target.last_attempt` | object or null | The last push of the kind that could be recorded; a failure is shown even while the mode reads `local_backed_up`. A push refused before it takes the lock, one whose records this build cannot use, or one whose state cannot be written, is not recorded here. |
| `kinds[].target.last_attempt.started_at` | string, an RFC 3339 time | When it started. |
| `kinds[].target.last_attempt.ended_at` | string, an RFC 3339 time | When it ended. |
| `kinds[].target.last_attempt.outcome` | string, one of: `uploaded`, `unchanged`, `failed` | How it ended. |
| `kinds[].target.last_attempt.code` | string or null | A failure's typed `backup_*` code, or a store refusal's code. |
| `kinds[].target.last_attempt.message` | string or null | What went wrong. |
| `restore` | object or null | The restore this home recorded; null when none. |
| `restore.state` | string, one of: `pending`, `restored`, `unreadable` | Where it stands. |
| `restore.record` | object or null | The record; null when it cannot be read. |
| `restore.record.format_version` | integer | The record's format. |
| `restore.record.occurrence_id` | string | Required opaque UUID for this restore occurrence; retries preserve it, a new restore of the same copy gets a new id. |
| `restore.record.project` | string | The project. |
| `restore.record.source` | object | Typed restore source; only the fields of its kind are present. |
| `restore.record.source.kind` | string, one of: `copy`, `local_file` | A configured backup copy or a local-file restore. |
| `restore.record.source.copy` | string | For `copy`, the restored copy. |
| `restore.record.source.path` | string | For `local_file`, the original absolute source selector; retries need not reopen it. |
| `restore.record.source.replace` | boolean | For `local_file`, whether the original request allowed replacement; retries keep this intent. |
| `restore.record.sha256` | string | The copy's SHA-256. |
| `restore.record.source.origin_host` | string or null | For `copy`, the host that captured it, as its manifest names it. |
| `restore.record.source.origin_retired` | object | For `copy`, the operator's statement that the origin will never run again. Local-file restore makes no retirement assertion. |
| `restore.record.source.origin_retired.by` | string | Who made it, as asserted. |
| `restore.record.source.origin_retired.at` | string, an RFC 3339 time | When. |
| `restore.record.staging` | string | The staging file the copy was fetched into. |
| `restore.record.state` | string, one of: `pending`, `completed` | `completed` once the installed store was checked. |
| `restore.record.pending_at` | string, an RFC 3339 time | When this occurrence's pending record was first written. |
| `restore.record.completed_at` | string or null | When it was completed. |
| `restore.record.previous_completed` | object | Optional pending-only binding to the completed occurrence kept aside before replacement. |
| `restore.record.previous_completed.occurrence_id` | string | The earlier completed occurrence's unchanged UUID. |
| `restore.record.previous_completed.file` | string | Its filename within this project's backup-record directory. |
| `restore.unreadable` | string or null | For a record this build cannot use, its path and why. |
| `checks` | array, only with `--check-target` | One entry per kind checked; absent otherwise. |
| `checks[].kind` | string, one of: `store` | The kind checked. |
| `checks[].outcome` | string, one of: `confirmed`, `missing`, `unreachable`, `timed_out`, `unknown`, `nothing_to_check` | What the target said. |
| `checks[].code` | string or null | The typed code of an outcome that is not a confirmation. |
| `checks[].reason` | string or null | Its text. |
| `checks[].copy` | string or null | The copy checked. |
| `checks[].recorded` | boolean | Whether the result was recorded. |
| `checks[].not_recorded` | string or null | Why a confirmation or a missing finding was not recorded. |

<!-- backup-status-receipt:end -->

### MCP tools

Continue in [MCP tools](cli-mcp-and-work-protocols.md#mcp-tools).

#### Read-only mode

Continue in [Read-only mode](cli-mcp-and-work-protocols.md#read-only-mode).

### Work protocol contract

Continue in [Work protocol contract](cli-mcp-and-work-protocols.md#work-protocol-contract).

### Host-private work-session channel

Continue in [Host-private work-session channel](cli-mcp-and-work-protocols.md#host-private-work-session-channel).

#### Identifiers and fingerprints on the work-session channel

Continue in [Identifiers and fingerprints on the work-session channel](cli-mcp-and-work-protocols.md#identifiers-and-fingerprints-on-the-work-session-channel).

### Host configuration

Continue in [Host configuration](cli-mcp-and-work-protocols.md#host-configuration).

### Dogfood contract

Continue in [Dogfood contract](cli-mcp-and-work-protocols.md#dogfood-contract).
