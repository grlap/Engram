# Host and Operator CLI Reference

[CLI and MCP index](cli-and-mcp.md) · [Architecture](../architecture.md)

## Host integration

Selected checks may carry the typed
[Git-tree applicability restriction](verification-applicability.md). Its
active-store capability, refusal codes, attribution-only attachment, and
coordinated schema-barrier cutover are part of the host-private contract.

The [host checklist](../host-checklist.md) is the authoritative base-tier
recipe, including the Claude Code hooks form; this section keeps the
two-tier contract and the work-session details. Integration has two tiers,
and a host picks per project:

- **Base** — the tracker for agents: when the project's integration is enabled
  and supported by the runtime, the `engram mcp` server is injected into its
  supported sessions (the repository declares with its
  tracked `.engram-project`; the host supplies the stable project identity
  and asserted actor/session binding to the MCP child), plus a
  start-of-session nudge that runs `engram work next --peek` on session start and
  after compaction and injects its text as context at the next dispatched
  prompt, not an immediate runtime-authored continuation. Any host that can register
  an MCP server and run a hook can do this. It carried every benefit measured
  so far.
- **Execution Observability** (the optional tier of the checklist) — the
  [host-private JSON-lines work session](cli-mcp-and-work-protocols.md#host-private-work-session-channel) records host observations of
  execution, source and tests, named source roots and registered
  verification against the claims they belong to. It is switched on or off
  per project in the host, and never admits or refuses a model turn; see
  [Execution Observability](execution-observability.md).

A model can omit an MCP call, so the agent-facing MCP interface alone cannot
enforce synchronization, ownership, or finalization, and nothing in it grants
the agent authority. Everything below is the host's and operator's business:
the [agent words](cli-agent-usage.md#using-engram-as-an-agent) never require it.

### Build identity and doctor refusals

For host enablement, use [`readiness --json`](host-readiness.md) for scoped
existing-store/schema/policy checks. Keep `doctor --json` as a separate explicit
full audit; readiness is not full-store health and never returns `healthy`.
Like readiness, `doctor` examines an existing store and never creates one:
without an `engram.db` it refuses with `store_not_initialized`, whatever
else stands in the project directory, such as a restore's staging file.

`engram --version` prints `engram VERSION build FP12 (exe EXE12, schema
SCHEMA12, rev REV)`, where `REV` is the source revision's first twelve hex
digits, keeping a `+dirty` marker, or `unavailable`. Agent `next` ends its terminal text with one diagnostic line:
`build: FP12; read cut: project POSITION observed_at INSTANT`, followed by
`context_generation GENERATION` when supplied. Its structured CLI/MCP receipt
and core `work_next` carry one `build_fingerprint`, `read_cut` containing
`project_position` and `observed_at`, and optional `context_generation`.
For ordinary `next`, the cut is the shared advisory snapshot for focus, lists
and discovery, not the separately staged change-delivery cursor or the
project-memory signal's basis. For peek, all sections including changes and
the memory signal share this read snapshot; it still is not a delivery cursor.
`observed_at` is the call's supplied read instant; the feed position orders
committed state, not the timestamp. Retaining an older block retains its cut;
compare with a new read to see which committed state the block could reflect.
This does not promise automatic mid-turn refresh. TermAl, the live consumer,
fetches CLI `next --context-generation termal-N` text at dispatch and tolerates
these additive diagnostic fields; its generation is asserted host context, a
plain token as the [peek contract](cli-agent-usage.md#using-engram-as-an-agent) defines it.
Compare the build token with a fresh CLI process after an
install to detect a stale, long-lived MCP child; restart the child to run the
new executable. The token is diagnostic, not an execution hash to copy into
commands, authenticated identity, or store-admission authority.

Every `doctor --json` mode and `readiness --json` includes `build` and
`build_fingerprint`. The build
object contains `package_version`, `executable_sha256` (SHA-256 of the running
executable's bytes), and `schema_reference` (SHA-256 of the RFC 8785 canonical
ordered, whitespace-normalized SQLite definitions used by ordinary schema
admission), and `source_revision`: the commit the executable was built from,
that commit followed by `+dirty` when tracked files differed from it at build
time, or `unavailable` when the build could not determine it (no Git, no
checkout of this package, or a failed probe; the build itself never fails for
it). The build script records it, read-only, and reruns when a tracked file,
the index or the checked-out commit changes; after a failed probe it reruns
when `PATH` or the checkout's Git entry changes. Because the revision is part
of the executable, a commit or the first edit after one rebuilds it, so even
a documentation-only change gives a new executable and fingerprint. The fingerprint hashes the
RFC 8785 canonical build object, so the revision is part of it. These values
are captured once per process: at MCP startup, or when a short-lived CLI
process emits diagnostics. Agent words other than `next` do not compute
identity. There is no capability catalog or persisted last-writer row.
The source revision is recovery information: it names where to look for the
source, not proof that checking it out rebuilds the same executable. A
`+dirty` revision names its base commit plus changes that no commit records,
so the base alone does not hold them.
Equal inputs produce equal fingerprints; different executable bytes, schema
definitions or source revisions distinguish builds even when their package
versions agree.

The fingerprint identifies the exact executable reporting those build
components; it is not a reproducible identity for the runtime source tree.
Link metadata can change executable bytes between release builds of the same
clean commit. A landing record's installed build uses the fingerprint reported
by the installed executable. A restart signal names that executable's build,
the commit and the reason separately: a different fingerprint alone does not
establish that restarting gains a behavior change or fix.

An unreadable executable is explicitly `executable_sha256: null` with
`executable: "unavailable"`; an unavailable in-memory schema reference likewise
uses `schema_reference: null` and `schema: "unavailable"`. A canonicalization
failure leaves `build_fingerprint: null`, rendered as `unavailable`. Diagnostic
unavailability never refuses an agent word and must not be mistaken for proof
that two executables match.

An ordinary doctor open refusal emits `healthy: false` on stdout and exits
nonzero, including with `--json`. Text mirrors the same fields with escaped
values. `database` uses the same canonical path as a healthy report whenever
the path is readable, falling back to the supplied spelling otherwise.
Refusals carry `phase`: `open`, `verification`, `control_diagnostics`,
`control_policy_recovery`, or `projection_repair`. The phase identifies where
the error arose even when a diagnostic read failed after a successful open.
The codes are:

- `store_not_initialized`: an empty file has no user schema. Projection repair
  refuses it without initializing it. Run `engram init` explicitly when you
  intend to initialize that empty store.
- `projection_repair_required`: exact remedy
  `engram doctor --repair-projections` and safe scope
  `["indexes", "triggers", "fts", "derived_tables"]`; reporting performs no DDL.
  During repair, derived tables include reconstructed project-memory state and
  work projections; disposable project-memory advertisement bookkeeping is
  recreated empty.
  See the [projection inventory](sqlite-store.md#rebuildable-and-durable-projections).
- `different_build_schema`: `store_schema_reference`, read from the existing
  file using the same normalization, plus `running` build components. Use the
  build that created the store. There is no in-place store upgrade, and
  projection repair cannot convert a different durable schema.
  An extra index this build does not own also receives this refusal, including
  from `doctor --repair-projections`. Ordinary open gives the same safe advice
  to use the owning build, not to restore or re-initialize the store.
  FTS naming prefixes do not make undeclared objects repairable. Only declared
  objects and the declared FTS tables' known shadow tables are rebuildable.
  The schema digest is not a claim to know the original executable;
  a schema-marker-only mismatch can have equal definition digests. If the
  refused file cannot be read, the digest is null with
  `store_schema: "unavailable"`; reporting never creates a missing file.
- `corrupt_store`: one `findings` array of strings describing the refused or
  invalid records, including failures found during post-open verification.
- `store_open_refused`: an operational/configuration failure, not a corruption
  claim. Its `reason` preserves the underlying error verbatim and `kind` is
  `busy`, `permission`, `io`, or `path_policy`. The remedy names retry for
  contention, the path and reported error for access/I/O failures, or both
  policies and the flag that selects the recorded case policy for a mismatch.
  An incompatible host alias-rule setting cannot be changed by that flag;
  its remedy points to a compatible host or a fresh store at a new location.
  Ordinary open's reason uses the same two-branch advice and never tells the
  operator to re-initialize the existing store in place.
  Operational failures retain this category after open; they never become a
  corruption claim just because a later control diagnostic failed.

These diagnostics do not change ordinary admission or explicit repair. See
the [SQLite contract](sqlite-store.md#canonical-bytes-contract).

### Host and operator CLI

```bash
export ENGRAM_HOME=/absolute/host-local/path
engram init
engram doctor

# Fast read-only store/policy admission; not a full audit or execution authority.
engram readiness --json

# When ordinary open refuses a corrupt control-policy chain, inspect only that
# immutable family. This mode is read-only, enables no service/mutation API,
# and never selects or rewrites a policy head.
engram doctor --recover-policy [--json]

# Explicitly rebuild declared projections and reset memory advertisements.
# Ordinary open never performs this repair implicitly.
engram doctor --repair-projections [--json]

# One-time note-search index cutover, with consumers stopped. Builds only the
# absent work_note_fts; other projections must already have current shapes.
# Existing or damaged target objects require the full repair above.
engram doctor --build-note-search-index [--json]
```

The note-index build uses one writer transaction and one prepared insertion
statement, verifies the new index against canonical note text, and commits only
after strict schema admission succeeds. It preserves every other projection,
including memory advertisements. It does not run the full integrity audit:
JSON reports `scope: "work_note_fts"` and `full_audit: "not_run"`. Ordinary open
and readiness remain strict and never build the index implicitly. Stop store
consumers for this one-time cutover and restart them after readiness succeeds.
The existing full `--repair-projections` still rebuilds and verifies all declared
projections when repair is needed.

```bash

# On request only: check each landing the seals record against a local git
# repository (default: the project file's directory). Reads local objects and
# remote-tracking refs only; never fetches. Each landing's installed build is
# printed apart from that answer, in full, as asserted and unchecked.
engram doctor --check-landings [--repo PATH] [--json]

# Host/operator boundary: select a bounded typed obligation set as a new
# immutable policy version. The optional expected policy id is the `id=`
# reported by doctor and prevents a stale operator from overwriting a
# concurrent policy update.
engram control-policy set-obligation-rule-set \
  --input @obligation-rules.json \
  --authorized-by host-operator \
  --idempotency-key pin-repository-verification \
  --expected-policy-hash <active-policy-id>

engram mcp \
  --actor-id codex \
  --session-id session-unique-id \
  --actor-context 'model=opus-4.1;reasoning=high' \
  --source-skill engram-repo
engram control \
  --work-session \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo

# Host-local loss recovery: a verified full copy of the store, and the way
# back. A backup carries host-private state and private scratch, so it is exactly as
# sensitive as the store; schedule it on the host, never publish it.
engram backup                      # → <home>/backups/<project>/engram-<utc>.db + manifest
engram restore --from <backup-file> [--replace]   # stop other Engram processes first
engram restore --from <backup-file> --abandon-pending --abandoned-by <operator>

# Off-host backup targets: the operator's record of where a kind's copies go.
# Setting a target copies nothing.
engram backup target set --kind store --adapter directory --dir <absolute path> \
  --disclosure-authorized-by <operator> --off-host-asserted-by <operator> \
  [--window-hours N] [--keep N]
engram backup target show [--json]
engram backup target clear --kind store
# Read the copies a target holds, from any home that configured it.
engram backup list [--kind store] [--json]
engram backup fetch <copy> --out <file> [--kind store] [--json]
# Install one onto a home that has no store for the project.
engram backup restore <copy> --origin-retired-by <operator> [--json]

# Deterministic planning/history disclosure. The default path is
# <home>/snapshots/<project>/graph-<work-cut>-<memory-cut>-<first-12-body-digest>.json.
# --include-restricted deliberately widens restricted project-memory bodies and
# therefore requires a disclosure reason carried in the body and save audit.
engram graph save [--out <snapshot.json> | --stdout] \
  [--include-restricted --reason "<why>"]
engram graph load <snapshot.json> [--dry-run]

# Agent words use the stable project plus asserted actor/session binding.
engram work --actor-id codex --session-id session-unique-id next
engram work --actor-id codex --session-id session-unique-id \
  claim <short-ref>
engram mcp --actor-id codex --session-id session-unique-id
# The same identity, for a child that must not mutate the store.
engram mcp --actor-id codex --session-id session-unique-id --read-only

# Host/operator escape hatch: the six-operation JSON protocol from the shell.
engram work --actor-id codex --session-id session-unique-id \
  core focus <short-ref>
# Read the same view without selecting focus or touching delivery.
engram work --actor-id codex --session-id session-unique-id \
  core inspect <short-ref>
# List the claims this session holds, each with its binding or null.
engram work --actor-id codex --session-id session-unique-id \
  core held
```

`graph save` and `graph load` are operator-only CLI surfaces; neither is an MCP
tool or changes the fourteen agent words. Save reads the work graph, native
history, source provenance, and keyed project memories at one transaction cut,
then commits an immutable disclosure-attempt audit before publishing bytes.
The canonical body excludes the exporting build, so its digest remains content
identity across builds with the same runtime-derived format fingerprint. The
default output is owner-only where the platform has file modes, no save may
target an Engram project-store directory, and neither the default path nor
`--out` replaces different bytes. Load requires an empty destination project,
revalidates the fingerprint, canonical body digest, manifest, relations,
history proofs, and memories before one atomic recreation transaction, and
records a separate immutable load audit. `--dry-run` performs the same
validation and reports the landing plan without writing. See the
[work-graph snapshot](work-graph-snapshot.md).

`backup target set`, `show` and `clear` are operator-only words as well; they
change none of the fourteen agent words and never open the store. A target
belongs to one project and one copy kind; this build configures the `store`
kind at a `directory` adapter. `set` requires
`--disclosure-authorized-by` (the destination may hold everything the kind
carries) and, for a directory, `--off-host-asserted-by` (the destination
leaves this machine). Both are recorded as asserted context with the
operator's name and the time, and a directory target always reads
"off-host asserted; not verified". `--dir` must be an absolute path, a
Windows share included; it is kept as spelled and never contacted when set.
The window defaults to 24 hours and the retention to three copies. `set`
replaces any earlier target of its kind and derives the target's identity
from the project, kind, adapter, location and both statements whenever it is
needed; it is not stored. A first `set` starts the kind's recorded state
empty. A later one keeps a readable earlier state: its receipts and pending
attempt keep naming the identity they were made for, so they stay as history
that qualifies nothing for the new target; a state whose content this build
cannot use is started anew. The configuration and state are files under
`<home>/backup-records/<project>/`, each with a format version and replaced
whole. A record this build cannot use is left untouched: one it cannot read
or parse, one `set` would have refused, or a state recorded for another
target than the configured one. `show` and `clear` refuse it with
`backup_record_unreadable` and name the file. `set` is the way on from a
record whose content this build cannot use, writing both files for the kind
anew. A state file that cannot be read at all, as when another program holds
it, access to it is denied or a directory stands in its place, is not
replaced: `set` refuses with `backup_io` and changes nothing, and the
operator restores read access to the file or removes it before `set` again.
`set` and `clear` take the kind's push
lock, an operating-system lock that its holder's exit releases, and refuse
with `backup_push_running` while another process holds it. The CLI parser
refuses a missing required flag or an unknown kind or adapter before any
code applies; a request it accepts but Engram refuses is
`backup_target_invalid`. On a configuration/state identity mismatch, `show`
and `clear` also try that lock to reread both records: a concurrent `set`
may be between writing its state and configuration. A held lock returns
`backup_push_running`; a mismatch that remains after the locked reread is
`backup_record_unreadable`. A `show` or `clear` briefly holding the reread lock
can make a concurrent `set`, `clear` or restore refuse busy, make `backup push`
report `Busy` and skip that invocation, or keep a `status --check-target`
result from being recorded. The check still reports its result.
If acquiring the mismatch reread lock fails for another reason, the original
`backup_record_unreadable` diagnosis is kept. A consistent `show` takes no
lock. A `set` whose state was written but whose
configuration was not reports `backup_record_partial` and is repeated. A
record that exists but cannot be read is `backup_record_unreadable` like any
other record this build cannot use; a record that cannot be written or
removed, and a writer's lock file that cannot be opened or locked, is
`backup_io`. This includes `clear`'s own writer lock; only its earlier
mismatch reread preserves the original record diagnosis on a lock-file error.
`show` on a clean home prints that no target is configured. See
[off-host backup](off-host-backup.md#configuration).

Status, the backup reminder, and initial target selection for list, fetch and
target checking remain lock-free. On an identity mismatch they discard both
reads and reread the pair once. A valid pair settled before the retry reads
its configuration is accepted if nothing changes again. A writer still
publishing, or repeated changes, may leave another mismatch: this remains
`backup_record_unreadable`, with a hint that a target set may be in progress
and to retry the read. This bounded reread does not wait for a writer or
provide an atomic snapshot. Other record failures keep their existing
diagnoses; a check still revalidates under the lock before recording a result.

`backup push [--kind store] [--json]` is an operator word too. For each
configured kind it takes the kind's push lock and brings the copy at its
target up to date, in the steps of [off-host backup](off-host-backup.md#push).
It first resolves an attempt an earlier push left pending: a copy the target
confirms becomes the newest receipt, one that never arrived is dropped and
only its own recorded files are removed, and one recorded for another target
identity is set aside as history with nothing at either target touched. It
then captures a verified store copy into a local stage under the home. The
upload is skipped only when the newest receipt names the configured target's
identity and the capture's format identity and SHA-256, and the target
confirms that copy now; the capture's start is then recorded as the time the
store's content was last observed in it, and the confirmation's time as the
copy's last confirmation. When the target reports that copy missing, the
finding is recorded before a replacement is prepared, so it stands even if the
replacement fails. Otherwise the attempt is recorded as
pending, with its complete manifest, target identity and data file names,
before the gzip copy is put and read back, and its receipt then becomes the
newest. Once that receipt is recorded, and only after a push that did not
fail, copies beyond the target's retention count (`--keep`, three by default)
are removed from the target, oldest first, after the last attempt is
recorded, so its end time does not include them. Only copies whose receipts this
home recorded for the current target identity count, and the newest copy is
never removed, so a copy another home put there, or one made for an earlier
identity of the target, is never touched. A removal that fails is a warning,
and that copy stays recorded for a later push to remove; `--json` lists the
removed copies. The stage is removed at the end, and a stage a push could not remove
is removed by the next one before it captures: only a directory a capture
created, never a link, and in it only the files a push writes.

Each JSON kind report includes `capture_check`: `full` for a copy checked
in full, `same_bytes_as_newest` when its settled SHA-256 and length equal
the newest copy at the configured target and this same build checked that
copy, or `null` when no capture was made. Byte equality skips the full
check and compression, but still copies and hashes the store and confirms
the target. An unknown or different checking build runs the full check;
a missing target copy is checked in full before preparing its replacement.
The field reports work performed, not qualification: only equality plus a
successful target confirmation advances `observed_equal_at` to this
capture's start. Failed confirmation, `busy` and termination refresh nothing.
`backup status` exposes no cheap exact store-wide change marker; its cut
does not cover claims, control rows or delivery state.

Capture and transport have separate deadlines, `--capture-deadline-secs`
(900 by default, for the local copy and the compressed file prepared from
it) and `--transport-deadline-secs` (1800 by default, for every request to
the target together). A request to the target runs on a worker thread, and
one with no time left is not started. When a request before the receipt,
resolving a pending attempt, confirming the newest copy or the put, passes
its deadline, the push records the failure, keeps the push lock, and ends
its process with exit 1, which is how a request stalled inside the
operating system is cancelled; the lock is released only by that end. A
retention removal that passes the deadline comes after the receipt is
recorded: it is a warning, and the process ends the same way but with the
push's own exit code. Push is therefore a CLI process by design and must not
be run in-process inside a long-lived server. A read stuck in the kernel on
a hung share can delay that process exit, as it would delay any process's
end. A copy the target completed after the deadline is found by the next
push and recorded as confirmed. The contract a host builds on is
[below](#deadlines-and-cancelling-a-push).

Push exits 0 when it uploaded a copy, found the store unchanged, found no
target configured (it says so and creates nothing) or found another push
holding the lock (it says so and captures nothing); a removal for retention
that passes the transport deadline is a warning, and the process then ends
with that exit code too. It exits 1 when it failed,
and prints the typed code: `backup_stage_no_space`,
`backup_stage_space_unknown` and `backup_capture_deadline` from the capture,
a store refusal such as `store_not_initialized`, `backup_target_unreachable`,
`backup_target_no_space`, `backup_target_space_unknown`,
`backup_copy_invalid`, `backup_copy_exists` and `backup_io` from the put,
`backup_target_unconfirmed` when the target cannot say whether it holds an
unchanged copy, `backup_pending_unresolved` when it cannot say what became of
a pending attempt, `backup_transport_deadline`, and
`backup_record_unreadable` for records this build cannot use. A failed push
leaves the previous receipt and the previous copy at the target as they were,
advances no time, and records the attempt's start, end, outcome, code and
message as the last attempt, with three exceptions that the `backup status`
receipt's `kinds[].target.last_attempt` row also gives: a push refused before
it takes the kind's lock, one whose records this build cannot use (the kind's
`reason` shows those records), and one whose state cannot be written record
no last attempt, and the earlier one stays. A push whose final state write
fails may still have recorded earlier steps, such as its pending attempt or
a missing finding. When the push otherwise succeeded (`uploaded` or
`unchanged`) and only that final write failed, it is `failed` with the write
error as its code and message, and the error adds no warning; when it had
already failed, the unwritten state is a warning. A push
that asks the target to confirm the newest copy and learns that the copy is
gone first records that copy as missing, so it stops qualifying, and then
puts a new one; if that replacement fails, the missing finding stays
recorded. `--json` prints the project and, per kind, the
outcome (`uploaded`, `unchanged`, `not_configured`, `busy` or `failed`), the
code and message, the target identity, the newest receipt with its manifest,
the time the content was last observed in that copy, the copies it
recovered, dropped, set aside or left pending, the copies retention removed,
warnings and the elapsed milliseconds.

#### Deadlines and cancelling a push

This is the contract a host builds its push trigger on.

**Two deadlines, each set by its flag.**

- `--capture-deadline-secs N`, 900 by default, bounds the local work: the
  running build's identity, the stage's space check, the read-only open of
  the store, the copy, which is the only time the push holds a read
  transaction on the store, the copy's full check and hash, and the
  compressed file prepared from it. On a process's first capture, reading
  the build's identity hashes the running executable on a thread of its
  own; the capture waits for it only until the deadline, and a deadline
  passed first fails the capture in its `space` step before any stage
  exists, while the reading goes on for a later capture. The rest is
  checked cooperatively: after the space check, by SQLite's progress
  handler during the copy and its check, between chunks of the copy's hash
  and of the reads that build the compressed file, and once that file is
  written. When another connection holds a lock the copy needs, SQLite
  calls the copy's busy handler, which reads the clock after each short
  sleep. One such wait ends at the deadline or five seconds after it
  began, whichever comes first; a wait cut off by the five seconds fails
  the push with outcome `failed`, code `storage_error` and SQLite's
  "database is locked" message, not with the `busy` outcome, which means
  another push holds the lock. A sleep can last longer than asked, so a
  wait can end a little late. SQLite's own retries to begin a read on the
  write-ahead log sleep inside SQLite, outside this handler; the
  [SQLite store](sqlite-store.md) describes them.
  No call is interrupted while it runs, so one that stalls holds
  the push until it returns.
- `--transport-deadline-secs N`, 1800 by default, is one budget for every
  request to the target together: resolving a pending attempt, confirming
  the newest copy, the put with its read-back, and retention removals. Each
  request runs on a worker thread with the time left; with none left, no
  request starts.
- No deadline bounds the rest: taking the lock, which never waits, reading
  and writing the records, removing leftover stages and record files,
  removing the stage, and printing the report. These are local steps,
  usually brief but not guaranteed to be.
- 0 is accepted and expires at once, unlike `backup status`'s
  `--check-deadline-secs`, which must be at least 1. With a zero capture
  deadline the push
  still resolves an earlier pending attempt, which may ask the target, and
  then fails with `backup_capture_deadline` before it creates a stage. With
  a zero transport deadline no request starts and the push fails with
  `backup_transport_deadline`: when an upload was due, its attempt is
  recorded as pending and nothing is put; when the store equals the newest
  copy, no new attempt is recorded. An attempt already pending stays
  pending, since resolving it is a request too. Either way the failure is
  recorded as the last attempt.
- A deadline too far off for the clock to represent, such as
  18446744073709551615 seconds, sets no limit on what it bounds.

**At the capture deadline** the capture stops at its next check, which ends
its read transaction, and removes its stage. Nothing of this capture is sent
to the target. The push records `backup_capture_deadline` as the last
attempt and exits 1; the earlier receipt stands.

**At the transport deadline** the request still running is left on its
worker thread. Before a receipt, that is while resolving a pending attempt,
confirming the newest copy or putting a new one, the push records
`backup_transport_deadline` as the last attempt and exits 1 at once. A put's
attempt was recorded as pending before the put and stays pending, as does an
earlier attempt being resolved; a confirmation of an unchanged copy leaves no
pending attempt. The lock is held until the process ends, which is what
stops the stalled request. After a confirmation or a put, the capture's
stage is left for the next push to remove, since the worker may still read
it; a resolution runs before the capture and leaves none. When the budget
is spent before a request starts, no worker is left: the push removes its
stage, records `backup_transport_deadline` and exits 1 as usual.

A request also checks its own deadline between chunks of its reads, and it
can get there before the push stops waiting for it. The request then ends
as a failure of its own: the push removes any stage it made, releases the
lock as it exits, and records `backup_pending_unresolved` for a resolution,
`backup_target_unconfirmed` for a confirmation, or
`backup_transport_deadline` for the put's read-back, after removing the data
file it put. The exit code, 1, and what stays pending are the same either
way, so a host acts on the exit code and the outcome rather than on which of
these codes it got.

During retention the receipt is already recorded: the timeout is a warning,
the copy stays in the ledger for a later push to remove, and the process
ends with the push's own exit code, 0. A put the target completes after the
deadline is found by the next push, confirmed and recorded as recovered; one
that never arrived is dropped and only its own files are removed.

**Cancelling a push.** A host cancels a push by terminating its process:
`TerminateProcess`, for example `taskkill /F /PID`, on Windows; `SIGTERM` or
`SIGKILL` elsewhere. Push installs no signal handler, so Ctrl-C and
`SIGTERM` act as a kill, and it starts no child process, only threads, so
ending the one process stops all its work. A terminated push prints no
complete report. Whatever it recorded before the termination stands, and
nothing after it is recorded: its own last attempt is written only as it
finishes, so until then `backup status` keeps showing the earlier one, and
the host's own record of the termination is the only record of it. Its exit
code is whatever the system reports; treat it as a failure whose outcome is
unknown. What it leaves depends on where it was:

| Terminated | What it leaves | What the next push does |
| --- | --- | --- |
| Before the capture | Nothing of its own. An earlier pending attempt it was resolving stays pending unless its resolution was already recorded. | Resolves that attempt again and proceeds as usual. |
| During the capture, the confirmation of an unchanged copy, or the preparation of the compressed file | A stage directory under the home holding the partial or whole copy and, once prepared, the compressed file. The store's read transaction ended with the process; the store is untouched. | Removes the stage before its own capture. |
| After its attempt was recorded, before the target held the whole copy | The stage with the whole copy and the compressed file, the pending attempt, and possibly a temporary file at the target. | Removes the stage, finds the copy missing, removes the attempt's own files at the target, drops the attempt and puts a new copy. |
| After the copy's data file was in place, with or without its manifest, before the receipt was recorded | The stage with the whole copy and the compressed file, the pending attempt, and its copy at the target. | Removes the stage, writes the manifest when it is missing and the data matches it, confirms the copy and records it as the newest receipt, reported as recovered. |
| During retention | The receipt and the successful last attempt, both recorded; a copy may already be removed at the target while the ledger still lists it. | Removes it again, where a copy already gone counts as removed, and updates the ledger. |
| While a record file was replaced | The earlier or the new record, whole, and possibly a `<record>.<uuid>.tmp` file beside it. | Removes such files of its records under the lock: `store.target.json`, `store.state.json` and `store.restore.json`, with a UUID in its canonical form, and nothing else. |

In every case the system frees the lock when the process ends, though not
necessarily by the time the host's wait for it returns; a push started
meanwhile exits 0 with the outcome `busy`. The next push resolves what is
listed only when the target is reachable and its own cleanup succeeds;
otherwise it fails with its typed code, or warns about a stage it could not
remove, and the push after it tries again.

**The host's bound.** With the default deadlines a host terminates a push
still running after 900 + 1800 + 60 = 2760 seconds; with other deadlines,
their sum plus 60 seconds for the steps no deadline bounds. This is the
host's threshold, not a promise that the process has ended by then: a read
stuck in the kernel on a hung share can delay the end of any process,
including one being terminated. On reaching it, the host terminates the
process, records the termination on its side, and does not retry before its
next scheduled push.

**Control calls.** A push shares no lock with the work or control words. Its
lock is the file `store.lock` under the project's records directory, taken
only by the backup words. Its one read transaction on the store lasts for the
copy step only, about 2 to 3 seconds of the 37 to 40 seconds that a whole
capture of a 400 MB store took in the observation recorded in
[off-host backup](off-host-backup.md#what-a-capture-does-to-the-live-store).
Writers commit meanwhile under the write-ahead log, with no hard bound on how
long one commit can take. A host therefore runs a push as its own process,
outside any budget it gives a control call.

`backup status [--json]` is an operator word that reads only the records
under the home and the store, through the admitted read-only opener, and
contacts no target. It names the durability mode only together with what
backs it: `local_backed_up` when at least one kind qualifies under the
[freshness rule](off-host-backup.md),
with each qualifying kind's off-host text, which for a directory target is
exactly "off-host asserted; not verified", and what it restores; `local`
otherwise, saying that nothing is known to be held off this host. For each
kind it then gives whether it qualifies or the first reason it does not, by
its `backup_*` code, and the recorded evidence as of its times: the target,
both of the operator's statements, the copy with its acknowledgement,
capture start and age, its cut and how far the store has moved since, the
last confirmation, a missing finding, the build that checked the copy when
it is not the running one, a pending attempt, and the last attempt with its
error. A configuration or state file this build cannot use is reported as
`backup_record_unreadable`, not as a failure. `--json` prints the same with
`schema_version` 1, the mode inside a `durability` object beside its
`off_host` list, and the reasons as their codes. The word exits 0.

`backup status --check-target [--check-deadline-secs N]` first asks each
configured target to confirm the newest copy, reading it in full under the
deadline (600 seconds by default), and then reports as above with a
`checks` list. The check reads the records without the push lock and asks
the target outside it; it then takes the lock and records a confirmation,
or a finding that the copy is missing (`backup_copy_missing`, which stops
the kind qualifying at once), only when the target, the newest receipt and
its evidence are unchanged since it read them, so a push that recorded a
newer receipt meanwhile is never overwritten. A target that cannot be
reached is reported as `backup_target_unreachable`, a check that reached the
target and passed its deadline before it had read the copy as
`backup_check_timed_out`, and one that could not read it as
`backup_target_unconfirmed`; each stands beside the recorded evidence and
changes nothing. A confirmation renews only the copy's last confirmation,
never the time the store's content was observed in it, so an old copy that
is confirmed again still reads `backup_stale`. A check stalled inside the
operating system past its deadline ends the process, which still exits 0.
The check deadline is at least 1 second: `--check-deadline-secs 0` is a
usage error, refused with exit 2 before anything is read, as every other
usage error is. This differs from `backup push`, whose deadlines accept 0
and expire at once.

#### The `backup status --json` receipt

`backup status --json` prints one object, the [receipt](cli-and-mcp.md#the-backup-status---json-receipt), with
`schema_version` 1; `engram doctor --json` carries the same object as its
`backup` field, except for a store outside an Engram home's layout, for which
that field names why it was not read (see below). Every field is always present: an absent value is `null`,
never left out, and only `checks` appears or not, with `--check-target`.
Paths name nested fields with dots and array elements with `[]`. A host may
rely on these names, types and values, and on the typed codes; not on the
order of keys, the wording of messages or reasons, or any fingerprint as an
identity. A test compares this table with the receipt the command emits:
the field paths, and every value a listed field takes.

The [backup status receipt field table](cli-and-mcp.md#the-backup-status---json-receipt) remains with the CLI index.

`engram doctor` prints the same block as `backup status`, in text after its
own lines and in JSON as a `backup` field holding the status object, beside
the store's health and apart from it: a stale, missing or unconfigured
backup never changes `healthy` or the exit code, and the block stands on a
refused store too. It reads only the recorded evidence and contacts no
target. For a store outside an Engram home's `<home>/projects/<project
digest>/engram.db` layout it reads `backup: not read (...)`, and in JSON
`{"unavailable": "backup_store_outside_home", "reason": ...}`. `engram
readiness` carries no backup field and reads the same with or without a
target.

`next` and `next --peek`, on the CLI and through MCP, add one reminder line
when a target is configured and the mode is `local`, or when a kind's last
push failed, and nothing when no target is configured or all is well:
`backup: mode local (store: REASON); see engram backup status`, or `backup:
the last store push failed: CODE; an earlier copy still qualifies (store
copy: off-host asserted; not verified); see engram backup status`, or both
parts in one line. It reads only the recorded state under the home, never
the store or a target, and reads nothing more when no kind has a
configuration file. A configuration file whose existence cannot be checked,
for example because access to it is denied, counts as present, so its
records are read and an unreadable one is reported. It stands right after
the direction to list memories,
ahead of every other reminder, and neither the reminder count limit nor byte
fitting sheds or shortens it. A recorded failure code that is not a plain
lowercase code of at most 64 bytes reads `unrecognised code`. See
[off-host backup](off-host-backup.md#what-the-operator-sees).

`backup list [--kind store] [--json] [--deadline-secs N]` prints the
manifests of the copies the configured target holds for this project, with
each copy's name, capture start, uncompressed and stored sizes, SHA-256,
cut, format identity, capturing build, source revision and host, and names
the manifests it could not use and the copies whose manifest names another
project (`foreign` in JSON). `backup fetch COPY --out FILE [--kind store]
[--json] [--deadline-secs N]` writes that copy to FILE, which must not exist
(`backup_copy_exists`). Before it creates anything it checks that the local
disk has room for the stored file and the uncompressed copy together
(`backup_local_no_space`, or `backup_local_space_unknown` when the room
cannot be measured) and that the stored file is the length its manifest
declares; it then decodes no more than the declared length and checks the
result against the manifest's SHA-256 (`backup_copy_invalid` otherwise). A
copy the target does not hold is `backup_copy_unknown`, and one whose
manifest names another project is `backup_project_mismatch`. The copy is
decoded into a hidden file beside FILE and renamed to FILE, without
replacing anything, only once it is checked, so FILE never holds a partial
or unchecked copy. On Windows the rename needs no hard links; elsewhere,
where the system cannot rename without replacing, the move links FILE and
then unlinks the hidden file, ignoring an unlink that fails. That hidden
`.FILE.PID.fetching` file is left only in three cases. A process that ends
in the middle, or a removal that fails, leaves it, and the refusal then
names it. Such an unlink leaves it too: after every successful move the
fetch checks whether the hidden file still stands, and when it does, or
when that cannot be told, a warning names it (`warnings` in JSON, a
`warning:` line otherwise) beside a fetched copy that is complete. Both words read
only the target's records under the home and the target itself, never open
or create a store, and so work in a home that has only configured the
target; without one they refuse with `backup_not_configured`. Their
requests to the target run under one deadline, thirty minutes by default,
and a request stalled past it ends the process with exit 1. Their `--json`
receipts carry `schema_version` 1; under `--json` a refusal is also printed
on standard output as a JSON object with `code` and `message`.

`backup restore COPY --origin-retired-by NAME [--json]
[--deadline-secs N]` installs one `store` copy from the configured target as
this home's store. It refuses without a non-blank statement that the origin
store will never run again (`backup_restore_origin_unstated`), while a push
holds the lock (`backup_push_running`), and when `engram.db` or any of its
`-wal`, `-shm` or `-journal` files already stands in
`<home>/projects/<project digest>/` (`backup_restore_store_exists`); it never
replaces one. Holding the push lock throughout, it fetches the copy as
`fetch` does into a `.backup-restore-<id>.staging` file beside the store, a
name that store open, `doctor` and `readiness` never take for a store. A copy
whose manifest names another project, or whose store holds rows of another
project, is refused as `backup_project_mismatch`. When this build does not
accept the copy's format identity (`backup_restore_format_unaccepted`),
restore refuses, leaves the fetched file in place, prints its path and names
both ways on: the build at the manifest's source revision, unless that is
`unavailable`, or `migration export` on that file and `migration import`
with a build that still names every conversion since. A failed full check
(`backup_restore_check_failed`) instead asks for inspection and a healthy,
accepted copy. SQLite path/access failures (`backup_restore_path_unopenable`)
name the original path, its UTF-16 length and the available error, with a
path/access remedy; they do not suggest another build or migration. A
deterministically unsupported filename or capacity refuses before fetching.
On Windows, SQLite disk opens and copy destinations use native extended paths
and the locking long-path VFS; 260 characters alone is not a refusal. Logical
paths in receipts and manifests remain unchanged. Otherwise it records a
pending restore in `<home>/backup-records/<project digest>/store.restore.json`
(its opaque `occurrence_id`, the copy, its SHA-256, the origin host and the
statement), moves the copy
into place with a rename that never replaces anything, checks the installed
store and marks the record completed. A restore stopped before the move is
finished by a retry of the same copy, and another copy is refused while it is
pending (`backup_restore_pending_other`), naming both ways on: run `engram
backup restore COPY --origin-retired-by NAME` again, or abandon it with
`engram backup restore COPY --abandon-pending --abandoned-by NAME`. The
printed commands give a name as `--option=value` and quote a value that a
shell would split, which both POSIX shells and PowerShell read as one
argument. A retry fetches the copy
again into a new staging file, checks that it holds the pending copy's
SHA-256 (`backup_restore_pending_mismatch` otherwise, and the new file is
removed), points the pending record at it, and only then removes the
earlier staging file. When the record cannot be written, the earlier
staging file and the record stay as they were and the new file is removed.
A later refusal of the new copy for its format or its check leaves it in
place with the record naming it; a refusal that removes the new copy, such
as another project's rows, leaves the record naming a staging file that is
gone, which a retry and an abandonment both accept. A process that ends
between that write and the removal leaves the
earlier staging file behind, no longer named by the record; nothing else
beside the store is ever removed. A restore stopped after the move is
completed by a retry of the same copy only when `engram.db` holds the
recorded SHA-256 with no `-journal` and no `-wal` that holds any bytes (an
empty `-wal` and an `-shm`, which a read leaves, are allowed); it then prints
`completed an interrupted restore`, and any other store is refused as
`backup_restore_store_exists`, naming what did not match. A completed record
whose store is gone is kept as `store.restore-<completion time, UTC>Z.json`
before a new restore records its own. Each new restore, even of the same copy,
gets a fresh UUID; pending retries, refetches and interrupted completion retain
it and the original pending provenance. A retry still requires
`--origin-retired-by NAME`, but does not replace the original recorded name or
statement time; the receipt shows the original statement. Status exposes
`restore.record.occurrence_id`, and doctor exposes
`backup.restore.record.occurrence_id`. Missing, invalid or nil ids make a
record unreadable; readers never mint or repair one. These fields do not fence
consumer admission. The host/operator must exclude every destination opener
under the [destination admission contract](off-host-backup.md#restore-occurrences-and-destination-admission);
Engram supplies no guard, and current TermAl project flags do not close the
independent-writer gap. Restore changes no row. It prints,
read from the checked copy with one clock reading, the number of unexpired
active claims and when the last of them expires (`as_of`, `unexpired_claims`,
`claims_expire_by`); a restored claim serves only its old
session, so an item still held refuses a new session's claim
(`work_claim_held`) until that claim expires and is then recovered the
ordinary way. `backup status` and `doctor` show the record as `restore:
pending since ...` or `restore: restored ...`, and in JSON as a `restore`
field with `state` `pending`, `restored` or `unreadable`, without contacting
the target. The `--json` receipt carries `schema_version` 1, the copy,
store, bytes, SHA-256, origin host and statement, `completed_interrupted`,
`kept_record` and `authority`; a refusal under `--json` is printed as for
`fetch`. See [off-host backup](off-host-backup.md#restore).

`backup restore COPY --abandon-pending --abandoned-by NAME [--json]`
abandons the pending restore of COPY instead, for a restore that cannot be
finished, for example because its copy is gone from the target. Local-file
pending occurrences instead use `restore --from FILE --abandon-pending
--abandoned-by NAME`; see [local restore recovery](off-host-backup.md#restore-occurrences-and-destination-admission).
Its JSON receipt has `source`, `occurrence_id`, `abandoned_by`, `abandoned_at`
(an RFC 3339 time), `staging`, `staging_removed`, `archive` and `warnings`.
It carries no `schema_version`. `source` retains the pending local-file selector
and replacement intent; `occurrence_id` names the archived attempt, even when
the active record returns to an earlier completed occurrence. `warnings` is an
array of strings; `staging_removed` is a boolean. Abandoning takes no
`--origin-retired-by`.
Holding the push lock and contacting no target, it refuses without a
non-blank name free of control characters
(`backup_restore_abandoner_unstated`), when no restore is pending
(`backup_restore_not_pending`), when the pending restore names another copy
(`backup_restore_pending_other`), and when `engram.db` or any of its `-wal`,
`-shm` or `-journal` files stands where the store goes
(`backup_restore_store_exists`); it takes no `--deadline-secs`. It
removes only the staging file the record names, and only once it has
checked that the file is a regular file named
`.backup-restore-<uuid>.staging`, spelled through no `..`, directly in
`<home>/projects/<project digest>/` as this home spells it. That directory
and `projects` are opened from the home without following a link and are
held open while the file is removed by its name in them, so neither can be
redirected in between: a link or reparse point there, at the file, or a path
that cannot be examined, is refused (`backup_restore_staging_unowned`) and
nothing is removed. On Windows the file is removed by the store directory's
full path, once that path is checked to lead to the directory held, which
also serves a home on a network share. A staging file already gone is allowed, and a removal
that fails leaves the restore pending. Then it archives the pending record
beside it as `store.restore-abandoned-<UTC time>Z.json`, or under the next
free `-N` suffix, written whole to a temporary file first and moved there
without replacing anything: an envelope with `format_version`,
`abandoned` (`by` and `at`, asserted context) and `pending`, the record
exactly as it was parsed. Only then is the active record cleared, so
`backup status` and `doctor` no longer show a pending restore and another
copy can be restored. An archive that cannot be written, or an active record
that cannot be cleared, leaves the restore pending, says whether its staging
file was already removed, and abandoning can be run again, which after a
record that could not be cleared archives it once more under the next name;
an abandoned restore is never reported as completed. Archives are kept as provenance and
no word lists them. The `--json` receipt carries `schema_version` 1,
`abandoned` (the copy), `abandoned_by`, `abandoned_at`, `staging`,
`staging_removed`, `archive` and `warnings`, which names a temporary file
the archive's move left behind and could not remove; a removal of the
staging file counts only once the held directory no longer holds it. A
pending record written while the home was spelled differently, by case or
as a relative path, names its staging file outside this spelling and is
refused; run the abandonment with the home spelled as it was then.

Actor context binds the work/MCP service and the work session, which records
it on its attribution, never on the principal.

The optional actor context can equivalently arrive through
`ENGRAM_ACTOR_CONTEXT`; it is fixed when the CLI invocation or MCP connection
binds its session. The MCP process retains one `LocalWorkService` and its
lazily opened SQLite connection for its lifetime. The fifteen work tools use
that connection; the separate `task_sync_settings` operation uses the service
without opening SQLite. A failed store operation rolls back before the next call uses the
connection.

The same variable, `ENGRAM_MCP_PHASE_TRACE=1`, at `engram control` start
traces the host-private work-session transport too. Its records differ
from the MCP records described next; they and their correlation are
described under the
[work-session phase trace](execution-observability.md#opt-in-phase-trace).

`ENGRAM_MCP_PHASE_TRACE=1` at `engram mcp` start turns on a phase trace for
diagnosing slow calls. Any other value, or none, leaves stderr phase tracing
and detailed SQLite profiling disabled. The default
[operational diagnostics](diagnostics.md) remain active. With tracing on, every tool call whose
handler returns ends in one final JSON summary on stderr, at most 4 KiB,
keyed `engram_mcp_phase_trace`. Separate stage lines can precede that summary,
including for a handler that has not returned. A handler that panics has no
final summary.

The line carries:
- the call's numeric request `id`, with `correlation` `numeric` and a local
  `call_seq` only when receive, handler and send association is unambiguous.
  A string id is never retained or echoed: `id` is null and `correlation` is
  `unavailable`. Lost association also has a null `call_seq` and a fixed
  `uncorrelated_reason`;
- the `tool` label (`unknown` for an unlisted name);
- `state`, one of:
  - `complete`: its response was sent;
  - `send_failed`: sending the response failed;
  - `incomplete`: a uniquely associated settled handler was still waiting
    for its response's dispatch when the transport closed;
  - `uncorrelated`: the handler settled, but its response cannot safely be
    associated by the wire ID. Cancellation, overlapping duplicate IDs,
    missing receive observation, string IDs, saturation and late settlement
    can produce this state; it supplies no send duration;
  - `send_dropped`: the send future was destroyed without a result, even
    when it had never been polled;
- `cancel_requested`, true when a client cancellation notification was
  attributed to this call before response dispatch, false when none was
  observed for its known generation, or null when attribution is unavailable.
  A token cancelled by rmcp alone never proves a client notification. An
  attributed notification survives later duplicate-ID or saturation loss;
- `handler_total_ms`;
- `receive_ms` when known and `handler_start_ms`, relative to this trace's
  monotonic origin;
- `count`, `total_ms` and `max_ms` for each of:
  - `store_open_total`: opening a connection, schema checks included; a read
    word, `next` with peek included, opens its own read-only connection per
    call;
  - `store_mutex_wait`;
  - `begin_immediate`, including any wait for the write lock, whether it
    ends in the lock or in the 5-second busy timeout;
  - `commit`, which counts every `COMMIT`, read transactions' included;
  - `receipt_serialize`;
  - `read_snapshot`, including nested and reused `work_read_snapshot` calls,
    but not every direct `on_one_snapshot` caller;
  - `sqlite_statement`, every statement covered by the existing profile
    callback, including writes and transaction statements. Profiling begins
    after store admission: schema-preflight work remains in `store_open_total`;
  - `backup_reminder`, project-home lookup and local backup-record reads,
    including an early return with no reminder;
- `wire_encode_send_inclusive_ms`, the encode, write and flush of the
  response together, for `complete` and `send_failed`;
- `dispatch_send_inclusive_ms`, which also includes scheduling between
  dispatch and the send future's first poll;
- `unmatched_sends`, `unassociated`, `association_disabled`, `dropped_lines`
  and the retained `evicted` counter (this implementation sacrifices
  association on saturation rather than evicting a slot for reuse).
  `unassociated` counts lossy observations, not calls: a string-ID call can
  increment it at receive, handler start and settlement.

`uncorrelated_reason` reports the latest loss reason available to that
observation. A later cancellation, token cancellation or duplicate can replace
an earlier reason; changing this label never clears lifecycle ambiguity.

The lifecycle, settled-summary, cancellation and quarantine state share at most
256 fixed-size slots. Numeric keys and static labels are retained, never raw
string IDs. Cancellation of an observed live entry and duplicate IDs quarantine
a key; an unknown or already-dispatched ID adds no cancellation tombstone. A
new request cannot clear quarantine. A cancelled entry is retired only when
its original, single-generation handler has settled and its response dispatch
is observed, with no duplicate, unowned invocation or other unsafe lifecycle.
Any held summary is emitted at that dispatch as `uncorrelated`, with its
client-cancellation fact preserved and its call sequence and send durations
unavailable. If settlement already emitted the summary, dispatch emits no
second summary. The send attempt can then finish or be dropped without
claiming a later request that reuses the ID.

This reclaims only observed dispatched cancellations. In rmcp's ordinary
response loop, a client cancellation removes the request token and its
response can be dropped before transport dispatch. Such an entry remains
quarantined; the trace cannot prove a response was handed over. Duplicate,
token-cancelled-before-start and missing-receive entries also remain
quarantined. If a tombstone cannot fit, or the sequence is exhausted,
association stays disabled for the rest of that process, even after later
dispatches. A normal send captures its immutable instance synchronously at
dispatch; a later ID reuse, close or cancellation cannot reassign completion.

An enabled process emits an `engram_mcp_trace_instance: 1` descriptor with a
random local `trace_instance`, PID and monotonic origin offset. Separate
`engram_mcp_call_stage: 1` lines carry that instance, the sanitized ID and tool,
safe `call_seq` or an explicit unavailable reason, and `server_elapsed_ms` for
`receive`, `handler_start`, `handler_settle`, `send_dispatch` and `send_finish`.
`send_finish` includes `send_succeeded`; cancellation and dropped sends may add
`cancel_requested` and `send_dropped`. These are observed stages, not a claim
that every call reaches every stage. Trace tokens are local correlation, not
authenticated identity.

`receive` observes a decoded message when the transport future is polled,
not raw bytes reaching the OS pipe. `send_finish` observes the send future's
result, not client receipt. Stages can narrow handler scheduling, handler work
and response dispatch/send delay. Delay before decoded receipt may still be
client delivery, input/decode or server runtime starvation. Client write
completion alone does not distinguish them. Correlate client and server
records without subtracting unrelated monotonic clocks; otherwise leave the
cause unknown. A synchronous stall interrupted before its guard/profile
finishes may have only a `handler_start` record, not a phase duration.

Records are written by their own thread through a bounded queue, never on a
request or send path. A host that stops reading stderr therefore costs
dropped lines, counted in `dropped_lines`, and never a stalled server. Read
stderr while the trace is on. Close waits up to a second for queued lines;
lines queued at or after close are best effort, since the process may exit
before they are written.
The counter records queue submission failures, not sink write/flush failures;
zero drops does not establish complete capture. Missing lines remain unknown.
The new synchronous hooks add only the existing TLS-active check when tracing
is disabled: no timestamp, allocation, profile registration or writer is added.

The same opt-in trace also writes a separate `engram_mcp_shutdown_trace: 1`
record at each observed shutdown boundary: `input_ended` (the transport
returned no next message), `transport_close_started`,
`transport_close_finished`, `trace_flush_started`, `trace_flush_finished`,
`service_waiting_returned`, `store_owner_drop_started`, and
`store_owner_drop_finished`. These contain only the fixed `stage`,
`server_elapsed_ms` from trace creation, and `dropped_lines`. A reached stage
does not assert success.

`service_waiting_returned` is recorded after the service wait returns and
before the retained work-service owner is released. `store_owner_drop_started`
and `store_owner_drop_finished` bracket that synchronous release and any
resulting destruction of its cached SQLite connection. They do not establish
completion of all process cleanup or a durability guarantee.

The records use the existing bounded queue and add no flush wait. The final
four stages are best effort after the existing close-flush attempt;
`trace_flush_finished` records that the bounded attempt returned, not that
every line was written.
The dogfood close diagnostic retains server times and client receipt times
relative to stdin EOF separately. Missing records and uninstrumented cleanup
remain unknown. With tracing off, stderr tracing and the retained-owner
tracing wrapper are absent;
the default [bounded operational diagnostics](diagnostics.md) observer retains
observed failures and overruns without changing rmcp close behavior, then adds
a bounded diagnostic exit flush of at most one second.

Durations are elapsed wall time and overlap where one phase contains another.
They are not summed, and a fast server record does not prove a fast client.
`begin_immediate` and `commit` come from SQLite's statement profile, which
has millisecond granularity, so a short statement reads as 0. The line never
carries SQL, a path, parameters or a request or response body.

`scripts/mcp-dogfood.test.mjs` runs its servers with the trace on and writes
every record to its log. It prints a call's record beside its soft-threshold
timing line when the call is slow, or says why there is none. A slow call
whose record has not arrived yet prints `unavailable (record not yet
received)` at once, and its record follows when it arrives; one that never
comes reads `unavailable (no record before close)` when the client closes. A
record in a state that did not time the whole call (`incomplete`, `uncorrelated`
or `send_dropped`) reads `unavailable (record STATE)` followed by its fields, so
it never passes for a fast call; only `complete` and `send_failed` records
are printed as the call's timing.

When `--project-file` is omitted, CLI and MCP search from the working directory
through its ancestors to the filesystem root for the nearest `.engram-project`,
without stopping at `.git`. Its stable project
identity resolves to the same opaque SQLite path for every worktree and
session on the host. Relative project-file paths resolve from the caller's
current directory and select exactly that file, even for an explicit
`.engram-project`; failures never fall back. An encountered invalid, unreadable
or broken-link marker refuses without searching farther. A nested worktree's
own marker wins; without it search can reach an outer repository's marker.
Markers must resolve to regular files of at most 4,096 bytes, including
whitespace; a link to a regular file is allowed. A directory, special-file
target or oversized marker receives an `unreadable` refusal naming the path
and reason. File type and length are checked before opening and on the opened
handle; reads consume at most 4,097 bytes. Unix opens are nonblocking so a FIFO
substituted after inspection cannot block the open. These checks bound marker
content, not the duration of stalled filesystem I/O, and do not provide a
snapshot of concurrently edited regular files.
The selected marker's parent is the project root, including for the host-path
probe and doctor's default repository. Search exhaustion names its starting
directory and the search through root; change into the intended tree or pass
its marker explicitly, rather than create a new marker in a subdirectory.
If that file is missing, unreadable, invalid UTF-8, or empty, every CLI work
word refuses before store opening or session setup, with `details.kind`
naming which: `missing` (no such file), `unreadable` (any other read failure,
such as a directory in its place), `undecodable` (content that is not UTF-8)
or `empty` (blank content). For those work words,
text and `--json` emit
`project_resolution_failed` on stderr with exit status 1 and no stdout.
The operator command `readiness --json` instead emits structured refusals on
stdout and exits 1; see the [readiness receipt](host-readiness.md#refusals).
For the work words, error details name the reason, attempted `project_file`,
`searched_directory`, `cwd` (null if unavailable), selection rule and remedy.
The `next` command uses `--project-file 'PROJECT_DIRECTORY/.engram-project'`
with `work next`; replace the placeholder with the intended absolute project
directory, or change to that directory before retrying. No project is created
implicitly. Paths and OS error text are safely framed in terminal output:
text mode prints each string detail as written, so a Windows path keeps its
single backslashes, and escapes every ASCII control and non-ASCII character
(bidi controls included) as `\uXXXX`. Text is for reading: a literal
backslash sequence in a path looks like an escape there, so `--json`, which
keeps `details.project_file` as the raw path, is the unambiguous form.

`doctor` verifies every canonical object plus record-bound projection,
reports the active immutable policy id, epoch and selected obligation-rule-set
id, and warns on stderr that V1's development no-op redactor provides no
secret or PII protection. Engram provides no organizational-authority
mediation, and doctor prints no warning about it. Doctor audits recorded
state; it does not verify what a host observed or what an agent did outside
Engram.

`engram doctor --json` performs the same checks and keeps that warning on
stderr while emitting a machine-readable report on stdout. Its `project_id`
and canonical absolute `database` path give a host the stable pair used to key
project-local authority queries. Its `control.acceptance_evaluation` names the
evaluator modes the store admits, the mechanical basis, and whether completion
needs a fresh source fingerprint; the text report prints the same policy on one
`Acceptance evaluation:` line. The database path is absolute with symlinks
resolved. On Windows it never exposes the verbatim-path prefix (`\\?\`), and
UNC paths use their ordinary `\\server\share` form. When integrity is
unhealthy and the active control envelope cannot be decoded, the report still
prints with `healthy: false`, `control: null`, and a `control_error`; stderr
then also states that control diagnostics are unavailable and that the
report provides no enforcement assurance, beside the redactor warning, before
the command exits nonzero.

If normal open itself refuses because the active control-policy chain is
corrupt, `engram doctor --recover-policy` uses a separate existing-file,
read-only/query-only path. It verifies the active selector and every projected
policy version together with the canonical authority and selected rule-set
objects, emits a typed finding for each invalid binding, and exits nonzero
while the store remains unusable. `--json` reports
`mode: "control_policy_recovery"` and `mutation_enabled: false`. This mode
cannot start MCP/control/work, issue grants, initialize schema, or repair/select a
policy; its guidance is limited to restoring verified bytes or explicit
operator inspection. Because the connection is read-only, it also cannot run
SQLite crash recovery for an uncheckpointed WAL. If SQLite itself requires
recovery, stop writers and diagnose a byte-consistent verified copy of the
database together with its sidecars; the command fails without changing them.

Missing or malformed declared indexes, triggers, FTS tables, or derived tables
also make ordinary open fail without DDL. `engram doctor --repair-projections` is the
separate mutating operator path: it first fingerprints the complete exact-current
durable definitions and validates control-policy bindings, recreates every
declared rebuildable object, reconstructs project-memory state and work projection
tables, recreates disposable project-memory advertisements empty, repopulates FTS
from verified durable rows in one transaction, and then runs full integrity
verification. It never recreates
missing durable state or rewrites canonical objects.

For the one-time addition of an absent public-note index,
`doctor --build-note-search-index` instead builds and verifies only that index,
preserving every other projection and advertisement. It refuses existing or
damaged target objects and any other schema mismatch. It does not replace the
full repair or full integrity audit.

On a fresh store, `engram init` installs the built-in policy at epoch one
with synthetic system attribution. It remains an idempotent create-or-verify
operation and preserves any existing active policy.

Policy administration requires no justification text. No `control-policy`
setter accepts `--reason`; policy authority records carry the attributed
operator and selected policy, not a reason field. This does not change the
reasons required for separate waiver operations.

`engram control-policy show` prints the active policy as JSON: `policy` (the
record id a compare-and-swap names), `epoch`, `obligation_rules` and
`acceptance_evaluation`. A policy version written before turn gating was
removed stays readable as stored. It reads
the policy head and changes nothing, so a host asks it whenever it needs the
admitted evaluator modes; the `doctor` report carries the same keys but runs
the whole-store audit first.

The operator-only `set-obligation-rule-set` command records asserted
operator attribution, creates immutable authority and policy objects, and
atomically advances the active policy id and epoch, with an optional
compare-and-swap policy id. Its required idempotency key binds the complete
normalized intent and persists the exact receipt in that same transaction. A
retry after restart or an uncertain response returns the original receipt
even though its expected policy id is now stale; reusing the key for another
intent is a typed conflict. It is not an MCP tool or work-session
operation. Its `--input <JSON|@file>` is limited to 64 KiB
of raw input, including any UTF-8 BOM. For files, one leading BOM is removed
after the limit check. The input must be UTF-8. Unknown fields are rejected
at every nested V1 object. The input passes through the same typed validator
that storage uses. `check_fingerprint` compares a canonical check description
and does not accept a shell command in its place. A requirement cannot name an
environment: `required_environment` is refused by name, like any other unknown
field. Re-supplying the active set under a fresh key
records an exactly replayable `changed=false` receipt. Rollback likewise
re-supplies the desired prior JSON; a rule-set id alone is never accepted as
activation authority. The operator identity is asserted host context, not
authenticated administration.

`engram work` exposes the fourteen agent words; `--json` after any word prints
the structured receipt (the existing shape plus `reminders` and `next`)
instead of text. A successful mutation with a process-defaulted session also
adds top-level `effective_session_id`. `done` exits with status 2 when the typed
`open_work_obligations` refusal says something is still owed. The stock
source-change rule never causes that refusal. A source change that no matching
passing test followed is recorded at `done` as a waiver in the completing
actor's name. The item still completes, and `done` and `show` each print
`untested source change: ID (source revision REV; HOW); no matching passing
test followed it`, then `untested source changes: N more not shown (T in
total)` when the bounded obligation page names only some of them. `HOW` is
how the host said it established the change (`host compared content
revisions`, `host assumed the change, no earlier revision`, `host had file
notifications only`) or `detection not reported`: the host's word, never
verification. With `--json`, the named changes are in `untested_changes`,
each with `reported_source_change` when the host said, and the count left
out is in `untested_changes_omitted`; every obligation of the stock
source-change rule on the obligation page carries the same
`reported_source_change`, when the host said. Under a named root, a change
another workspace recorded before the binding is displaced instead, and
`done` and `show` print `foreign workspace change: ID (workspace WS; source
revision REV); captured before the named root, displaced and not verified`,
then `foreign workspace changes: N more not shown (T in total)`; with
`--json` they are in `foreign_workspace_changes` and
`foreign_workspace_changes_omitted`. When the completed claim had a named
root bound at its completion, `done` adds the reminder `REF's named root WS
has served it: remove that worktree once nothing else needs it (Engram
removes nothing)`, and `--json` and MCP carry `named_root` with `workspace`
and `shared_with_open_claim`. While another live claim of the same session
names the same workspace, `shared_with_open_claim` is true and the reminder
is left out; a lapsed claim or another session's claim never holds it back.
Engram removes and inspects no file. If the root cannot be read, a reminder
says so and the completion stands. A peer's `next` delta for an
execution observation begins with the change, in one word each: `changed:
WORD` when the host said how it found the change, `changed` when it did
not, `repeat; host: WORD` when the host said how it found a change the core
read as a repeated revision, `unchanged` otherwise; the effect and outcome
follow. `WORD` is `compared` for `content_comparison`, `assumed` for
`assumed_missing_baseline`, `watcher` for `watcher_only`. The delta for an
untested change begins `host said VALUE; ` when the host said, before the
observation id and the revision. In the compact `next` text these two kinds
print their text before the peer attribution, and the line is bounded at
96 bytes, so the change and the host's word survive while a long
attribution, id or revision is cut; the delta's summary, in the same words
and bounded at 192 bytes, is in the verbose `next --json` form. The
protocol's value itself is on the obligation page and in the `--json` forms
of `show` and `done`.
Before completion, the receipt reminder for an open obligation of the stock
rule says what `done` would do with it at that read, as completion classifies
it:
- a change in the named root, or one recorded while no root was bound that a
  later root does not displace (see the next case), is recorded as untested
  without a test;
- a change made in another workspace before the root was named is recorded as
  displaced, and needs no action;
- a change with no workspace, recorded while a root was bound, needs the
  credited check in the active named root or an authorized waiver; once no
  root is bound, it first needs a named root for that check;
- a change made outside the named root while it was bound needs an authorized
  human waiver, since no check in a named root can resolve it, even after
  that root ends.

In each refusing case the reminder says that `done` refuses until then. The
words are read guidance: `done` classifies again when it runs. Under a
self-asserted policy only the obligations the bounded page shows are
classified. A page without a classification says that `done` will say whether
it records the change as untested or needs a credited check or waiver.
Criteria bound with `--bind` still refuse, and
so does every operator-selected rule except the exact stock definition (the
stock id at version 1 with an unpinned test). The
six-operation JSON protocol stays reachable for hosts and operators as
`engram work core {next,focus,propose,update,complete,handoff}`, whose
mutation payloads accept an inline JSON object or `@path`. The `@path` inputs
of `propose`, `update`, `complete` and `handoff` accept one leading UTF-8 BOM.
Each of these four operations has the same raw input limit: 2 MiB,
including whitespace, any file BOM, and the outer envelope, checked before
decoding. Inline JSON is counted the same way. This is a transport ceiling,
not a promise that every semantically valid payload fits; oversized JSON is
refused even when a typed field would otherwise be admissible. A second
leading BOM or otherwise invalid JSON is still refused. That host/operator surface retains
the core-only explicit delivery acknowledgement, typed evidence attach, and
reopen operations alongside typed forms of the ordinary lifecycle
words. Typed `gate` and atomic `note` are word-only `work_update:gate` and
`work_update:note` suboperations, not variants callers can reach directly
through `work core update`; they still use the same service and storage core as
MCP. Stats/import/export and the remaining broad administrative CLI in the
specification are still planned.

The operator-intended shell command can waive one exact open run obligation
with an attributed reason and retry key:

```bash
engram authority waive-obligation \
  --obligation-id <uuid> \
  --expected-definition <record-id> \
  --waived-by host-operator \
  --reason "accepted without the required test" \
  --idempotency-key <retry-key>
```

There is no equivalent host-private JSON-lines operation: `obligation_waive`
has been removed. Revising or dropping an acceptance binding still clears its
open obligation through the ordinary audited work update. Existing resolution
history remains readable.

MCP and `work_update` cannot request a work-obligation waiver, and agent-facing
projections omit its reason. That surface separation is not authentication:
the shell command has no credential or run-binding check, so any local process
with the binary and store access can invoke it. Removing the host-private
operation does not make the retained shell command authenticated.
