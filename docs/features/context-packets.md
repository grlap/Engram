# Historical Context Packet Format

> Reference: [spec §4](../spec.md#4-memory-retrieval).
> Related briefs: [typed memory model](typed-memory-model.md),
> [CLI & MCP](cli-and-mcp.md), and
> [Execution Observability](execution-observability.md).

Context-packet construction and delivery have been removed. No CLI, MCP, or
host operation builds a packet. Agents recover work through `next` and read
project notes through `memories`; see [agent usage](cli-and-mcp.md#using-engram-as-an-agent).

`ContextPacket` and its nested header, item, omission, and payload types
remain the historical serialized format, and packet fields retain their
original meaning. A packet was stored only inside a turn grant's
`ControlDelivery` page. That type was deleted with turn gating, and the store
migration that removed turn gating retires the tables that held those grants,
by name with their counts (see [SQLite store](sqlite-store.md)); their rows
remain only in the store file kept as that migration's backup. Dormant
context-revision tables remain in the store.

Task-, work-, agent-, and project-scoped historical memories share the
`memory_version` and `memory_assertion_event` object kinds. Scope identifies
their visibility; an object kind does not identify a task memory. Historical
decoding, projection repair, and graph restore preserve that distinction.
The retired generic capture/search helpers exist only as test fixtures.
Live project memory (`remember`, `memories`, `forget`) and work-memory
retrieval remain available.
