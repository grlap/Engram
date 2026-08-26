# Roadmap

Normative source: [spec §11–12](spec.md#11-delivery-plan). This page tracks
the phases and — just as deliberately — what is deferred and what would
trigger revisiting it.

## V1 — close the loop

Everything in V1 serves one loop: **bind a backlog item → coordinate local
sessions under shared task memory → assemble and freeze one report → publish
it → review promotion candidates.**

- Rust core; local SQLite canonical store (append-only, content-addressed)
  with stable project identity, WAL multi-process access, ordered task events,
  and derived FTS5 tables — [SQLite store](features/sqlite-store.md)
- Same-host multi-session tasks: atomic claim leases, explicit handoff,
  task-shared memory, participant contributions, and a finalization barrier
- Context packets: budgets, fail-closed pinned tier, omission manifest,
  packet hash + explain, event cursor + peer delta, and review counts —
  [context packets](features/context-packets.md)
- `engram note` / `memory_note`: prose-first capture with inspectable inferred
  defaults; one write feeds peer and report views
- Write policy matrix, proposal/approval, review queue,
  supersede/contradict/contested, tombstones —
  [write policy & review](features/write-policy-and-review.md)
- Local tasks: deterministic report assembly, polish/freeze state machine,
  `DummyTrackerAdapter` publication under idempotent receipts —
  [local tasks & reports](features/local-tasks-and-reports.md),
  [tracker adapter](features/tracker-adapter.md)
- Audit attribution at asserted-runtime-context assurance; visibly labeled
  no-op Redactor — [security & trust](features/security-and-trust.md)
- CLI + MCP over one core — [CLI & MCP](features/cli-and-mcp.md)
- Safe JSONL export + local backup; fixture-level retrieval checks;
  `doctor` / `rebuild-index`

## V1.x — improve the loop

- Session-end distillation into working memory (proposer + dedup)
- Episodic compaction automation
- Post-publication retention compaction
- Budget tuning from retrieval decision logs

## V2+ — widen the loop

- Proprietary tracker adapter (real publication)
- Git object-store team backend with org/team scopes
  ([design preserved](spec.md#32-deferred-git-object-store-for-team-sync))
- Optional embeddings for retrieval
- Wider outbound ticketing: comments, link-backs, webhook checkpoints
- Real Redactor/DLP integration
- Postgres/service `Store` backend behind the same ports
- Signer-based attestation; envelope encryption for crypto-shredding

## Deferred — and what would revive each

| Deferred capability | Revisit when |
| --- | --- |
| Git team-sync backend | Coordination must cross machines; same-host peers are V1 |
| Proprietary tracker adapter | Work authorizes real publication |
| Real DLP/redaction backend | A tool is mandated, or memory starts holding sensitive material |
| SSO/LDAP identity | Compliance-grade attribution becomes a deployment promise |
| Embeddings | FTS5 + good titles measurably stop being enough (per the evaluation harness) |
| Service backend / Signer / envelope encryption | Team scale or compliance posture demands them |

## Open decisions

- Default grace period for post-publication retention — pick during V1
  implementation.
- See [spec §12](spec.md#12-decisions) for the resolved decision record.
