# Host Checklist

> Normative reference: [spec §8](spec.md#8-interfaces).
> Related briefs: [CLI & MCP](features/cli-and-mcp.md),
> [behavioral control plane](features/behavioral-control-plane.md),
> [security & trust](features/security-and-trust.md), and
> [external adapters](features/tracker-adapter.md).
>
> Everything on this page is installed behavior; see
> [shipped today](shipped.md). Planned tiers and sections are marked as such.

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
   exposes the thirteen words plus `search`. The child keeps one store
   connection for its lifetime; a failed operation rolls back before the next
   call.
4. **Show the agent what is ready.** Run `engram work next` at session start
   and after every context compaction, and inject its text; it is the whole
   orientation an agent needs. Receipts end with `reminders` and `next`
   commands; agents follow them.
5. **State the assurance honestly.** Without turn mediation the deployment is
   `advisory`: the agent can bypass Engram. `engram doctor` prints the
   required assurance and supported effects in its `--json` report and the
   unavailable capabilities as human warnings on stderr; do not describe the
   integration as gated.
6. **Kill switch.** Stop injecting; nothing else to undo. Stores, evidence,
   and memories remain readable with the shell words.

## Claude Code as the host (no TermAl)

An external coordinator that launches `claude` directly gets the whole base
tier from three pieces, because Claude Code starts one MCP child per session
and runs hooks whose stdout enters the model's context.

1. **Set the identity in the environment of the `claude` process** before
   launching it: an absolute `ENGRAM_HOME`, `ENGRAM_ACTOR_ID`, one opaque
   `ENGRAM_SESSION_ID` per logical session (never the reserved
   `local-process-` prefix), and optionally `ENGRAM_ACTOR_CONTEXT`. The
   coordinator persists that session id and reuses it when it resumes the
   same conversation in a new process, because claims, focus, delivery
   cursors, and same-holder retake are keyed by it; it mints a fresh id only
   for a genuinely new or concurrent session. Claude Code exposes no session
   id of its own to MCP servers, so the coordinator's value is the session id
   for the whole session; Bash tool calls inherit the same environment, so
   shell words and MCP words agree.
2. **Start the MCP child from `.mcp.json`** with environment expansion, which
   Claude Code supports in `command`, `args`, and `env`. The child reads
   `.engram-project` from its working directory, which Claude Code sets to
   the project root; both `--actor-id` and `--session-id` are required, so a
   `claude` started without the coordinator's environment gets an MCP child
   that fails to start rather than one that guesses an identity:

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
   added to the model's context, which is exactly what `engram work next`
   prints:

   ```json
   {
     "hooks": {
       "SessionStart": [
         {
           "matcher": "startup|resume|clear|compact",
           "hooks": [{ "type": "command", "command": "engram work next" }]
         }
       ]
     }
   }
   ```

`engram work next` stages a delivery page that the following `next` call
acknowledges, so a hook whose stdout never reaches the model still consumes
that page: surface a failing `SessionStart` hook instead of swallowing it.

Everything else on this page applies unchanged: one store per project on
each host initialized with an explicit `advisory` assurance, the same values
at every injection point, and no claim of gating. The coordinator owns plan
items as an external source (see the last section); it does not need the
turn-gated channel for an advisory pilot.

## Version story

There is exactly one: a store written by a different build is refused
generically before mutation, and `session_bind` carries the host's
`capability_map_revision`. Engram negotiates no protocol features or
versions; a host pins the build it ships with and re-initializes stores
through the recreation path in [development](development.md).

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
new snapshot and a proposed local revision, never a silent update. This
intake path is designed and tracked; until it ships, a planner creates items
with the ordinary `add` word and cites its key in the outcome text.
