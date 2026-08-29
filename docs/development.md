# Development & Review Workflow

How work happens in this repository. Agent-facing operational rules live in
`CLAUDE.md` / `AGENTS.md` (owned by the tooling scaffold); this document is
the human-readable overview and the documentation conventions.

## Task tracking

This project uses **beads** (`bd`) for all task tracking — run `bd prime` for
the full workflow context.

- `bd ready` — find available work; `bd show <id>` — detail;
  `bd update <id> --claim` — claim before starting.
- Implementers claim beads, work them, and leave completed implementation
  beads **in_progress with a completion/validation comment**. Final `bd
  close` belongs to Greg.
- No TODO lists in markdown; no ad hoc memory files — durable insights go
  through `bd remember`.

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

Every gate must pass. A failure is investigated and classified as a product,
test, or environment defect; it is never normalized by retrying until green.

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
commit is proposed. Review findings that warrant follow-up work become beads,
not inline TODOs.

## Documentation conventions

- The [specification](spec.md) is normative. Feature briefs under
  [`docs/features/`](features/README.md) explain single pillars and defer to
  the spec on conflict.
- Cross-link documents both ways when one references another.
- Never put beads task IDs in source-code comments, identifiers, or
  user-facing copy — code comments explain the invariant in self-contained
  language. Task IDs live in beads, commits, and review notes.
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
