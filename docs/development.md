# Development & Review Workflow

How work happens in this repository. Agent-facing operational rules live in
`CLAUDE.md` / `AGENTS.md` (owned by the tooling scaffold). Both files contain
the same complete rules, so either runtime can read its own entry point alone.
This document is the human-readable overview and documentation conventions.

The [adopted coordination rules](agent-pair-workflow.md#adopted-coordination-rules-2026-09-13-revision-1)
govern review ownership, revalidation, communication, and durable recovery.
The remaining [Fable and Codex pilot proposal](agent-pair-workflow.md#pilot-proposal-fable-and-codex-working-as-a-pair)
is a proposal, not a change to standing instructions.

## Task tracking

This project tracks its work in Engram — the fourteen agent words, documented in
[CLI & MCP](features/cli-and-mcp.md#using-engram-as-an-agent).

- `engram work next` — what you hold, what is ready, what others changed;
  `engram work show REF` — detail; `engram work claim REF` — claim before
  changing anything.
- Implementers `note` decisions and validation evidence once and `done` the
  item they hold; `done` says what is still owed instead of closing anyway.
- No TODO lists in markdown; no ad hoc memory files — durable rules live in
  the instruction files, while attributed changing observations use
  `remember` and are retrieved through `memories`.
- Agent-facing local work is project-bound and has no grant token or grant
  expiry. Stores created by the prerelease grant-bearing build must be
  recreated; schema marker 1 intentionally has no migration chain. Today
  recreation is archive + `engram init` + re-adding open items with the
  words for stores that predate snapshots. The shipped
  [work-graph snapshot](features/work-graph-snapshot.md) replaces that path
  with `graph save`, then `engram init` and `graph load FILE` into an empty
  project store on the destination build. The two builds must share the
  runtime-derived snapshot format fingerprint; a format change or an older
  store keeps the manual re-add path.
  Either way that file carries no control policy: `engram init` on the new
  build repeats the project's `--required-assurance … --authorized-by …`
  bootstrap and any obligation rule set is re-applied by hand.
  Those bootstrap steps belong to graph and manual reconstruction only. A
  [whole-store transfer](features/full-store-migration.md) is the other route
  and carries the store's policy, authority and rule-set rows with everything
  else, so nothing is re-applied by hand after one.
- Every `HostControlRequest` variant is strict: the paired TermAl consumer must
  send exactly the current field set for every operation, with no additive or
  legacy fields. This cleanup is paired with the coordinated
  TermAl update that removes the `obligation_waive` operation; do not land either side alone and
  do not add a legacy-frame compatibility shim.

## Quality gates

Run `node scripts/test-launcher.mjs full` before landing any changeset with a
non-`.md` path; a changeset touching only `.md` files runs the two checks
[Required Quality Gates](../AGENTS.md#required-quality-gates) names instead.
Full mode preserves these gates in order:

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs scripts/test-launcher.test.mjs
node --test scripts/mcp-dogfood.test.mjs
node --test scripts/control-dogfood.test.mjs
node --test scripts/parity.test.mjs
node scripts/check-doc-links.mjs
```

On Windows the full clap command graph exceeds the default main-thread stack.
The CLI therefore parses and drives `run_cli` on a named 8 MiB thread; Tokio
worker stacks remain unchanged because only parsing and the top-level
`block_on` need the larger stack. A command-graph construction test pins the
workaround and panics are resumed so the standard panic payload and exit
semantics are preserved.

On Windows, run `pwsh -NoProfile -File scripts/test-rust.ps1` in place of
`scripts/test-rust.sh`. Both entry points run the ordinary Rust suite with
bounded test concurrency, then separate ignored claim-mutation,
`root_delta_scale_` and `planning_scale_` phases, each selected by name so
that a long fixture (a thousand-checkpoint history, a thousand sequential
prerequisite adds) stays out of the ordinary run and still runs in every full
gate. The shell entry point also raises the Unix
file-descriptor soft limit when the host permits it; that step is not
applicable on Windows.

The default is eight test threads, or the number of available processors on a
host with fewer, or four on a host that reports no usable count. It comes from
one measurement, of the unit-test binary on a
quiet 24-core Windows host: 68 s at four threads, 42 s at eight, 59 s at
twelve and 119 s at twenty-four, while total processor time rose from 242 s to
2210 s. Rebuilding the bundled SQLite without memory-status tracking, which
was tried and not adopted, removed that rise in processor time and part of the
extra wall time (38 s at eight threads, 66 s at twenty-four), but wall time
still rose past eight threads, so the cause is only partly known. Other hosts
and the other test binaries were not measured. Set `ENGRAM_TEST_THREADS` or
`RUST_TEST_THREADS` to use another count.

The shell entry point tries to raise the file-descriptor soft limit to 16384,
or to the hard limit when that is lower. When the host refuses, it asks for
smaller limits, largest first: the per-process maximum the host reports and
4096, the previous target. It stops at the first of them that the host accepts
or that the inherited limit already reaches, so an inherited limit that is
already higher is kept. Only when neither holds for any of them does it warn
and run with the inherited limit. The target itself was not measured. Set
`ENGRAM_TEST_FD_LIMIT` to use another.

The gate's Node tests run the thread default and this step-down against
stubbed hosts, so they need a POSIX `sh` on every host: on Windows the one
installed with Git or, without one, one on `PATH`.

The dev profile builds two dependencies optimized, `sha2` and
`libsqlite3-sys`; Engram's own code stays unoptimized, with full debug
information. Every process that reports a build hash digests its own
executable: with `sha2` optimized that took 0.05 s in place of 0.5 s for each
process, and the `diagnostics_cli` test binary 14 s in place of 101 s. With
SQLite optimized the unit tests, run on one thread with `sha2` optimized in
both builds, used about 15% less processor time: 231 s in place of 273 s.

The root-delta phase builds long-history fixtures, 500 steps by default. Each
scale test prints the size it builds and where the size came from. Set
`ENGRAM_ROOT_DELTA_SCALE` to a whole number from 500 to 1000, for example
1000, to run the fixtures at that size. Other values fail the test: the
ordinary tests already cover smaller histories, and the fixtures' timestamps
and claim lifetimes are laid out for at most 1,000 steps. The review
fingerprint does not record the variable, so unset it after a deliberate run.
Every root mutation reads the whole root state, so each fixture's build time
grows with the square of its size; the waiver fixture dominates. On a Windows
debug build the phase took about 2 minutes at 500 steps and about 7 minutes at
1,000; this is an observation, not a timeout or a performance limit. The test harness
can print "running for over 60 seconds" while a fixture is still working.
Measurements appear when each fixture reaches its reporting point, not as a
periodic heartbeat. Do not stop the gate just because it is quiet or passes
that warning threshold. Check process activity and possible lock contention
when investigating a suspected hang. The root-delta checks assert byte and
operation bounds; elapsed time is diagnostic only.

The ordinary Rust suite includes `tests/source_file_size.rs`, which keeps every
`.rs` file under `src`, at any depth, at 2,499 physical lines or fewer. Blank
and comment lines count, and CRLF counts like LF. Every `.rs` file under `src`
is counted, so a new or growing file cannot pass unlisted. Any file or
directory there that cannot be read fails the check, and so does a symbolic
link or junction, which is refused rather than followed. To see every count,
run the whole-tree test alone:

```bash
cargo test --test source_file_size -- --exact every_source_file_stays_within_the_limit --nocapture
```

A file already over the limit passes only as a known exception, listed in
`tests/source_file_size_exceptions.json` as `{"path": "src/…rs", "max_lines":
N, "split_item": "…"}`. `max_lines` is the file's current count, which it may
never exceed, and `split_item` names where the work that splits it is tracked;
this file is the one place such a reference belongs, and no report prints it.
The list is checked data, not a baseline to regenerate: a malformed or
duplicate entry fails every run. The whole-tree test also fails an entry that
names no file, a file already back within the limit, or a file that shrank
below its `max_lines`: lower `max_lines` to the new count, so a ceiling only
ever comes down, and remove the entry in the change that splits the file. The
list is empty today. A change that
pushes a file over the limit splits it in the same change rather than adding an
entry.

The suite also keeps guarded families, for per-file evidence. A family is a
module file and every `.rs` file under its child-module directory, so a module
split out of it stays counted with it. That directory is `MODULE/` unless the
family names another one, as `src/main.rs` does with `src/bin_support`, where
the binary's modules live. A missing module file or a missing child directory
of a family marked as split fails the check, and so does a link at either,
including a child-directory link whose target is missing. An unsplit family
may have no child directory, but a dangling link is refused rather than
treated as absent. Families are listed in the
`guarded_families!` list there, which also generates one test per family,
`family::NAME`.

For per-file evidence, such as a host-observed check behind a file-size
criterion, run the one family the criterion covers, for example:

```bash
cargo test --test source_file_size -- --exact family::storage_work_query --nocapture
```

It prints one line per file of that family, in path order, as
`PATH: N physical lines (limit 2499)`, or `(known exception, ceiling N)` for a
listed exception, and fails naming every file of the family over the limit or
past its ceiling. A family run checks only its own files, so a stale entry for
another file does not fail it. A family's report must stay within 3 KiB, so a
host-observed run of it fits the 4096-byte verification summary with the
command and result lines. A family that outgrows that budget fails its test,
naming its printed size. Its children cannot simply be listed as families of
their own, because the inventory refuses a file guarded by two families; that
failure means the family description must first learn to divide one module's
files. For every family's files, sorted by path, run:

```bash
cargo test --test source_file_size -- --exact guarded_source_families_stay_within_the_limit --nocapture
```

Running the file without `--exact` runs every test, so each family's lines
print again from its own test, and parallel test threads interleave those
lines; add `--test-threads=1` when reading that combined output.

### Doctor performance measurements

Compare `doctor --json` on two stores using one binary for both stores in each
measurement round. Make consistent SQLite online backups from read-only
connections into separate homes under this checkout's `target/`; never copy
an open database file directly or modify a live store for a benchmark. Keep
the same backup files for the before and after builds. Point `--home` at each
backup home and `--project-file` at that store's real project-id file, using
absolute paths. Time the complete command, record its exit code and health,
and repeat it to expose host-load variation. Record each binary's build
identity, SQLite version, backup time, and relevant row counts beside the
elapsed times. This makes differences between the two stores comparable
within each build and differences between builds comparable on fixed data.

The catalog FTS integrity regression test runs `verify_all` with SQLite
`query_only=ON` on a healthy index and after removing its structure and
segment records without changing the content row. It verifies that this
query-only connection still reports corruption; the timing procedure above
measures the real doctor path on realistic stores.
Another regression changes FTS content without rebuilding its intact index
and confirms that the read-only check detects the posting mismatch.

### Test launcher

One entrypoint handles full validation and focused checks:

```bash
node scripts/test-launcher.mjs full
node scripts/test-launcher.mjs focused -- node --test scripts/test-launcher.test.mjs
node scripts/test-launcher.mjs focused -- cargo test --lib control_runtime
```

Full mode is the landing validation of a frozen changeset with any non-`.md`
path, including configuration, lockfiles and fixtures. Focused mode is the
fast correction check between fixes: it runs the tests that cover a fix before
the next freeze, and never replaces full mode for such a changeset. A
changeset touching only `.md` files lands on two focused runs instead, the
link check and the AGENTS/CLAUDE byte comparison (see
[Required Quality Gates](../AGENTS.md#required-quality-gates)).

Commands are argument arrays, not shell strings. Name `pwsh -NoProfile -File`
or `sh` explicitly for shell scripts. Full mode chooses the correct Rust runner,
including its ordinary and scale phases, then the Node integration gates.
Both full and focused modes clear inherited `RUSTUP_TOOLCHAIN` overrides,
leaving selection to Rustup. A directory override at the repository root still
takes precedence over `rust-toolchain.toml`; `rustup show active-toolchain`
reports the selection. Removed names and values
are recorded in `results.json` as `clearedToolchainOverrides`, with a summary
notice when present. To intentionally use another toolchain for a focused check,
pass it explicitly, e.g. `focused -- cargo +nightly test ...`.
Full mode probes Cargo components; both modes check required executables before
expensive stages. For a focused external
integration requiring a supplied binary, add `--require-binary-env VARIABLE`
before `--`; the variable must contain an absolute executable path that passes
`--version`. No test command formats source, installs binaries or restarts a host.

Each invocation creates a unique directory below Git's `review-runs` metadata
directory (also works with linked worktrees). `request.json` identifies commands,
owner and expected source fingerprint; `input.json` uses the existing review
freeze format. Before yielding, retain the exact expected fingerprint in the
parent's work status, independently of these run files. Use that parent-held
literal when checking recovered results. Source and index must stay unchanged.
`results.json` records
stage exits, timestamps, full log paths and bounded diagnostics. Stdout/stderr
go directly to logs, never through a terminal stream. Only the final summary,
warnings, bounded failures and the closing record for the host reach context;
truncation points to the full log.
Filtering does not decide success: actual process exits do, with one addition.
A test stage whose runners' complete summaries count failed tests is failed
even when its command exited 0.
A failed command stops subsequent stages, which stay explicitly unrun. A stage
name consists of lowercase letters, digits and hyphens. Exceptions fail the
run;
a killed process writes no terminal result, and its run is never a pass.

`results.json` also carries `heartbeat: { at, everyMs }`. The launcher writes
it when it creates the run and again when the run is admitted, then refreshes
`at` every `everyMs` milliseconds, during a long stage too. The default is
10 000; `ENGRAM_LAUNCHER_HEARTBEAT_MS` sets another cadence from 1 000 to
600 000. A reader tells running from interrupted without trusting the recorded
`pid`, which a killed launcher leaves behind and the system may give to another
process. A run with no terminal result is running while `heartbeat.at` is at
most three intervals old. Once it is older the run is interrupted: the launcher
has most likely stopped. A system sleep or clock change can also age a
heartbeat, and a later beat then shows the run running again. `summary`
reports such a run as `RUNNING` or `INTERRUPTED`, and one without a heartbeat
as `UNKNOWN`. Saves of `results.json` retry a briefly refused rename, as
Windows can refuse one while another process holds the file open; that retry is
file I/O and never reruns a test.
The snapshot checks are boundary checks, not proof against transient edits.
Normalization limits are emitted in freeze CLI stderr and every run's
results/summary; fingerprint stdout remains unchanged.
Tracked content is compared through Git's normalized diff, not raw filesystem
bytes: line-ending-only changes (or changes erased by clean filters) can be
invisible on any platform. Known intervening edits still invalidate a run.
The Windows executable-mode/symlink limitation is separate. Artifacts use
owner-only POSIX modes (0700 run directory, 0600 files); Windows uses its ACLs.

A run whose launcher ended without saving a result can be recovered, which
saves for it the terminal result of an interrupted run:

```bash
node scripts/test-launcher.mjs recover RUN_DIRECTORY
```

Before it reports ready, the process that executes the stages records itself in
`results.json` as `executor: { pid, created }`: its process id and the creation
time the operating system gives for it. On Windows that comes from the system's
own `powershell.exe` under `SystemRoot`, never a copy found by name; on Linux
from `/proc`; on macOS from `/bin/ps`, never one found on PATH, run in UTC and
the C locale so that every query gives the same text. Other systems give a start
time that moves when the clock is set, so there an existing process reads as
unknown, and a run is recovered only once no process has its executor's id. The
executor also records `host`: its platform, host name and, on Linux, its process
namespace. When that query fails, the executor records `created: null` with the
reason in `unidentified`. `recover` asks the system for that process by id,
never by command line. Only the system's own answer that no such process exists
counts as gone: on Windows the query's, and elsewhere signal 0, which sends
nothing; a process that has ended but is still held, a Linux zombie or a Windows
process some handle keeps open, is gone too. A status or `ps` query that fails
for an existing process leaves it unknown. On Windows a process the query may
not open, such as a system service that took a dead launcher's id, is asked of
WMI through .NET, with no module loaded by name. WMI gives its creation time to
the microsecond, in local time with its offset, which the launcher applies
itself. That process is the recorded executor only when the recorded time cut to
the microsecond equals it, and an executor records only an exact time of its
own. When the process is alive with the same creation time, `recover` refuses,
whatever the heartbeat says. It refuses too when the query cannot answer, when
the run has not yet published its executor, when that record or its creation
time is malformed, when `results.json` describes another run, and when the
executor ran on another system or in another process namespace, or recorded a
creation time this system does not give, since its id means nothing to a query
made here. A refusal changes no file and exits 1. When no process has that id,
or the id now names a later process, the run is settled with the result TermAl's
launcher writes for an interrupted run and its reader already reads: `state:
"failed"`, `exitCode: 1`, `interrupted: true`, `ended` at the time of recovery,
and an `error` that begins `interrupted:`. The stage that was running is failed
with `outcome: "unknown"` and an `error` that begins `interrupted:`; it gets no
exit code, signal, end time or test count. Finished stages keep their fields and
logs, and unrun stages stay unrun. A `recovered` record adds where the run
stopped (`stage`, `startup`, `between-stages` or `finishing`), the stage, when
it was recovered and what the system answered. A run recorded before executors
were recorded names its id alone, with no host, so its id is asked of the system
running `recover` unguarded; it is recovered only when no process has that id.
An executor that could not ask for its own creation time records `created: null`
and is likewise recovered only when no process has its id. A terminal run is
left as it is, so recovering twice changes nothing. `recovery.lock` in the run
directory serialises recoveries and is released after every attempt, a failed
write included; a lock left by a killed recovery makes the next one refuse and
name the file. Under that lock `recover` reads the result again after the system
has said the executor is gone, so a result the executor saved just before it
ended is kept. `recover` never kills, reruns or notifies, and prints no record
line. `summary` shows a recovered run as `INTERRUPTED`, with the stage or phase,
never as a pass or a failure.

Foreground and detached launch receipts both print the run directory, manifest
path and expected fingerprint before waiting for completion. Foreground mode
prints this after execution admission and before running stages, so the parent
can retain recovery details while using its host's process-completion wait.

For an existing root worker delivering completion to a different coordinator:

```bash
node scripts/test-launcher.mjs full --detach --notify COORDINATOR_SESSION_ID
```

The worker must inherit its genuine `TERMAL_SESSION_ID`, `TERMAL_CLI` and host
connection configuration. Never supply someone else's identity; self-send is
refused before detachment. On Windows `TERMAL_CLI` must be an absolute `.exe`
path, not a `.cmd` or `.bat` shim. `STARTED` is emitted only after the child
has acquired its execution lock and read its run files; failures of those
admission steps return to the caller before it is told to yield. Subsequent
command validation and preflight failures arrive in the terminal completion
message. A duplicate execution never overwrites
the original owner's results. The detached process has no terminal handles and
saves results before
sending a stable-key mailbox message through `--message-file`. End the agent
turn after the launch receipt. No sleep/status/log-tail loop or watcher agent.
If admission fails after the request can be read, the caller can receive an
error without `STARTED` while the coordinator also receives the saved `FAIL`.
These describe the same run, not two executions; use its run directory to
inspect the failure before deciding what needs correction.
Actual survival after turn end and wake delivery must be verified on the host;
`STARTED` alone is not proof of either. User Stop and host dispatch restrictions
still apply. If the coordinator launches its own command, use foreground mode
and the host's supported process-completion wait; do not pretend self-mailbox
notifications work. Where no completion wake exists, disclose that limitation.

Recover an existing run at completion, without rerunning its tests:

```bash
node scripts/test-launcher.mjs summary RUN_DIRECTORY
node scripts/test-launcher.mjs notify RUN_DIRECTORY
```

`notify` is only for a recorded notification target, using the original root
identity. It resends the same saved body and idempotency key, not the tests.
New automatic messages group clean zero-exit successes and list unrun stages
separately. Warnings, exceptional states, failure diagnostics, truncation,
toolchain notices and freeze limitations stay visible, with results and log
locators. The input line reports the saved boundary comparison; it cannot
detect transient edits or establish host test credit. Unknown test counts
remain explicit. `summary` keeps the detailed stage and preflight listing
on demand. Previously frozen detailed messages retain their original bytes
on retry; changing presentation never changes notification timing.
Sender equality checks asserted session context, not authenticated identity.
Diagnostic excerpts are not redacted: anything a gate prints within the bounded
excerpt can be sent to the coordinator. Do not put secrets in gate output.
Notification errors do not erase test results. `notification.json` retains the
latest attempt's receipt; each attempt's separate log remains available.
The notification body is written to a unique temporary file, then published
without replacement through a hard link. Filesystems without hard-link support
refuse notification publication; test results remain available. An interrupted
write leaves a temporary artifact rather than a partial retry body. This does
not add a power-loss durability guarantee.
An unreadable request cannot supply a notification target, so it fails startup
without a `STARTED` receipt. A killed process or unwritable results directory
cannot guarantee completion delivery; a run without terminal results reads as
interrupted once its heartbeat is stale and never as a pass; `recover` settles
it as failed and interrupted.
`execution.lock` prevents a second execution of the same run directory, not
separate launches; the caller owns repository-level serialization. After a
crash, recover its artifacts and classify
the failure before choosing a new run. Diagnose actual failures and repair them
within the authorized scope, then validate the changed input; never retry blindly.

#### Record for the host

A process that executed the stages and waited for them ends its stdout with a
record of what ran: one line per stage in stage order, then one overall line.
The human summary above it is unchanged. The detached parent prints no record,
because its run has not finished; `summary`, `recover` and `notify` never print
one, so a saved run cannot be replayed as a new one. A detached worker writes
its record into `launcher.log`, which is a file like `results.json`.

```text
test-launcher/v1 run=ID stage=NAME kind=KIND state=STATE exit=EXIT
test-launcher/v1 run=ID stage=NAME kind=test state=STATE exit=EXIT executed=N passed=N failed=N ignored=N runners=N filtered=yes|no
test-launcher/v1 run=ID stage=NAME kind=test state=STATE exit=EXIT executed=unknown why=TOKEN runners=N filtered=yes|no
test-launcher/v1 run=ID overall=passed|failed|interrupted scope=full|focused stages=N [reason=TOKEN]
```

Fields are separated by single spaces, in this order, and no value contains a
space. `ID` is the run id, the name of the run directory. `NAME` and `TOKEN`
match `[a-z0-9][a-z0-9-]*`. `stages` is the number of stage lines, and every
line carries the same run. The format is agreed with TermAl, which reads it;
change it only together with that reader.

The record supports little, and that correctly. It gives test counts for
runners whose exit the launcher can rely on: `cargo test` and `node --test`
started by the launcher itself, and the stages of full mode, whose wrappers
are this repository's own and pass their runners' exits on. It reads runner
summaries and nothing else: no line of a log is taken for a test or for a
failure. A summary is known by its shape alone. Text of that shape which a
test prints at the start of a line, such as the forwarded output of a runner
it started itself, cannot be told from the stage's own summary and is counted
with it: it can add to the counts and fail a stage, and it cannot hide a
failure, because the stage's exit and its runners' own summaries still count.
Text that begins like a summary line but does not read as one makes the count
unknown. Node's TAP reporter prints every line a test writes, and every
diagnostic, behind `# `, and its default reporter prints a top-level
diagnostic behind `ℹ `; so a test that prints a line beginning with `tests`
under TAP, or gives such a diagnostic, leaves its stage without a count. So
does a coverage run: with `--experimental-test-coverage` Node prints its
coverage table behind the same mark, one row per directory, and a covered
directory named `tests` gives a row that begins like a summary line.
A wrapper that may run anything and end as it likes, `npm test` included, is
not a test stage; it runs as before and the record claims nothing about its
tests.

- `kind` is `test`, `build`, `lint`, `typecheck` or `other`. Full mode assigns
  it per stage: `fmt` and `clippy` are `lint`, `check` is `build`, `rust`,
  `freeze`, `mcp`, `control` and `parity` are `test`, `docs` is `other`.
  Focused mode takes it from the command: `cargo test` and `node --test` are
  `test`; `cargo clippy` and `cargo fmt` are `lint`; `cargo check` and
  `cargo build` are `build`; anything else is `other`. A `node` command
  that names more than one `--test-reporter` is `other`: each reporter
  prints a summary, and the log would count the tests once for each. For
  `node`, `--test`
  counts only among Node's own options, before `--` and before the script,
  and only after options the launcher knows; after one it does not know the
  command is `other`, because `--test` could be that option's value. Only a
  `test` stage carries counts.
- `state` is `passed`, `failed`, `skipped` or `interrupted`, and comes from
  the stage's process: it passed when the process exited 0. `exit` is the
  number the process exited with, or `none` when there is no exit code: the
  stage was skipped after an earlier failure or never started, the command
  could not be spawned (`failed`), or a signal that the launcher observed
  ended it (`interrupted`). One thing besides its exit fails a stage: its
  runners' complete summaries counting failed tests. Such a stage is `failed`
  and keeps the exit it had, so `state=failed exit=0` is possible. A child
  ended by a signal behind a wrapper that turns it into an exit code, such as
  `scripts/test-temp.mjs`, is `failed` with that code.
- Counts are read from the stage's complete log after it closed and are sums
  over the stage. `runners` is the number of complete runner summaries summed:
  one libtest `test result:` line per test binary and doctest run, one summary
  block per `node --test` process, from its TAP or its default reporter.
  `executed` is `passed` plus `failed`; ignored, skipped and todo tests are
  `ignored`. A failing test marked todo is todo to its runner, which counts
  it so whatever mark its reporter prints.
- `executed=unknown` means the log gave no complete count, and `why` names
  the first reason met: `not-run`, the stage was skipped or never started;
  `no-summary`, as when compilation fails, the reporter is one the launcher
  does not read, or the log could not be read; `incomplete-summary`, a
  libtest runner that announced its tests and printed no result, a TAP
  stream that started and printed no summary, or a summary cut short;
  `malformed-summary`, a summary line that does not read, its first line
  included, or one longer than 16,384 characters; `inconsistent-summary`,
  totals that do not add up or a result that does not account for the tests
  announced; `cancelled-tests`; `benchmarks`, a nonzero libtest `measured`
  count. A line longer than 16,384 characters is known by its beginning: it
  is a summary line when it lies inside a Node summary or begins as a
  summary line does, and any other is passed over like any other line of
  the log. A later complete summary does not take a reason back. Complete
  summaries found beside an incomplete one still count in `runners`, and
  never stand for the stage. An unknown count by itself fails nothing: the
  stage's state is still its exit; an unreadable log does not fail the run.
  The failed tests of every complete summary whose failed count reads are
  kept, whatever its other numbers say, so a stage whose count is unknown
  still fails on them, as when a summary counts both cancelled and failed
  tests. It is the reader that asks every test stage for a
  complete count. The log is read in bounded memory. Node's default reporter
  prints no line when a runner starts, so a Node runner that ended without a
  summary is visible only through its exit.
- `filtered=yes` means a part of the tests was selected: a runner reported
  filtered-out tests, or the stage's own command line names a selection. For
  `cargo test` that is a test name, a target or package option (`--lib`,
  `--bin`, `--test`, `--doc`, `-p` and the like, `-pNAME` included, and
  `--all-targets`, which runs every target but not the doctests),
  `--ignored`, `--skip` or `--exact`; for `node --test` a test file,
  `--test-name-pattern`, `--test-skip-pattern`, `--test-only` or
  `--test-shard`. Other options, with their values, change how tests run and
  not which. The command line is known before a stage runs, so a skipped
  stage says it too. A selection made through the environment, such as
  `NODE_OPTIONS`, is not visible. Every test stage of a full run is
  `filtered=yes`, and the stage list says so itself, whether or not the stage
  ran: `rust` because its three scale phases select tests by name, the four
  Node stages because each names its test files. `scope=full` says the
  prescribed stage list ran, not that no runner filtered.
- `overall` covers validation only: the stages and the input fingerprint check.
  A failed completion notification changes the process's exit status, not the
  record. `reason` is given when the run did not pass: `stage-failed`,
  `stage-interrupted`, `preflight-failed`, `spawn-failed`, `input-changed`,
  `fingerprint-check-failed`, or `launcher-error` for any other refusal. A
  prerequisite probe that fails or cannot be started, and a stage's command
  that cannot be found, are `preflight-failed`: every command is looked up
  before any stage runs, so every stage is `skipped`. `spawn-failed` is a
  command that was found and that the system refused to start; its stage is
  `failed`.

The record carries no source revision; the launcher's input fingerprint is its
own drift check and is not a host's source revision. A killed launcher prints
no record. Neither does a run whose id or whose stage name is not a token of
the grammar; the command line cannot produce one, only a caller of the
module. Each stage's kind and selection, the scope, and whether to notify
are the ones read before the first stage ran; a stage that rewrites or
breaks `request.json` changes neither what the record says of it nor
whether the record is printed. The notification's target is read when it
is sent. A stage that removes the run directory or makes `results.json`
unwritable leaves the run without saved results, like any unwritable
results directory, and then no record is printed: a host reads its absence
as unknown. The lines are text that any command
could print, so they are not evidence by themselves: a host may treat them
as such only for a command it recognised as this launcher at its path
inside the checked root, from the output of that process, and it records
test evidence under its own name. TermAl's reader counts a run as passed
tests only when `overall=passed`, no stage is `failed` or `interrupted`,
every `test` stage is `passed` with a numeric `executed` and `failed=0`, and
at least one of them has `executed` of one or more. Any `reason` means the
run did not pass, and any `why` that there is no count. A record that does
not parse means unknown.

Run evidence is retained until explicitly removed by the operator; there is no
automatic pruning. After a run has terminal results, its outcome has been
recovered and recorded, and any pending notification or review use is resolved,
the operator may archive or delete that exact run directory if its full logs
are no longer needed. Preserve referenced evidence before removal. Never delete
running, unrecovered, or still-needed runs; inspect their state first. Deleting
a run also removes its notification-retry data. Do not remove the whole
`review-runs` directory as a shortcut.

### Test temporary files

Rust and Node fixtures live in this repository's own `target/tmp/engram`,
never in the operating system's Temp folder: test homes stay inside the
repository. Node derives the repository from the location of
`scripts/test-temp.mjs` and Rust from `CARGO_MANIFEST_DIR`, so each checkout or
worktree keeps its own fixtures, whatever `CARGO_TARGET_DIR` says. A launcher
chooses the run root once; Rust accepts that root only when it is this
repository's own `target/tmp/engram` child. Every run owns a unique
`run-<pid>-<random>` subtree beneath it. Rust tests use the shared
`test_support::temp_home` guard; Node gates use `scripts/test-temp.mjs`. Each
owns and removes only the unique fixture it created. Rust launchers pass their
owned `ENGRAM_TEST_RUN_ROOT` to child test processes; direct Cargo tests choose
a process-unique run root and remove it when the last fixture closes. Close
database handles and child processes first. Struct fields drop in declaration
order, so a fixture directory must follow its store fields.

Every recursive delete in test support goes through one guard per runtime,
`test_support::remove_fixture_dir` and `removeFixturePath`. The guard is
anchored to this repository's `target/tmp/engram`, whatever root its caller
passes. Before deleting anything it refuses a root that does not resolve below
that anchor, a path that is not strictly below its root (compared by path
component, so `root-evil` is not below `root`), and any symlink or junction on
the way from `target/` down to the path, including one above the root. It
checks again before every attempt; what remains is the moment between the last
check and the delete itself. Neither guard creates anything before checking
that `target/` and `target/tmp` are not links. The final removal of an empty
run directory is not recursive and is checked the same way: the run directory
must be a direct child of the anchor, and neither it nor any step from
`target/` down may be a link. After a refused fixture removal, Rust leaves the
run directory alone. On Windows a test keeps each
runtime's run root short enough that the deepest SQLite files stay under the
260-character path limit. Git run by Node tests, or by a run launched through
`scripts/test-temp.mjs`, stops at the run root (`GIT_CEILING_DIRECTORIES`), so
a fixture without its own repository never resolves to this checkout; a direct
`cargo test` sets no ceiling, and no Rust test runs Git.
Each guard retries transient removal failures within a bounded budget: Rust
waits 10, 25, 50 and 100 ms (185 ms), and Node retries `EBUSY`, `EMFILE`,
`ENFILE`, `ENOTEMPTY` and `EPERM` up to five times, waiting 25 to 125 ms
(375 ms). A refusal is not retried. Each reports the path and OS error if
cleanup still fails. This does
not close another owner's SQLite handle or excuse a broken lifetime.
The open-handle and field-order regressions prove the sharing-violation cause
on Windows only; POSIX permits removal while a SQLite handle is still open.

Both Rust launchers audit their owned run subtree before and after each test
phase, including failed commands. Each Node fixture gate performs the same
audit. It prints counts and requires that run's subtree to be empty, then
removes the empty run directory. A failure lists exact remaining names but
does not sweep them. Sibling runs and any other entries under
`target/tmp/engram` are never counted or removed: concurrent
creation/deletion cannot mask this run's
residue or fail another run's audit. There is no age-based sweep. Fingerprint
test repositories use this same fixture ownership, including cleanup when
repository initialization fails.

Build artifacts are separate: leave `CARGO_TARGET_DIR` unset to use the
worktree's `target`, or select another non-Temp build directory. Never put a
dogfood build target under user Temp. Use the repository's stable toolchain;
an inherited `RUSTUP_TOOLCHAIN` can override `rust-toolchain.toml`.

Fixtures from builds before this layout may remain under the system Temp
folder's `engram` directory, and a checkout still on such a build keeps adding
them. No gate cleans them, and agents never delete outside the repository. A
person may delete an old `run-*` directory there by hand once no test run uses
it. This guide deliberately gives no command for that: a pasted directory name
is parsed as code, and path checks can be led out of the folder. Builds from
before that `engram` directory left unnamed `.tmp*` folders directly in Temp;
they cannot be told apart safely from other programs' folders, so leave them.

After updating to a build that adds a rebuildable projection, an existing
development store can refuse until `engram doctor --repair-projections` is run
once. The Cut A gate lookup adds the rebuildable
`objects_work_evidence_gate_name` expression index, and Cut B adds the
rebuildable `objects_project_memory_key` expression index plus its advisory
advertisement table. Project-memory revisions make that lookup nonunique and
add the rebuildable unique `objects_project_memory_root` index, reserving each
key only at its canonical root. Repairing them does not rewrite canonical objects.
The advertisement table is discardable delivery bookkeeping rather than
canonical memory state: repair drops its acknowledgements, so each session may
receive one harmless content-free memory-count reannouncement afterward, and a
session whose host supplies a context generation is directed once more to
list its memories.

Every gate must pass. A failure is investigated and classified as a product,
test, or environment defect; it is never normalized by retrying until green.

The claim-mutation scale test asserts maximum canonical, work-event, and item
decode budgets per operation. Those counters bound Engram's canonical and
projection materialization work and remain stable under foreign host load.
The test prints p95 wall-clock latency as diagnostic evidence but does not
assert it; the separate `work_next` scale test also asserts its decode and
response-size budgets.
These deterministic counters do not bound every possible SQLite query-plan
regression; p95 remains visible until a portable SQL-work counter can replace
that remaining diagnostic gap.
This contention-robust contract is recorded in the
[roadmap](roadmap.md#v1--close-the-loop).

Documentation-only changes: verify all relative links resolve and that docs
stay consistent with the [specification](spec.md) — the spec is normative;
briefs explain.

## Git policy

Conservative by default: **no commits or pushes without explicit
authorization**, which is Greg's word or, for commit, push and install, the
standing approval in [AGENTS.md](../AGENTS.md#authority-and-git). Without
either, at handoff report changed files, validation performed, and proposed
next commands, then wait.

## Review cadence

Changes are reviewed through the delegated review workflow (two independent
review passes from different vendors, Codex and Claude, with Kimi standing in
for an unavailable Codex, over staged, unstaged, and untracked changes)
before any commit is proposed. Review findings that warrant follow-up work
become Engram items, not inline TODOs.

## Documentation conventions

- The [specification](spec.md) is normative. Feature briefs under
  [`docs/features/`](features/README.md) explain single pillars and defer to
  the spec on conflict.
- Cross-link documents both ways when one references another.
- Never put work refs in source-code comments, identifiers, or user-facing
  copy — code comments explain the invariant in self-contained language.
  Work refs live in Engram, commits, and review notes.
- Deferred capabilities (see [roadmap](roadmap.md)) are documented where they
  belong and clearly marked deferred — never silently omitted, never
  presented as shipping.

## Terminology

Use the spec's vocabulary consistently: *memory* (identity), *version*
(immutable record), *packet* (delivery unit), *task* (local operational
unit), *work claim* (fenced execution responsibility), *cursor* (ordered task-feed
position), *contribution* (one participant's finalization input), *report*
(frozen publication artifact), *receipt* (adapter's durable publication
acknowledgment). Don't introduce synonyms.
