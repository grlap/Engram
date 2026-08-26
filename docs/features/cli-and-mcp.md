# CLI & MCP Surface

> Normative reference: [spec §8](../spec.md#8-interfaces). Related briefs:
> [context packets](context-packets.md),
> [local tasks & reports](local-tasks-and-reports.md).

One core library owns classification, object storage, scope authorization,
task binding, deltas, and context-packet construction. The CLI and MCP server
are thin faces over it; transport code does not redefine memory policy.

## Shipped CLI

```bash
export ENGRAM_HOME=/absolute/host-local/path
engram init
engram doctor
engram mcp \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo
```

`--project-file` defaults to the tracked `.engram-project`. Its stable project
identity resolves to the same opaque SQLite path for every worktree and
session on the host. `doctor` verifies every canonical object and visibly
warns that V1's development no-op redactor provides no secret or PII
protection.

The broad administrative CLI in the specification is planned, not shipped in
this slice. Daily coding-agent work uses MCP so capture does not require shell
bookkeeping.

## Shipped MCP tools

| Tool | Purpose |
| --- | --- |
| `task_start` | Start or ref-idempotently bind a local task |
| `task_join` | Join the same task using only its external reference |
| `memory_note` | Capture prose once with inferred classification and a retry key |
| `memory_contradict` | Link two visible shared version hashes as explicitly incompatible |
| `memory_context` | Build a bounded packet for the session's active task |
| `memory_delta` | Return only task changes after a processed cursor |
| `memory_search` | Full-text search visible memory |
| `memory_show` | Verify and inspect an authorized memory by the receipt's version hash |
| `context_explain` | Reproduce the inclusion and omission reasons for a packet |
| `task_claim` | Acquire an expiring, safely retryable task lease with typed conflict details |

Every result is structured JSON. Expected execution failures are MCP tool
errors with stable error codes and details—for example, a live claim conflict
returns `task_claim_held` with holder plus epoch and ISO-8601 expiry. Repeating
a claim with the same key and TTL returns the original lease even though
wall-clock time advanced; changing the TTL under that key is an idempotency
conflict. A private memory hash does not become a capability: `memory_show`
repeats project/task/participant/owner authorization and returns
`memory_access_denied` to a peer.

## Normal agent loop

1. The first session calls `task_start` with the external tracker reference;
   peers call `task_join` with that reference alone. Engram persists each
   session's active binding, including across MCP process restart.
2. Call `memory_context` once and retain its `event_cursor`. Inject the pinned
   and index tiers into the working context.
3. Call `memory_note` when a decision, constraint, finding, evidence pointer,
   or handoff fact becomes worth sharing. Natural-language prefixes such as
   `Decided:` help classification but flags are not required. Natural rules
   beginning with `Never`, `Always`, `Must`, `Do not`, or `Only` become firm
   pinned constraints without flags. The receipt
   returns the inferred fields, their basis, canonical hashes, cursor, and
   idempotency key.
4. Before the next work turn or after a host wake, call `memory_delta` with the
   retained cursor. Advance it only after processing the returned changes.
5. Use `private: true` only for incomplete scratch. It remains searchable and
   inspectable by the owning agent but never enters peer packets or deltas.
6. When two shared records cannot both guide action, call
   `memory_contradict` with their version hashes and a reason. Both become
   visibly contested. If both are applicable firm/hard pinned records,
   `memory_context` fails with `pinned_contradiction` and names the edge and
   versions instead of asking the agent to invent precedence.

One capture powers peer context and the ordered feed. The host may use a
mailbox as a doorbell, but must not relay full state or make the agent repeat
the same fact into another status ledger.

## Host configuration

Build an executable and configure one stdio MCP process per agent session:

```json
{
  "mcpServers": {
    "engram": {
      "command": "/absolute/path/to/engram",
      "args": [
        "--project-file",
        "/absolute/project/.engram-project",
        "--home",
        "/absolute/host-local/engram-data",
        "mcp",
        "--actor-id",
        "codex",
        "--session-id",
        "replace-with-this-runtime-session-id",
        "--source-skill",
        "engram-repo"
      ]
    }
  }
}
```

The proprietary runtime supplies actor/session/tool/skill instruction context.
V1 records it with `asserted` assurance; configuration text is not
authentication. Distinct concurrent sessions need distinct `--session-id`
values. The database is shared; the MCP processes are not.

## Dogfood contract

`scripts/mcp-dogfood.test.mjs` launches two real stdio MCP processes against a
fresh home and checks ref-only rendezvous, flag-free classification, context,
single-item delta, private-scope non-disclosure (including a raw-hash probe),
restart durability, provenance/explain, idempotent retry/conflict, and lease
contention/expiry/retry. It also verifies zero-flag natural constraint
classification and explicit pinned-contradiction fail-closed behavior. It is
part of `scripts/check.sh`.

Report finalization/publication, lease renewal/handoff/release, review actions,
history, explicit contradiction resolution, and the complete administrative
CLI remain planned surfaces. They must reuse this core rather than fork its
semantics.
