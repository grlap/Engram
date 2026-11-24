# Engram

Engram is a local task tracker and persistent memory store for coding agents.

It keeps tasks, decisions, test results, and project notes outside the chat.
An agent can return after a restart or context compaction and see what it
owns, what changed, and what to do next. Multiple sessions share one project
database without sharing the same task claim.

Engram is written in Rust. It provides a CLI and a Model Context Protocol
(MCP) server. Data stays in SQLite on your machine. No external tracker or
cloud service is required.

This is prerelease software. We use it as the only writable tracker for two
projects: Engram itself and the TermAl agent host.

## Current status — 2026-10-29

Recent changes on master:

- Selected verification evidence can be restricted to the host-measured Git
  tree it verified; a missing or different tree cannot qualify it (`56f84d7`).
- Graph restore inserts parents before children when the snapshot has no
  supersession references, avoiding deferred foreign-key scans. Snapshots
  with supersession retain their original insertion order (`6ebcc62`).
- The launcher verification workflow uses the repository's Node 24.14.0 pin
  (`6ebcc62`). The [review procedure](.claude/commands/review-changes.md#3-freeze-the-review-input)
  describes how review uses the shared `source_id`.
- A known-committed note or gate stays successful if later response work
  fails, returning its evidence locator and readback while disclosing the
  unavailable response details (`9f4a1c7`).

The dogfood stores completed the verification-applicability cutover. This
dated checkpoint is historical and does not describe current host settings;
the turn gating in use at that time has since been removed (see [records from earlier
builds](docs/features/execution-observability.md#records-from-earlier-builds)).

Repository landing validation can run on a second trusted Windows host or
the Mac for an exact test ref under the same-tree evidence rules; Windows-only
changes still need Windows coverage. See the [current inventory and
limits](docs/shipped.md) and [next steps](docs/roadmap.md).

End-to-end Git task exchange and reconciliation remain planned. Explicit
local planning-text export/import and preparation already ship; they do not
transfer private scratch, live claims or execution authority.

## What you can do

- Create tasks with acceptance criteria, dependencies, and child tasks.
- Assign work and claim it for one executing session at a time; hand a live
  claim to another session with an explicit handoff.
- Record progress, decisions, test results, and handoffs.
- Resume work without marking unread changes as delivered.
- Bind acceptance criteria to host-verified checks, so completion refuses
  while a required check is missing or stale.
- Complete work with a permanent record of its criteria and evidence. Under
  an operator-set acceptance policy, a recorded, attributed per-criterion
  evaluation replaces self-assertion.
- Keep project notes with revision history.
- Check database integrity, make local backups, and export planning history.

The [feature inventory](docs/shipped.md) lists what is available today.

## Build and set up

You need a stable Rust toolchain and a C/C++ compiler for bundled SQLite.
From this repository, install the CLI:

```sh
cargo +stable install --path . --locked
engram --version
```

Each project needs a tracked `.engram-project` file containing one stable
project ID, such as `example.com/team/my-project`. This repository already
has one. Keep the same ID across checkouts of the same project.

Without `--project-file`, CLI and MCP search from the working directory up
through its ancestors to the filesystem root for the nearest `.engram-project`,
without stopping at `.git`. Explicit paths win (relative to the working
directory), and an invalid or unreadable selected file refuses without falling
back. A nested worktree uses its own marker if present; otherwise it can find
an outer repository's marker. The selected marker's parent is the project root.
If none is found, change into the intended project tree or pass its marker
explicitly; do not create another marker in a subdirectory to fix the refusal.

Set an absolute data directory, an actor name, and a session ID. For example,
in a POSIX shell:

```sh
export ENGRAM_HOME=/absolute/path/to/engram-data
export ENGRAM_ACTOR_ID=alice
export ENGRAM_SESSION_ID=alice-session-1
```

In PowerShell, use:

```powershell
$env:ENGRAM_HOME = 'C:/engram-data'
$env:ENGRAM_ACTOR_ID = 'alice'
$env:ENGRAM_SESSION_ID = 'alice-session-1'
```

Use a different session ID for each concurrent session. Reuse it when
resuming that same logical session. An agent host should set these values
for both its MCP process and shell commands.

Run these commands from the project directory:

```sh
engram init
engram doctor
```

Engram tracks work and stores what the host observes; it never controls the
agent's tool execution.

The multi-line command examples use POSIX shell syntax. In PowerShell, put
each command on one line without the trailing backslash. See the
[CLI and MCP guide](docs/features/cli-and-mcp.md) for more configuration options.

## Basic workflow

Create a task:

```sh
engram work add "Fix configuration loading" \
  --accept "Invalid configuration returns a clear error"
```

Use the task reference returned by `add` in place of `REF` below:

```sh
engram work claim REF
engram work note REF "The parser accepts an empty configuration."
```

Do the work and run your checks. Then record their result and complete
the task:

```sh
engram work gate parser-tests --work-ref REF
engram work done REF "Added validation and regression tests."
```

`gate` records a result; it does not run the test. Use `--failed "failure"`
to record a failed check. Do not record a pass before running it.

If work is still required, `done` refuses completion and explains what is
missing. Responses include reminders and suggested next commands.

### Resume and inspect

```sh
engram work next --peek
engram work show REF
engram work show REF --notes --gates
```

`next --peek` reads the current work summary without changing focus or
delivery state. Use it after context compaction or when you only want to
look. It requires an initialized store. Repeated calls show a bounded preview;
they do not move through older pages.

Ordinary `next` advances delivery. Do not use it in a recovery hook: it can
stage a page that a later call acknowledges even if the agent never saw it.

`show REF` does not select a target for later writes. Use explicit task
references when recording or changing work.

Compact responses and note/history windows have a 12 KiB limit. They report
omitted content. Note/history pages provide commands to read more. Follow
a note's detail command when its full text is needed, especially for approval
or stop instructions.

### Hand work to another session

To give a live claim to another session, use an explicit handoff. The holder
offers it to the recipient's real session ID, which the host or coordinator
supplies, and the recipient accepts:

```sh
engram work handoff REF --to OTHER-SESSION-ID \
  --summary "Parser fixed; the regression tests remain."
# In the receiving session:
engram work handoff REF --accept
```

Until the offer is accepted, the offering session still holds the claim.
`--cancel "why"` withdraws the offer, and an offer never outlives its claim.
Each step is recorded as an audited event on the run.

A claim that is not renewed lapses when its time runs out. Another session
can then recover it with `engram work claim REF --recover "why"`. The
recovery keeps the same run, and the item's history records it.

### Link evidence to a criterion

Completion does not silently change acceptance criteria. It also reports
which criteria have no evidence linked to them.

Links are optional. To add one, read `show REF` for criterion positions and
`acceptance_basis`, then read `show REF --notes --gates` for evidence locators:

```sh
engram work done REF "Added validation and regression tests." \
  --link POSITION=LOCATOR --link-basis BASIS
```

Replace the placeholders with values from those reads. A link must refer to
an existing holder note, status, or gate from the current run. If the task
changes before completion, read it again before linking.

A link is the author's evidence citation, not independent verification.
“No evidence linked to this criterion” does not mean “no evidence exists.”

### Bind a criterion to a host check

A criterion can be bound to a host-verified check with
`add`/`update --bind POSITION=KIND`, where `KIND` is `test`, `build`, `lint`,
`review` or `acceptance`. This works with or without an acceptance policy.
The binding opens an obligation on the run, and completion refuses until a
passing host check of that kind covers the run's latest source change, or
while a newer check of that kind has failed. Only a host that runs Execution
Observability records such checks as registered verification; without one, a
bound criterion can only be revised or waived.

### Complete under an acceptance policy

By default completion is self-asserted: the completing actor's record seals
every criterion. An operator can instead activate an acceptance-evaluation
policy for the project:

```sh
engram control-policy set-acceptance-evaluation \
  --modes independent_session \
  --authorized-by alice --idempotency-key enable-acceptance-evaluation
```

Under that policy `done` refuses self-assertion. The newest `evaluate` record
on the run must be fresh and pass every criterion; an earlier pass never
stands in for a later failing record. A record holds one immutable, attributed
verdict per criterion, bound to the exact criteria revision, the run, and its
evidence. The `evaluate` step runs in the evaluator session, which must never
have held the run; `done` runs back in the claiming session:

```sh
# In the evaluator session (never held REF):
engram work evaluate REF --mode independent_session \
  --acceptance-basis N --evidence-basis M \
  --verdict 1=pass:judgment --rationale 1="..." --evidence 1=LOCATOR

# Back in the claiming session:
engram work done REF "Added validation and regression tests."
```

Read `N`, the acceptance basis, and `M`, the evidence basis on the line after
it, from `show REF`, and each `LOCATOR` from `show REF --notes --gates`.
Under the policy, evidence is cited in `evaluate`, and `done` refuses author
`--link`s.

With a host that runs Execution Observability, such as TermAl, which records
host checks and measures the source, the policy can require more:

- With `--mechanical-basis observed`, no pass may rest on the `asserted`
  basis, a gate the agent recorded. A check-backed pass is `observed` and
  cites the record id of a passed host-minted check:
  `--verdict 1=pass:observed --evidence 1=RECORD-ID`. A bound criterion
  passes only this way, whatever the mechanical basis.
- With `--require-source-freshness`, the evaluator declares the complete source
  it judged with `--source-id-file PATH` on `evaluate`, and `done` must present
  the same identity, measured by the host at completion. A record without one
  never matches. An evaluation stored with an older source fingerprint can't be
  presented again, so it is evaluated afresh; see [registered Git-input
  evidence](docs/features/acceptance-evaluation.md#registered-git-input-evidence)
  for the exception.

Revising the criteria or bindings a failing evaluation judged does not clear
the failure. After the executor's own revision, the next evaluation must name
the failed record with `--supersedes RECORD_ID`, which `show` prints, and come
from an evaluator that never held the run.

The host evaluates; Engram enforces. Engram never calls a model, a build, or
a command. Independent verification is a matter of host configuration: the
policy names the allowed evaluator modes (`same_session`, `sub_agent`,
`independent_session`), and the host produces the evaluation — for
`independent_session`, in a separate session that never held the run, which a
host such as TermAl can spawn as a read-only evaluator that records verdicts
under its own identity. Evaluator identity stays asserted host context unless
the host channel mediates it. Engram validates that citations are what the
verdict claims; their relevance is the evaluator's judgment, recorded as
such.

See [acceptance evaluation](docs/features/acceptance-evaluation.md) for the
policy contract and the evaluation record.

### Keep project notes

```sh
engram work remember "Configuration files use UTF-8." --key config-format
engram work memories
engram work memories config-format --full
engram work remember "Configuration files use UTF-8 without a BOM." \
  --key config-format --revise --expected-revision 1
engram work memories config-format --full --revision 1
```

Revisions preserve earlier versions under the same key. Use positional text
or `--text "Project note"`, not both. The optional expected revision prevents
a stale update. `forget KEY` permanently retires the key and stops current
and historical reads; it does not erase the stored
history. Do not put secrets in project notes.

## Connect an agent host

Start one MCP process per session, with the same environment as the agent:

```sh
engram mcp --actor-id alice --session-id alice-session-1
```

MCP exposes the same work commands as the CLI, plus `search`. For recovery,
call the `next` tool with `peek: true`. A host can inject this summary at the
next prompt after session start or compaction.

Engram checks its task rules, but the agent can still edit files or run
tools without asking it. Claims schedule work; they grant no file or external
authority.

See the [host checklist](docs/host-checklist.md) for setup and
[Execution Observability](#execution-observability) below for what a host
can record.

## Execution Observability

Execution Observability lets the agent host, not the agent, supply the
evidence that work was done. A host such as TermAl records, through a private
work-session channel, what it saw run, how the source changed, and which
registered checks passed or failed. Engram stores these facts, ties them to
the item and run, and uses them at completion. It never admits, delays or
refuses a model turn, and it grants no file or tool authority. In TermAl the
project setting "Execution Observability", on by default, switches only the
automatic observations; registered checks work either way.

What is delivered:

- **Host-recorded checks.** The stages of a test-launcher run that the host
  registered before it started can enter as verification, when the host
  submits the run's result against a still valid item, claim and named root.
  Credit needs equal, complete source identities before and after the run,
  and Engram measures the named root again when the check is used. A passing
  stage can satisfy a criterion bound with `--bind POSITION=test`.
  Registration alone earns nothing.
- **Remote test results.** A test run on another host, such as the DL560 or
  the Mac, can enter as verification evidence from its launcher's completion
  facts and a stated destination. Credit needs a finished passing result, a
  still valid destination whose expected commit matches the tested commit
  (for a Merger's `refs/heads/test/` ref, whose expected tree matches the
  tested tree; an author's test ref counts as a commit), and equal, complete
  source identities before and after the run; each result Engram does not
  credit gets a named reason. A focused
  or full run can verify a criterion; only a full run counts as a landing
  gate. Whether a change may land is the repository's own rule, such as this
  one's [Authority and Git](AGENTS.md#authority-and-git).
- **Source identity.** Evidence is tied to a complete `source_id`: the root
  commit and tree, one digest of the staged, unstaged and untracked Git
  changes, and the pins and checkout commits of dirty or off-pin submodules.
  `engram source --root ABSOLUTE-PATH --json` prints it without opening a
  store. When the capture succeeds, its `source_id` field is what
  `--source-id-file` expects; otherwise the source is reported unavailable.
  Equal identities qualify even with equal local changes.
- **Acceptance evaluation.** It applies only when an operator activates an
  [acceptance policy](#complete-under-an-acceptance-policy) for the project.
  Then `done` refuses self-assertion and needs a fresh evaluation that passes
  every criterion. Under `independent_session`, the evaluation must come from
  a separate session, supplied by the host, that never held the run. When the
  run carries registered checks, a passing evaluation, even one citing only
  notes, declares the complete `source_id` it judged. A registered check must
  have passed on that source at the evaluated cut, and Engram measures the
  named root itself at evaluation and again at `done`.

Limits:

- Engram trusts what the host reports. Actor identity is asserted, not
  authenticated, and Engram itself never runs a check, a build or a model.
- Without an active acceptance policy, `done` stays self-asserted.
- Recording across every supported agent harness is still listed as
  remaining work in the [roadmap](docs/roadmap.md).
- A remote run moves no store and no claim.
- `readiness` does not detect mixed process builds. Every process using a
  store needs a build that reads remote results and shared `source_id`
  records before the first one arrives, and an older build is no rollback
  once the store holds them.
- The Git reader can miss edits hidden by `assume-unchanged` or
  `skip-worktree` flags.
- Records stored by earlier builds that name a commit, tree or source
  fingerprint are read as stored; a new remote result in that older form is
  refused.

See the [Execution Observability guide](docs/features/execution-observability.md)
and the [feature inventory](docs/shipped.md) for the full contract.

## Data and limits

SQLite is the source of truth for a project on one host. Work history and
memory versions are stored as immutable records under minted ids.
Concurrent sessions use transactions and checked ownership to coordinate.

`engram doctor` verifies objects and stored state in one read snapshot.
`backup` and `restore` provide verified local copies. `graph save` and
`graph load` export and restore planning history; they do not transfer live
claims or execution authority.

Important limits:

- Identity comes from the caller. It is recorded, not authenticated.
- The development redactor is a no-op. It does not detect or remove secrets.
- Prerelease builds can reject an incompatible store. There is no automatic
  migration chain: a whole store moves with `migration export` and
  `migration import`. Read the [upgrade guidance](docs/development.md) and
  [full store migration](docs/features/full-store-migration.md) first.
- Installing a new binary does not update an already-running MCP process.
  Compare its `next` build token with `engram --version` and restart the
  child process when needed.
- External plan intake, automatic off-host backup, cross-host sync, report
  assembly, and external publication are not yet available as a complete
  workflow.

See the [roadmap](docs/roadmap.md) for planned work.

## Documentation

- [CLI and MCP guide](docs/features/cli-and-mcp.md): commands and configuration.
- [Available features](docs/shipped.md): current implementation.
- [Local work model](docs/features/local-work-system.md): tasks and completion.
- [Acceptance evaluation](docs/features/acceptance-evaluation.md): evidence-based completion policy.
- [Memory model](docs/features/typed-memory-model.md): types, scope, and history.
- [Security and trust](docs/features/security-and-trust.md): guarantees and limits.
- [Architecture](docs/architecture.md): internal components and data flow.
- [Development](docs/development.md): builds, tests, and contribution workflow.
- [Vision](docs/vision.md) and [specification](docs/spec.md): goals and full design.
- [Website preview](docs/website.md): run the project website locally.

## License

Copyright 2026 Engram contributors.

Code, documentation, and website content are licensed under
[Apache License 2.0](LICENSE), unless stated otherwise.

Third-party components keep their own licenses. See
[third-party notices](THIRD-PARTY-NOTICES.txt) and include the applicable
notices when distributing Engram.
