# Shipped Today

This page inventories only behavior present in the installed build. It excludes
planned, target, and deferred capabilities; those belong in the
[roadmap](roadmap.md), not in this table.

| Surface | Installed behavior | Contract |
| --- | --- | --- |
| Agent words | The CLI ships thirteen work words (`next`, `ls`, `show`, `add`, `claim`, `update`, `gate`, `note`, `done`, `handoff`, `remember`, `memories`, and `forget`), and MCP exposes the same words plus `search`. | [Using Engram as an agent](features/cli-and-mcp.md#using-engram-as-an-agent) |
| Planning words | `add --under REF [--optional]` creates required or optional children, while `update REF` exposes `--after OTHER`, `--drop-after OTHER`, `--waive CHILD --reason`, `--supersede-with NEW --reason`, `--cancel REASON`, `--blocked WHY`, and `--unblock`. | [Graph invariants](features/local-work-system.md#graph-invariants) |
| Structured receipts | `--json` on any word prints its structured receipt, a successful mutation under a process-defaulted session adds `effective_session_id`, and `show` or `next` reports typed count and byte-budget omissions such as `children_omitted` and `notes_omitted` instead of silently truncating. | [Using Engram as an agent](features/cli-and-mcp.md#using-engram-as-an-agent) |
| Runtime binding | `ENGRAM_HOME` selects host-local data, `ENGRAM_ACTOR_ID` and `ENGRAM_SESSION_ID` supply asserted principals, optional `ENGRAM_ACTOR_CONTEXT` adds bounded attribution, generated process-default ids have seven-day reuse, and each newly created session reclaims at most 64 inactive predecessors. | [Host configuration](features/cli-and-mcp.md#host-configuration) |
| TermAl tiers | The base TermAl tier injects the agent words and remains advisory, while a `turn_gated` tier must use the host-private control channel and withhold every prompt until Engram grants and begins the turn. | [Control assurance](features/behavioral-control-plane.md#control-assurance) |
| Canonical store | One host-local SQLite database per stable project id is the canonical source of truth shared by concurrent sessions and worktrees. | [SQLite store](features/sqlite-store.md) |
| Store version | The prerelease store uses schema marker 1, and ordinary open refuses an established store written by a different build before mutation. | [Canonical-bytes contract](features/sqlite-store.md#canonical-bytes-contract) |
| Projection repair | `engram doctor --repair-projections` explicitly rebuilds only declared indexes, triggers, and FTS from verified durable rows, never missing durable state or canonical objects. | [Rebuildable and durable projections](features/sqlite-store.md#rebuildable-and-durable-projections) |
| Project memories | `remember`, `memories`, and `forget` provide attributed project notes with permanent safe keys, on-demand full reads, and terminal tombstones. | [Gates, prerequisites, supersession, and project memories](features/local-work-system.md#gates-prerequisites-supersession-and-project-memories) |
| Work evidence | On open work the holder uses `note` and `gate`; after completion any project-bound session may append either as an attributed late finding after the frozen seal cut, without reopening, resealing, or adding a completion barrier. | [Gates, prerequisites, supersession, and project memories](features/local-work-system.md#gates-prerequisites-supersession-and-project-memories) |
| Handoff | `handoff` ships offer, accept, and cancel actions backed by the current claim and checkpoint-coupled transfer rules. | [Using Engram as an agent](features/cli-and-mcp.md#using-engram-as-an-agent) |
