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

Run `node scripts/test-launcher.mjs full` before any commit prompt. It preserves
these gates in order:

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
bounded test concurrency, then separate ignored claim-mutation and
`root_delta_scale_` phases. The shell entry point also raises the Unix
file-descriptor soft limit when the host permits it; that step is not
applicable on Windows.

The root-delta phase includes 1,000-step history fixtures and can take several
minutes. Recent Windows debug runs took about 6–9 minutes for that phase;
this is an observation, not a timeout or a performance limit. The test harness
can print "running for over 60 seconds" while a fixture is still working.
Measurements appear when each fixture reaches its reporting point, not as a
periodic heartbeat. Do not stop the gate just because it is quiet or passes
that warning threshold. Check process activity and possible lock contention
when investigating a suspected hang. The root-delta checks assert byte and
operation bounds; elapsed time is diagnostic only.

The ordinary Rust suite includes `tests/source_file_size.rs`, which keeps each
guarded source family at 2,499 physical lines or fewer: a module file and every
`.rs` file under its child-module directory, so a module split out of a guarded
file stays counted. That directory is `MODULE/` unless the family names another
one, as `src/main.rs` does with `src/bin_support`, where the binary's modules
live. Blank and comment lines count, and CRLF counts like LF. A
missing module file, a missing child directory of a family marked as split, or
any file or directory that cannot be read fails the check. When a file is
brought under the limit, add its family to the `guarded_families!` list there
rather than writing another checker. The list also generates one test per
family, `family::NAME`.

For per-file evidence, such as a host-observed check behind a file-size
criterion, run the one family the criterion covers, for example:

```bash
cargo test --test source_file_size -- --exact family::storage_work_query --nocapture
```

It prints one line per file of that family, in path order, as
`PATH: N physical lines (limit 2499)`, and fails naming every file of the
family over the limit. A family's report must stay within 3 KiB, so a
host-observed run of it fits the 4096-byte verification summary with the
command and result lines. A family that outgrows that budget fails its test,
naming its printed size. Its children cannot simply be listed as families of
their own, because the inventory refuses a file guarded by two families; that
failure means the family description must first learn to divide one module's
files. For the whole inventory, sorted by path across all families, run the
whole-tree test alone:

```bash
cargo test --test source_file_size -- --exact guarded_source_families_stay_within_the_limit --nocapture
```

Running the file without `--exact` runs every test, so each family's lines
print again from its own test, and parallel test threads interleave those
lines; add `--test-threads=1` when reading that combined output.

### Test launcher

One entrypoint handles full validation and authorized focused checks:

```bash
node scripts/test-launcher.mjs full
node scripts/test-launcher.mjs focused -- node --test scripts/test-launcher.test.mjs
node scripts/test-launcher.mjs focused -- cargo test --lib control_runtime
```

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
warnings and bounded failures reach context; truncation points to the full log.
Filtering does not decide success: actual process exits do. A failed command
stops subsequent stages, which stay explicitly unrun. Exceptions fail the run;
a killed process without terminal results remains unknown, never a pass.
The snapshot checks are boundary checks, not proof against transient edits.
Normalization limits are emitted in freeze CLI stderr and every run's
results/summary; fingerprint stdout remains unchanged.
Tracked content is compared through Git's normalized diff, not raw filesystem
bytes: line-ending-only changes (or changes erased by clean filters) can be
invisible on any platform. Known intervening edits still invalidate a run.
The Windows executable-mode/symlink limitation is separate. Artifacts use
owner-only POSIX modes (0700 run directory, 0600 files); Windows uses its ACLs.

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
cannot guarantee completion delivery; missing terminal results remain unknown.
`execution.lock` prevents a second execution of the same run directory, not
separate launches; the caller owns repository-level serialization. After a
crash, recover its artifacts and classify
the failure before choosing a new run. Diagnose actual failures and repair them
within the authorized scope, then validate the changed input; never retry blindly.

Run evidence is retained until explicitly removed by the operator; there is no
automatic pruning. After a run has terminal results, its outcome has been
recovered and recorded, and any pending notification or review use is resolved,
the operator may archive or delete that exact run directory if its full logs
are no longer needed. Preserve referenced evidence before removal. Never delete
running, unrecovered, or still-needed runs; inspect their state first. Deleting
a run also removes its notification-retry data. Do not remove the whole
`review-runs` directory as a shortcut.

### Test temporary files

Rust and Node fixtures use the operating system's Temp directory with an
`engram` child. A launcher chooses the run root once; Rust accepts that
validated root without independently resolving Temp again (the runtimes can
use different environment-variable precedence or platform fallbacks).
Every run owns a unique `run-<pid>-<random>` subtree beneath it. Rust
tests use the shared `test_support::temp_home` guard; Node gates use
`scripts/test-temp.mjs`. Each owns and removes only the unique fixture it
created. Rust launchers pass their owned `ENGRAM_TEST_RUN_ROOT` to child test
processes; direct Cargo tests choose a process-unique run root and remove it
when the last fixture closes. Close database handles and child processes
first. Struct fields drop
in declaration order, so a fixture directory must follow its store fields.
The guard retries transient removal failures for a bounded 185 ms backoff
budget and reports the path and OS error if cleanup still fails. This does
not close another owner's SQLite handle or excuse a broken lifetime.
The open-handle and field-order regressions prove the sharing-violation cause
on Windows only; POSIX permits removal while a SQLite handle is still open.

Both Rust launchers audit their owned run subtree before and after each test
phase, including failed commands. Each Node fixture gate performs the same
audit. It prints counts and requires that run's subtree to be empty, then
removes the empty run directory. A failure lists exact remaining names but
does not sweep them. Sibling runs and unrelated user Temp entries are never
counted or removed: concurrent creation/deletion cannot mask this run's
residue or fail another run's audit. There is no age-based sweep. Fingerprint
test repositories use this same fixture ownership, including cleanup when
repository initialization fails.

Build artifacts are separate: leave `CARGO_TARGET_DIR` unset to use the
worktree's `target`, or select another non-Temp build directory. Never put a
dogfood build target under user Temp. Use the repository's stable toolchain;
an inherited `RUSTUP_TOOLCHAIN` can override `rust-toolchain.toml`.

Existing legacy Temp leftovers are not cleaned by any gate. After stopping
tests and inspecting a specific old fixture directory, a user may remove
that exact directory with the following one-line PowerShell command (replace
the placeholder with the inspected basename; do not target Temp itself):

```powershell
Remove-Item -LiteralPath "$env:TEMP\<inspected-fixture-directory>" -Recurse -Force
```

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
receive one harmless content-free memory-count reannouncement afterward.

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
authorization.** At handoff, report changed files, validation performed, and
proposed next commands, then wait.

## Review cadence

Changes are reviewed through the delegated review workflow (one Codex and one
Claude review pass over staged, unstaged, and untracked changes) before any
commit is proposed. Review findings that warrant follow-up work become Engram
items, not inline TODOs.

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
