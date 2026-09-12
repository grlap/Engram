# Host Checklist

> Normative reference: [spec §8](spec.md#8-interfaces).
> Related briefs: [CLI & MCP](features/cli-and-mcp.md),
> [behavioral control plane](features/behavioral-control-plane.md),
> [security & trust](features/security-and-trust.md), and
> [external adapters](features/tracker-adapter.md).
>
> Engram capabilities below are installed unless marked as planned; see
> [shipped today](shipped.md). Host setup requirements are not a claim that a
> particular launcher implements them. MADE is the external planner and
> coordinator host used for the integration pilot. Its recipe still needs
> runtime acceptance in that host.

A host is any runtime that starts agent sessions and wants Engram to own
their work: TermAl today, an external planner and coordinator tomorrow. The
base tier below is the whole integration for an advisory pilot. Nothing in
it requires the host to mediate turns or actions.

## Base tier — advisory

1. **One store per project on each host.** Ship a tracked `.engram-project`
   with the stable project id; every session and worktree of that project
   resolves the same SQLite store under an absolute `ENGRAM_HOME` (a
   relative value resolves against each process's working directory and
   silently splits the project across stores). Initialize it once per
   project on each host, before any other word touches the store, with
   `engram init --required-assurance advisory --authorized-by <operator>
   --reason "<why>"`: a plain `engram init` selects `turn_gated` as the
   project's required assurance, which an advisory pilot cannot honestly
   claim, and `engram doctor` would print that requirement. The flagged
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
   exposes the thirteen words plus `search`. Ordinary calls reuse a cached
   store connection; each peek opens a separate transient read-only connection.
   A failed operation rolls back before the next call.
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
2. Read `engram work memories` without a query. Follow returned continuation
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
   is the intended absolute store home, actor and session ids are nonblank,
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
and repeating pure reads never acknowledges it. MCP hosts use `peek: true`.

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

There is exactly one: a store written by a different build is refused
generically before mutation, and `session_bind` carries the host's
`capability_map_revision`. Engram negotiates no protocol features or
versions; a host pins the build it ships with and re-initializes stores
through the recreation path in [development](development.md).
`session_bind` belongs to the optional host-private control channel; the
advisory MCP-and-hooks recipe does not require that channel.

## Turn-gated tier — optional

A host that wants Engram to admit every model turn uses the host-private
JSON-lines control channel (`session_bind → turn_evaluate → turn_begin →
turn_checkpoint`) and withholds the prompt until Engram grants and begins the
turn. Every frame is strict: exactly the current field set, no additive or
legacy fields. Action gating, organizational-authority mediation, and
action-outcome reconciliation are planned and fail closed today; see the
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
