# Development & Review Workflow

How work happens in this repository. Agent-facing operational rules live in
`CLAUDE.md` / `AGENTS.md` (owned by the tooling scaffold); this document is
the human-readable overview and the documentation conventions.

## Task tracking

This project tracks its work in Engram — the thirteen agent words, documented in
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
  words. Once the exporter ships, the designed
  [work-graph snapshot](features/work-graph-snapshot.md) will replace that
  with `graph save` on the old build and `graph load` on the new one for
  stores written by a build that has it, whenever the two builds share the
  snapshot format fingerprint; a format change or an older store keeps the
  manual path.
  Either way the file carries no control policy: `engram init` on the new
  build repeats the project's `--required-assurance … --authorized-by …
  --reason …` bootstrap and any obligation rule set is re-applied by hand.
- Every `HostControlRequest` variant is strict: the paired TermAl consumer must
  send exactly the current field set for every operation, with no additive or
  legacy fields. The no-agent-grants build is paired with the coordinated
  TermAl update that also removes the former
  `obligation_waive.authority_grant` field; do not land either side alone and
  do not add a legacy-frame compatibility shim.

## Quality gates

Run before any commit prompt (once the Rust workspace exists):

```bash
cargo fmt --check
cargo check
cargo clippy --all-targets --all-features -- -D warnings
scripts/test-rust.sh
node --test scripts/review-freeze-fingerprint.test.mjs
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
bounded test concurrency and then the ignored claim-mutation scale test with
one thread. The shell entry point also raises the Unix file-descriptor soft
limit when the host permits it; that step is not applicable on Windows.

After updating to a build that adds a rebuildable projection, an existing
development store can refuse until `engram doctor --repair-projections` is run
once. The Cut A gate lookup adds the rebuildable
`objects_work_evidence_gate_name` expression index, and Cut B adds the
rebuildable `objects_project_memory_key` expression index plus its advisory
advertisement table. Repairing them does not rewrite canonical objects.
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
unit), *lease* (exclusive expiring claim), *cursor* (ordered task-feed
position), *contribution* (one participant's finalization input), *report*
(frozen publication artifact), *receipt* (adapter's durable publication
acknowledgment). Don't introduce synonyms.
