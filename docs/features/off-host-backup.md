# Off-host backup

> Normative reference: [spec §3](../spec.md#3-storage--sync) and
> [spec §9.2](../spec.md#92-external-adapter-ports).
> Related briefs: [SQLite store](sqlite-store.md),
> [work-graph snapshot](work-graph-snapshot.md),
> [full store migration](full-store-migration.md),
> [external adapters](tracker-adapter.md),
> [security & trust](security-and-trust.md),
> [CLI & MCP](cli-and-mcp.md) and the [host checklist](../host-checklist.md).
>
> Status: design. Nothing in this brief is shipped except the commands it
> names as shipped: `engram backup`, `engram restore`, `engram graph save`,
> `engram graph load` and `engram migration export` / `import`, and of this
> design so far, for the `store` kind at a `directory` target, the
> `engram backup target` words, `engram backup push` with its capture,
> pending attempts, deadlines and retention, and `engram backup status
> [--json] [--check-target]` with the freshness rule and the
> `local_backed_up` mode it reports, the backup block of `engram doctor`,
> the `next` reminder, `engram backup list` and `engram backup fetch`, and
> `engram backup restore` of a `store` copy. The `graph` kind is not
> shipped. The [shipped inventory](../shipped.md) stays the record of what
> exists.

Today every Engram store lives on one machine. `engram backup` writes its
copy under the same home as the store, in `backups/`, and `engram graph
save` writes its file under that home too, so losing the machine loses all
work state. Before this design, `doctor` said nothing about
copies. This brief designs the missing part: a copy that reaches a configured
target outside the store's home, a record of what that target confirmed, and
an exact rule for when Engram may report `local_backed_up` instead of `local`.

`local_backed_up` means a verified copy at the configured target, no more.
Whether that target is off the machine is shown beside the mode every time,
as either asserted by the operator or confirmed by the remote.

A backup is restore-only. Nothing reads the target while work runs, a copy
grants no authority, and two hosts never share one. Moving live work between
machines is the separate `portable` mode.

## Copy kinds

There are two kinds of copy. A project may configure either or both. Each is
captured, confirmed, reported and restored on its own, and status always says
which kind a claim rests on and what that kind restores. The two are never
promised to share one cut.

| | `store` copy | `graph` copy |
| --- | --- | --- |
| Artifact | The verified full-store file that the shipped `engram backup` writes: SQLite's own consistent copy, then a full check that every record decodes and agrees | The shipped [work-graph snapshot](work-graph-snapshot.md) document, saved without widening |
| Contains | Every row: work graph, runs, claims, seals, evidence, acceptance evaluations, obligations, control policy and its history, sessions and grants, delivery state, project memories with every body, agent-private scratch | Work items, active blockers, cited source snapshots, per-item history as inert records, keyed project memories with their history and tombstones |
| Excludes | Nothing | Runs, root executions, claims, seals, checkpoints, waivers, evidence bodies, acceptance evaluations, obligations, control policy and rule sets, sessions, grants, delivery state, unkeyed observations, private scratch. A `restricted` memory body is a typed placeholder |
| A restore gives | The same store at the copy's cut | A new store with the same planning graph: completed items land completed by record, never by seal; open items start execution again; the operator asserts the control policy again at `init` |
| Bound to | One store schema, and the full check of the build that restores it | One snapshot format fingerprint |
| Exposure at the target | Everything above, readable by anyone who can read the target, including host-private state. The target must be a place where the store itself may be kept, readable only by the operator's own account | Planning text, history and memory bodies labelled `public`, `internal` or `secret-ref`, readable by anyone who can read the target |
| Measured on this repository's store on 2026-10-02 | 400.6 MB, written and verified in 37 to 40 s while agents kept working; 72.7 MB under gzip | Not measured for this brief |

Only the `store` copy can restore a store that equals its source. The `graph`
copy trades that for a much smaller disclosure and a text file that suits a
Git remote. A backup push never widens a graph save: load turns every
`restricted` body into a placeholder even when the file holds its text, so
widening would disclose more and restore nothing more.

Engram encrypts nothing. Encryption of the stored copy at the target and
security of the transfer are two separate properties, and the operator
provides both: a transport that encrypts does not protect the bytes at rest.
Compression, which the `directory` adapter applies, hides nothing.

## Targets and receipts

A target is configured per project and copy kind and is reached through an
adapter. The core produces the artifact and its manifest and owns the
freshness rule; an adapter only moves bytes and reports what the target
confirmed. Adapters live outside the core library, behind the four requests
of the `BackupAdapter` port in
[spec §9.2](../spec.md#92-external-adapter-ports):

| Request | Meaning |
| --- | --- |
| `put(project, manifest, artifact)` | Store one immutable copy; answer with a receipt or an error. The `directory` adapter takes the recorded attempt, which carries the complete manifest, and the stored file it prepared in the local stage |
| `confirm(project, manifest)` | Say whether the target holds exactly that copy: confirmed, missing, or unknown |
| `list(project, cursor)` | The manifests of the copies the target holds for this project |
| `get(project, copy)` | Return one copy. The `directory` adapter writes it to a new local file the caller names, decoded and checked against its manifest, and removes that file on any failure |

**A target is a disclosure decision.** Whatever a kind carries is readable
at its target, so configuring or changing a target is the operator's
decision and never an agent's. `target set` therefore records the
operator's attributed authorization of that destination for exactly what
the kind carries (`--disclosure-authorized-by`): for the `store` kind the
whole store, host-private state and every memory body included; for the
`graph` kind the unwidened graph document, with the keys and relations of
its redacted entries. A push needs no approval per copy, because the
configured target carries that authorization. A destination the operator
has not authorized for a kind is not configured for it. This brief does not
design a reduced export for a destination that may not hold a kind's full
artifact.

A receipt names the artifact's digest, the target's identity, the time and
the kind of acknowledgement. Status repeats the acknowledgement as recorded
and never says more than it.

| Adapter | Carries | Acknowledgement | `off_host` | What that proves |
| --- | --- | --- | --- | --- |
| `directory` | `store`, `graph` | `read_back` | `asserted` | The bytes were written under the configured absolute path and read back equal. That the path leaves the machine is the operator's assertion, recorded with a name and time. Engram does not verify it |
| `git-ref` | `graph` | `remote_accepted` | `remote_confirmed` for a network remote, `asserted` for a local one | The configured remote accepted a push of a dedicated ref and lists that ref at the pushed commit. This is not evidence about the provider's own durability |

Every place that shows the durability mode shows, for each kind that
qualifies, its `off_host` value and what it restores, as fields and not as a
footnote. The mode never appears alone. For a `directory` target the text is
exactly "off-host asserted; not verified". Only an adapter that observes the
remote's own acknowledgement reports `remote_confirmed`. An asserted target
protects against losing the machine only as far as the operator's assertion
is true: a read-back from a sync client's folder shows that the file reached
the folder, not that it was replicated. No output and no document may present
an asserted target as verified protection against machine loss.

The `directory` adapter fits another machine's share, a network drive, a
removable disk or a folder that a sync client replicates. It stores each
artifact gzip-compressed, because a folder that uploads sends every new copy
whole: this repository's 400 MB store copy is 73 MB compressed. No ratio is
promised; an artifact that does not compress is stored all the same. A copy is
still compared by the content fingerprint of its uncompressed bytes; the
manifest also records that the stored file is gzip. The adapter compresses in
the local stage, writes the stored file under a temporary name that carries
the attempt's id, renames it without replacing anything, reads it back,
decompresses it and compares the length and content fingerprint of the result
with the manifest's, and writes the manifest last in the same way. Every
`confirm` does that same read: under its deadline it decompresses the stored
file and compares length and fingerprint. Decompression writes no more than
the length the manifest declares and reads at most one byte beyond it, in
memory, to find a file that would decode to more, which is refused. The
deadline is checked between chunks of the read, so a read that stalls inside
one chunk, as on a hung network share, is not interrupted by the adapter; a
push bounds that from outside. A stored file that holds other bytes, or is not
a gzip of the copy, or has more after its gzip stream, makes the copy missing;
a target that cannot be reached or read makes the answer unknown. Every
request checks that the manifest describes this project's copy; `put`,
`confirm` and a push's reconciliation also check that it was made for the
target's current identity, while `get` returns a copy made for an earlier
identity, which a restore after configuring the target again needs. The move
into place is not forced to disk at the target, so a power loss there can
still lose a copy that was acknowledged; the next `confirm` then finds it
missing. A `put` to a configured directory that cannot be reached is refused
with `backup_target_unreachable`: the adapter creates only the project's
folder inside it and never recreates the directory itself, which could land
the copy on this machine. A look at names and sizes can show that a copy is
missing, but it never confirms one, because a file of the same size with other
content would pass. The stored file is an ordinary gzip file, so a copy can be
recovered by hand with standard tools. A data file without its manifest is not
a copy. The pending attempt records the temporary name and the final name of
its data file. When a push was cut off after the rename and before the
manifest, the next push reconciles that recorded attempt before it confirms
anything, as step 2 of [Push](#push) says: if the file's content matches the
recorded manifest, the manifest is written and the copy is complete; if not,
the file is removed as part of that abandoned attempt. The adapter removes
only files of attempts that this home recorded and then resolved as abandoned;
it never deletes other files in the directory because they look orphaned. It
uses plain file operations and accepts network paths and cloud folders on
purpose; the link and placeholder refusals of `graph save --out` protect a
disclosure default and do not apply to a path the operator configured as a
target.

The `git-ref` adapter writes commits to `refs/engram/backup/<project digest>`
in a scratch repository under `ENGRAM_HOME` and pushes that ref, never a
branch and never a working tree. The push is not forced, so a remote ref that
moved is refused and reported. The remote keeps every pushed copy in its
history; Engram promises no erasure there. An attempt that step 2 of
[Push](#push) drops as missing keeps its commit in the scratch repository,
and the next `put` adds its commit on top of it. If the dropped push
reaches the remote after all, the remote ref is then an ancestor of the
next push, which is accepted; the late copy has no receipt, qualifies
nothing, and `list` shows it. The first version carries only the
`graph` kind over Git, and only up to a stated artifact size, which is
never above the load limit that every `graph` copy must meet. That is a
chosen limit, not a property of Git: a `store` copy is a large binary file
that changes whole with every copy, and some providers refuse large files
(GitHub refuses files over 100 MiB).

`remote_confirmed` is reported only for a remote reached over the network.
Git also accepts a path, a `file://` address or a loopback host as a remote;
a push there lands on this machine. Such a remote is treated like a
directory: it requires `--off-host-asserted-by` and reads "off-host
asserted; not verified". A network acknowledgement in turn shows only that
the configured endpoint answered, not where or how it keeps the bytes.

SQLite never opens a file on a target, and no store transaction is held while
an adapter runs.

## Configuration

Operator words; the fourteen agent words are unchanged. Each word resolves
the project the ordinary way and acts on that project only. The shipped
`engram backup`, called without one of these subcommands, keeps writing one
verified local copy as it does today.

```bash
engram backup target set --kind store --adapter directory --dir <absolute path> \
  --disclosure-authorized-by <operator> --off-host-asserted-by <operator> \
  [--window-hours N] [--keep N]
engram backup target set --kind graph --adapter git-ref --remote <url> \
  --disclosure-authorized-by <operator> [--off-host-asserted-by <operator>] \
  [--window-hours N]
engram backup target show
engram backup target clear --kind <kind>
```

A target belongs to one project and one kind. Its configuration holds the
adapter, the location, the window, the retention count and two statements
by the operator, each kept as asserted context with a name and a time:

- `--disclosure-authorized-by`, required for every target: the destination
  may hold what this kind carries.
- `--off-host-asserted-by`: the destination leaves the machine. It is
  required for a `directory` target and for a `git-ref` remote that is a
  path, a `file://` address or a loopback host. A `git-ref` remote reached
  over the network needs none, because the remote's own answer is recorded.

The configuration is not in the store: a restored store must not inherit a
claim about a target that its new host never configured, and adding it
needs no change to the store format. Setting a target copies nothing, and a
clean home has no target and no receipt.

The configuration and the recorded state below are files under `ENGRAM_HOME`,
kept per project and kind, under one contract:

- Each carries a format version and is replaced whole by writing a new file
  and renaming it into place. A build that meets a file whose format version
  it does not know, or that it cannot read, refuses that file by name: the
  kind does not qualify, status gives `backup_record_unreadable` as the
  reason, and a push fails. It never converts the file, guesses its meaning
  or carries on as if the file were absent. The way on is explicit:
  `target set` with the running build writes both files for that kind anew,
  and the next push makes a new copy. A state file that cannot be read at
  all, as when access to it is denied, is not replaced: `target set` refuses
  with `backup_io` and changes nothing until the operator restores access to
  the file or removes it.
- A target's identity is derived from the project, the kind, the adapter, the
  location and both statements. A receipt names the identity it was issued
  for, so any change to the target ends the qualification of older receipts.
  They are kept only as history that does not qualify.
- Every command that writes them takes the push lock for that project and
  kind: `push`, `target set`, `target clear`, `backup restore`, and a
  `status --check-target` that has a check result to record. A
  configuration therefore never changes under a running push. `target set`,
  `target clear` and `backup restore` refuse while a push holds the lock. A
  check records its result only under the lock and only when the newest
  receipt is still the one it checked; otherwise it reports the result
  without recording it.
- They hold no secret. They are operational records of this host: not
  canonical work state, not part of any copy, and never authority for a
  restore.

## Push

```bash
engram backup push [--kind <kind>]
```

One bounded command does all the work, for each configured kind. With no
target configured it says so and exits 0.

1. Take the push lock for this project and kind: an exclusive lock that the
   operating system holds for the process. A push that cannot take it exits 0
   and says that another push is running. Releasing it unlocks the lock file
   explicitly before closing it, so once the unlock succeeds a child process
   that inherited the file before it executed another program does not keep
   the lock. An unlock that fails is reported on standard error, and the lock
   then stays held until every inherited reference closes. A push that ends
   abnormally before its release leaves the lock to the operating system in
   the same way. There is no takeover by age: a push ends by its own
   deadlines, and only its release or the end of everything holding the lock
   frees it.
2. Resolve a pending attempt, if the state records one, in this order.
   First reconcile that attempt's own files at the target, and only those:
   when its data file was renamed into place and its manifest was not
   written, and the file's content matches the recorded manifest, write the
   manifest and so finish the publication. Then ask the adapter to `confirm`
   that exact manifest. Confirmed, it becomes the newest receipt. Missing,
   it is dropped and its recorded files, temporary or renamed, are removed.
   Unknown, the push fails here and the attempt stays pending. Only a push
   does this, for an attempt this home recorded: a `confirm` made for
   `status --check-target` never writes a manifest because it sees a file.
   An attempt that was recorded for another target identity is not resolved
   against the current target: it leaves the pending record, is kept as
   history that does not qualify, and nothing at either target is touched
   for it.
3. For the `graph` kind only: read the store's current cut. When the newest
   receipt covers that cut, for the same target and format, `confirm` it.
   The snapshot body is a function of its cut and format, so a capture
   would produce the same bytes. Confirmed, the confirmation time is renewed
   and the push ends successfully. Missing or changed, the receipt stops
   qualifying and the push goes on to capture and `put` a replacement.
   Unknown, the push records the failure and ends without advancing
   anything.
4. Check that the stage has room for the copy: for the `store` kind, at
   least the size of the store file and its log, and as much again when the
   target stores a compressed file, which is built in the stage beside the
   copy. Without it the push fails
   with `backup_stage_no_space` before anything is written. A `directory`
   target is checked before `put` against the known size of the compressed
   file and its manifest (`backup_target_no_space`). The check reserves no
   space.
   A later out-of-space error still fails the push, records the failure and
   does not advance freshness; cleanup touches only this attempt's files.
   Then note the capture start time and capture into a local stage under
   `ENGRAM_HOME`. The `graph` kind reads one coherent cut and then commits
   its shipped disclosure audit in one short write. A `graph` document is
   admitted by size before it can become a copy, and before that audit is
   committed: when the complete serialized, uncompressed file, as
   `graph load` would read it, is larger than `engram graph load` accepts,
   128 MiB in this build, the push fails with
   `backup_graph_too_large`, records no disclosure, puts nothing, records
   the failed attempt and advances no receipt and no time.
   The bound is the load limit itself, read from the same definition and
   not kept as a second number, and it holds for both adapters, so no copy
   qualifies that the restore below cannot load.
5. Write the manifest: project digest, kind, cut, capture start time, bytes,
   digest, format identity, the fingerprint of the build that captured and
   checked the copy, that build's source revision (`unavailable` when the
   build could not determine it), and the
   host name as asserted context. The cut is read from the finished staged
   copy, or reused from the same-build checked copy when the settled bytes
   equal it as described below, never from the live store. These identities
   are separate fields and are never merged into one.
6. Decide whether anything has to be uploaded. The upload is skipped only
   when all of these hold: the newest receipt names the configured target's
   identity, its format identity is the capture's, and the captured
   artifact's digest equals the receipt's. Then `confirm` that copy.
   Confirmed, record that the store's content was observed equal to it at
   this capture's start; nothing is uploaded and the copy's own manifest is
   not rewritten. Unknown, the push fails here. Missing or changed, and in
   every other case, record the attempt as pending, with its complete
   manifest and the target's identity, and then `put` under its own
   deadline. Every other case includes a newest receipt that names an
   earlier identity of the target: after `target set` changed anything that
   the identity is derived from, the next push uploads the copy again, as a
   new copy under its own name, and its receipt names the current identity,
   even when the store has not changed and the same bytes already lie at the
   same location. A push that ran through its capture and ended
   successfully therefore never leaves the kind reading
   `backup_target_changed`; a push that step 1 ended because another holds
   the lock changes nothing. A receipt makes the attempt the newest
   receipt and clears the pending record. An error or an unknown outcome
   leaves it pending for step 2 of the next push. What the copy is known to
   cover advances on a receipt, on a confirmed equal capture, or, for the
   `graph` kind, on a confirmed unchanged cut in step 3, and on nothing
   else.
7. With a receipt in hand, remove copies beyond the retention count from a
   `directory` target, never the newest confirmed one. The count covers only
   copies whose receipts this home recorded for the current target identity.
   A copy from another home, or from an earlier identity of the target, is
   never removed; `list` shows it. A removal that fails is a warning.
8. Remove the local stage and record the attempt: time, outcome and, on
   failure, a typed code and the message.

Capture and transport have separate deadlines. Passing the capture's stops
the capture and removes its stage. Passing the transport's ends the push's
process, which stops the request still running on its worker thread; push
starts no child process, so nothing else is left running. A request the
remote had already received may still complete there after the local
deadline; that is the unknown outcome, and the attempt stays pending until a
later push resolves it. A failed push exits 1 and leaves the previous
confirmed copy and its receipt untouched. What each deadline stops, and what
a push a host terminates leaves behind, is in
[deadlines and cancelling a push](cli-and-mcp.md#deadlines-and-cancelling-a-push).

The recorded state holds the newest receipt with its manifest, the time the
store was last observed equal to that copy, the target's last confirmation
of that copy and any finding that the target no longer holds it, each naming
the copy it concerns, a pending attempt if there is one, the last attempt, the receipts whose copies this home has not removed,
which retention counts, the pending attempts set aside because they were
recorded for another target identity, and, in a restored home, the restore
record described below. When the state is lost, nothing is guessed from a
file name at the target: the next push makes a new copy, or confirms one
only after checking that exact artifact against its manifest.

### Comparing the settled copy

A `store` push always takes a whole `VACUUM INTO` copy, settles its journal
mode and hashes the self-contained file. When both SHA-256 and byte length
equal the newest receipt's copy, that receipt names the configured target,
and its manifest names this build as the checking build, the earlier full
check stands for the equal bytes. The push reuses that copy's cut and schema
reference and skips the full check and compression. Its JSON kind report
says `capture_check: "same_bytes_as_newest"`. Every other case, including an
unknown checking build, runs the full check and reports `"full"`.

The target must still confirm the stored copy, reading its gzip in full.
Only successful confirmation after the equality observation advances
`observed_equal_at`, to this capture's start. Skipping a check alone advances
no time. If the target copy is missing, the replacement is checked in full
before preparing it. Failed, busy and terminated pushes refresh nothing.
An older fully checked copy can thus remain qualified after a recent exact
equality observation and target confirmation within the freshness window.

There is no cheap exact store-wide marker in `backup status`. The work and
memory cut misses claims, control rows and delivery state; SQLite's
`data_version` is per connection and its main-file change counter misses
WAL commits. Trigger counters do not cover virtual FTS tables or
`sqlite_sequence`. None of these is shipped as an exact substitute for the
settled-byte comparison. The comparison covers every row the copy restores,
without changing the live store's schema or rows.

One Windows measurement on 2026-10-03 used the release binary against an
isolated 435,761,152-byte copy of this repository's store and a local
directory target. The first full push took 81.4 seconds; the next unchanged
push took 2.1 seconds and reported `same_bytes_as_newest`. After one project
memory write, a full push took 51.3 seconds. These are whole push times,
including target reads, rather than timings of the check alone.
The target was local and the data cached; a network or sync-folder target
can take longer. The earlier 37–40-second measurement timed a verified
capture, not a whole push. No fixed speedup or latency is promised.

### What a capture does to the live store

Agents keep writing while a copy is taken, so this is part of the contract.

A `store` capture opens the store read-only and runs SQLite's `VACUUM INTO`
to a file in the local stage. This open is new with this design: the shipped
`engram backup` opens the store with its ordinary read-write opener. What
is reused from it is the artifact only, a `VACUUM INTO` copy that passes
the same full check. The contract is:

- It takes no write transaction on the store. It holds one read transaction
  for the length of the copy step, which the capture deadline bounds.
- The store is in write-ahead-log mode, which lets writers commit while that
  read is open. This is concurrency, not a promise that no write is ever
  delayed or refused under every load; the store's busy timeout is a
  refusal bound, not a latency guarantee.
- While the read is open, a checkpoint cannot recycle the log past it; the
  log grows by what is written in that time and is recycled afterwards.
- The check of the copy, its digest and the transport all work on the staged
  file, with no transaction on the store.
- The read-only connection cannot create or initialize a store: a missing
  store is refused. It must not set `query_only`, under which SQLite refuses
  `VACUUM INTO`.
- A capture that fails or passes its deadline is reported as a failed push.

One observation, from 2026-10-02, against a 400 MB scratch copy of this
repository's store with a second connection committing a 4 KB row every
20 ms: the copy step took 1.9 to 2.9 s of the 37 to 40 s that a whole
verified capture takes. In eight copies no commit was refused. In six, the
slowest commit took at most 61 ms; in two, one commit took 0.8 and 0.9 s.
The probe timed each whole commit and did not separate waiting for a lock
from waiting for the disk, so the cause of those two pauses is not
established. The first implementation item repeats this as a test that
shows the writer making progress under its declared fixture.

A `graph` capture holds one read transaction while it reads both the work
and the memory positions and builds the body, and then commits its
disclosure audit in one short write transaction, as the shipped `graph save`
does.

The cut of a `graph` copy is the pair the snapshot body already carries: the
project work-feed head and the project-memory change position. A `store`
copy records the same pair for the reader, but no position covers every row
of a store, so the `store` kind never skips a capture because a cut is
unchanged. Equal settled bytes can skip the full check and compression as
above, and skip the upload only after target confirmation.

## Trigger

Engram runs no daemon and no agent word copies anything. The host calls
`engram backup push` for each project it runs: at start, on a cadence while
sessions are active, and when the last session ends. Where no host runs, an
operating-system scheduler calls the same command. Calling it often is safe
for correctness, because the lock makes overlapping calls harmless, but it
is not free. An unchanged `graph` cut costs one `confirm`. A `store` push
always copies the whole store locally. Only unchanged settled bytes checked
by the same build skip the full check and compression; target confirmation
still reads the stored gzip. Any real store write, including a claim renewal,
control turn or delivery change, can require the full path. An open session
is a reason to schedule an hourly push, not a change detector. Push at host
start, hourly while sessions are open, and when the last session ends;
whether active or idle, push at least once per half of `window_hours` (12
hours at the default 24 hours). Successful pushes need no separate
`--check-target` cadence; failed, busy or terminated pushes refresh nothing.
An earlier assurance due takes priority. Before enabling automatic pushes,
the host checks full-path cost and control latency on a representative store;
an idle-push measurement does not establish either for a changing store.
A host may put a scheduled push off, for example while a timing-sensitive test run is in
progress: the window, not the cadence, decides the claim, so a late push
costs nothing but the age of the copy.

## Freshness: when `doctor` says `local_backed_up`

For each configured kind, let `R` be the newest receipt and `W` the
configured window. The kind **qualifies** when all of these hold:

- `R` exists and names the configured target's identity;
- the running build accepts `R`'s format identity: the store schema
  reference for a `store` copy, the snapshot format fingerprint for a
  `graph` copy;
- no recorded time lies in the future;
- the target confirmed the copy within `W`, and no later check found it
  missing or changed;
- the store's content was observed in the copy within `W`: the copy's
  capture started within `W`, or a later capture that started within `W`
  produced the same bytes; for the `graph` kind a copy that covers the
  store's current cut also satisfies this.

The mode is `local_backed_up` when at least one kind qualifies, and `local`
otherwise. For the `store` kind, age counts from the start of a capture,
never from its end, an upload or a confirmation: confirming old bytes again
never makes them fresh. For the `graph` kind the same holds once the cut has
moved; while the cut is unchanged, the copy still covers the store and only
its confirmation has to be renewed. No process has to run for the claim to
end: when the window passes without what the rule requires, the same rule
answers `local`.

A kind that does not qualify reports the first reason that applies:

| Reason | Meaning |
| --- | --- |
| `backup_record_unreadable` | The configuration or state file has a format version this build does not know, or cannot be read |
| `backup_not_configured` | No target for this kind |
| `backup_never_confirmed` | A target, but no receipt yet |
| `backup_target_changed` | The newest receipt is for another target identity |
| `backup_other_format` | The running build does not accept the copy's format identity |
| `backup_clock_invalid` | A recorded time lies in the future |
| `backup_copy_missing` | A check found that the target no longer holds the copy, or holds other bytes |
| `backup_confirmation_expired` | The target last confirmed the copy longer ago than the window |
| `backup_stale` | The store's content was last observed in the copy longer ago than the window |

The rule is a pure function of the configuration, the recorded state, the
store's cut, the running build's format identities and the clock.

A copy is checked in full by the build that captured it, and the manifest
names that build. The rule does not ask that the running build be the same
one: several builds run side by side on one host, and each would then
disown the others' copies. A build that restores a copy checks it in full
again and may refuse a copy that an earlier build accepted; status therefore
names the checking build whenever it differs from the running one, and the
restore section says what to do then.

## What the operator sees

```bash
engram backup status [--json] [--check-target]
```

`status` reads the configuration, the recorded state and the store's cut. It
reports recorded evidence "as of" its times and does not contact the target
unless asked. It opens with the mode and, for each qualifying kind, its
`off_host` text and what it restores; a `graph` copy always reads "planning,
history and keyed memories only". When both kinds qualify, both are listed.
For each configured kind it then prints the acknowledgement as recorded, who
authorized the disclosure and who asserted that the target is off the
machine, each with its time, the capture start and
its age, the cut and how far the store has moved since, the last
confirmation, the build that checked the copy, a pending attempt, and the
last attempt with its error. A failed last attempt is shown at once, even
while an earlier confirmed copy still qualifies. This is the read a host
uses for its own display.

`engram doctor` prints the same block, in text and JSON, from the recorded
evidence, on a healthy store and on a refused one alike. A store outside an
Engram home's `<home>/projects/<project digest>/engram.db` layout has no
home whose records name it, and doctor says so with `backup: not read`
rather than printing nothing. It does not contact the target: checking a `directory` target
means reading and decompressing the whole stored file, about 73 MB read and
400 MB decompressed for this repository's store today, and that cost
belongs to a command the operator asks for. `status --check-target` asks
each adapter to `confirm` under a deadline. A copy the target no longer
holds is recorded and stops qualifying at once (`backup_copy_missing`). A
target that cannot be reached is reported as `backup_target_unreachable`.
A check that reached the target and passed its deadline before it had read
the copy is reported as `backup_check_timed_out`, which says nothing
against the copy. Both stand beside the recorded evidence, which keeps
counting until its window ends. No check has to be asked for to find a
copy that disappeared: every push that runs checks the newest copy, by its
`put` or by its `confirm`, and when pushes fail or are put off the window
ends the claim by itself. Durability is reported apart from store health: a stale
backup never makes a healthy store unhealthy, and `readiness` is unchanged.

`next` and `next --peek` add one reminder line when a target is configured
and the mode is `local`, or when the last attempt failed. They add nothing
when no target is configured or when all is well. They read only the
recorded state, and read nothing more when no kind has a configuration
file; a state file alone configures nothing. A configuration file whose
existence cannot be checked, for example because access to it is denied,
counts as present, so its records are read and an unreadable one is
reported. The line names the mode with
each kind's reason, or the failed push with its code and the earlier copy
that still qualifies with its off-host text, and ends with `see engram
backup status`. It follows the direction to list memories, ahead of every
other reminder, and no output budget sheds or shortens it. A stale backup
never refuses or delays a word.

## Restore

Both kinds restore onto a clean home and never over a store in use. A clean
home has no target, so the first step on the new machine is to configure the
target again. That configuration qualifies nothing until a push from the new
home is confirmed.

**From a `store` copy**, for a replacement machine after the origin is lost
or retired:

1. Retire the origin first. Stop every consumer of the origin store, and
   make sure nothing will open it for writing again: no host, no scheduled
   push, no agent session. Two stores that both accept writes diverge, and
   restore cannot see that happening; its only trace would be a newer copy
   at the target that this home did not record. This is why restore asks
   for the operator's statement in step 4.
2. Install Engram. The manifest names the build that captured the copy and
   its source revision, unless that is `unavailable`; the current build
   serves when it accepts the copy's format and its own full check of the
   copy passes.
3. Set `ENGRAM_HOME` to an empty home, check out the project, and run
   `engram backup target set` for the target that holds the copies.
4. Run `engram backup list` to see the copies, each with its capture time,
   SHA-256, cut, format, capturing build and host, then
   `engram backup restore <copy> --origin-retired-by <operator>`. Restore
   fetches the copy from the configured target itself; it takes no local
   file. `engram backup fetch <copy> --out <file>` is a separate, standalone
   checked extraction for an operator who wants the file. Under the push
   lock restore checks that the local disk has room for the stored file and
   the uncompressed copy the manifest declares, fetches the copy into a
   staging file beside the store, decompresses no more than the declared
   length and checks the result against its manifest. It then runs the full
   check a backup gets on the staged copy, records a pending restore under
   `backup-records`, moves the copy into place with a rename that never
   replaces anything, checks the installed store and marks the record
   completed. It never replaces an existing store, and it refuses a copy
   whose manifest or whose rows name another project.
5. Run `engram doctor` and `engram readiness`. Both resolve the project
   root's path identity, which restore itself does not: a store restored
   onto an operating system with different path rules is refused here. Start
   the host, with new sessions, only after both pass.

**A restore that stops.** The staging file is named
`.backup-restore-<id>.staging`, which store open, `doctor` and `readiness`
never take for a store; neither `doctor` nor `readiness` creates a store
beside it. A restore that stops before it writes its pending record leaves
no record, and may leave its staging file. One that stops after the record
and before the move leaves no store and the record pending; a retry of the
same copy finishes it, and another copy is refused while it is pending. The
refusal and the pending status line name both ways on: run
`engram backup restore <copy> --origin-retired-by <operator>` again, or
abandon it with
`engram backup restore <copy> --abandon-pending --abandoned-by <operator>`.
A retry fetches the copy again, checks it against the pending copy's
SHA-256, points the record at the new staging file and only then removes
the earlier one, so a record write that fails leaves the earlier file and
record as they were; a process that ends between the two leaves the earlier
staging file behind, no longer named by the record. A pending restore whose
copy is gone from the target cannot be finished, and abandoning it is the
way on: bound to that copy, under the push lock, with no store in place and
without contacting the target, it removes only the restore's own staging
file, through the store's directory and `projects` held open from the home
without following a link, archives the record whole as
`store.restore-abandoned-<UTC time>Z.json`
with who abandoned it and when, and only then clears it, so another copy can
be restored. A
no-replace rename within one directory is all or nothing for a process that
stops, so a restore that stops after the move leaves the whole copy in
place with its record pending. A retry of the same copy then completes the
record when `engram.db` holds the recorded SHA-256 and, of the store's own
`-wal`, `-shm` and `-journal` files, at most what a read leaves: an empty
`-wal` and an `-shm`. A `-wal` with any bytes, a `-journal` or other bytes
in `engram.db` mean something wrote to it, and the retry refuses it as an
existing store, naming what did not match. `backup status` and `doctor`
show a pending record from the moment it is written.

**What restore does with live authority.** A `store` copy holds the claims,
grants, begun turns, control sessions and delivery state that were live at
its cut. Restore changes no row, so the restored store equals the copy. It
never renews, resets or revives authority, and it never uses this machine's
clock to extend or end any of it: the clock only decides what the report
calls unexpired. Restore does three things about that authority:

- It requires the operator's statement that the origin store will never run
  again (`--origin-retired-by`). Nothing in this mode prevents a second
  writer, so this is the one precondition Engram cannot check. The
  statement is asserted context: restore prints it and writes it, with the
  copy's digest and origin host name, to this home's recorded state, and
  `status` and `doctor` show it from then on.
- It reports, read from the checked copy with one reading of this machine's
  clock, how many active claims and how many issued grants have not yet
  expired, when the last of each expires, and, separately, how many begun
  turns are still open, not yet checkpointed.
- It resets nothing, and what it restores ends only as the origin's would
  have:
  - A restored claim stays held until its recorded expiry (one hour by
    default; its holder may have asked for another lifetime). Until then a
    new session's claim of the item is refused (`work_claim_held`); after
    it, the item is recovered the ordinary way, with
    `engram work claim <ref> --recover "<reason>"`.
  - A restored issued grant expires at its recorded time, seconds after it
    was issued, or sooner, when a control connection is opened for its
    session; either way it is then unusable.
  - A restored begun turn has no clock end: a grant's expiry does not close
    a turn that began under it. It stays open until a caller acting as its
    session checkpoints it, and no other session can close it today. It
    does not keep a new session from the work: once the old claim expires,
    the item is recovered as above.

  Who can use what restore keeps is the same asserted boundary the origin
  had: the session id, and nothing more. A caller that asserts a restored
  session's id can use and renew its claim, as on the origin. Opening the
  control transport with that id reconnects the session under a new
  connection token, which also ends its issued grants, and the session's
  routing token is in the copy, so that caller can also checkpoint the
  session's begun turn. Nothing in the restored store keeps a reused session
  id out. This is why the host must give its sessions identities the
  restored store has not seen.

The preconditions before any consumer starts are therefore: every consumer
of the origin is stopped for good, the new machine's clock is right, and the
host gives its sessions identities the restored store has not seen. A reset
of live authority at restore would be a new store operation and is not part
of this design. Moving a store to a machine with different path rules, and
refusing a second writer, are `portable`'s.

When the running build does not accept the copy's format, or its full check
of the copy fails, `backup restore` refuses and leaves the fetched file in
place, naming its path. The refusal names the ways on: install the build at
the manifest's source revision and restore with it, which it names only
when that revision is not `unavailable`, or run `engram migration export`
on the fetched file and `engram migration import` with a build that still
names every conversion since ([full store migration](full-store-migration.md)).
Engram records the capturing build's fingerprint, the format identity and
the source revision; it does not keep the executable at the target and does
not claim that a revision rebuilds to the same binary. When neither a
compatible executable nor a supported conversion can be obtained, recovery
is unavailable, and the bytes remain at the target.

**From a `graph` copy** (planned: the `graph` kind is not shipped):

1. `engram init` on an empty home with the project's control policy, which
   the file never carries, and apply its obligation rule set and
   acceptance-evaluation policy again. Run `engram backup target set` for
   the target that holds the copies.
2. `engram backup fetch <copy> --out <file> --kind graph`, which
   decompresses the copy when it is stored compressed and checks the file
   against its manifest, then `engram graph load <file> --dry-run`, then
   `engram graph load <file>` (shipped).
3. Run `engram doctor`.

A format mismatch refuses. The way on is the one the
[work-graph snapshot](work-graph-snapshot.md) brief gives: load with the
build at the manifest's source revision into a disposable store, then
convert that store forward. When that build cannot be obtained, recovery is
unavailable, and the bytes remain at the target.

## Acceptance tests

Each test runs against homes and targets under the repository's `target/`
folder.

- **Store equality.** Export the rows of a quiescent source store, whose
  write-ahead log holds no frames. Push a `store` copy to a directory
  target. Change the source. Run `backup restore` into a second, clean
  home. The restored file's SHA-256 equals the manifest's, checked before
  `doctor` or any claim opens it; `doctor` reports it healthy; and its
  exported rows, compared as parsed JSON with only the export header's
  `exported_at` left out, equal the rows exported before the push and
  differ from the changed source's.
- **Graph surface.** Push a `graph` copy, change the source, fetch and load
  into a clean initialized home. A graph save of the restored store has the
  same items, blockers, sources and memories as the pushed file and carries
  the pushed file's native history layer as an inherited one. A `restricted`
  body is a placeholder. The restored store holds no run, claim, seal,
  grant, session or scratch.
- **The rule.** A table test over the pure rule produces `local_backed_up`
  and every reason above, including a future timestamp, a changed target,
  and three cases of an old copy confirmed a minute ago: a `store` copy on
  an unchanged store reads `backup_stale`; a `graph` copy whose cut has
  moved reads `backup_stale`; a `graph` copy whose cut is unchanged
  qualifies.
- **No upload when unchanged.** A second push of an unchanged `graph` cut
  captures and uploads nothing and renews the confirmation. A `store` push
  whose capture has the same digest uploads nothing, records the new
  observation time and leaves the copy's manifest unchanged. When the copy
  was deleted at the target while the `graph` cut stayed unchanged, the push
  captures and puts a replacement; when the target cannot say, the push
  fails and nothing advances.
- **A changed target is copied to again.** After `target set` changes the
  target's identity while the store stays unchanged, for example a new
  disclosure authorization for the same directory, the kind reads
  `backup_target_changed`. The next push of either kind uploads the copy
  again and records a receipt that names the new identity, and the kind
  qualifies: a push that ran through its capture and exited 0 never leaves
  the reason `backup_target_changed`. A push that exits 0 at the lock
  because another push is running is not such a push and changes nothing.
  A pending attempt recorded for the earlier identity is not resolved
  against the new one.
- **Assurance is always shown.** Every output that prints the mode prints,
  for each qualifying kind, its `off_host` text and what it restores. A
  `directory` target reads "off-host asserted; not verified" in `status`,
  `doctor` and the `next` reminder, and a `graph` copy reads "planning,
  history and keyed memories only".
- **A check that runs out of time.** `status --check-target` against a
  target that answers too slowly to read the copy within the deadline
  reports `backup_check_timed_out`, records nothing, and leaves the kind
  qualifying on its recorded evidence. `doctor` contacts no target.
- **Failure is visible.** With an unreachable or unwritable target, push
  exits 1 with a typed code, the previous copy and receipt stay, status shows
  the failed attempt at once, and with the clock moved past the window the
  mode reads `local`.
- **No room.** With less free space in the stage than the store needs, push
  exits 1 with `backup_stage_no_space` and writes nothing; with too little
  room at a `directory` target it exits 1 with `backup_target_no_space` and
  leaves no partial file there. Status shows the failed attempt. When the
  space runs out after the check passed, during the capture or the `put`,
  the push still exits 1, records the failure, advances nothing and removes
  only this attempt's files.
- **Unknown record format.** A configuration or state file with a format
  version the build does not know makes the kind read
  `backup_record_unreadable`, makes push fail, and is left byte for byte as
  it was.
- **Capture refusal.** When the capture itself refuses, for example a
  `graph save` that refuses the store or a copy that fails its full check,
  the push exits 1, no receipt and
  no time advances, status shows the failed attempt at once, an earlier
  confirmed copy qualifies only until its window ends, and then the mode
  reads `local`.
- **A graph too large to restore.** A `graph` capture whose complete
  serialized file is one byte larger than the load limit makes push exit 1 with
  `backup_graph_too_large`, against a `directory` target and against a
  `git-ref` target alike; nothing is put, status shows the failed attempt,
  and the kind does not qualify on it. A document exactly at the limit is
  pushed, fetched and loaded. The test lowers the limit through the
  definition that `graph load` reads; it builds no 128 MiB document.
- **Unknown delivery.** When the copy reached the target but the receipt was
  not recorded, and the source then changed, the next push resolves the
  pending attempt as confirmed without uploading it again; freshness did not
  advance in between. A pending attempt that never arrived is dropped, and
  only its own recorded files are removed. A push cut off after the data
  file was renamed and before the manifest was written is completed by the
  next push when the file matches the recorded manifest, and removed when
  it does not.
- **One push at a time.** A second push started while the first holds the
  lock exits 0 without capturing. A push killed in the middle leaves the
  lock free once its process has ended, and the next push resolves what it
  left: a stage, a pending attempt, or a copy whose receipt was not yet
  recorded. A push that passes its transport deadline holds the lock until
  its process ends; a `put` the target completes after that deadline is
  resolved as confirmed by the next push. `target set`, `target clear` and
  `backup restore` refuse while a push holds the lock.
- **A check never overwrites a newer push.** A target check that read one
  receipt, and finishes after a push recorded a newer one, reports its
  result and records nothing.
- **Writers during capture.** In a fixture where a session commits at a
  fixed rate while a `store` copy is captured, the session keeps
  committing, the copy verifies at one cut, and the test prints the commit
  count and the slowest commit. A missing store is refused and nothing is
  created. For a `graph` capture, work and memory changes made
  during it leave a body whose two positions describe one state.
- **Restore and authority.** `backup restore` without `--origin-retired-by`
  is refused. With it, the output names the unexpired claims and issued
  grants with the last expiry of each, and the begun turns apart; the
  restored store's rows are unchanged; `status` shows the statement; and a
  new session's claim on a still-held item is refused until the old claim's
  expiry, then recovered with `--recover`.
- **Stored compressed.** The file at a `directory` target is a gzip file;
  for a compressible fixture it is smaller than the artifact, and an
  artifact that does not compress still makes the round trip.
  Decompressing the stored file with a standard tool gives bytes whose
  content fingerprint is the manifest's. A stored file replaced by one of
  the same size with other content is found by the ordinary `confirm` of a
  push and of `status --check-target`, and by `fetch`. A stored file that
  would decompress
  to more than the manifest declares is refused without writing the excess.
- **Retention.** Pruning never removes the newest confirmed copy, a failed
  push prunes nothing, and a file in the target directory that this home
  never recorded is left alone, as is a copy from another home or from an
  earlier identity of the target.
- **Disclosure authorization.** `target set` without
  `--disclosure-authorized-by` is refused for either kind and either
  adapter; `target show` and `status` print who authorized the disclosure
  and when; changing it changes the target identity, so earlier receipts
  stop qualifying.
- **Another build.** A receipt whose format identity the running build does
  not accept yields `backup_other_format`, and `backup restore` of that copy
  refuses with both ways on named.
- **Git remote.** Against a bare repository on the local disk used as the
  remote: put, confirm, a second put that adds one commit, and a refusal
  when the remote ref has moved. A dropped attempt whose push lands later
  does not make the next put fail, and that late copy has no receipt. That
  remote requires the operator's
  assertion and reads "off-host asserted; not verified"; a path, a
  `file://` address and a loopback host never read `remote_confirmed`.

## Fit with portable handoff

`portable` reuses the adapter requests and receipts, the manifest's cut, the
freshness rule and its status surface, the host trigger and the clean-home
restore test. It adds what backup must not have: its own payload (the closed
shared-state projection, without live authority or private scratch), a
compare-and-swap on the remote head, and writer epochs with release and
acquire. A backup copy stays inert bytes and gives no writer ownership. When
`status --check-target` finds a copy at the target that is newer than the
newest receipt this home recorded, it reports it: that is the first sign of
a second writer, and the only one this mode gives.

## Known limits

- A `store` copy is a whole file every time, so its cost grows with the
  store: about 13 MB a day on this repository before compression.
  Incremental copies arrive with the portable object tree.
- A `directory` target's place off the machine is asserted, not verified.
- Copies are not encrypted by Engram.
- A manifest's source revision is recovery information, not proof that the
  build can be reproduced. A `+dirty` revision names its base commit plus
  changes no commit records, and an `unavailable` one leaves finding the
  source manual. When the code remote is also the backup target, losing that
  remote loses both.
- Neither kind backs up the host's own state, such as its sessions and
  mailboxes.

## Decisions

Greg left these choices to the agents on 2026-10-02. The project coordinator
decided each on that date, on the two architects' joint recommendation.

| Decision | Chosen | Why |
| --- | --- | --- |
| Which kind is built first | `store`. `graph` follows | Only a `store` copy restores a store equal to its source |
| Which adapter is built first | `directory`. `git-ref` follows with the `graph` kind | A `store` copy needs a `directory` target |
| Default window | 24 hours | The claim ends within a day of the last fresh copy, and one missed night does not end it |
| Host cadence | At start, hourly while sessions are open, when the last session ends, and at least once per half window even when idle | An open session is a scheduling reason, not a change signal. Equal settled bytes can skip the full check and compression; ordinary activity still needs the full path. Successful pushes confirm the target without a separate check cadence. The window decides the claim and is not a promise of one hour |
| Default retention at a `directory` target | Three copies | Two earlier copies survive one bad copy, at about 220 MB compressed for this repository |
| Who starts the copies | The host. TermAl's coordinator agreed on 2026-10-02 that TermAl owns the trigger, the status display and session identities that a restored store has not seen, on the condition that a push must not distort a running gate's timing-sensitive stages; TermAl decides how, for example by putting the push off | Engram runs no daemon, and the host already runs Engram for each session |
| Who may read a target | Only the operator's own account | A `store` copy holds the whole store in readable bytes |
| Storage at a `directory` target | gzip-compressed | A folder that uploads sends every new copy whole |

Two statements stay with the operator and are made when a target is
configured: that the destination may hold what the kind carries, and that
a `directory` target leaves the machine. Engram records them and never
makes them.
