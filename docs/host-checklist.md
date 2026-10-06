# Host Checklist

> Normative reference: [spec §8](spec.md#8-interfaces).
> Related briefs: [CLI & MCP](features/cli-and-mcp.md),
> [behavioral control plane](features/behavioral-control-plane.md),
> [security & trust](features/security-and-trust.md), and
> [external adapters](features/tracker-adapter.md).
>
> Engram capabilities below are implemented in this source tree unless marked
> as planned; see [shipped today](shipped.md). Verify the deployed executable's
> build and supported commands separately: it may lag this source revision.
> Host setup requirements are not a claim that a
> particular launcher implements them. MADE is the external planner and
> coordinator host used for the integration pilot. Its recipe still needs
> runtime acceptance in that host.

A host is any runtime that starts agent sessions and wants Engram to own
their work: TermAl today, an external planner and coordinator tomorrow. The
base tier below is the whole integration for an advisory pilot. Nothing in
it requires the host to mediate turns or actions.

## Before upgrading: cut over each store

Installing a build does not make an existing store usable by that build.
Running consumers keep their loaded executable until they restart. Inventory
every live project store and every consumer of each store before upgrading;
a successful check on one project says nothing about another.

First classify the changeset, then run the new build's
[`readiness --json`](features/host-readiness.md) for each store, using its
explicit absolute home and project file. Readiness checks schema and policy
admission; it cannot decide whether a changed derivation needs rebuilding.

| Change | Required action for each existing store |
| --- | --- |
| No durable-row or projection change, and readiness reports ready | Replace the executable, retaining the old binary as a backup. Existing consumers continue on their loaded build until their restart. |
| Only the derivation or schema of a declared rebuildable projection changes | Treat the store as requiring explicit projection repair, even if readiness reports ready. Quiesce its consumers, repair with the new build, and restart every consumer of that store on the new build. |
| Existing durable rows change shape or meaning, or the new build retires a schema object | Follow [full-store migration](features/full-store-migration.md) for every live store, including the named import conversion, whatever readiness reports. A `different_build_schema` refusal requires that migration too. |

A `projection_repair_required` result takes the repair path even when no
projection change was expected. Any other readiness refusal stops the upgrade
for that store: follow [readiness refusals](features/host-readiness.md#refusals),
without silently initializing, repairing or retrying until it opens.

The host operator owns the cutover: identify and quiesce all consumers of the
selected store, including long-lived MCP children, host control processes,
agents and CLI activity; then restart every consumer against the selected
store with the new executable after verification. Installation does not
restart those processes. For this repository, consult
[Authority and Git](../AGENTS.md#authority-and-git) for installation and
restart authority. When a TermAl restart is needed for running sessions to use
a landed fix, the Engram and TermAl coordinators agree on a moment when running
work can resume. The TermAl coordinator sends Greg 'restart now' through
Engram::Advisor, with the build hash and reason. Agents keep working while that
restart is pending: do not hold a landing window, stop consumers or pause
proactively. The interruption happens when Greg performs the restart; recover
mailboxes, gates and reviews afterward, and recover or rerun work the restart
interrupted.

Until the actual cutover, stores awaiting repair or migration keep serving
their existing older consumers. Do not start new-build consumers on those stores or
restart an old consumer onto the new binary prematurely. Other stores may
be cut over separately; if the host reset affects several stores, coordinate
all of their consumers in that reset.

Once the installed executable is replaced, every process launched from that
path uses the new build: CLI words, startup `next --peek` hooks and new MCP
children alike. For every store not yet cut over, whether awaiting repair
or migration, route those interim launches
explicitly to the retained old executable. If the host cannot do that, refuse
the new-build launches for that store and disclose the access gap; do not
assume ordinary open will refuse a same-schema change to projection
derivation or durable-row shape or meaning. Existing old
consumers keep working. A `projection_repair_required` refusal during this
interval is not permission to repair early: keep repair inside the authorized,
quiesced cutover. Migration likewise stays in its coordinated window.
Readiness probes with the new build remain read-only.

For a projection repair at the coordinated cutover:

1. Quiesce that store's consumers and prevent automatic reconnection. With
   the old build, record the readiness and `doctor --json` receipts, and
   selected work-item and project-memory ids, bodies, attribution and feed
   positions for semantic read-back. Retain the old executable and a verified,
   consistent pre-repair backup made with the build that still opens the
   store; the new build may refuse it before repair. Keep a private copy of
   the new executable fixed throughout repair.
2. Run that executable explicitly for **each** selected store:

   ```text
   NEW_ENGRAM --home ABSOLUTE_HOME --project-file PROJECT_FILE doctor --repair-projections
   NEW_ENGRAM --home ABSOLUTE_HOME --project-file PROJECT_FILE readiness --json
   ```

   Substitute the executable and project paths for that store. Ordinary open
   never repairs automatically; repair rebuilds only declared rebuildable
   objects and verifies integrity before commit. A durable-schema refusal
   calls for migration, not initialization or a repair retry.
3. Check the repair result and read back the retained pre-cutover baselines
   before resuming ordinary writes, accounting for the expected new build,
   schema and documented projection or delivery resets. If semantic read-back
   differs or is incomplete,
   preserve the repaired store as evidence before considering rollback.
4. Reconnect every consumer of that store on the new build, verify its build
   and store identity, and resume ordinary writes only after cutover
   acceptance.

Repair that changes projection DDL is one-way **in place**: older builds
refuse the repaired store. A same-schema derivation repair still needs the
explicit rebuild and consumer cutover, although an older build can open it.
If an older build reports `projection_repair_required` after cutover, do not
run repair with that build: it could revert the projection in place.
Restoring the pre-repair backup with the old build is the rollback, not an
in-place downgrade. Restore requires fresh quiescence and replaces the
captured store state; it does not merge later writes. Keep ordinary writes
stopped through read-back and acceptance. Once writes resume, reconcile any
later discrepancy instead of restoring over new work.

## Base tier — advisory

Use [`engram readiness --json`](features/host-readiness.md) for fast, scoped
Verify/Save checks on an existing store. Keep `doctor --json` behind a separate
Full Audit action, with its own result. A readiness pass neither certifies the
work history nor replaces the host's identity, assurance and configuration-race
checks. Older binaries without readiness must report unsupported, not fall back.
Without an override, readiness probes the project root by looking the project
file up under the opposite case, which writes nothing; when the host already
knows its filesystem identity, supply `--host-path-policy` or
`ENGRAM_HOST_PATH_POLICY` to skip that probe on each Verify/Save. Supply it
too when the probe cannot test the project file: its name has no ASCII
letter, or it is named by an alias wider than ASCII case, such as a short 8.3
name.
Readiness does not emit Full Audit's development no-op-redactor warning or
control-limitation warnings (including unsupported action gating). Those remain
in `doctor --json`; their absence from readiness is not a protection or
enforcement assurance. Hosts must still compare `control.required_assurance`
with the mediation they actually implement.

For stale checkpoint handles, use [control session inspection](features/control-session-inspection.md)
only under the host's reset/quiescence and exact-store fences. A missing-binding
error or stopped session is not proof that no grant exists; the read-only
receipt supplies presence facts, never permission to clear recovery state.

1. **One store per project on each host.** Ship a tracked `.engram-project`
   with the stable project id; every session and worktree of that project
   resolves the same SQLite store under an absolute `ENGRAM_HOME` (a
   relative value resolves against each process's working directory and
   silently splits the project across stores). Initialize it once per
   project on each host, before any other word touches the store, with
   `engram init --required-assurance advisory --authorized-by <operator>`:
   a plain `engram init` selects `turn_gated` as the project's required
   assurance, which an advisory pilot cannot honestly claim, and
   `engram doctor` would print that requirement. The flagged
   `init` is idempotent when the stored assurance already matches (a re-run
   records no new attribution) and is refused when it differs; change an
   existing store with `engram control-policy set-required-assurance`.
2. **Inject identity into every agent process and shell.** Set
   `ENGRAM_HOME`, `ENGRAM_ACTOR_ID`, `ENGRAM_SESSION_ID`, and optionally
   `ENGRAM_ACTOR_CONTEXT` (free text such as `model=…;reasoning=…`,
   bounded and normalized by Engram, never refused). Use the developer name
   alone for the human seat (`alice`) and `<developer>/<agent kind>` for
   agent seats (`alice/claude`, `alice/codex`), with the context carrying
   `agent=<kind>;model=<exact model id>;reasoning=<level>`; both injection
   points of one session carry identical values. Identity is asserted
   context, not authentication.
3. **One MCP child per session.** Start
   `engram mcp --actor-id … --session-id … [--actor-context …]` on stdio; it
   exposes the fourteen words plus `search`. For a child that must not
   write, add `--read-only`: the server then lists only the read words and
   refuses every other call, and every writing form of a read word, as a
   tool error with a stable code (see
   [read-only mode](features/cli-and-mcp.md#read-only-mode)). Stateful calls reuse a cached
   store connection; each read that records nothing (`next --peek`, `ls`,
   `search`, `show`, and every `memories` form) opens a separate transient
   read-only connection, needs no write access to the database or WAL file,
   and refuses `store_not_initialized` instead of creating a store. Only the
   first page of an unfiltered `memories` listing that carries
   `context_generation` then records the listing through the cached
   connection. A failed operation rolls back before the next call.
4. **Show the agent what is ready.** Run `engram work next --peek` at session
   start and after every context compaction, and inject its text at the next
   dispatched prompt, not an immediate runtime-authored continuation; agents
   explicitly read `next --peek` before resuming action and follow
   clipped-status locators. Receipts end with `reminders` and `next` commands;
   agents follow them.
5. **State the assurance honestly.** Without turn mediation the deployment is
   `advisory`: the agent can bypass Engram. `engram doctor` prints the
   required assurance and supported effects in its `--json` report and the
   unavailable capabilities as human warnings on stderr; do not describe the
   integration as gated.
6. **Kill switch.** Stop injecting; nothing else to undo. Stores, evidence,
   and memories remain readable with the shell words.

### Recover saved guidance after compaction

Keep a short recovery instruction in the host's startup/resume instructions,
outside stored memory and outside any block of untrusted work data. Use this
sequence before substantive work at session start or after compaction:

1. Read `engram work next --peek` (MCP `next` with `peek: true`).
2. Run the `memories` command the peek names: `engram work memories` without
   a query, with `--context-generation` when the peek prints it (MCP
   `memories` with `context_generation`). Follow returned continuation
   commands to discover keys without needing to remember one.
3. Read relevant entries with `engram work memories KEY --full` (MCP
   `memories` with `query: KEY` and `full: true`). Omit `revision` for the
   current version. Keep the returned attribution.

These entries are saved notes and observations, not a normative rule channel.
They do not gain authority through retrieval and cannot override the current
user request or higher-priority instructions. Do not classify or promote them
from their wording. A count, a first-line preview, or a `changed` flag is not
the full guidance. Recover on resume even when `changed` is false: it tracks
the recorded advertisement, not what survived compaction. A failed read is a
visible recovery gap, not an empty collection. Do not create or repair a store
automatically to hide it.

The peek itself says when this read is due, if the host tells it. Pass
`--context-generation` with a value that is distinct for every context the
host starts or compacts, including across the host's own restarts. While no
`memories` listing of the session carries that value, the peek's text opens
with the direction to list memories before acting and the command to run,
and both survive fitting; see the
[peek contract](features/cli-and-mcp.md#using-engram-as-an-agent). A value
that repeats matches the earlier record, and the direction is withheld for a
context that is in fact new. The value is a plain token (1 to 256 ASCII letters, digits, dots, underscores or dashes, not starting with a dash); any
other value is refused. Supply the session id as well: with a session id the
process defaults for itself, each call is another session and the direction
cannot settle. Keep the opening of the peek text when truncating it. Only the
printed command records the listing, and that is a store write; every other
`memories` form, and the peek, record nothing, so a host that confines a
read-only agent to reads that record nothing can allow them and either allow
that one form too or leave the direction standing for that agent. `engram mcp
--read-only` enforces that confinement itself and refuses the printed form, so
for such a child the direction stays standing: it lists `memories` without
the generation. The
direction reports the host's assertion and what is recorded; the record it
waits for shows that a listing was delivered, not that the agent read or
applied the notes, so it replaces neither the startup instruction above nor
the natural observation below.

Verify the actual runtime boundary. A hook that marks context for the next
dispatched prompt does not cover a continuation inside an already running
turn. The host must provide a supported instruction-delivery path at that
boundary or disclose the limitation. Do not claim delivery merely because
the instruction exists in a file or appears in a conversation summary.

Keep two kinds of evidence separate: source/process tests of the delivery and
read paths, and a natural first action after real compaction. For the latter,
record where the recovery instruction came from and the commands and current
version actually read before work resumed. A peer reminder or a test that
starts with the target key does not demonstrate independent discovery. This
check requires neither a new agent word nor turn-gated control.

## Off-host backup

Engram copies a project's store to an off-host target only when something
runs `engram backup push`; it schedules nothing itself. The host owns that
trigger. See [off-host backup](features/off-host-backup.md) for the design
and [the status receipt](features/cli-and-mcp.md#the-backup-status---json-receipt)
for the fields named here.

1. **Push at three moments, per project.** Run `engram backup push --json`
   for each project when the host starts, about every hour while the project
   has an open session, and when its last session ends. An open session is a
   scheduling reason, not evidence that the store changed. Even when no work
   happens, push at least once per half of `window_hours` (12 hours with the
   default 24-hour window); an earlier assurance due takes priority.
   Successful pushes also confirm the target, so
   they need no separate `--check-target` cadence. A failed, busy or
   terminated push refreshes nothing. Run it as its own
   CLI process, never inside a long-lived server: a request to the target
   that stalls past its deadline ends that process, which is how it is
   stopped.
   Each push still copies the whole store and hashes the settled file.
   `kinds[].capture_check` is `same_bytes_as_newest` only when its SHA-256
   and length equal the newest copy at this target and this build checked
   that copy. It skips the full check and compression, then reads the
   stored gzip in full to confirm it. Any changed row, including claims,
   control rows and delivery state, can force `full`; another or unknown
   checking build also forces the full check. A missing target copy needs a
   fully checked replacement. This helps idle stores; an hourly push during
   ordinary agent activity still does a full capture. See the
   [comparison and measured cost](features/off-host-backup.md#comparing-the-settled-copy)
   before choosing the cadence for a large store. Before automatic pushes
   are enabled, observe full-path cost and control latency on a representative
   store; an idle-push measurement does not prove changing-store cost.
2. **A push may be put off.** When pushing now would disturb something that
   matters more, such as the timing-sensitive stages of a running full gate,
   push later. Nothing is lost: the next push captures the store as it then
   is, and the kind's freshness window, not the push schedule, decides
   whether the copy still qualifies.
3. **Read the exit code; do not retry in a loop.**

   | Exit | Outcome in the JSON report | What it means |
   | --- | --- | --- |
   | 0 | `not_configured` | No target is configured for the kind; nothing was done. |
   | 0 | `busy` | Another push holds the kind's lock; nothing was done. |
   | 0 | `uploaded` | A copy was put, read back and its receipt recorded. |
   | 0 | `unchanged` | The store equals the newest copy, which the target confirmed. |
   | 1 | `failed` | The push failed; its typed `code` and `message` say why. The earlier receipt stands, and `backup status` says whether its copy still qualifies. |
   | 1 | no report on standard output | The command was refused before it pushed anything, for example without a home or a resolvable project; the reason is on standard error. |

   Any other exit code is a usage error. A failed or interrupted push needs
   no retry loop: the next scheduled push first resolves any attempt an
   earlier one left pending, then captures again. Keep and show a failed
   push's own report, its `code`, `message` and `warnings`: `backup status`
   shows a failure only when the push could record it as the kind's last
   attempt. A push refused before it takes the kind's lock records nothing,
   and neither does one whose records this build cannot use. A push whose
   state cannot be written leaves the earlier last attempt in place and
   reports it in its `code`, `message` or `warnings`: when the push
   otherwise succeeded (`uploaded` or `unchanged`) and only the final state
   write failed, the push is `failed` with the write error as its `code`
   and `message`, and the error adds nothing to its `warnings`, which keep
   only what earlier steps reported; when the push had already failed, the
   unwritten state is added to its `warnings`.
4. **Bound each push, and cancel it by terminating its process.** Push runs
   under two deadlines: `--capture-deadline-secs` (900 by default) for the
   local copy, its check and the compressed file, and
   `--transport-deadline-secs` (1800 by default) for every request to the
   target together. Past the capture deadline the capture stops, its stage
   is removed, and the push exits 1 with `backup_capture_deadline`. Past the
   transport deadline the push exits 1, with `backup_transport_deadline`,
   or with `backup_pending_unresolved` or `backup_target_unconfirmed` when
   the request noticed its own deadline first; a request still running ends
   with the process, and an attempt it recorded before its put stays
   pending. Act on the exit code, not on which of these codes came back. A
   retention removal past the deadline is only a warning, with exit 0. A
   push started meanwhile reports `busy`. Terminate a push still running
   after 2760 seconds with the
   default deadlines (900 + 1800 + 60 for the steps no deadline bounds), or
   their sum plus 60 with others, and cancel one at any time the same way:
   end its process (`taskkill /F`, `SIGTERM` or `SIGKILL`). Push starts no
   child process and installs no signal handler. A terminated push prints no
   report and records nothing more, so record the termination on the host's
   side, and do not retry before the next scheduled push. A push the host
   terminated is a failure whose outcome is unknown, whatever exit code the
   system then reports: `taskkill /F` gives 1 with no report, Ctrl-C on
   Windows 0xC000013A, a signal its own status. The host's record of the
   termination takes precedence over the exit code table above. The next
   scheduled push removes a stage the terminated one left and resolves its
   pending attempt: a copy
   that reached the target is recorded as recovered, one that did not is
   dropped with its own files. Never run a push inside a control call: it
   shares no lock with them, but a capture of a 400 MB store takes about
   40 seconds. What a push terminated at each step leaves, and how the next
   one resolves it, is in
   [deadlines and cancelling a push](features/cli-and-mcp.md#deadlines-and-cancelling-a-push).
5. **Show the status from `engram backup status --json`.** Read the receipt
   rather than parsing the text. Always show `durability.mode` together with
   `durability.off_host`, each entry's `kind`, `off_host` and `restores`;
   never show the mode alone, because `local_backed_up` means only what the
   off-host text says (for a directory target, "off-host asserted; not
   verified"). Show `kinds[].target.last_attempt`, the last attempt recorded,
   when its `outcome` is `failed`, with its `code` and `message`, even while
   the mode reads `local_backed_up`: an earlier copy can still qualify after
   a failed push.
   Show `restore` when it is not null. Plain `backup status` contacts no
   target; `--check-target` asks the target and is a separate, slower call.
6. **After a restore, give sessions identities the restored store has not
   seen.** A restored store keeps the origin's claims, grants, begun turns
   and session rows unchanged, and the only boundary on them is the asserted
   session id. A session that reuses an old id can use and renew its claim,
   reconnect its control session and checkpoint its begun turn. A new
   session cannot claim an item still held in the copy until that claim's
   recorded expiry, and then recovers it with
   `engram work claim <ref> --recover "<reason>"`. A restored begun turn has
   no clock end: it stays open until a caller acting as its session
   checkpoints it. Engram resets none of this; see
   [restore](features/off-host-backup.md#restore).

## Claude Code as the host (no TermAl)

MADE can use this advisory recipe when it launches `claude` directly. No
TermAl process or mailbox is required. This is a host contract, not an
implemented MADE launcher. The configuration examples were checked against
Claude Code's official [MCP reference](https://code.claude.com/docs/en/mcp)
and [SessionStart reference](https://code.claude.com/docs/en/hooks#sessionstart).
No real MADE session has been tested against this recipe.

1. **Set the identity in the environment of the `claude` process** before
   launching it: an absolute `ENGRAM_HOME`, `ENGRAM_ACTOR_ID`, one opaque
   `ENGRAM_SESSION_ID` per logical session (never the reserved
   `local-process-` prefix), and optionally `ENGRAM_ACTOR_CONTEXT`. The
   coordinator persists that session id and reuses it when it resumes the
   same conversation in a new process, because claims, focus, delivery
   cursors, and same-holder retake are keyed by it; it mints a fresh id only
   for a genuinely new or concurrent session. Use this coordinator-owned
   value at every injection point; do not derive a second Engram identity in
   a hook. Stop the old process before resuming the same logical session.
   Concurrent conversations must not share a session id. Bash tool calls
   inherit the launch environment, so shell words and MCP words agree.
2. **Do not give `ENGRAM_HOME` a default.** Keep `${ENGRAM_HOME}` below;
   never replace it with `${ENGRAM_HOME:-some-path}`. A valid fallback path
   can select a different store without telling the caller it is the wrong
   one. This default-path risk is inferred, not an observed test result.
   Fix the launch configuration instead of hiding a missing value. Do not
   give required actor or session ids implicit defaults either.

   Before launching `claude`, the coordinator must check that `ENGRAM_HOME`
   is the intended absolute store home, actor and session ids are nonblank
   and session ids are at most 64 UTF-8 bytes,
   and none of these values is an unexpanded placeholder. Set the working
   directory to the project checkout containing `.engram-project`. The MCP
   child, hooks and shell words must use the same project and identity.
   These are launcher requirements, not checks performed by `.mcp.json`.

   Start the child from `.mcp.json`. Claude Code supports expansion in
   `command`, `args`, and `env`, but missing variables do not prevent the
   configuration from loading. With no default, it passes `${VAR}` literally
   and reports a warning in `claude mcp list`. See the official
   [environment expansion reference](https://code.claude.com/docs/en/mcp#environment-variable-expansion-in-mcp-json).

   In a Windows check with a real project marker and a literal
   `${ENGRAM_HOME}` home, Engram refused the first store open, showed the
   unexpanded path in the error, and created no files. This is a visible
   failure, not a silent write to another store. It is not a general
   placeholder-validation guarantee: a usable path or nonblank literal
   actor/session id still needs the launcher's checks. Do not repair this
   error by adding a home default or initializing an unintended store.

   A separate throwaway-store check used a valid home but literal
   `${ENGRAM_ACTOR_ID}` and `${ENGRAM_SESSION_ID}` identity values.
   `engram work add` succeeded without an Engram warning or refusal; the
   canonical object retained both literals verbatim. The path-open failure
   above does not protect identity. With the same broken configuration,
   multiple sessions would share one actor and session id, making their
   records indistinguishable by identity. Canonical attribution cannot be
   rewritten later. Engram accepts asserted identity; it does not detect
   this launcher mistake. Validate before starting the harness.

   Both `--actor-id` and `--session-id` remain explicit. The empty fallback
   below applies only to optional actor context, never to store or identity:

   ```json
   {
     "mcpServers": {
       "engram": {
         "type": "stdio",
         "command": "engram",
         "args": [
           "mcp",
           "--actor-id", "${ENGRAM_ACTOR_ID}",
           "--session-id", "${ENGRAM_SESSION_ID}"
         ],
         "env": {
           "ENGRAM_HOME": "${ENGRAM_HOME}",
           "ENGRAM_ACTOR_CONTEXT": "${ENGRAM_ACTOR_CONTEXT:-}"
         }
       }
     }
   }
   ```

3. **Inject orientation with a `SessionStart` hook** in `.claude/settings.json`
   for the `startup`, `resume`, `clear`, and `compact` sources; the hook's stdout is
   added to the model's context, which is exactly what `engram work next --peek`
   prints:

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "matcher": "startup|resume|clear|compact",
           "hooks": [{ "type": "command", "command": "engram work next --peek" }]
         }
       ]
     }
   }
   ```

`engram work next --peek` does not stage or acknowledge delivery, so a hook
whose stdout never reaches the model consumes no page. It is suitable for
orientation on a quiesced or verified established store, not initialization
or repair. Surface a failing `SessionStart` hook instead of swallowing it.
Ordinary `next` remains the explicit advancing call; peek is not a promise of
its exact later page. An ordinary `next` hook stages a page even if its stdout
never reaches the model; a following ordinary `next` can implicitly acknowledge
that unseen page. Do not substitute it for a resume peek.
Peek never writes the persistent database or WAL and never falls back to a
writable connection. SQLite may recreate its shared-memory coordination
sidecar. Surface any read refusal: initialize an absent store explicitly with
`engram init`; for quiesced verification compare database and WAL bytes,
not the directory inventory, because the coordination sidecar may appear.
Surface access/recovery errors to the operator before using ordinary `next`
when writes and delivery advancement are permitted. Peek retains memory
navigation: its `changed` compares the recorded advertisement, not whether notes
were read or applied,
and repeating pure reads never acknowledges it. This hook passes no context
generation, so its peek never directs the agent to list memories; to have it
do so after each of these events, pass `--context-generation` with a value
that differs on each one. MCP hosts use `peek: true`.

Everything else on this page applies unchanged: one store per project on
each host initialized with an explicit `advisory` assurance, the same values
at every injection point, and no claim of gating. The coordinator owns plan
items as an external source (see the last section); it does not need the
turn-gated channel for an advisory pilot.

### Recover a host delivery after an invalid ACK

This recipe is for a host using explicit acknowledgements through `work core
next`, not the agent's advisory `next` or startup `next --peek`. Preserve the
same project, store home and session identity. Serialize the entire sequence
against other advancing calls and focus changes for that session.

1. Run `engram work core next --sections focus` with neither ACK flag. Read
   `session.confirmed_project_cursor` as `C` and `session.pending_delivery`.
   Without `changes` or ACK fields, this does not stage or acknowledge a page;
   do not infer that it performs no session writes.
2. Run `engram work core next --sections changes --acknowledge-through C`,
   substituting that cursor and omitting `--acknowledge-token`. An ACK of the
   already-confirmed cursor is a no-op. If a pending page exists, its retained
   change payload, `delivered_through` and `delivery_token` are replayed exactly,
   even if new feed data has arrived. If none exists, this call stages the next
   page from `C`. Dynamic advisory response fields need not be identical.
3. Deliver the returned page before acknowledging it. Pass its exact
   `delivered_through` and `delivery_token` as `--acknowledge-through` and
   `--acknowledge-token` on the next core call. Include `--sections changes`
   to stage the following page, or `--sections focus` to ACK without staging.
   Repeating the old ACK is idempotent and does not acknowledge a following
   pending page; that page needs its own returned pair.

A wrong token for an unconfirmed pending page still refuses. Never recover by
omitting `--acknowledge-through` on a changes call: that implicitly acknowledges
the pending page, even if its earlier response was lost. If another caller
advances the session, a stale recovery cursor may refuse; restore serialization
and re-read it. This procedure does not promise exact replay across concurrent
advancement or a focus change discarding the staged page, and does not repair
data that was already acknowledged without delivery. For agent startup
orientation, retain the non-advancing peek recipe above.

### Deployment on the target computer

The MADE integration will run on another computer. This repository supplies
the Engram contract and recipe; runtime acceptance belongs to that target
host. Its operating system and launcher are not assumed here. This is not a
request to transfer the current database or enable portable or remote sync.

The integrator chooses and verifies:

- Launcher location and entrypoint, including the project working directory,
  selected Engram executable and intended absolute store home.
- A durable mapping from MADE conversation identity to Engram session id.
  Keep the actor principal separate from the session id and optional context.
- Restart and concurrent-launch rules: when the old process is stopped,
  when the same id is reused, and when a new id is allocated.
- A run of the smoke test below through that actual launcher, with the
  results recorded by the integrator.

These are target-host acceptance steps, not missing local prerequisites for
the contract or independent plan intake. The contract is delivered; the
actual deployment has not been tested here.

### MADE smoke test — required before deployment acceptance

Use an isolated test project and store, initialized explicitly as advisory.
Do not experiment on live work or repair an unexpected store automatically.

1. Start through the MADE launcher. Check its chosen project, absolute home,
   actor and session against the values used by both MCP and shell words.
   Check the installed build with `engram --version` and the build shown by
   `next --peek`. Inspect `/mcp` and `claude mcp list` for startup problems.
2. Verify that startup, resume, clear and compaction put peek output into
   the model's context. Confirm that hook-only reads leave focus and staged
   delivery unchanged. If output is discarded, it must still consume no
   delivery page. Ordinary `next` is a separate explicit action.
3. Stop and resume one conversation. Verify it uses the same mapped session.
   Launch a second concurrent conversation and verify it uses a distinct
   session while both resolve the same project store.
4. Test missing, blank and unexpanded required values. The launcher must
   refuse before starting Claude. Confirm that it does not substitute a
   home default, initialize another store, or allocate a replacement session
   silently. Test a failed peek too: the operator must see the error.
5. Record Claude Code and Engram versions, configuration locations and
   observed results. Restart old MCP children after an Engram install;
   replacing the executable does not update a running process.

## Version story

Ordinary opening refuses incompatible store schemas before mutation, and
`session_bind` carries the host's
`capability_map_revision`. Engram negotiates no protocol features or
versions; a host pins the build it ships with and follows the
[per-store cutover](#before-upgrading-cut-over-each-store) above for an
existing store. Initialization is for a new store, not an upgrade remedy.
`session_bind` belongs to the optional host-private control channel; the
advisory MCP-and-hooks recipe does not require that channel.

## Turn-gated tier — optional

A host that wants Engram to admit every model turn uses the host-private
JSON-lines control channel (`session_bind → turn_evaluate → turn_begin →
turn_checkpoint`) and withholds the prompt until Engram grants and begins the
turn. Every frame is strict: exactly the current field set, no additive or
legacy fields. Resource-lease acquisition/release, host obligation waiver,
and the finalizer purpose/phase are removed; hosts must not send those frames.
A grant carries no delivery page and there are no recovery turns: the work
context an agent sees comes from `next`. While hosts move off the old fields,
`turn_evaluate.purpose` may be `ordinary` or absent, and
`turn_begin.delivery_tokens` may be `[]` or absent; a non-empty token list is
refused with `grant_scope_mismatch`. A new bind leaves the session `ready`, so
it may request a turn at once; `turn_evaluate` still admits or refuses that
turn by its usual checks.
An empty resource_intents list remains valid. Control sessions bind directly
by project and external reference, without starting a compatibility task.
Action gating and action-outcome reconciliation are designed and deferred
([action gates](features/action-gates.md#when-to-build-it)), and
organizational-authority mediation is not built; all three fail closed
today. See the
[behavioral control plane](features/behavioral-control-plane.md).

## External planner as a work source

A planner that owns plan items keeps owning them. It admits each item into
Engram as an immutable `WorkSourceSnapshot` with a stable source key; Engram
owns the local work item from then on (readiness, claims, evidence,
completion) and never mirrors planner state back. A changed plan item is a
new snapshot and an immutable notice that applies nothing, never a silent
update. This file intake uses
[preview/apply and exact source lookup](features/source-intake.md).
First intake needs an authored local title and outcome; absent acceptance
means zero criteria. Refresh has no draft and records a visible notice, never
an automatic local patch. Integration testing belongs to the target host.
