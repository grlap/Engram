# Historical Context Packet Format

> Reference: [spec §4](../spec.md#4-memory-retrieval).
> Related briefs: [typed memory model](typed-memory-model.md),
> [CLI & MCP](cli-and-mcp.md), and
> [behavioral control plane](behavioral-control-plane.md).

Context-packet construction and delivery have been removed. No CLI, MCP, or
host operation builds a packet. Agents recover work through `next` and read
project notes through `memories`; see [agent usage](cli-and-mcp.md#using-engram-as-an-agent).

`ContextPacket` and its nested header, item, omission, and payload types remain
serialized history. Stored `ControlDelivery` records may contain a packet;
their readers and doctor audit checks remain supported without constructing
or delivering new packets. Packet fields, stored record ids, and links retain
their original meaning. Dormant context-revision tables remain in the store;
this removal changes no durable rows or schema.

Task-, work-, agent-, and project-scoped historical memories share the
`memory_version` and `memory_assertion_event` object kinds. Scope identifies
their visibility; an object kind does not identify a task memory. Historical
decoding, projection repair, and graph restore preserve that distinction.
The retired generic capture/search helpers exist only as test fixtures.
Live project memory (`remember`, `memories`, `forget`) and work-memory
retrieval remain available.
