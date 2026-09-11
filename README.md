# Engram

Engram is a local task tracker and persistent memory store for coding agents.

It keeps tasks, decisions, test results, and project notes outside the chat.
An agent can return after a restart or context compaction and see what it
owns, what changed, and what to do next. Multiple sessions share one project
database without sharing the same task claim.

Engram is written in Rust. It provides a CLI and a Model Context Protocol
(MCP) server. Data stays in SQLite on your machine. No external tracker or
cloud service is required.

This is prerelease software. We use it to track development of Engram itself.

## What you can do

- Create tasks with acceptance criteria, dependencies, and child tasks.
- Assign work and claim it for one executing session at a time.
- Record progress, decisions, test results, and handoffs.
- Resume work without marking unread changes as delivered.
- Complete work with a permanent record of its criteria and evidence.
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
engram init --required-assurance advisory \
  --authorized-by alice --reason "Use Engram as a local task tracker"
engram doctor
```

`advisory` means Engram tracks work but does not control the agent's tool
execution. A plain `init` defaults to a stronger policy, so use the explicit
option for this setup.

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

The standard integration is advisory. Engram checks its task rules, but
the agent can still edit files or run tools without asking it.

For hosts that need execution control, a separate private API can admit
model turns and manage exclusive resource leases. The host must enforce
those decisions. Per-tool action gating is not yet available.

See the [host checklist](docs/host-checklist.md) for setup and the
[control-plane guide](docs/features/behavioral-control-plane.md) for the
optional private API.

## Data and limits

SQLite is the source of truth for a project on one host. Work history and
memory versions are stored as immutable, content-addressed records.
Concurrent sessions use transactions and checked ownership to coordinate.

`engram doctor` verifies objects and stored state in one read snapshot.
`backup` and `restore` provide verified local copies. `graph save` and
`graph load` export and restore planning history; they do not transfer live
claims or execution authority.

Important limits:

- Identity comes from the caller. It is recorded, not authenticated.
- The development redactor is a no-op. It does not detect or remove secrets.
- Prerelease builds can reject an incompatible store. There is no automatic
  migration chain. Read the [upgrade guidance](docs/development.md) first.
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
