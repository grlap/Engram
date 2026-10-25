# Bounded operational diagnostics

The shipped `engram mcp` and `engram control` processes retain operational
diagnostics by default under the explicitly selected Engram home, in
`diagnostics/engram-0.jsonl` through `engram-3.jsonl`. These logs are separate
from SQLite and canonical memory. They do not change admission, receipts,
stdout, response bytes or store configuration.
Read-only MCP mode also writes these operational logs and its exit summary;
its read-only guarantee applies to store mutations, not diagnostic files.

MCP records observed tool/request errors, failed response delivery, requested
cancellation, unfinished tracked calls, and calls whose server duration from
handler start through response delivery exceeds 2,000 milliseconds. Durations
separate handler, dispatch wait and send where those boundaries were observed;
handler duration is not SQLite lock-wait duration. Successful fast calls do
not produce records. Control records actual typed refusals, including refusals
inside successful envelopes, and observed read/parse/write failures. Read
failure before a frame has no frame sequence. Errors enter
the bounded queue before response delivery is awaited.

rmcp transport receive termination starts a separate ten-second diagnostic clock.
It includes physical EOF and dependency-swallowed read or protocol-reply failures;
the observer cannot distinguish those causes. If
shutdown is still open at that threshold, one `shutdown_overrun` record names
its last observed milestone. This observer does not impose a close deadline,
cancel work, or change the close result. The MCP close check uses a 30,000-
millisecond deadline; the separate per-call sanity check retains its
10,000-millisecond deadline.
A late timer is checked again at service completion before closing the clock.
A host that terminates the process at ten seconds or earlier may pre-empt the
record; forced termination does not guarantee retention.

`ENGRAM_MCP_PHASE_TRACE=1` still adds the existing opt-in stderr phase and
shutdown traces, including detailed store timing. Default diagnostics use
cheap front-door timing and do not install SQLite statement profiling.
See [CLI/MCP tracing](cli-and-mcp.md#host-and-operator-cli).

For MCP verb refusals, the record's `code` is the code received by the client,
such as `work_invalid` or `engram_store_error`. Native store failures retain
their fixed reason and `sqlite_code`: inspect that SQLite extended code to
distinguish busy from locked failures, and the `writer_admission` block for
writer admission classification. Control-surface records keep their existing
native codes, including `sqlite_busy`, `sqlite_locked` and
`writer_admission_refused`.

A typed writer-admission refusal retains the native admission reason,
elapsed wait, budget and SQLite primary/extended codes in `writer_admission`.
These timings describe admission for that transaction; they are distinct from
the surrounding handler duration. The refusal proves that transaction was
not started, while earlier transactions of the same word may have committed.
No manual retry or attempts count is invented, and SQLite error text is excluded.

## Correlation and privacy

Each JSON line contains a version marker, producer build fingerprint and
source revision, process id, generated local instance UUID, surface, event
observation time, fixed operation/code/reason, applicable timings and loss
counters. The generated instance UUID is local metadata; the host does not
receive it through an invented handshake.

Control `frame_seq` numbers input frames from one per process. A host can join
its actual process/frame evidence using pid, frame sequence and session.
Available syntactically valid grant UUIDs and idempotency comparison
fingerprints describe that observed frame. They do not establish authority
or prove that earlier transactions in the same word did not commit.

Only an opaque `session-N` handle with one to twenty decimal digits is retained
directly. Other admitted session strings use the lowercase full SHA-256 of
UTF-8 `engram-diagnostic-session-v1`, one zero byte, then the exact UTF-8 session
string. An available idempotency key uses the same construction with namespace
`engram-diagnostic-attempt-v1`. Inputs are neither trimmed nor normalized.
These fingerprints compare input content; they provide neither authenticated
identity nor anonymity.

Numeric MCP request ids and bounded local request sequences are server-local
correlation. Ordinary sequential reuse after response dispatch gets a new sequence.
Cancelled and simultaneous duplicate ids are ambiguous: rmcp may forward an old
response under a replacement's id. The observer quarantines up to 256 distinct
such keys for the connection's lifetime, never restoring precision merely because
a handler settled. Further handlers and responses for a quarantined key have no
sequence and carry `uncorrelated_reason: ambiguous_id`. An already-started
handler's original ticket still identifies its own late typed error.
Quarantine overflow increments `quarantine_overflow` once and stops all new
receive-derived correlation for that connection; events instead carry
`uncorrelated_reason: quarantine_overflow`. Tracking capacity loss has
`uncorrelated_reason: tracking_unavailable`. Tracking losses are counted;
uncorrelated typed handler faults and observable RPC/send errors remain readable.
Slow uncorrelated handlers report handler duration only; receive-to-send timing
is unavailable. String request ids
are not retained in records. TermAl runtime tool-call ids have no asserted
mapping to MCP wire ids. Raw actors, contexts, arbitrary identities or keys,
routing/connection/delivery tokens, SQL, paths, arguments, results, error text
and stored text are excluded. Reasons come from fixed allowlisted wording.

## Retention and losses

The home-wide ring owns four files of at most 1 MiB each and one fixed empty
`engram.lock` file. Each record is at most 4,096 bytes including its newline.
Each producer has a queue of at most 256 fixed events; request tracking is
also bounded. A dedicated writer performs serialization, destination creation,
nonblocking OS lock acquisition, append, rotation and flush. Cooperating
processes never interleave records. Owned destinations are opened without
following symbolic links or junctions; hard links are not refused. Unexpected
layouts are refused, and a bounded torn final
record is removed under the same lock before appending.
An invalid layout needs operator intervention: after diagnostic producers
have exited, retain the directory for inspection and move it aside so the
next process can create a fresh ring. Engram does not remove unrelated files
or repair unexpected layouts automatically.

Producer build identity is captured on the writer thread, so starting diagnostics
does not wait for the first executable fingerprint read. A blocked identity read
can delay retention; the queue and exit flush retain their bounds.
Requests and sends never await disk. Admission uses a nonblocking in-memory
publication lock; contention drops and counts the event as a queue loss.
Queue overflow, writer-start failure,
lock contention, permission/full-disk/unreadable destination, invalid layout,
oversized records, tracking overflow and write/flush failure preserve the
original protocol result and increment saturating loss counters. Subsequent
successful records and the exit loss summary disclose those losses.
The exit loss summary has kind `summary`, including when every loss counter is zero.
The idle writer blocks until an event or flush wake arrives; it does not poll.
Normal exit uses an independent flush signal even when the data queue is
full, waits at most one second and never joins a blocked writer indefinitely.
The flush result distinguishes successful draining from timeout or writer
failure; a timeout increments `flush_timeout`. A flush encountering a
publication still in progress returns false immediately and counts a timeout
rather than acknowledging a partially published record. Only successful
queue admissions and their ordered writer completions advance the flush fence;
dropped attempts do not advance it. An always-unwritable destination
cannot retain its own loss report. Forced termination or power loss has no
durability guarantee.

## Visibility limits

The observer sees decoded messages and front-door events delivered by rmcp.
Wire errors swallowed inside rmcp and calls that never reached Engram remain
outside this visibility. A host failure classified `not_started` may have no
Engram record because no process or frame was sent; a deadline after send
leaves application outcome unknown. Missing records may also reflect loss,
an older process build, rotation or abrupt exit. They never prove that a stall
was entirely on the client. Compare actual producer and host evidence.

Deterministic tests may substitute thresholds; production thresholds stay
2,000 milliseconds for slow calls and ten seconds after observed receive termination.
