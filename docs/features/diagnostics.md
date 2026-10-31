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

Control also records one `slow_call` for a successful frame whose duration
from the first available byte through response newline and flush is strictly
greater than 2,000 milliseconds. A typed refusal inside a successful envelope
is still a fault, so it does not add a slow-success record. Successful fast
frames produce none. `handler_ms` ends when the response value is ready,
before serialization and delivery. Only the new slow-success record includes
`store_write`: attempted and acquired write-transaction counts, aggregate
`wait_ms` around `BEGIN IMMEDIATE`, and aggregate `hold_ms` from acquisition
through commit, rollback or drop cleanup. These counts cover control write
transactions (`BEGIN IMMEDIATE`), not the operation's name: `turn_grant_read`
has zero counts and durations, while `session_status` takes a write transaction
and may expire an unbegun turn. Admission elapsed time includes SQLite work,
scheduling and I/O: `wait_ms` is not pure lock wait. The acquired interval is not pure SQL
time, and failed cleanup does not prove the writer lock was released.

A frame whose process is killed or cut off at the host's deadline before its
response flush leaves no slow-success `slow_call` record. This includes a host
dispatch budget of 20 seconds; missing records do not attribute the delay to
the host. Only the opt-in phase trace's in-flight lines cover an unfinished
frame. They remain best effort, with the trace's documented queue and progress
limits. Normal diagnostics neither impose a deadline nor install SQL profiling.

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
shutdown traces, including detailed store timing. Default diagnostics time
covered work and control transaction boundaries directly and do not install
SQLite statement profiling.
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

## Observed store transaction intervals

MCP tool calls and parsed control frames observe the common work-mutation
and control-mutation wrappers. Normal capture retains acquisition metadata
in memory. When elapsed admission or the acquired interval exceeds 1,000 ms,
it submits an acquisition/settlement pair retrospectively. Every failed BEGIN
is retained as an acquisition-failure observation, even below the threshold;
it never invents an acquired interval. Successful fast transactions add no
transaction records. One word may have several separate transactions.

`ENGRAM_STORE_TRANSACTION_TRACE=1` at process start instead captures every
observed transaction, submitting acquisition immediately after successful
BEGIN. This finite reproduction/incident mode does not install SQL profiling,
a monitor or a timer thread. It changes neither budgets nor results. The
existing shared note admission window remains ten seconds; ordinary work
acquisitions retain their configured timeout (normally five seconds), and
control retains its existing cooperative request allowance.

Records have kind `store_transaction`, code `store_transaction_observation`,
and stage `acquired`, `settled` or `acquisition_failed`. Their
`store_transaction` object carries a producer-local transaction sequence,
fixed boundary label, attempt and acquisition observation times, elapsed
admission, optional acquired interval, disposition, configured/remaining
allowance where available, and connection autocommit observed after cleanup.
For a control frame with an active request budget, configured and effective
native timeouts come from its existing busy-policy cap. A call outside that
budget scope leaves those fields unavailable rather than performing another
policy query for diagnostics.
An acquisition has no settlement or release claim. A failed cleanup with
autocommit false leaves release unconfirmed; `scope_exit` does not assert
commit or rollback success. BEGIN/COMMIT/rollback calls contain internal work,
so these are observed intervals, not exact OS-lock acquisition/release times
or pure SQL/lock-wait durations.

Join by producer instance UUID and transaction sequence, never PID alone.
`store_path_fingerprint` compares the selected path's exact encoded bytes
under a diagnostic namespace; it is neither physical-file identity nor an
authority claim. Aliases, case differences and separately selected paths can
have different comparisons for the same file. The unsalted comparison lets a
reader confirm a guessed path; it provides neither authenticated identity nor
anonymity. Raw paths and SQL are absent.
Store mutex waiting has its own `store_mutex` event when slow or fully traced,
with code `store_transaction_observation`, stage `mutex_wait`, and a
`store_mutex` object naming the store comparison and elapsed wait;
the transaction's `store_mutex_wait_ms` is cumulative for its enclosing call,
not an extra SQLite lock-wait measurement.

Always-on capture has no acquisition record when a fast BEGIN is followed
by termination before settlement. Full capture may retain the acquisition,
but missing settlement, loss, rotation or an uninstrumented writer cannot
prove that a transaction still owns the lock. Multiple overlapping observed
intervals do not establish which process blocked a waiter. Analysis must
report unavailable or ambiguous evidence instead of guessing a current owner.
An admission refusal proves only that attempt failed to acquire; other
transactions of the same word may already have committed.

Initial coverage excludes standalone CLI/library calls without an installed
diagnostic scope, startup/schema initialization, project-memory writes outside
the work wrapper, explicit repair/restore, deferred transactions promoted by
writes, read-snapshot lifetimes and the store-replacement TRUNCATE checkpoint.
WAL readers can prevent complete checkpoint progress without blocking ordinary
writers; a blocking checkpoint itself can acquire the writer lock. Neither
slow reads nor full-gate overlap establishes historical lock ownership.

All records use the same bounded asynchronous queue and ring as other
diagnostics. Submission never waits for disk and can lose either member of
a pair. The existing loss counters remain part of every retained record.
Concurrent writers can lose correlation records when the shared ring lock is
contended; subsequent retained records disclose this through `loss.lock`.

The explicit test workload
`cargo test --lib seven_lane_real_words_preserve_admission_budgets -- --ignored --nocapture`
uses seven owned test processes, two rounds of real claim/note/update/peek
words per process, and fresh stores under `target/`. It compares separate
stores and one shared store, each without an observer and with full capture.
It prints UTC measurement bounds, per-operation latency/refusal numbers,
unchanged configured budgets, observed transaction intervals and loss
summaries. It is excluded from ordinary gates. Coordinate its measured run
with an already-authorized full gate; the workload neither starts a gate nor
proves overlap by itself. An optional `ENGRAM_CONTENTION_GATE_RUN` value is
only an unverified label until compared with that gate's actual run bounds.
These are capture-off/on measurements, not an old/new product speed claim.

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
processes never interleave records. The writer explicitly unlocks after the
append's protected file operations, including cleanup on error or unwind.
Successful unlock releases the lock even if a child inherited a duplicate
descriptor before executing another program. Owned destinations are opened without
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
2,000 milliseconds for slow calls, 1,000 milliseconds for transaction/mutex
observations, and ten seconds after observed receive termination.
