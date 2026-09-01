# Development & Review Workflow

How work happens in this repository. Agent-facing operational rules live in
`CLAUDE.md` / `AGENTS.md` (owned by the tooling scaffold); this document is
the human-readable overview and the documentation conventions.

## Task tracking

This project tracks its work in Engram — the nine agent words, documented in
[CLI & MCP](features/cli-and-mcp.md#using-engram-as-an-agent).

- `engram work next` — what you hold, what is ready, what others changed;
  `engram work show REF` — detail; `engram work claim REF` — claim before
  changing anything.
- Implementers `note` decisions and validation evidence once and `done` the
  item they hold; `done` says what is still owed instead of closing anyway.
- No TODO lists in markdown; no ad hoc memory files — durable rules live in
  the instruction files, and project memories arrive with the planned
  `remember` word.

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

On Windows, run `pwsh -NoProfile -File scripts/test-rust.ps1` in place of
`scripts/test-rust.sh`. Both entry points run the ordinary Rust suite with
bounded test concurrency and then the ignored claim-mutation scale test with
one thread. The shell entry point also raises the Unix file-descriptor soft
limit when the host permits it; that step is not applicable on Windows.

Every gate must pass. A failure is investigated and classified as a product,
test, or environment defect; it is never normalized by retrying until green.

The claim-mutation scale test today asserts absolute p95 latencies plus a
canonical-decode maximum per operation; work-event and item decode counts are
printed for the record, not asserted (the separate `work_next` scale test
asserts those counts without timing). On a shared host another agent's build
can push the latency assertion over its budget; such a failure is classified
as an environment defect only with observed contention evidence (the foreign
process named) and a quiet-host rerun that passes. A failure that repeats on
a quiet host is a product defect. The evidence-backed quiet rerun is a
classification step, not a retry-until-green exception, and the contention
evidence plus the rerun result are recorded as a durable note on the
affected work item.

Planned: a contention-robust scale gate replaces the absolute wall-clock
assertion so the test measures work, not the host's mood; the concrete
design is decided at implementation
([roadmap](roadmap.md#v1--close-the-loop)). The classification rule above
is the discipline until it lands.

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
