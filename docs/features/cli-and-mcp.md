# CLI & MCP Surface

> Normative reference: [spec §8](../spec.md#8-interfaces). Related briefs:
> [context packets](context-packets.md),
> [local work system](local-work-system.md),
> [atomic work plans](atomic-work-plan.md),
> [local tasks & reports](local-tasks-and-reports.md), and
> [behavioral control plane](behavioral-control-plane.md).

One core library owns classification, object storage, scope authorization,
task binding, deltas, and context-packet construction. The CLI and MCP server
are thin faces over it; transport code does not redefine memory policy. The
agent sees fourteen words; every host and operator control lives under
[Host integration](#host-integration).

## Using Engram as an agent

Record changed duties, waits, decisions, and the next permitted action with
`note REF --status TEXT` (MCP `note` with `status: true`), not only in a
conversation summary; record each real duty/wait/next-step change before going
quiet. After compaction or replacement, explicitly read `next --peek` before
acting and follow clipped status locators; a conversation summary may predate a
decision. A coordinator with no code work keeps an assigned or held
coordination item; `next --peek` is the resume read across fresh processes and
replacement sessions, never a transfer of execution authority. Storage marks
status at capture as owner-qualified only for the live holder session or the
assigned actor when unclaimed; other status notes remain peer observations.
Ownership uses exact actor-principal bytes; discovery's normalized search
matching does not make differently spelled actors the same owner.
For a wait that must survive claim expiry or session replacement, assign the
item to the accountable actor with `add --assignee ACTOR` or
`update REF --assignee ACTOR`. A held-only, unassigned status is current only
while that claim is live; after expiry or release it remains history, never
promoted into a commitment for an unassigned item. Assignment grants no
execution authority and needs no periodic claim renewal. A holder's planning
edit, including assignment, does renew its existing live claim.
`current_status` selects the newest owner-qualified note by the currently
accountable actor, using project-feed order, unaffected by ordinary notes or
gates; former-owner notes remain history. It appears at top level on `show`
and per held/assigned `next` row as `{body_or_first_line, complete, recorded_at,
locator, by}`. `by` is `you` for this actor and session, or a stable
project-scoped `peer-…` display label for another session, including a session
of the same actor. Actor-only records use a distinct `peer-actor-…` label.
Text follows the parent line
on `show` and is indented beneath `next` rows. Bodies start with a 768-byte
UTF-8 cap; larger bodies show a bounded first nonblank line. Final text/JSON
fitting may shorten either preview further before shedding resume rows,
setting `complete: false` with explicit omission and a note-detail command.
Compact `next` renders each identified status capture and latest note head
once, with `context_ref` on repeated discovery rows and references in change
summaries; ordinary verbose `next` retains the exact staged page and full
change summaries. Verbose peek instead carries a bounded unstaged preview.
References require the same immutable capture, never text similarity; absent
capture identity keeps the body. References retain change kind and actor
attribution, and note session markers precede untrusted note text.
If any retained status is clipped, receipt guidance requires reading its full
note before acting on approval or STOP conditions: a prefix grants no
permission. This guarantee covers status projections with `complete` and a
locator; ordinary note heads remain bounded previews with `note_detail`
navigation to `show REF --notes`, not status commitments.
Missing current-owner status is explicit on `show` and adds no `next` status
line; peer status observations appear separately. Older statuses remain in
`show --notes`. `add --external REF` and
`update REF --external REF` (MCP `external`) record audited opaque linkage as
`external_ref` (nonblank, at most 1024 encoded JSON bytes), shown by
`next`/`ls`/`show` and searched by `ls --search`;
capture source criteria in acceptance and source context in notes, because a
reference alone is neither immutable intake nor external synchronization.
The operator [source-intake workflow](source-intake.md) provides immutable
file-based preview/apply instead. On an imported item, ordinary `show.source`
reports its exact source key, notice count, older-notice omission count and
latest notice time, with an `engram import lookup` detail command. Lookup
keeps the original citation distinct from the latest proposed snapshot.
Source notices never apply local changes or count as execution evidence.
`update REF --clear-external` (MCP revise `clear_external: true`) removes
linkage through an ordinary audited revision, including catalog search and
subsequent snapshots. Setting and clearing together is refused; blank
`--external` remains invalid. Snapshots retain linkage and status provenance,
not live claims.
Opaque references retain their normalized bytes, including control characters,
through native writes and snapshots; terminal rendering frames those bytes.
An assignee's late status on completed work may update this advisory display;
it remains outside the frozen completion seal and grants no execution credit.

Engram tracks the work of this repository.

Engram MCP tools and host-provided `ENGRAM_*` configuration are injected into
TermAl sessions only when the project's Engram integration is enabled and
supported by the runtime. Hosting alone does not guarantee either. When the
injected `engram` tools are available, use them directly as the agent words.
Without that integration, a session may have neither the MCP words nor the
configuration variables.

Without injected tools, the CLI route requires an available `engram` executable
and an explicit home. Use an absolute path that the host or operator has
confirmed as the project's store home. Before any Engram store read or write,
including startup recovery, supply that confirmed home with `--home` or
`ENGRAM_HOME`. If it is missing or unverified, report the access/recovery gap
and ask the host or operator for it; do not guess a path, initialize a store,
or enable the integration yourself. `ENGRAM_HOME` has no default; without
`--home` or `ENGRAM_HOME`, the CLI refuses with `pass --home or set ENGRAM_HOME`.
The shell examples below assume this home configuration is already supplied.

With the integration enabled and supported, hosts normally supply
`ENGRAM_ACTOR_ID` and `ENGRAM_SESSION_ID`; optional `ENGRAM_ACTOR_CONTEXT`
adds attribution without changing the actor principal. Unlike home, either
actor or session may be omitted by a local CLI caller: Engram uses explicitly
audited OS-user-environment or synthetic-actor and process-session defaults.

Actor context may contain bounded free text such as
`model=opus-4.1;reasoning=high`. Actor context is
attribution, not a principal: assignment, `--mine`, handoff, and claim/session
authority continue to use the unchanged actor and session ids. Context never
refuses the session: Engram replaces each unsafe-control run with one space,
trims the value, and cuts it at a UTF-8 boundary to 256 bytes; altered input
receives an explicit `actor_context:normalized` provenance marker, and an
empty result is absent. It is excluded from retry/idempotency identity, so a
replay retains the original operation's attribution instead of duplicating or
refusing it.
A local shell that omits either principal value remains usable: actor derives
from the first nonblank conventional OS-user environment variable and session
defaults to one stable id for that `engram` process. The actor derivation is
asserted context, not an authenticated OS identity; if no conventional user
variable exists, Engram uses a synthetic process actor instead of refusing.
Durable actor provenance distinguishes `defaulted:os_user_environment`,
`defaulted:process_actor`, and `defaulted:process_session`. Explicit actor and
session ids are recorded verbatim. A live caller, planning-actor,
handoff-recipient, or control session-bind participant and actor session id is
admitted only when it is at most 64 UTF-8 bytes; longer values refuse before
store, session, focus, attempt, offer, planning-write, or task-bind effects,
with a bounded error that does not echo the rejected id. The same live
length-only admit applies to a generic note-capture actor session, a
graph-snapshot save or load operator actor, a control-policy administrator
actor session, and project-memory remember, forget, full, or list caller
sessions. A caller-supplied catalog `held_by` filter is length-admitted the
same way: that is live filter admission, not validation of a persisted claim
holder. A persisted claim
holder used only for comparison is not length-admitted. That length is
inclusive and length-only: it does not trim, normalize charset, or rewrite
stored historical ids. UUID
(36), TermAl `session-<n>`, and the generated `local-process-v1-<pid>-<uuidv7>`
form (max 64) all fit. Because separate shell invocations are
separate processes, multi-command ambient workflows still need a host-injected
stable session id. The `local-process-` prefix is reserved for generated
process-default work sessions; a `local-process-v1-*` id may be reused for
seven days, after which the caller must omit `--session-id` to receive a fresh
process default. Every defaulted-session invocation prints its generated id and
that exact reuse instruction. A successful mutating word with `--json` also
returns that id as top-level `effective_session_id`; read receipts, explicitly
bound CLI or MCP receipts, and the host-only `work core` protocol retain their
existing shape.
`next` can stage a delivery cursor, but remains a read receipt by this
contract: compact `next` relies on the stderr notice, while verbose `next`
already returns its session object.

Reading an item never steers where a later write lands: `show REF`, including
notes, history, continuations and note detail, preserves ambient focus and
staged delivery. Use the explicitly targeted commands offered by its receipt.
Claiming or explicitly targeting a mutation establishes focus, except that a
`note`, `gate` or `evaluate` naming an item this session does not hold leaves
focus where it was (see
[turns, focus and evaluation timing](acceptance-evaluation.md#turns-focus-and-evaluation-timing)).
A bare mutation keeps its existing target, not the item just read. These
reads do not register a fresh process-default session; registration waits for
a stateful operation.

`add` and `add --under` do move focus, to the item they create. So a bare
`note`, `gate`, `evaluate`, `update` or `done` acts on the focus only when
this session holds it, or holds nothing else. When the focus is an item this
session does not hold while it holds others, the word is refused with
`work_implicit_target_conflict`:
- nothing is recorded;
- the refusal names the focus and up to three held items, counting the
  rest;
- `next` offers the explicit command for each. For the focus it offers only
  a command its state admits: `details.focus_state` is `unclaimed`,
  `held_elsewhere` or `not_open`, and the command is a claim where the word
  needs one, a late `gate --work-ref` on finished work, or `show` where
  nothing else applies.

Repeating the word with the item named acts as before. A bare `handoff` keeps
the focus without this check: its recipient accepts an item it does not hold
yet, and an offer or a cancel already needs the claim.

These are the reads that record nothing:

- `next --peek` (MCP `next` with `peek: true`);
- `ls` in every form, and MCP `search` (CLI `ls --search`);
- `show` in every form: plain, `--full`, `--notes` and `--notes --gates`,
  `--history`, `--note LOCATOR`, `--evaluations`, `--evaluation RECORD_ID`,
  `--observations`, and their continuations;
- `memories` in every form but one: the listing, a search, a continuation
  page, `KEY --full` and `KEY --full --revision N`;
- the host's `work core held` and `work core inspect`.

Each opens the existing store read-only for that one call: it never writes
database or WAL bytes, needs no write access to those files, and never
creates or initializes a store. On a path with no store, or a file with no
schema, it refuses with `store_not_initialized` and `engram init` guidance,
and it never retries through a writable connection. SQLite may still create
or map the store's coordination files (the shared-memory `-shm` file, and an
empty `-wal` file that it removes again when the last connection closes); a
live store that a writer has open already has both. The one `memories` form that records is the first
page of an unfiltered listing that carries `--context-generation` (MCP
`context_generation`): it reads like the others and then records the
listing for the session through the writable connection (see the peek
contract below). Where that record cannot be written, the listing is still
delivered. Every other word, and ordinary `next`, is stateful.

```bash
engram work next --peek [--verbose]  # orientation without advancing delivery
engram work next [--verbose]         # explicitly advance ordinary delivery
engram work ls [--search TEXT] [--ready | --blocked] [--mine] [--label L] [--all] [--under PARENT [--optional | --required]] [--limit N] [--after CURSOR] [--verbose]
engram work show REF [--notes [--gates] | --history] [--after CURSOR]
engram work show REF --note ID[:INDEX]  # complete immutable note detail
engram work show REF --evaluations [--after CURSOR]  # the run's evaluation records, oldest to newest
engram work show REF --evaluation RECORD_ID  # one evaluation record complete
engram work show REF --observations [--after CURSOR]  # the run's source observations, oldest to newest
engram work add "Title" [--note "Initial finding"]... [--outcome "..."] [--accept "criterion"]... [--bind POSITION=KIND[:FINGERPRINT]]... [--under REF [--optional]] [--priority 0-4] [--kind KIND] [--label L]
engram work claim REF [--ttl SECONDS] [--recover "why"]   # same holder renews; --recover is for another prior holder
engram work claim --under PARENT [--ttl SECONDS] [--recover "why"]   # hold the parent's next ready child, chosen in ls --ready order and claimed in one transaction
engram work update REF [--release [--reason "why"] | --blocked "why" | --unblock [--blocker SELECTOR] | --cancel "why" | --reject "why" | --after OTHER | --drop-after OTHER | --waive CHILD --reason "why" | --supersede-with NEW --reason "why" | --assignee A | --priority N | --defer DATE | --accept "criterion"... | --bind POSITION=KIND[:FINGERPRINT]... | --title "..." | --kind KIND | --label L | --unlabel L]
engram work gate NAME [--work-ref REF] [--failed FAILURE]... [--ref opaque-reference]
engram work note [REF] "What you found or decided" [--ref path-or-url]
engram work done ["What was delivered"] [--link POSITION=LOCATOR --link-basis N] [--landed COMMIT --remote R --branch B --pushed-at RFC3339 [--installed-build FINGERPRINT]]
engram work handoff REF --to SESSION | --accept | --cancel "why"
engram work remember ("Project note" | --text "Project note") [--key KEY [--revise [--expected-revision N [--append | --section NAME]] [--clear-retires-with]]] [--retires-with local:REF|external:PROJECT#REFERENCE]
engram work memories [QUERY] | engram work memories --after KEY | engram work memories KEY --full [--revision N] | engram work memories --context-generation GENERATION
engram work forget KEY
```

Human receipt fields use the existing terminal text policy before byte
bounding, including compact next/list titles, labels, holders, show outcomes
and blockers, child rows, and guidance. Single-line prose fields flatten whitespace;
multiline acceptance and note bodies keep indented newlines and fold tabs to spaces.
Printed commands escape unsafe characters but preserve every safe literal byte, including repeated spaces inside quoted arguments.
The same prefix-free `terminal_error_line` / `terminal_error_command` helpers serve those receipt fields and CLI error lines; they are not a store or JSON sanitizer.
Every `anyhow` `Err` returned from `run_cli` prints one framed `error: <cause>` line per cause on stderr (Display order, exit 1), in any output mode, including `--json` and core input or argument errors. Work-word text refusals frame the message and reminder lines the same way. `next` commands keep safe quoted spacing. The host-path probe `WARNING` uses the same line policy. Clap help and parser diagnostics stay on clap's writer. `--version` is a custom `DisplayVersion` branch that prints build identity, not clap's version writer and not this error renderer. Panics still unwind. Structured JSON success receipts and structured JSON refusal envelopes keep source projections without terminal sanitization; this includes `--json` work-word envelopes, core `StoreError` envelopes, import, and doctor. Project-file `terminal_detail` is a separate refusal framer and is unchanged. This is not a blanket stdout/stderr sanitizer and does not rewrite the store.
Structured JSON retains its existing source projections, not terminal escapes;
this includes both MCP structured content and its equivalent JSON text content.
See [text framing](local-work-system.md#agent-native-protocol).

Add `--json` to any word for its structured receipt. Agent reads are short by
default in text, JSON, and MCP: `next` and `ls` return only navigation rows
and one-line changes, while `show REF` returns one safe detail view. Structured
`show` keeps short refs, planning state, holder words, relations, blocker and
note summaries with their evidence kind, meaningful history, a superseded
item's successor short ref, and allowed actions. Its exact note total and
latest note are independent of the bounded evidence page; latest means the
highest dense run-feed position for execution evidence, or their shared
root-feed position when non-holder observations are present. Evidence
timestamps are asserted metadata, never ordering authority. Observations fill
spare evidence-page slots without displacing selected execution evidence.
Selection decides which rows the bounded page keeps. On a full page the latest
note replaces the least-priority selected note. `notes` then emits every kept
row in the item's dense root-work feed order, whatever its asserted timestamp,
so the latest note comes last. This is the order of the rows this page keeps;
the `--notes` window below pages the item's whole note stream instead.
`notes_omitted` is the exact remainder after all fitting, while
`evidence_count_limit` reports its count-limit share. Open or proposed children
precede terminal children inside the bounded relation page,
so terminal history cannot hide unfinished work while page capacity remains.
Text prints `(+N more)` and structured output carries the exact
`children_omitted` total; when any child is omitted, both the children line and
`children_navigation` name `engram work ls --under PARENT --all` so terminal
optional members remain reachable even when neither obligation group links them.
Typed count omissions distinguish unfinished from
terminal children that did not fit. Show, note/history windows and detail,
compact holders, statuses, reminders and changes use the same display labels.
`you` identifies this session, not every session using its actor. Another
session has a deterministic project-scoped `peer-…` pseudonym; actor-only
attribution uses `peer-actor-…`. Labels do not depend on row order or the reader.
Bounded host-asserted context may follow the label in parentheses.

These are display pseudonyms, not anonymization. Low-entropy actor/session
inputs are dictionary-guessable. No command resolves a label as an alias for
a real identity. Other identity arguments remain literal asserted strings,
not aliases. Handoff is a usability exception: `--to` refuses generated
`peer-` and `peer-actor-` label shapes before any write, so copying a display
label cannot create an offer the intended recipient cannot accept. Ask the
host or coordinator for the recipient's real session id. This refusal does
not authenticate the target or resolve aliases. Stored audit attribution is
unchanged. Arbitrary bodies and host context may themselves identify people.
Ordinary terse show represents work items by short refs. It omits raw
actor/session metadata, claim fences, and host-only run, claim,
control-binding, obligation-page, and memory-version fields. It retains
note/detail locators, sealed evidence links, and an open item's
`acceptance_basis` when it has criteria to link; the basis is a
read-concurrency token, not execution authority. Acceptance evaluation
exposes its full record id in JSON; the text evaluation summary uses a
12-character prefix. The evaluated work revision and any source fingerprint
also remain visible. Humans and hosts that need the rich projection use
host-only `work core focus`. Core Summary focus, including core `next` and
`work_propose`, bounds `outcome` to 192 UTF-8 bytes like other summary
fields; `show REF --full` returns the complete authored contract. Full
list projections remain available through
`next --verbose` and `ls --verbose` (or the equivalent MCP arguments).
Verbose JSON/MCP retains rich raw identity and integrity fields. It is an
explicit diagnostic option, not a safe variant of terse show. This optional
presentation policy is not a global confidentiality or authorization boundary.
The agent `work_claim_held` error uses `details.work_ref` and the display
`details.holder`, not `work_id` or `holder_session_id`. It retains `expires_at`,
`expires_at_ms` and `remedy`; its message and reminder use a human-readable
expiry. CLI JSON and MCP carry the same envelope. Host-core errors keep their
raw identifiers. Other work-error variants are not covered by this conversion.
Project-memory attribution, caller-owned process-default session notices and
encoded continuation context also retain their documented contracts. Compact
rows retain up to 80 UTF-8 bytes of title, omit redundant lifecycle and blocked
fields, cap labels, and report `labels_omitted`. When fitting an oversized
advisory response, `next` sheds discovery rows before any existing section,
then sheds labels from the least-important navigation rows before dropping
rows; `ls` does not shed labels. Compact `next` uses the same
12 KiB agent-response ceiling as its core view, so the default limit of 20
remains meaningful. Section removal is recorded in explicit `omissions`
instead of failing.

Compact `next` and ordinary `show` fit their complete emitted CLI text and compact
application-receipt JSON (`serde_json::to_vec`), including guidance (and the
`next` build footer), after projection. CLI text includes the final `println`
LF. Both that terminal measure and compact JSON must stay strictly under
12288 bytes; pretty JSON is not a production or acceptance measure. Compact
JSON stays payload-only and does not charge that LF. The same
pair binds verbose `next`, `ls`, and note/history windows. `done` refits its
post-completion envelope after attaching a process-default
`effective_session_id`. Ordinary mutation receipts
are compact and have no receipt fitter; `add` may still refuse after commit
if a reminder pushes that already-written receipt over the ceiling. An
oversized stored title does not cause a post-commit budget-only refusal:
core summary focus bounds `outcome` with the same 192-byte compact text as
other summary fields, covering both the nested `work_propose` envelope and
the inner `work_focus` view. Mutation `work.title` remains that existing
192-byte summary. The complete canonical title and outcome remain stored;
ordinary `show` JSON `status.work.title` is the 192-byte summary with
shortening disclosed. Its outcome stays complete when it fits; otherwise
the whole outcome is omitted with its byte size and `show REF --full`
navigation. Hidden core metadata does not consume that budget or cause visible
rows to disappear.
Summary and relation limits still apply; host-only core and verbose views
retain their own rich-response fitting. Safe `show` reads acceptance criteria
in full, without the summary's 192-byte truncation or six-criterion cap. If the
final receipt cannot fit, it removes whole criteria from the end and reports
the exact `status.work.acceptance_omitted` count; retained criteria never gain
an ellipsis. JSON retains their stored bytes. Terminal output escapes unsafe
controls and frames every continuation line as criterion data. Omitted
criteria remain available through `show REF --full`.

`show REF --full` (MCP `show { work_ref: REF, full: true }`) is an explicit
complete authored-contract read, not a verbose host projection. It returns
the stored title, outcome and entire acceptance list, together with the short
ref and item revision from one read snapshot. JSON adds the criteria's
verification bindings as `work.acceptance_bindings` under any policy,
omitted when there are none; the text marks each bound criterion as ordinary
`show` does. JSON retains exact stored text;
terminal text frames unsafe controls and multiline content as data. It does
not expose host claims, fences or control bindings. The revision identifies
the read version, not execution authority. Like `show --note` full-note
detail, this explicitly requested full-text receipt may exceed 12288 bytes;
ordinary `show`, notes/history windows, lists and orientation stay bounded.
The `--full` mode cannot be combined with `--notes`, `--gates`, `--history`,
`--after` or `--note`. Neither full nor ordinary reads select focus, register
a session, stage or acknowledge delivery, or mutate the work item.

`add`, `claim`, `gate`, `evaluate`, `note`, and `done` share a compact mutation envelope.
`operation` and its result facts accompany exactly one `work` summary
(`short_ref`, title, lifecycle, revision). Live `claim` context contains only
relative `holder` and `held_until`, never a fence or control binding.
`obligations.open` counts every open obligation on the run, including any the
source obligation page leaves out (its `open_total`); only a page stored before
that count existed falls back to the open entries it shows.
`obligations.omitted` retains its exact undisplayed count, not an assertion
that omitted entries are resolved. Actionable reminders, source omissions,
refusal `code`/`remedy`/`recovery`, and done's child-follow-up groups remain.
A stale-evaluation refusal whose source move an observation decided adds
`recovery.deciding_observation` beside the unchanged `recovery.cause`, as
[acceptance evaluation](acceptance-evaluation.md) describes. The CLI writes
JSON receipts so that no field spells the locked-store phrase, with its spaces
as `\u0020` escapes; every field still decodes to what was recorded, and the
response budget measures that written form.
Creation names root/child kind and any parent/requirement; gate reports name,
pass/fail, failure count and reference presence; note retains its evidence
locator and a distinct checkpoint when present; done retains seal and time.
Repeated focus, status, planning, history and parent projections are absent.
One ASCII-quoted `full_detail` command restores item detail (`--notes` for
add/note, `--notes --gates` for gate). Text prints it once as `full detail:`.
Those bounded item reads in turn offer `show REF --full` for the complete
authored contract, including text omitted to keep the item overview small.
`build_fingerprint` remains once where already supplied (currently `next`);
successful process-defaulted shell mutations still add `effective_session_id`
on the receipt. Only `done` then refits that envelope; other mutation words
do not run a receipt fitter.
The six-operation envelope and host-private protocol stay the same; core
Summary focus bounds `outcome` text like other summary fields.

Rules that matter:

- Compact `next`, including `--peek`, shows held and assigned work before
  at most five ready candidates. A smaller `--limit` reduces this prefix;
  a larger limit does not raise the compact cap. `ready_limit` states the
  effective requested limit, clamped to 1..5, even if fewer rows fit. Text
  prints the cap only when candidates remain. A ready row carries an
  optional distinguishing `ready_reason` from the same readiness
  projection that selected it, including prior-claim recovery when present.
  This is not claim permission: inspect the item before claiming it.
  `ready_more` is a boolean, not an exact backlog count. When true,
  `ready_next` and the text's `more ready candidates` command lead to
  `ls --ready`, after the last row retained by byte fitting. If no row fits,
  the command starts a fresh ready listing. These fields participate in
  fitting and navigation is retained even when all candidates are shed.
  Compact ready candidates and `ls --ready` (compact and verbose) use priority
  ascending, then work id ascending. Work id is a deterministic tie-break,
  not a chronological guarantee. Ordinary `ls` without `--ready`, verbose
  `next`, and host-core catalog queries keep catalog id order. The listing
  cursor binds the advisory snapshot and the
  last emitted `(priority, work_id)` key after byte fitting; a changed or
  expired cut, including feed, priority, or time changes, refuses with a
  fresh same-filter `ls --ready` command. Exactly-once concatenation holds
  only while that cut remains valid. `ls --ready` cannot be combined with
  `--blocked`. Compact rows omit the constant plain-ready sentence and keep
  additional reasons such as prior-claim recovery. Show claim reminders and
  verbose/core reason codes are unchanged. Verbose `next` retains its
  requested richer list limit. Delivery and memory advertisement behavior
  are unchanged. Host-core `ready_work` ranking is a separate path.

- `next --peek` (MCP `next` with `peek: true`) answers what you hold, what is
  ready and what changed in one read snapshot. It opens only an established
  store, read-only, and never initializes or repairs one. It does not stage,
  acknowledge, move a cursor, change focus/claims or register a default
  process session. Text says `delivery: not advanced`; JSON carries
  `peek.delivery_advanced: false`, no delivery token and no delivered-through
  position. An existing pending page stays byte-identical. The preview starts
  at the confirmed cursor, using current read authorization rather than
  treating a tentative page as delivered. Compact mode scans at most eight
  bounded pages locally to find visible changes; verbose reads one page.
  Repeating peek does not paginate. `peek.more_changes_available` means more
  feed entries outside the retained preview, not an exact peer-change count.
  This orientation question is shared with ordinary `next`; the preview does
  not promise the exact page a later advancing `next` will return.
  It never writes the persistent database or WAL and never retries through a
  writable connection; SQLite may recreate a shared-memory coordination
  sidecar. Missing or schemaless stores refuse with `store_not_initialized`
  and explicit `engram init` guidance, not a different-build diagnosis. Other
  access/recovery refusals must be surfaced and investigated before using
  ordinary `next` when writes and delivery advancement are permitted.
  Text/JSON fitting may omit rows with explicit counts, but never the
  non-advancement disclosure, memory signal or `memories_detail` command
  (`engram work memories`, carrying the host's context generation while a
  listing is due, as described below). That command also remains in `next`. Memory
  `changed` compares the recorded advertisement, not whether notes were read
  or applied. Pure reads, including every `memories` form without a context
  generation, do not acknowledge it; ordinary `next` retains its existing
  rendered-signal acknowledgement of the memory position and never records a
  context generation.
  When a peek carries a context generation that no recorded `memories`
  listing of the session carries (a session with no record carries none), a
  listing is due. The peek's text then opens with "the host reports a new
  context for this session: before acting, list project memories through the
  continuation and read the relevant current entries in full" and, on the
  line under it, the command
  `engram work memories --context-generation GENERATION`. The structured
  receipt carries the direction as its first reminder, that command as its
  first `next` command and as `memories_detail`, and
  `peek.memory_listing_due: true`; the text's memory detail names the same
  command. Fitting never sheds the direction or the command, and the
  direction precedes every other reminder. A peek with no context generation
  gives no direction. The direction reports the host's assertion and what is
  recorded, never that a compaction happened.
  Only the first page of an unfiltered `memories` listing that carries
  `--context-generation` (MCP `context_generation`) records anything: once
  that page has been rendered, it records the generation and the memory
  position its snapshot read. Searches, `--full` reads, history reads and
  `--after` pages accept the argument and record nothing, and without the
  argument no `memories` form records anything. The record shows that a
  listing was delivered, not that notes were read or applied. Writing it is
  best-effort: when the store cannot be written, or a writer stays busy for
  the store's ordinary five-second wait, the listing is still delivered
  after that wait and the direction stays. An ordinary `next` that is given
  a context generation keeps reporting `changed` until a listing carries it.
  A context generation is a plain token, 1 to 256 ASCII letters, digits, dots, underscores or dashes, not starting with a dash;
  `next` and `memories` refuse any other value, so the printed command always
  carries exactly the value supplied.

- `show PARENT`, including first `--notes` pages and MCP, carries
  `child_obligations.required_owed` and `child_obligations.open_optional`
  when the item has any direct children, even when both groups are empty.
  Each group has an exact `count`, at most five `items` with `ref`, title and
  remedy, an exact `omitted` count, and scoped `navigation`; byte fitting may
  omit more whole refs but preserves both counts and commands. These totals
  use the complete child set in the same read snapshot as the show projection,
  not its bounded `children` rows. When the generic `children` line omits
  rows, it and `children_navigation` name `engram work ls --under PARENT --all`.
  Required owed means unfinished required
  children or disposed required children without a current revision-bound
  waiver; completed native or restored children are not owed. Open optional
  follow-ups never block completion. Traversal uses
  `ls --under PARENT --required`, adding `--all` if any owed child is disposed
  (a superset including completed/waived siblings), or
  `ls --under PARENT --optional` for open optional follow-ups. Disposed owed
  rows under Open parents name the explicit
  `update PARENT --waive CHILD --reason "…"` remedy; under terminal parents
  they offer `show CHILD` and explain that retained child context needs
  inspection, not an unavailable waiver. Other rows offer `show CHILD`.
  A leaf has no block. This is advisory current-state accounting, not
  completion proof or execution authority.
- Claimless `next` includes nonempty `assigned` and `participated` sections
  between held and ready work, at most five rows each with exact omitted counts.
  Full rows name the work, title, holder word, and first line of this session's
  latest own note when present. For the reader's actor, compact rows use
  `note_by: "you"` and text prints `[note session you]` before the body.
  Rich verbose JSON retains the original `note_session_id` instead.
  Compact repeated rows instead contain only `{ref, context_ref}`; the
  presence of `context_ref` is the discriminator. It names the retained
  `held REF`, `assigned REF`, or `participated REF` primary row containing
  the full projection. Do not read title, holder, status or note from a
  reference row. Verbose rows retain the full shape.
  Another actor's session field is omitted. This is asserted attribution,
  not authenticated identity. This is recent-work discovery, not a review
  obligation or claim; keep owed decisions on a claimed coordination item.
  See the [resume discovery contract](local-work-system.md#agent-native-protocol).
- `update CHILD --detach "why"` (MCP `update { work_ref: CHILD, action:
  "detach", reason: "why" }`) atomically creates an independent root and
  supersedes an Open child stranded beneath a terminal ancestor. It copies
  title, outcome, acceptance, kind, labels, and priority with source provenance;
  assignment and history stay on the child. The receipt names the new root
  and its claim command. `show` on that root exposes `detached_from` with the
  original ref and recorded reason, plus a `show ORIGINAL` next command;
  source notes and gates stay on the original with their attribution.
  The reason is complete, or omitted whole with `reason_omitted: 1` when the
  final response budget requires it. The original ref and navigation remain;
  `show` never substitutes a shortened reason. JSON preserves stored text and
  terminal output uses safe single-line framing.
  No parent reopen or old claim/fence change occurs.
  Sealed/terminal root executions stay unchanged; a still-open root's live
  execution receives cancellation's audited waiver for a missing contributor.
  Open descendants, live ownership, independent blockers, unfinished
  prerequisites, and future deferral refuse with `work_detach_refused` and a
  remedy naming what to resolve first. `show CHILD` and `ls --blocked` display
  the terminal-parent cause and exact detach command when admitted. `next`
  does so when the child is already focused; reading it does not select focus.
  Inspect the old child's successor after an uncertain response; a keyless
  repeat after supersession refuses without creating another root. See
  [detached follow-ups](local-work-system.md#gates-prerequisites-supersession-and-project-memories).
- `show CHILD` names its direct parent with `parent_ref`, `parent_title`, and
  `parent_lifecycle`; `status.work.child_requirement` is always `required` or
  `optional` for a child. Text includes `parent: REF "title" (lifecycle),
  required|optional` and `next` offers `engram work show PARENT`. This relationship
  survives acceptance/note trimming. Roots print `parent: root` and omit
  parent fields and child requirement. The safe focus read loads one bounded
  parent row in its existing snapshot; its private carrier does not change the
  ambient/core wire. CLI JSON and MCP agree.
- `show REF --notes` (MCP `show { work_ref: REF, notes: true }`) returns
  the newest note/observation window, excluding structured gate evidence,
  rendered oldest to newest within that window. `--notes --gates` (MCP
  `notes: true, gates: true`) adds gate rows through the same window reader.
  The default page states the item's gate-evidence count once and offers that
  explicit command when gates exist. Gate-like prose is still an ordinary note.
  Inherited generations precede native dense project-feed positions, not
  asserted timestamps. `notes[].summary` is the complete body, never a
  shortened preview. Text and JSON windows, including guidance and cursor,
  fit 12 KiB. `notes_omitted` is the exact total minus shown;
  `notes_window` states `shown`, `total`, `newer`, `older`, and `after`.
  These counts describe the selected stream. `notes_window.families` gives
  item-wide `notes`, `observations`, and `gates` totals, each with exact `shown`
  and `omitted` counts; excluded gates count as omitted in their family, not
  as omitted notes in the default stream. JSON rows keep the plural `family`
  values `notes`, `observations`, and `gates`. Text rows mark the same families
  as `[note]`, `[observation]`, and `[gate]` immediately after their locator;
  the marker comes from stored kind, gate structure, and observation provenance,
  never from body prose. The complete `--note LOCATOR` detail uses that marker
  without changing the body.
  `includes_gates` records the mode. This choice is bound into the existing
  cursor and preserved in continuation and fresh-window guidance. Switching
  it requires starting a fresh window. Gate detail locators work in either mode.
  Follow the printed continuation command for older notes; it retains
  `--gates` when that mode was requested.
  Every notes/history page reflects `byte_budget` and `read_cut`
  (`project_position`, `observed_at`, `valid_until_ms`) in its window metadata
  and prints the active byte ceiling and cut. These describe this read, not
  delivery acknowledgement or execution authority. A `--after` page carries
  only a compact `work` header (ref/title), counts, records, navigation and
  one `full_detail` item-read command; it does not repeat outcome, acceptance,
  completion or child-context projections. The first page keeps ordinary
  item context. Both forms fit the same 12 KiB text/JSON ceiling.
  Every continuation retains its shared quoted `full_detail` command in
  `next`, including an exhausted page, so item context remains reachable
  through the runnable navigation list.
  `--history` (MCP `history: true`) uses the same window fields under
  `history.window`, with records in `history.items` and exact `omitted`.
  Its row `family` is `history` for events/completion, or `notes`,
  `observations`, or `gates` for inherited note members; no `notes_window`
  is emitted for this mode. `history.window.families` counts those four
  families over the combined stream, with total/shown/omitted for each. Text
  marks history-family rows as `[history]` and uses the same note-family markers
  for inherited members; JSON family values remain plural.
  This explicit mode replaces ordinary show's native-change `history` and
  separate `restored_history` with one stream: inherited notes, events and
  completion members, then native work events. Its `history.total` counts
  that combined stream; ordinary show's total counts only native changes.
  Window rows carry `locator`, `kind`, `summary`, `by`, `created_at`, and
  `body_bytes`, rather than ordinary show's compact change-row shape.
  Note-family rows in explicit window/detail JSON carry the native project
  `feed_position`. Attribution uses `by`, never a raw `actor_session_id`.
  Inherited members omit the feed position rather than substituting their
  member ordinal. Positions and display labels grant no authority.
  Consumers distinguish these shapes by the presence of `history.window`.
  An inherited note summarized in history retains its exact original body
  size and adds `summary_truncated: true` plus a `detail` command when
  shortened. The detail read returns the complete note, not that summary.
  Ordinary show advertises this history reader. Notes and history are mutually
  exclusive; `after` requires one of them, and `gates` requires `notes`.
  A cursor binds item, project, kind, immutable boundary/member, order, the
  window's selected total and the observed time. A new record in the window,
  a missing boundary, a mismatch or a reversed clock refuses with
  `work_show_cursor_invalid` and a fresh same-kind command; a write elsewhere
  in the project does not. The refusal states its reason once. Tokens encode
  readable context, are not confidential, and grant no authority.
- Every full-note row prints a copyable `locator`. Native notes accept a unique
  prefix of the record's id, at least eight hex digits of an id of 32 or 64;
  inherited notes use `RECORD_ID:INDEX`, where INDEX is the one-based immutable
  member position,
  not a display ordinal. `show REF --note LOCATOR` (MCP `note: LOCATOR`)
  returns the complete body and references, with `body_bytes` in UTF-8, and
  deliberately may exceed 12 KiB. Ambiguous or wrong-item references refuse
  with `work_note_reference_invalid` and candidate locators when available.
  A body too large for a window is represented by `body_omitted: true`, its
  byte size and a `detail` command instead of `summary`/`refs`; its member
  still advances continuation, so it cannot hide older notes. Bodies and
  reference lines are framed as data in text; JSON preserves exact content.
  These existing canonical locators are read-navigation exceptions, not
  execution tokens. No per-note identity is invented for inherited members.
- For a native verification record, `--note LOCATOR` adds `assessment`: each
  obligation of the record's check kind on its run, reconstructed at the
  record's own run-feed position under the current matching rules
  (`label`), as `status` `matches`, `mismatch` with the matcher's first
  `mismatch` code, or `left_out` with its reason, beside the obligation's
  `recorded` end as stored. A `stale_source_revision` row adds
  `stale_source`, the record that decided it:
  - `decider` is one of:
    - `latest_change`: the run's latest source change;
    - `root_sighting`: the active named root's newest sighting;
    - `root_binding`: the root's binding;
  - its run-feed `position`, `workspace`, `revision`, `root_generation`, and
    whether it reported a change;
  - beside them, the check's own `verification_workspace` and
    `verification_revision`.
  Text shows it as one line under the row. At most eight rows are shown, with exact `total`,
  `shown`, `earlier` and `omitted` counts; its `continuation` command,
  `show REF --note LOCATOR --after CURSOR` (MCP `note` with `after`), shows the
  rest and refuses once the run has moved on. See the
  [local work system](local-work-system.md) for what each part means.
- Newly written note bodies are limited to 64 KiB of normalized UTF-8 text.
  `work_note_too_large` reports actual bytes, limit and the remedy to carry
  bulk content as a reference. Initial-note batches remain atomic. Existing
  larger bodies remain readable through `--note`; read validation adds no
  retroactive limit. Default terse notes retain their existing summary shape.
- `--bind POSITION=KIND[:FINGERPRINT]` (MCP `bindings`) on `add` and `update`
  binds the criterion at that one-based position to host verification of
  that kind (`test`, `build`, `lint`, `review` or `acceptance`), optionally of
  one exact check. `FINGERPRINT` is that check's command fingerprint: the
  `check_fingerprint` the host records on its verification evidence. It is
  never a record id: no evidence could match one, so the id of a stored
  record is refused where the binding is authored, whatever its shape.
  Positions follow the call: with `--accept` in the same call
  they count the acceptance list as typed, blanks included, and a binding on
  a repeated criterion lands on its first occurrence; a binding on a blank is
  refused. `--bind` alone on `update` counts the stored list, as `show`
  numbers it. `show` marks each bound criterion `[requires host KIND verification]`
  and counts bound criteria behind a clipped list. A bound criterion opens a
  typed obligation on the item's run, so `done` refuses until the host has
  minted passing verification evidence of that kind (recorded through the
  control turn checkpoint; a host that runs no control plane can only revise
  or waive a bound requirement); a newer failed check of
  that kind contradicts it, and a check that does not verify the run's latest
  observed source change (judged by the source revision it ran against, not
  by when it was recorded) no longer carries it. Both are enforced at `done`,
  not when an evaluation is recorded; the refusal's remedy is a passing check
  of the changed source, or dropping the binding. Under an evaluated policy
  the criterion keeps exactly the citations it was judged on, and its pass
  needs an `observed` basis citing that evidence — never judgment or a gate
  record. Revision changes the requirement: `--accept` without `--bind` drops
  the bindings and the receipt says so; a dropped binding's obligation is
  waived in the revising actor's name, a new one opens from that revision,
  and a criterion rewritten under an unchanged binding owes its verification
  again. Obligations are keyed by position, so reordering a bound criterion
  owes its verification again too.
- `update REF --accept "criterion"...` replaces the whole acceptance list in
  one attributed revision. Omission preserves it; empty lists and any blank
  criterion are refused. The core trims criteria and drops a repeat of an
  earlier one, keeping the order given, so stored position N is the Nth
  criterion kept. A revision that only reorders the list is a real revision:
  it carries a failed evaluation like any other change to the criteria.
  Revisions stored sorted by an earlier build read as stored.
  History names the supplied fields, and prior canonical criteria remain in
  history. Completed work is immutable; `note` is the late-finding path.
- `ls` prints `showing X of N` and returns exact `total` and `omitted` counts
  beside `more`. Count and page share the same normalized filters and SQLite
  read transaction. `--mine` is assignment to this actor OR a live claim held
  by this session, counted once before limiting. The default limit is 20
  (explicit limits clamp to 1–1000). The complete text and JSON receipts,
  including footer and continuation, fit 12 KiB. The footer names the active
  `limit` and `byte_budget` (12288 bytes). A continuation is refused only
  when the listing's membership or order changed since its page
  ([local work](local-work-system.md)), and the refusal states its reason
  once. Nonempty truncated pages include an
  `after` token and one `next` command repeating all filters and the
  active limit with `--after CURSOR`; MCP `ls` accepts the same `after` value.
  The cursor names the last row actually emitted, including after byte fitting.
  `shown_before` counts the prior prefix; `omitted` is the remaining total after
  that prefix plus this page, and `more` is true exactly when some remain.
  Ordinary listings stay ascending work id. `ls --ready` uses priority then
  work id, matching compact `next`. A changed membership or order (an item
  entering or leaving the filtered set, an equal-count replacement, a priority
  move or a time transition that changes either), a reversed clock, a
  malformed cursor or one without its basis, or different filters/project
  returns `work_catalog_cursor_invalid` with a fresh same-filter command, never
  a silent restart. Unrelated project notes and other writes that change no
  member or order do not, and neither do focus-only reads. Only a token minted
  by compact `next`'s ready navigation keeps the conservative basis, where any
  project-feed advance or crossed time boundary refuses it. Tokens are opaque to the caller, not confidential: they encode readable
  filters, project and session context. They are not encrypted, authenticated,
  or execution authority, and have no server-side state. Avoid sharing them
  when that query context is sensitive. Filter text must be single-line and
  terminal-safe; commands use ASCII quoting syntax shared by PowerShell and
  POSIX shells. If continuation metadata prevents even one otherwise fitting
  row, the query refuses explicitly with `work_catalog_cursor_invalid` and
  the fresh same-filter command; shorten the filters before retrying.
  If even one verbose row cannot fit, no cursor skips it: the zero-row receipt
  names the first remaining match for `show` and reports the remainder honestly.
  `--under PARENT` (MCP `under`) lists direct children, not grandchildren;
  `--optional`/`--required` (MCP booleans) select one requirement class and need
  `under`. Both together are refused. Other filters and `--all` still apply.
  MCP `search { query: TEXT }` is shell `ls --search TEXT --all`; both search
  every lifecycle, while plain `ls --search TEXT` defaults to open work.
- Shell notes take the target positionally: `engram work note REF "text"`.
  MCP uses `note { work_ref: REF, text: TEXT }`; the shell has no `note
  --work-ref` flag (that flag belongs to `gate`).
- An MCP word refuses an argument its input schema does not list, before it
  reads or changes anything, and the refusal names that argument and every
  accepted one: `accept` on `add` is refused and points to `acceptance`,
  rather than being ignored while a defaulted criterion is recorded. Each
  word's input schema says so with `additionalProperties: false`.
- `add` needs only a title. Outcome and acceptance criteria are welcome; they
  are what `done` is checked against. When acceptance is omitted, text and JSON
  reminders say `acceptance defaulted to the title being done; set --accept`,
  and MCP names its own field instead: `acceptance defaulted to the title
  being done; set acceptance`.
  This keeps the signal without repeating the item title; the final receipt
  includes the reminder in its response budget.
  Explicit acceptance suppresses that reminder; blank criteria are refused.
  Afterwards, while an open item's only criterion is still the placeholder it
  was created with, `"<title> is done"` with its creation title, and its list
  has never been revised to other criteria, ordinary `show`, `show --full`,
  the claim receipt and `done`'s owed list carry one first reminder:
  `acceptance is only the title placeholder ('<title> is done'); set real
  criteria by revising acceptance with update`, the same on CLI and MCP, and
  both `show` reads' JSON says `acceptance_placeholder: true`. When `done`
  completes a child whose open parent is in that state, the success receipt
  carries the parent's line once, prefixed with the parent's ref (`<parent>:
  acceptance is only the title placeholder …`), so it never reads as the
  child's own status. It states what
  the stored list is, so a title rename keeps it and the same sentence typed
  by hand at creation reads the same. A revision to other criteria drops it
  for good, even if a later revision restores the sentence; a replacement
  with the identical text leaves no trace in the store and so keeps it. No
  evaluator can judge that sentence, so it never changes admission, and
  budget fitting never sheds it.
  Repeatable `--note TEXT` (MCP `notes: [TEXT, ...]`) records ordered initial
  non-holder observations atomically with creation, at most 16 across the
  complete creation/decomposition batch. Blank or excess notes refuse the
  whole creation; identical entries are distinct observations. Only an exact
  creation replay recovers the original item and notes; see the limits in the
  [session and intent retry rule](local-work-system.md#agent-native-protocol).
  These notes
  carry creator attribution verbatim, not a claim or checkpoint; receipts
  call them initial observations (no execution credit). Observation markers
  never replace the supplied source tool or reason.
  Under a parent held by another live session, a peer may use `--optional`
  to create an Open, unclaimed child with attributed proposal provenance.
  The holder sees that proposal in `next`; their item, run, claim, expiry,
  fence, and checkpoint remain unchanged. Required children and prerequisite
  edits need the parent holder; peer attempts receive
  `work_peer_decomposition_refused`. There is no approval or activation word.
  `add --under` refuses completed, cancelled, or superseded parents with
  `work_parent_not_open`: file an independent root follow-up or add under an
  open ancestor. Proposed parents also refuse new children, with guidance to
  inspect the not-yet-open parent instead. Existing children and their
  execution fences are untouched.
- `update CHILD --reject "why"` (MCP `action: "reject", reason: "why"`)
  cancels an open required child and records its open parent's required-child
  waiver atomically, with the same attributed reason on both existing events.
  Existing cancellation ownership and project-bound waiver checks still apply;
  no claim, completion credit, or acceptance change is implied. The receipt
  names both effects. Optional children and other unwaivable shapes return
  `work_reject_refused`, naming the child/parent and the conditional two-word
  path: cancel the child when admitted, then waive it from the parent only if
  required and waivable. No partial cancellation commits on refusal.
  The root execution must be able to record the waiver, including an eligible
  restored execution bootstrap. Completed work is intercepted first by the
  existing late-finding refusal pointing to `note`/`gate`.
  A keyless retry with the same project, session, child, and canonical intent
  recovers both committed effects only while the child still exactly matches
  the cancelled result. A changed child receives `work_reject_refused` with
  inspection guidance. Pending attempts without a committed result retain
  strict basis checks; explicit caller keys keep their existing semantics.
  Record the evidence that rejects a finding in a note first; `done` is for
  satisfied acceptance, not a synonym for rejecting a finding.
- Successful `done` asserts satisfaction of the sealed revision's acceptance
  criteria. Text and JSON disclose `acceptance_criteria_asserted` (the count)
  and `acceptance_criteria_changed: false`; completion changes no criterion.
  `acceptance_evidence` also reports `criteria_count`, `unlinked_count`,
  `unlinked_label`, and `unlinked_positions`: one-based positions in the frozen
  seal's acceptance vector, never criterion-text matches. Each position means
  exactly "no evidence linked to this criterion", not that no evidence exists on the
  work. Whole positions are byte-fitted with `omitted_count` and the existing
  omission manifest; text names the same positions and remainder. Native
  seals with no criteria omit this disclosure's text and JSON field entirely.
  Completed native `show`, including notes/history windows, and committed
  completion replay remain readable if their advisory seal reload fails,
  with `acceptance_evidence_unavailable` explicitly stating
  "per-criterion evidence unavailable for this completed work", without positions
  or an inferred unlinked count. `acceptance_evidence_error_class` and its text
  twin retain a fixed diagnostic class, never raw error text or identifiers.
  This does not relax canonical item/run validation or replay verification.
  A record window keeps the disclosure in its header on both surfaces. Native
  completed `show` and replay derive the same facts from that seal, not later
  notes, gates, checkpoints, or a work-level evidence set. Restored-record-only
  completions have no native seal: `acceptance_evidence_unavailable` explicitly
  says this store holds no per-criterion evidence record, with no inferred count.
  This does not refuse completion or change satisfaction. `done`'s summary and
  shared `--note` do not link per-criterion evidence; completion without links
  remains admissible. See the
  [historical binding qualification](local-work-system.md#audited-waivers-and-model-autonomy).
- To link existing evidence explicitly, read `show REF` and copy its
  `acceptance_basis`, then use `done REF --link 2=LOCATOR --link-basis N`.
  Repeat `--link` for multiple links. MCP uses `links: [{criterion: 2,
  locator: "..."}]` and `link_basis: N`. Criterion positions are one-based in
  the displayed acceptance order. Both open and completed `show` number their
  criteria instead of dash bullets, so the author can select a position and
  compare the same position in sealed readback. This basis is the work revision,
  exposed only as a read-concurrency token: any intervening work revision
  refuses and asks for a fresh read. Reads do not save a basis or steer writes.
  A basis is mandatory with links and is refused without them.
  Find the evidence in `show REF --notes --gates`; reuse its native note/gate
  locator (a unique prefix of at least eight hex digits or its full identity),
  not an opaque artifact URL, checkpoint, or history-event identity. Current-run
  holder notes, status captures, and gates already belong to completion
  evidence. Observations made before claiming or by non-holders, earlier-run
  evidence, and inherited members do not; refusals distinguish these causes
  and offer current-item read commands. Citation never widens that set.
  `acceptance_evidence` retains the unlinked disclosure and adds `link_count`,
  bounded `links` with criterion/locator/preview/detail, and `links_omitted`
  when nonzero. The label is "author-linked evidence; not verification".
  Previews are bounded; the original detail command is the readable basis.
  An unavailable preview does not erase a frozen link; failed advisory reads
  disclose a bounded `preview_error_class`, never the underlying error text.
  Input is limited to 64 links. Readback retains at most 16, and byte fitting
  can retain fewer. `show` has the same cap: no complete frozen-mapping
  continuation exists on this surface yet, so another `show` is not promised
  to reveal omitted links. Original note detail is not a mapping continuation.
  Exact same-session linked intent replays its committed receipt. An identical
  request from a different session is not that author's retry; it refuses,
  as do new or late links attempting to amend the frozen seal.
  Open items with no criteria advertise no link basis. Hidden criteria continue
  the visible one-based numbering; omission does not renumber them.
  Core explicit acceptance cannot be combined with these positional links.
  Neither a link nor a passing gate verifies that
  the criterion is satisfied: satisfaction remains the author's assertion.
- Unlinked criteria are named before the seal, while linking is still
  possible. Under a self-asserted policy, the holder's `show` of an open item
  reports `unlinked_criteria: {count, positions}` in JSON: the criteria that
  carry no evidence link yet. That is every unbound criterion, and every
  bound one whose newest binding obligation is not satisfied, because a
  satisfied binding is the one link completion adds on its own, decided the
  same way the seal decides it. Positions are capped at eight; the count
  stays exact, and every criterion linked reads `{count: 0, positions: []}`.
  While the count is above zero, the holder's `show` and `gate` receipt add
  one reminder after the holder's existing guidance, the same on CLI and MCP:
  `criteria 1, 3 have no evidence link yet; link evidence in done, or pass
  the bound check first` (for one criterion, `criterion N has …`), with `and
  K more` past eight positions. A bound criterion whose obligation is still
  open needs its check, since `done` refuses an open obligation; any other
  needs a link in `done`. The line states what is linked now, not a
  forecast: completion checks a satisfied binding again. A peer's `show`
  carries neither, since only the holder completes. Under an evaluated policy
  neither appears, since the seal cites the consumed evaluation's evidence
  and a pass must cite at least one record. Completed work reports its
  frozen seal instead.
- Claim before execution. `claim REF --ttl SECONDS` renews your live claim
  with the same identity and fence; expiry becomes the later of its existing
  expiry and now plus the requested TTL (one hour by default).
  `claim --under PARENT` selects the parent's next ready direct child in the
  `ls --ready` order (priority, then work id) and claims it in the same core
  transaction, so parallel sessions under one parent never receive the same
  child and never a blocked, deferred, held or closed one. The receipt names
  the child and its place among the ready children; a repeat renews the
  child you already hold under that parent. A ready child whose prior claim
  lapsed under another holder is passed over unless you pass `--recover`.
  With nothing ready the call refuses with the reason and holds nothing.
  Open-work `gate` and `done` require the holder. A non-holder may `note`
  open work, including blocked work or a child of a completed parent: this
  produces a marked observation, never execution or completion credit.
  After completion, any project-bound session may use
  `note` or `gate` for a late finding without claiming or reopening the item;
  the existing seal stays frozen.
- `update REF --release` (MCP `action: "release"`) gives up your claim. If
  this session has neither a note, gate, or checkpoint nor a waiver under the
  item's root execution, add `--reason "why"` (MCP `reason`): it is recorded
  as the attributed waiver of that missing contribution, the receipt says so
  (`waiver_recorded: true` in JSON), and the next holder claims without
  `--recover`. Without a nonblank reason that release is refused with
  `work_release_waiver_required`, whose remedy and `next` command name
  `--release --reason`; nothing changes. After a contribution or an earlier
  waiver the reason is optional and no new waiver is recorded. See
  [work claims](local-work-system.md#work-claims).
- `update --kind`, repeatable `--label`, and repeatable `--unlabel` revise
  indexed planning metadata through the existing audited planning path;
  unclaimed planning updates remain allowed.
- `note` is for decisions, findings, and evidence pointers. A holder note
  feeds peers, handoff, and the final report. A non-holder observation feeds
  project/root peers without a checkpoint, claim renewal, or run credit.
  Its immediate receipt marks `non_holder: true` and says
  `(observation, no run credit)`. On open work that receipt never nudges its
  writer to claim: it carries no `claim it before execution` reminder, its
  first next command is the read `engram work next --peek` (the item's own
  read is its `full detail` line), and when the item is unclaimed the claim
  comes last, with a reminder naming it as the way to execute the item rather
  than observe it. The guidance follows the recorded note, not who holds the
  item afterwards; a holder's note keeps its own. A late note feeds peers but remains outside
  the frozen seal; never repeat either elsewhere.
- `done` completes the item you hold. If something is still owed, the answer
  is one sentence saying what and a command that resolves it. Do it and run
  `done` again. Only when the completed parent has direct open optional
  children, the successful CLI/MCP receipt adds `child_obligations.open_optional`:
  exact `count`, at most five
  `items` (`ref`, bounded `title`, `remedy`, and `resolve_first` when needed),
  exact `omitted`, and a `navigation` command. Final text and JSON byte pressure
  can reduce the shown rows further. Detach is offered only when its current
  admission checks pass; otherwise the row names the condition to resolve.
  Navigation is `show PARENT`; the terminal receipt mentions the broader
  `ls --blocked` view once, not a child-filtered listing. This is read-only
  advice after completion, not a new completion obligation or an automatic
  mutation. With no remaining children the group is absent. If the diagnostic
  read fails, `child_obligations_unavailable: true`, a fixed
  `child_obligations_error_class`, and parent navigation retain the successful
  outcome without pretending the remaining count is zero. The diagnostic
  class never contains the underlying error body, path, record id, or actor text.
- `done --landed … --installed-build FINGERPRINT` records the build installed
  from the landing as the agent asserts it. Take the full value from the
  installed executable itself: run `readiness --json` or `doctor --json` by its
  path after copying it and copy `build_fingerprint`. `--version` shortens each
  token, and a long-running MCP process reports the build it started with; a
  shortened value is never completed by hand. `show` and `doctor
  --check-landings` print it in full beside "asserted, unchecked", or "no
  installed build recorded"; Engram never compares it with any build. See the
  [landing record](local-work-system.md#completion-seal-and-report-assembly-claim).
- `show` lists each visible active blocker with a selector (`b1-` and the
  stored blocker id in unpadded base64url, one spelling per id), its kind,
  its detail and the exact command that clears it, whoever reads it (the
  clear is admitted or refused when it runs): `update REF --unblock --blocker SELECTOR` (MCP `blocker`). It
  states the exact number of active blockers and how many it does not show.
  A selector is navigation, not authority: the clear it names is admitted
  like any other. A blank, malformed or differently spelled selector is
  refused before anything is attempted; one of another item, one already
  cleared, and an old one after a new blocker with the same text are refused
  and never clear a remaining blocker. A bare `--unblock` still clears an
  item's only blocker and refuses when several are active. Repeating the same
  selected command after a lost answer returns the recorded result, with one
  clear and one event, even after the item changed. An attempt interrupted
  before the core answered refuses once the item changed, naming `show REF`
  and who can still clear the blocker; one the core
  refused, such as a clear under a peer's live claim, is retired, so the same
  command repeated later is admitted afresh. A keyless unblock naming its
  blocker on the core JSON update surface has the same identity. The receipt
  names the blocker its committed clear removed, by selector, kind and
  detail, and how many blockers remain, and
  history names each raised and cleared blocker by the same selector, kind
  and detail, clears recorded before selectors existed included.
- Every answer ends with `reminders` (what is owed, in words) and `next`
  (commands you can run now). Ordinary mutation words never ask for fences or
  idempotency keys. Words accept record ids as inputs only for scoped evidence
  citations and note-detail navigation. Structured receipts also return record
  ids in fields such as `seal`, `evidence`, and `evaluation`.
  Optional criterion linking explicitly reuses note locators and the
  `acceptance_basis` read token; it grants no authority.
  `evaluate` also accepts full record ids of host-minted verification or
  environment evidence on the active run, which no note-locator window prints.
  Safe project-memory keys are
  intentional navigation tokens for `memories` and `forget`. JSON retains the
  complete command list; the text renderer shows at most four and prints
  `(+N more)` when it omits any.
- Before repeating an uncertain mutation, follow the
  [session and intent retry rule](local-work-system.md#agent-native-protocol),
  including its child-creation and append-only exceptions. If a shell used
  the process default and
  lost the entire notice too, inspect with `ls`/`show` before repeating a
  mutation; exact replay cannot cross processes without the printed session.
- A failed gate is work, not a stop — `gate NAME --failed FAILURE`
  records the failures as evidence on held open work, or as a late finding on
  completed work selected by focus or `--work-ref`, nothing more. With no
  focus, use `gate NAME --work-ref REF`; no last-completed item is inferred.
  A bare gate on a completed focus is refused while this session holds other
  open work, as described under reading and focus above; name it with
  `--work-ref`.
  Gate names follow the repository's
  [quality gates](../development.md#quality-gates).
  Classification stays your judgment. Record a small in-scope correction's
  diagnosis, fix and verification on the held item. Create a required child
  for separate ownership, independently scoped work or a real dependency,
  with the failing test as its acceptance criterion; block landing on it when
  necessary. Test or environment findings go into the durable note too.
  In-scope defects must be fixed before completion. A pre-existing defect
  outside the changed scope, or a late failure on completed work, gets an
  independent root follow-up with its evidence and provenance. Never delete,
  skip or loosen the test to pass.
  `gate NAME` alone always records a pass. Every failure supplies at least one
  bounded `--failed` label; when no test id exists, use the check command or
  check name. A consecutive identical result replays;
  the same result after an intervening different result records a fresh gate
  transition.
- `evaluate` records one immutable acceptance evaluation on the targeted
  item's active run: `evaluate [REF] --mode MODE --acceptance-basis N
  --evidence-basis M --verdict POSITION=VERDICT[:BASIS] --rationale
  POSITION=TEXT [--evidence POSITION=LOCATOR]... [--supersedes RECORD_ID]`,
  where `show` prints both bases and `LOCATOR` is a note/gate locator
  exactly as `show --notes --gates` prints it (resolved as `done --link`
  resolves it) or the full record id of host-minted verification or
  environment evidence. The evaluator's own session is the attributed
  identity; a pass needs at least one run-evidence citation; the core
  validates structure and provenance, never relevance, and refuses a
  submission whose evidence basis a host-observed change has already
  overtaken. When the criteria a failing evaluation judged, or their
  verification bindings, were revised after it, `show` discloses that
  carried failure (`acceptance_evaluation.carried_failure`; `show --full`
  adds the criteria and bindings it judged as `judged_criteria` and
  `judged_bindings`, and after a later failing evaluation named it, that
  evaluation's bindings as `newest_judged_bindings`), and after a revision by
  the run's executor `evaluate` must name the failed record with
  `--supersedes RECORD_ID` (MCP `supersedes`), from an evaluator that never
  held the run, and only a passing one that names it ends the carry; a
  mismatch is refused with `acceptance_evaluation_refused`,
  details `reason: carried_failure_unacknowledged`,
  `carried_failure_self_acknowledged` or `nothing_to_supersede`. The receipt and
  ordinary `show` carry a bounded prefix of verdict rows with an exact
  `verdicts_omitted` count and the decision facts (passed count, first
  blocking verdict, evaluator label, freshness, recorded source
  fingerprint); `next` prints one `evaluation:`
  line under the focused evaluated item; `show REF --full` returns the
  complete newest evaluation with every rationale and citation. It is
  refused until an operator enables the
  [acceptance evaluation](acceptance-evaluation.md) policy, and under that
  policy `done [--source-fingerprint F]` consumes the newest fresh, all-pass
  evaluation instead of the author's self-assertion, presenting the
  host-measured fingerprint when the policy requires source freshness. An
  independent evaluator that later takes the run cannot consume its own
  pass. Under that policy `done` on an item with no acceptance criteria
  answers the error code `acceptance_criteria_required`: add a criterion
  with `update REF --accept "criterion"`, have the host evaluate it (the
  host refuses to evaluate an item without criteria), then run `done` again.
  `done` and the completed item's `show` say where the sealed
  acceptance came from (`evaluated (<mode>, <assurance>) by <evaluator>` or
  `self-asserted`); `add --evaluation-mode MODE` pins a task's mode
  from creation, `update REF --evaluation-mode MODE` pins it later, and
  `--clear-evaluation-mode` releases it. `show` prints the pin as
  `evaluation mode:` and its JSON carries it as `status.work.evaluation_mode`,
  which is what a host reads to spawn the right evaluator.
- `remember` stores a retrievable project note — an attributed
  observation, never a rule or a decision record, kept in full until an
  explicit `forget`. `next` only signals how many notes exist and whether
  any changed; `memories` is the source of truth.
  CLI `remember` requires exactly one body: positional `TEXT` or `--text TEXT`.
  Both together are refused. The `--key`, `--revise` and `--expected-revision`
  options work with either form; MCP keeps its existing `text` field.
  `remember TEXT --key KEY --revise` retains earlier attributed versions under
  that key. Optional `--expected-revision N` refuses a stale basis with the
  current revision; without it, the receipt names the replaced and new
  revisions. An identical same-session body and explicit basis replays;
  without a basis, only an identical current revision replays. `memories KEY
  --full` reads the current body and offers previous-version navigation;
  `--revision N` reads one historical body. `forget` permanently retires all
  reads of the key, retaining local canonical history. Snapshots transfer live
  histories, but only the tombstone for forgotten keys. MCP uses `revise`,
  `expected_revision`, and `revision` with the same meanings.
  A revise can change part of a memory without resending the rest:
  `--append` (MCP `append: true`) adds the text as a paragraph after a blank
  line, and `--section NAME` (MCP `section`) replaces only the interior of the
  section marked by full lines `<!-- engram-section NAME -->` and
  `<!-- /engram-section NAME -->`, NAME being 1 to 64 bytes of `a-z`, `0-9`
  and `-`; empty text clears the section. Both require `--revise`, `--key`
  and `--expected-revision N`, are alternatives, and are built from revision
  N inside the write, keeping every other byte, CRLF included, so a stale
  basis conflicts (also when the edit could not be built on it) and an exact
  retry replays rather than appending twice. A section the basis lacks is
  refused with `memory_section_not_found`, naming the sections it has; add a
  new one with `--append` and its markers. Markers must pair, never nest, and
  never appear in a section's replacement text; an append to a body whose
  markers pair must keep them paired, and an append to one that only quotes
  a marker needs its own text to pair. Every revise's receipt shows what changed:
  `changed (EDIT): B → A bytes; R removed and N added at byte S`, then the
  removed and added text as bounded excerpts with any bytes left out, and
  `next` offers full reads of both revisions; its JSON carries `change`.
  `--retires-with local:REF` or `--retires-with external:PROJECT#REFERENCE`
  (MCP `retires_with`) names the item whose resolution makes a workaround
  memory worth reviewing; a revise keeps the current target, and
  `--revise --clear-retires-with` (MCP `revise` with `clear_retires_with`)
  removes it and records the clear. `done`, and a supersede, detach, cancel or
  reject through `update`, list the memories that name the item as bounded
  candidates, never changing them. See
  [project memories](local-work-system.md#gates-prerequisites-supersession-and-project-memories)
  for target resolution, the clear and a target dropped without one.

The same fourteen words are MCP tools (`next`, `ls`, `show`, `add`, `claim`,
`update`, `gate`, `evaluate`, `note`, `done`, `handoff`, `remember`, `memories`, and
`forget`) with the same flat arguments, plus `search` — fifteen tools. The
six-operation work core is unchanged: `gate` wraps the existing evidence
path, `remember`/`memories`/`forget` are a thin project-memory surface outside
it (no focus mutation, no claim renewal), and `evaluate` calls the service's
separate evaluation entry, which neither `engram work core` nor the
host-private protocol exposes. Reads require
the cooperative asserted project binding. `remember` and `forget` validate the
same non-empty actor/session binding inside the write transaction;
`memory_binding_invalid` means that binding is absent or inconsistent. The normative
spec, tool count, and every agent instruction file move atomically with the
code.

If you know Beads, the words map one to one:

| Beads | Engram |
| --- | --- |
| `bd ready` | `engram work next` |
| `bd list` | `engram work ls` |
| `bd show <id>` | `engram work show REF` |
| `bd create --title=T [--parent=<id>]` | `engram work add "T" [--under REF]` |
| `bd update <id> --claim` | `engram work claim REF` |
| `bd update <id> --status=blocked` | `engram work update REF --blocked "why"` |
| `bd update <id> --notes=N` | `engram work note "N"` |
| `bd close <id>` | `engram work done ["what was delivered"]` |

`done` differs from `bd close` in one way: it is checked against the item's
acceptance and against anything the host recorded as owed, and it tells you
what is missing instead of closing anyway. Under a project policy that enables
[acceptance evaluation](acceptance-evaluation.md), that check also requires a
fresh, passing per-criterion evaluation recorded by the evaluator the host ran.

## Host integration

The [host checklist](../host-checklist.md) is the authoritative base-tier
recipe, including the Claude Code hooks form; this section keeps the
two-tier contract and the control-plane details. Integration has two tiers,
and a host picks per project:

- **Base** — the tracker for agents: when the project's integration is enabled
  and supported by the runtime, the `engram mcp` server is injected into its
  supported sessions (the repository declares with its
  tracked `.engram-project`; the host supplies the stable project identity
  and asserted actor/session binding to the MCP child), plus a
  start-of-session nudge that runs `engram work next --peek` on session start and
  after compaction and injects its text as context at the next dispatched
  prompt, not an immediate runtime-authored continuation. Any host that can register
  an MCP server and run a hook can do this. It carried every benefit measured
  so far.
- **Turn-gated** (`turn_gated`, the optional tier of the checklist and of
  [shipped today](../shipped.md)) — behavioral control: the host-private
  JSON-lines turn channel below (bind, evaluate, begin, checkpoint), dispatch
  withheld until a turn is granted, fenced work claims, obligations
  before completion. Opt-in per project, off by default, for hosts that need
  enforcement rather than coordination.

The agent-facing MCP interface is **advisory**. A model can omit an MCP call,
so that surface alone cannot enforce synchronization, ownership, or
finalization. The separate host-private JSON-lines channel implements the turn
lifecycle; grants are not exposed as tools with which the agent can authorize
itself. Everything below is the host's and operator's business: the agent
words above never require it.

### Build identity and doctor refusals

For host enablement, use [`readiness --json`](host-readiness.md) for scoped
existing-store/schema/policy checks. Keep `doctor --json` as a separate explicit
full audit; readiness is not full-store health and never returns `healthy`.

For missing-session reconciliation evidence, use
[`control-session-inspect`](control-session-inspection.md). It reads exact
session/grant presence in one admitted snapshot without checkpointing or
changing state. The host retains its own quiescence and ownership fences;
neither all-false presence nor a refusal authorizes clearing state.

`engram --version` prints `engram VERSION build FP12 (exe EXE12, schema
SCHEMA12, rev REV)`, where `REV` is the source revision's first twelve hex
digits, keeping a `+dirty` marker, or `unavailable`. Agent `next` ends its terminal text with one diagnostic line:
`build: FP12; read cut: project POSITION observed_at INSTANT`, followed by
`context_generation GENERATION` when supplied. Its structured CLI/MCP receipt
and core `work_next` carry one `build_fingerprint`, `read_cut` containing
`project_position` and `observed_at`, and optional `context_generation`.
For ordinary `next`, the cut is the shared advisory snapshot for focus, lists
and discovery, not the separately staged change-delivery cursor or the
project-memory signal's basis. For peek, all sections including changes and
the memory signal share this read snapshot; it still is not a delivery cursor.
`observed_at` is the call's supplied read instant; the feed position orders
committed state, not the timestamp. Retaining an older block retains its cut;
compare with a new read to see which committed state the block could reflect.
This does not promise automatic mid-turn refresh. TermAl, the live consumer,
fetches CLI `next --context-generation termal-N` text at dispatch and tolerates
these additive diagnostic fields; its generation is asserted host context, a
plain token as the peek contract above defines it.
Compare the build token with a fresh CLI process after an
install to detect a stale, long-lived MCP child; restart the child to run the
new executable. The token is diagnostic, not an execution hash to copy into
commands, authenticated identity, or store-admission authority.

Every `doctor --json` mode and `readiness --json` includes `build` and
`build_fingerprint`. The build
object contains `package_version`, `executable_sha256` (SHA-256 of the running
executable's bytes), and `schema_reference` (SHA-256 of the RFC 8785 canonical
ordered, whitespace-normalized SQLite definitions used by ordinary schema
admission), and `source_revision`: the commit the executable was built from,
that commit followed by `+dirty` when tracked files differed from it at build
time, or `unavailable` when the build could not determine it (no Git, no
checkout of this package, or a failed probe; the build itself never fails for
it). The build script records it, read-only, and reruns when a tracked file,
the index or the checked-out commit changes; after a failed probe it reruns
when `PATH` or the checkout's Git entry changes. Because the revision is part
of the executable, a commit or the first edit after one rebuilds it, so even
a documentation-only change gives a new executable and fingerprint. The fingerprint hashes the
RFC 8785 canonical build object, so the revision is part of it. These values
are captured once per process: at MCP startup, or when a short-lived CLI
process emits diagnostics. Agent words other than `next` do not compute
identity. There is no capability catalog or persisted last-writer row.
The source revision is recovery information: it names where to look for the
source, not proof that checking it out rebuilds the same executable. A
`+dirty` revision names its base commit plus changes that no commit records,
so the base alone does not hold them.
Equal inputs produce equal fingerprints; different executable bytes, schema
definitions or source revisions distinguish builds even when their package
versions agree.

An unreadable executable is explicitly `executable_sha256: null` with
`executable: "unavailable"`; an unavailable in-memory schema reference likewise
uses `schema_reference: null` and `schema: "unavailable"`. A canonicalization
failure leaves `build_fingerprint: null`, rendered as `unavailable`. Diagnostic
unavailability never refuses an agent word and must not be mistaken for proof
that two executables match.

An ordinary doctor open refusal emits `healthy: false` on stdout and exits
nonzero, including with `--json`. Text mirrors the same fields with escaped
values. `database` uses the same canonical path as a healthy report whenever
the path is readable, falling back to the supplied spelling otherwise.
Refusals carry `phase`: `open`, `verification`, `control_diagnostics`,
`control_policy_recovery`, or `projection_repair`. The phase identifies where
the error arose even when a diagnostic read failed after a successful open.
The codes are:

- `store_not_initialized`: an empty file has no user schema. Projection repair
  refuses it without initializing it. Run `engram init` explicitly when you
  intend to initialize that empty store.
- `projection_repair_required`: exact remedy
  `engram doctor --repair-projections` and safe scope
  `["indexes", "triggers", "fts"]`; reporting performs no DDL.
- `different_build_schema`: `store_schema_reference`, read from the existing
  file using the same normalization, plus `running` build components. Use the
  build that created the store. There is no in-place store upgrade, and
  projection repair cannot convert a different durable schema.
  An extra index this build does not own also receives this refusal, including
  from `doctor --repair-projections`. Ordinary open gives the same safe advice
  to use the owning build, not to restore or re-initialize the store.
  FTS naming prefixes do not make undeclared objects repairable. Only declared
  objects and the declared FTS tables' known shadow tables are rebuildable.
  The schema digest is not a claim to know the original executable;
  a schema-marker-only mismatch can have equal definition digests. If the
  refused file cannot be read, the digest is null with
  `store_schema: "unavailable"`; reporting never creates a missing file.
- `corrupt_store`: one `findings` array of strings describing the refused or
  invalid records, including failures found during post-open verification.
- `store_open_refused`: an operational/configuration failure, not a corruption
  claim. Its `reason` preserves the underlying error verbatim and `kind` is
  `busy`, `permission`, `io`, or `path_policy`. The remedy names retry for
  contention, the path and reported error for access/I/O failures, or both
  policies and the flag that selects the recorded case policy for a mismatch.
  An incompatible host alias-rule setting cannot be changed by that flag;
  its remedy points to a compatible host or a fresh store at a new location.
  Ordinary open's reason uses the same two-branch advice and never tells the
  operator to re-initialize the existing store in place.
  Operational failures retain this category after open; they never become a
  corruption claim just because a later control diagnostic failed.

These diagnostics do not change ordinary admission or explicit repair. See
the [SQLite contract](sqlite-store.md#canonical-bytes-contract).

### Host and operator CLI

```bash
export ENGRAM_HOME=/absolute/host-local/path
engram init --required-assurance advisory \
  --authorized-by host-operator
engram doctor

# Fast read-only store/policy admission; not a full audit or execution authority.
engram readiness --json

# Read-only snapshot evidence for exact retained handles; not reconciliation.
# Any refusal supplies no absence evidence; the host must retain its own fences.
engram control-session-inspect --target-session-id session-id \
  --retained-grant-id grant-id --json

# When ordinary open refuses a corrupt control-policy chain, inspect only that
# immutable family. This mode is read-only, enables no service/mutation API,
# and never selects or rewrites a policy head.
engram doctor --recover-policy [--json]

# Explicitly rebuild only declared indexes, triggers, and FTS projections.
# Ordinary open never performs this repair implicitly.
engram doctor --repair-projections [--json]

# On request only: check each landing the seals record against a local git
# repository (default: the project file's directory). Reads local objects and
# remote-tracking refs only; never fetches. Each landing's installed build is
# printed apart from that answer, in full, as asserted and unchecked.
engram doctor --check-landings [--repo PATH] [--json]

# Host/operator boundary: activate a new immutable policy version. The
# optional expected policy id is the `id=` reported by doctor and prevents a stale
# operator from overwriting a concurrent policy update.
engram control-policy set-required-assurance turn_gated \
  --authorized-by host-operator \
  --idempotency-key enable-host-turn-mediation \
  --expected-policy-hash <active-policy-id>

# Select a bounded typed obligation set.
engram control-policy set-obligation-rule-set \
  --input @obligation-rules.json \
  --authorized-by host-operator \
  --idempotency-key pin-repository-verification \
  --expected-policy-hash <active-policy-id>

engram mcp \
  --actor-id codex \
  --session-id session-unique-id \
  --actor-context 'model=opus-4.1;reasoning=high' \
  --source-skill engram-repo
engram control \
  --actor-id codex \
  --session-id session-unique-id \
  --source-skill engram-repo

# Host-local loss recovery: a verified full copy of the store, and the way
# back. A backup carries host-private state and private scratch, so it is exactly as
# sensitive as the store; schedule it on the host, never publish it.
engram backup                      # → <home>/backups/<project>/engram-<utc>.db + manifest
engram restore --from <backup-file> [--replace]   # stop other Engram processes first

# Off-host backup targets: the operator's record of where a kind's copies go.
# Setting a target copies nothing.
engram backup target set --kind store --adapter directory --dir <absolute path> \
  --disclosure-authorized-by <operator> --off-host-asserted-by <operator> \
  [--window-hours N] [--keep N]
engram backup target show [--json]
engram backup target clear --kind store

# Deterministic planning/history disclosure. The default path is
# <home>/snapshots/<project>/graph-<work-cut>-<memory-cut>-<first-12-body-digest>.json.
# --include-restricted deliberately widens restricted project-memory bodies and
# therefore requires a disclosure reason carried in the body and save audit.
engram graph save [--out <snapshot.json> | --stdout] \
  [--include-restricted --reason "<why>"]
engram graph load <snapshot.json> [--dry-run]

# Agent words use the stable project plus asserted actor/session binding.
engram work --actor-id codex --session-id session-unique-id next
engram work --actor-id codex --session-id session-unique-id \
  claim <short-ref>
engram mcp --actor-id codex --session-id session-unique-id
# The same identity, for a child that must not write.
engram mcp --actor-id codex --session-id session-unique-id --read-only

# Host/operator escape hatch: the six-operation JSON protocol from the shell.
engram work --actor-id codex --session-id session-unique-id \
  core focus <short-ref>
# Read the same view without selecting focus or touching delivery.
engram work --actor-id codex --session-id session-unique-id \
  core inspect <short-ref>
# List the claims this session holds, each with its binding or null.
engram work --actor-id codex --session-id session-unique-id \
  core held
```

`graph save` and `graph load` are operator-only CLI surfaces; neither is an MCP
tool or changes the fourteen agent words. Save reads the work graph, native
history, source provenance, and keyed project memories at one transaction cut,
then commits an immutable disclosure-attempt audit before publishing bytes.
The canonical body excludes the exporting build, so its digest remains content
identity across builds with the same runtime-derived format fingerprint. The
default output is owner-only where the platform has file modes, no save may
target an Engram project-store directory, and neither the default path nor
`--out` replaces different bytes. Load requires an empty destination project,
revalidates the fingerprint, canonical body digest, manifest, relations,
history proofs, and memories before one atomic recreation transaction, and
records a separate immutable load audit. `--dry-run` performs the same
validation and reports the landing plan without writing. See the
[work-graph snapshot](work-graph-snapshot.md).

`backup target set`, `show` and `clear` are operator-only words as well; they
change none of the fourteen agent words and never open the store. A target
belongs to one project and one copy kind; this build configures the `store`
kind at a `directory` adapter. `set` requires
`--disclosure-authorized-by` (the destination may hold everything the kind
carries) and, for a directory, `--off-host-asserted-by` (the destination
leaves this machine). Both are recorded as asserted context with the
operator's name and the time, and a directory target always reads
"off-host asserted; not verified". `--dir` must be an absolute path, a
Windows share included; it is kept as spelled and never contacted when set.
The window defaults to 24 hours and the retention to three copies. `set`
replaces any earlier target of its kind and derives the target's identity
from the project, kind, adapter, location and both statements whenever it is
needed; it is not stored. A first `set` starts the kind's recorded state
empty. A later one keeps a readable earlier state: its receipts and pending
attempt keep naming the identity they were made for, so they stay as history
that qualifies nothing for the new target; a state whose content this build
cannot use is started anew. The configuration and state are files under
`<home>/backup-records/<project>/`, each with a format version and replaced
whole. A record this build cannot use is left untouched: one it cannot read
or parse, one `set` would have refused, or a state recorded for another
target than the configured one. `show` and `clear` refuse it with
`backup_record_unreadable` and name the file. `set` is the way on from a
record whose content this build cannot use, writing both files for the kind
anew. A state file that cannot be read at all, as when another program holds
it, access to it is denied or a directory stands in its place, is not
replaced: `set` refuses with `backup_io` and changes nothing, and the
operator restores read access to the file or removes it before `set` again.
`set` and `clear` take the kind's push
lock, an operating-system lock that its holder's exit releases, and refuse
with `backup_push_running` while another process holds it. The CLI parser
refuses a missing required flag or an unknown kind or adapter before any
code applies; a request it accepts but Engram refuses is
`backup_target_invalid`. A `set` whose state was written but whose
configuration was not reports `backup_record_partial` and is repeated. A
record that exists but cannot be read is `backup_record_unreadable` like any
other record this build cannot use; a record that cannot be written or
removed, and a lock file that cannot be opened or locked, is `backup_io`.
`show` on a clean home prints that no target is configured. See
[off-host backup](off-host-backup.md#configuration).

`backup push [--kind store] [--json]` is an operator word too. For each
configured kind it takes the kind's push lock and brings the copy at its
target up to date, in the steps of [off-host backup](off-host-backup.md#push).
It first resolves an attempt an earlier push left pending: a copy the target
confirms becomes the newest receipt, one that never arrived is dropped and
only its own recorded files are removed, and one recorded for another target
identity is set aside as history with nothing at either target touched. It
then captures a verified store copy into a local stage under the home. The
upload is skipped only when the newest receipt names the configured target's
identity and the capture's format identity and SHA-256, and the target
confirms that copy now; the capture's start is then recorded as the time the
store's content was last observed in it, and the confirmation's time as the
copy's last confirmation. When the target reports that copy missing, the
finding is recorded before a replacement is prepared, so it stands even if the
replacement fails. Otherwise the attempt is recorded as
pending, with its complete manifest, target identity and data file names,
before the gzip copy is put and read back, and its receipt then becomes the
newest. Once that receipt is recorded, and only after a push that did not
fail, copies beyond the target's retention count (`--keep`, three by default)
are removed from the target, oldest first, after the last attempt is
recorded, so its end time does not include them. Only copies whose receipts this
home recorded for the current target identity count, and the newest copy is
never removed, so a copy another home put there, or one made for an earlier
identity of the target, is never touched. A removal that fails is a warning,
and that copy stays recorded for a later push to remove; `--json` lists the
removed copies. The stage is removed at the end, and a stage a push could not remove
is removed by the next one before it captures: only a directory a capture
created, never a link, and in it only the files a push writes.

Capture and transport have separate deadlines, `--capture-deadline-secs`
(900 by default, for the local copy and the compressed file prepared from
it) and `--transport-deadline-secs` (1800 by default, for every request to
the target together). A request to the target runs on a worker thread, and
one with no time left is not started. When a request before the receipt,
resolving a pending attempt, confirming the newest copy or the put, passes
its deadline, the push records the failure with the attempt still pending,
keeps the push lock, and ends its process with exit 1, which is how a
request stalled inside the operating system is cancelled; the lock is
released only by that end. A retention removal that passes the deadline
comes after the receipt is recorded: it is a warning, and the process ends
the same way but with the push's own exit code. Push is therefore a
CLI process by design and must not be run in-process inside a long-lived
server. A read stuck in the kernel on a hung share can delay that process
exit, as it would delay any process's end. A copy the target completed after
the deadline is found by the next push and recorded as confirmed.

Push exits 0 when it uploaded a copy, found the store unchanged, found no
target configured (it says so and creates nothing) or found another push
holding the lock (it says so and captures nothing); a removal for retention
that passes the transport deadline is a warning, and the process then ends
with that exit code too. It exits 1 when it failed,
and prints the typed code: `backup_stage_no_space`,
`backup_stage_space_unknown` and `backup_capture_deadline` from the capture,
a store refusal such as `store_not_initialized`, `backup_target_unreachable`,
`backup_target_no_space`, `backup_target_space_unknown`,
`backup_copy_invalid`, `backup_copy_exists` and `backup_io` from the put,
`backup_target_unconfirmed` when the target cannot say whether it holds an
unchanged copy, `backup_pending_unresolved` when it cannot say what became of
a pending attempt, `backup_transport_deadline`, and
`backup_record_unreadable` for records this build cannot use. A failed push
leaves the previous receipt and the previous copy at the target as they were,
advances no time, and records the attempt's start, end, outcome, code and
message as the last attempt. `--json` prints the project and, per kind, the
outcome (`uploaded`, `unchanged`, `not_configured`, `busy` or `failed`), the
code and message, the target identity, the newest receipt with its manifest,
the time the content was last observed in that copy, the copies it
recovered, dropped, set aside or left pending, the copies retention removed,
warnings and the elapsed milliseconds.

`backup status [--json]` is an operator word that reads only the records
under the home and the store, through the admitted read-only opener, and
contacts no target. It names the durability mode only together with what
backs it: `local_backed_up` when at least one kind qualifies under the
[freshness rule](off-host-backup.md),
with each qualifying kind's off-host text, which for a directory target is
exactly "off-host asserted; not verified", and what it restores; `local`
otherwise, saying that nothing is known to be held off this host. For each
kind it then gives whether it qualifies or the first reason it does not, by
its `backup_*` code, and the recorded evidence as of its times: the target,
both of the operator's statements, the copy with its acknowledgement,
capture start and age, its cut and how far the store has moved since, the
last confirmation, a missing finding, the build that checked the copy when
it is not the running one, a pending attempt, and the last attempt with its
error. A configuration or state file this build cannot use is reported as
`backup_record_unreadable`, not as a failure. `--json` prints the same with
`schema_version` 1, the mode inside a `durability` object beside its
`off_host` list, and the reasons as their codes. The word exits 0.

Actor context currently binds only the work/MCP service. The behavioral
control plane keeps its existing actor/session and environment-evidence
attribution contract.

The optional actor context can equivalently arrive through
`ENGRAM_ACTOR_CONTEXT`; it is fixed when the CLI invocation or MCP connection
binds its session. The MCP process retains one `LocalWorkService` and its
lazily opened SQLite connection for its lifetime. All fifteen MCP tools use
that service. A failed operation rolls back before the next call uses the
connection.

The same variable, `ENGRAM_MCP_PHASE_TRACE=1`, at `engram control` start
traces the host-private `engram control` transport too. Its records differ
from the MCP records described next; they and their correlation are
described under the
[control phase trace](behavioral-control-plane.md#opt-in-control-phase-trace).

`ENGRAM_MCP_PHASE_TRACE=1` at `engram mcp` start turns on a phase trace for
diagnosing slow calls. Any other value, or none, leaves the server and its
transport untouched and writes nothing. With it on, every tool call whose
handler returns ends in exactly one JSON line on stderr, at most 4 KiB,
keyed `engram_mcp_phase_trace`; a handler that panics writes none.

The line carries:
- the call's numeric request `id`, with `correlation` `numeric`. A string id
  is never echoed: `id` is null and `correlation` is `unavailable`;
- the `tool` label (`unknown` for an unlisted name);
- `state`, one of:
  - `complete`: its response was sent;
  - `send_failed`: sending the response failed;
  - `cancelled`: the client sent `notifications/cancelled` for the call
    before its response was handed to the transport, and no response was
    sent. rmcp drops such a response while it serves. After the input
    ends, though, rmcp still sends the responses already queued, a
    cancelled call's included. So the line is written at close, or at once
    for a call that settles after close. A cancelled call whose response
    went out all the same reads `complete`. A cancel arriving after the
    response was handed over has no effect. Only the client's cancel
    counts: a server shutdown is not a cancel;
  - `incomplete`: the transport closed before the response was sent, or the
    call settled after the close;
  - `evicted`: more than 256 settled calls waited for their sends, and this
    one, the oldest, was let go;
- `cancel_requested`, true when the client cancelled the call before its
  response was handed over, whatever `state` says;
- `handler_total_ms`;
- `count`, `total_ms` and `max_ms` for each of:
  - `store_open_total`: opening a connection, schema checks included; a read
    word, `next` with peek included, opens its own read-only connection per
    call;
  - `store_mutex_wait`;
  - `begin_immediate`, including any wait for the write lock, whether it
    ends in the lock or in the 5-second busy timeout;
  - `commit`, which counts every `COMMIT`, read transactions' included;
  - `receipt_serialize`;
- `wire_encode_send_inclusive_ms`, the encode, write and flush of the
  response together, for `complete` and `send_failed`;
- the `evicted`, `unmatched_sends` and `dropped_lines` counters.

Records are written by their own thread through a bounded queue, never on a
request or send path. A host that stops reading stderr therefore costs
dropped lines, counted in `dropped_lines`, and never a stalled server. Read
stderr while the trace is on. Close waits up to a second for queued lines;
lines queued at or after close are best effort, since the process may exit
before they are written.

Durations are elapsed wall time and overlap where one phase contains another.
They are not summed, and a fast server record does not prove a fast client.
`begin_immediate` and `commit` come from SQLite's statement profile, which
has millisecond granularity, so a short statement reads as 0. The line never
carries SQL, a path, parameters or a request or response body.

`scripts/mcp-dogfood.test.mjs` runs its servers with the trace on and writes
every record to its log. It prints a call's record beside its soft-threshold
timing line when the call is slow, or says why there is none. A slow call
whose record has not arrived yet prints `unavailable (record not yet
received)` at once, and its record follows when it arrives; one that never
comes reads `unavailable (no record before close)` when the client closes. A
record in a state that did not time the whole call (`evicted`, `incomplete`
or `cancelled`) reads `unavailable (record STATE)` followed by its fields, so
it never passes for a fast call; only `complete` and `send_failed` records
are printed as the call's timing.

`--project-file` defaults to the tracked `.engram-project`. Its stable project
identity resolves to the same opaque SQLite path for every worktree and
session on the host. Relative project-file paths resolve from the caller's
current directory; Engram does not search ancestors or select another project.
If that file is missing, unreadable, invalid UTF-8, or empty, every CLI work
word refuses before store opening or session setup. For those work words,
text and `--json` emit
`project_resolution_failed` on stderr with exit status 1 and no stdout.
The operator commands `readiness --json` and `control-session-inspect --json`
instead emit structured refusals on stdout and exit 1; see the
[readiness receipt](host-readiness.md#refusals) and
[inspection receipt](control-session-inspection.md#refusals).
Inspection uses `control_session_inspection_refused` with `phase:"resolve"`,
null `project_id`/`database`, and omitted selectors and presence fields for
resolution failures, including a missing home. It does not use the work-word
`project_resolution_failed` code or distinguish a separate `home_required` code.
For the work words, error details name the reason, attempted `project_file`,
`searched_directory`, `cwd` (null if unavailable), selection rule and remedy.
The `next` command uses `--project-file 'PROJECT_DIRECTORY/.engram-project'`
with `work next`; replace the placeholder with the intended absolute project
directory, or change to that directory before retrying. No project is created
implicitly. Paths and OS error text are safely framed in terminal output.

`doctor` verifies every canonical object plus
record-bound control projection, reports the active immutable policy id, epoch,
required assurance, selected obligation-rule-set id, built-in effect
envelope, and live
issued/begun turns, and visibly warns that action gating, organizational
authority mediation, and action-outcome reconciliation are unavailable. V1's
development no-op redactor provides no secret or PII protection.
Doctor audits recorded state and reports the configured assurance requirement;
it does not verify that every caller is mediated by a host.
`engram doctor --json` performs the same checks and keeps those warnings on
stderr while emitting a machine-readable report on stdout. Its `project_id`
and canonical absolute `database` path give a host the stable pair used to key
project-local authority queries. Its `control.acceptance_evaluation` names the
evaluator modes the store admits, the mechanical basis, and whether completion
needs a fresh source fingerprint; the text report prints the same policy on one
`Acceptance evaluation:` line. The database path is absolute with symlinks
resolved. On Windows it never exposes the verbatim-path prefix (`\\?\`), and
UNC paths use their ordinary `\\server\share` form. When integrity is
unhealthy and the active control envelope cannot be decoded, the report still
prints with `healthy: false`, `control: null`, and a `control_error`; independent
limitations remain on stderr before the command exits nonzero.

If normal open itself refuses because the active control-policy chain is
corrupt, `engram doctor --recover-policy` uses a separate existing-file,
read-only/query-only path. It verifies the active selector and every projected
policy version together with the canonical authority and selected rule-set
objects, emits a typed finding for each invalid binding, and exits nonzero
while the store remains unusable. `--json` reports
`mode: "control_policy_recovery"` and `mutation_enabled: false`. This mode
cannot start MCP/control/work, issue grants, initialize schema, or repair/select a
policy; its guidance is limited to restoring verified bytes or explicit
operator inspection. Because the connection is read-only, it also cannot run
SQLite crash recovery for an uncheckpointed WAL. If SQLite itself requires
recovery, stop writers and diagnose a byte-consistent verified copy of the
database together with its sidecars; the command fails without changing them.

Missing or malformed declared indexes, triggers, or FTS tables also make
ordinary open fail without DDL. `engram doctor --repair-projections` is the
separate mutating operator path: it first fingerprints the complete exact-current
durable definitions and validates control-policy bindings, recreates every
declared rebuildable object, repopulates FTS from verified durable rows in one
transaction, and then runs full integrity verification. It never recreates
missing durable state or rewrites canonical objects.

On a fresh store, plain `engram init` defaults to `turn_gated`;
`--required-assurance` may instead select `advisory`, `turn_gated`, or
`action_gated` for that first policy and requires `--authorized-by`;
the resulting epoch-one authority object records that operator
choice as asserted context. Plain `engram init` remains an
idempotent create-or-verify operation and preserves any existing active
policy. Explicitly passing a different bootstrap value for an existing store
fails instead of silently changing policy.
`engram control-policy set-required-assurance` records asserted operator
attribution, creates immutable authority and policy objects, atomically
advances the active policy id and epoch, and supports an optional
compare-and-swap policy id while preserving the selected obligation rule set.
Its required idempotency key
binds the complete normalized intent and persists the exact receipt in that
same transaction. A retry after restart or an uncertain response returns the
original receipt even though its expected policy id is now stale; reusing
the key for another intent is a typed conflict.

Policy administration requires no justification text. Neither explicit
bootstrap nor any `control-policy` setter accepts `--reason`; policy authority
records carry the attributed operator and selected policy, not a reason field.
This does not change the reasons required for separate waiver operations.

`engram control-policy show` prints the active policy as JSON: `policy` (the
record id a compare-and-swap names), `epoch`, `required_assurance`,
`obligation_rules`, `acceptance_evaluation` and `supported_effects`. It reads
the policy head and changes nothing, so a host asks it whenever it needs the
admitted evaluator modes; the `doctor` report carries the same keys but runs
the whole-store audit first.

The sibling operator-only `set-obligation-rule-set` command selects a
validated canonical rule set with the same atomic policy successor,
compare-and-swap, attribution, and replay contract; it is not an MCP tool or
host turn-protocol operation. Its `--input <JSON|@file>` is limited to 64 KiB
of raw input, including any UTF-8 BOM. For files, one leading BOM is removed
after the limit check. The input must be UTF-8. Unknown fields are rejected
at every nested V1 object. The input passes through the same typed validator
that storage uses. `check_fingerprint` compares a canonical check description
and does not accept a shell command in its place. A requirement cannot name an
environment: `required_environment` is refused by name, like any other unknown
field. Re-supplying the active set under a fresh key
records an exactly replayable `changed=false` receipt. Rollback likewise
re-supplies the desired prior JSON; a rule-set id alone is never accepted as
activation authority. Reapplying the active assurance under a fresh key also
records an exactly replayable no-op receipt. Issued grants from the prior
epoch fail begin with `policy_epoch_changed` and require one fresh evaluation;
if the new requirement exceeds the host's declaration, that fresh evaluation
instead fails `control_assurance_insufficient` because assurance is checked
before epoch adoption. Already-begun grants remain checkpointable under their
frozen basis. Selecting `action_gated` through either initialization or the
setter prints a warning that no V1 host can bind at that level plus the
`set-required-assurance turn_gated` recovery command. The operator identity is
asserted host context, not authenticated administration.

`engram work` exposes the fourteen agent words; `--json` after any word prints
the structured receipt (the existing shape plus `reminders` and `next`)
instead of text. A successful mutation with a process-defaulted session also
adds top-level `effective_session_id`. `done` exits with status 2 when the typed
`open_work_obligations` refusal says something is still owed. The stock
source-change rule never causes that refusal. A source change that no matching
passing test followed is recorded at `done` as a waiver in the completing
actor's name. The item still completes, and `done` and `show` each print
`untested source change: ID (source revision REV; HOW); no matching passing
test followed it`, then `untested source changes: N more not shown (T in
total)` when the bounded obligation page names only some of them. `HOW` is
how the host said it established the change (`host compared content
revisions`, `host assumed the change, no earlier revision`, `host had file
notifications only`) or `detection not reported`: the host's word, never
verification. With `--json`, the named changes are in `untested_changes`,
each with `reported_source_change` when the host said, and the count left
out is in `untested_changes_omitted`; every obligation of the stock
source-change rule on the obligation page carries the same
`reported_source_change`, when the host said. Under a named root, a change
another workspace recorded before the binding is displaced instead, and
`done` and `show` print `foreign workspace change: ID (workspace WS; source
revision REV); captured before the named root, displaced and not verified`,
then `foreign workspace changes: N more not shown (T in total)`; with
`--json` they are in `foreign_workspace_changes` and
`foreign_workspace_changes_omitted`. A peer's `next` delta for an
execution observation begins with the change, in one word each: `changed:
WORD` when the host said how it found the change, `changed` when it did
not, `repeat; host: WORD` when the host said how it found a change the core
read as a repeated revision, `unchanged` otherwise; the effect and outcome
follow. `WORD` is `compared` for `content_comparison`, `assumed` for
`assumed_missing_baseline`, `watcher` for `watcher_only`. The delta for an
untested change begins `host said VALUE; ` when the host said, before the
observation id and the revision. In the compact `next` text these two kinds
print their text before the peer attribution, and the line is bounded at
96 bytes, so the change and the host's word survive while a long
attribution, id or revision is cut; the delta's summary, in the same words
and bounded at 192 bytes, is in the verbose `next --json` form. The
protocol's value itself is on the obligation page and in the `--json` forms
of `show` and `done`.
Before completion, the receipt reminder for an open obligation of the stock
rule says what `done` would do with it at that read, as completion classifies
it:
- a change in the named root, or one recorded while no root was bound that a
  later root does not displace (see the next case), is recorded as untested
  without a test;
- a change made in another workspace before the root was named is recorded as
  displaced, and needs no action;
- a change with no workspace, recorded while a root was bound, needs the
  credited check in the active named root or an authorized waiver; once no
  root is bound, it first needs a named root for that check;
- a change made outside the named root while it was bound needs an authorized
  human waiver, since no check in a named root can resolve it, even after
  that root ends.

In each refusing case the reminder says that `done` refuses until then. The
words are read guidance: `done` classifies again when it runs. Under a
self-asserted policy only the obligations the bounded page shows are
classified. A page without a classification says that `done` will say whether
it records the change as untested or needs a credited check or waiver.
Criteria bound with `--bind` still refuse, and
so does every operator-selected rule except the exact stock definition (the
stock id at version 1 with an unpinned test). The
six-operation JSON protocol stays reachable for hosts and operators as
`engram work core {next,focus,propose,update,complete,handoff}`, whose
mutation payloads accept an inline JSON object or `@path`. The `@path` inputs
of `propose`, `update`, `complete` and `handoff` accept one leading UTF-8 BOM.
Each of these four operations has the same raw input limit: 2 MiB,
including whitespace, any file BOM, and the outer envelope, checked before
decoding. Inline JSON is counted the same way. This is a transport ceiling,
not a promise that every semantically valid payload fits; oversized JSON is
refused even when a typed field would otherwise be admissible. A second
leading BOM or otherwise invalid JSON is still refused. That host/operator surface retains
the core-only explicit delivery acknowledgement, typed evidence attach, and
reopen operations alongside typed forms of the ordinary lifecycle
words. Typed `gate` and atomic `note` are word-only `work_update:gate` and
`work_update:note` suboperations, not variants callers can reach directly
through `work core update`; they still use the same service and storage core as
MCP. Stats/import/export and the remaining broad administrative CLI in the
specification are still planned.

The operator-intended shell command can waive one exact open run obligation
with an attributed reason and retry key:

```bash
engram authority waive-obligation \
  --obligation-id <uuid> \
  --expected-definition <record-id> \
  --waived-by host-operator \
  --reason "accepted without the required test" \
  --idempotency-key <retry-key>
```

There is no equivalent host-private JSON-lines operation: `obligation_waive`
has been removed. Revising or dropping an acceptance binding still clears its
open obligation through the ordinary audited work update. Existing resolution
history remains readable.

MCP and `work_update` cannot request a work-obligation waiver, and agent-facing
projections omit its reason. That surface separation is not authentication:
the shell command has no credential or run-binding check, so any local process
with the binary and store access can invoke it. Removing the host-private
operation does not make the retained shell command authenticated.

### MCP tools

`engram mcp` registers exactly the fifteen agent-facing tools below: the
fourteen words plus `search`.

| Tool | Purpose |
| --- | --- |
| `next` | What is ready, what this session holds, and the changes since its previous call |
| `ls` | Open items with `search`, `ready` or `blocked`, `mine`, `all`, `label`, direct-parent `under` and `optional`/`required` filters, plus non-confidential `after` continuation |
| `show` | One item in safe agent detail; changes neither focus nor claims |
| `add` | A root from a title, or one child with `under`; `optional` permits a peer proposal beneath a foreign-held parent; `notes` records ordered initial observations atomically; outcome and acceptance default from the title |
| `claim` | Hold an item; later calls default to it |
| `update` | One `action`: `release`, `blocked`, `unblock` (optional `blocker` selector), `revise`, `cancel`, `reject`, `after`, `drop_after`, `waive`, `detach`, or `supersede` |
| `gate` | Record one bounded pass/fail observation; completed work accepts it as a late finding without a claim or reopen |
| `note` | Record evidence and checkpoint open work; completed work records only late evidence, both keyless |
| `done` | Complete the held item. A source change with no later matching passing test is recorded and disclosed as untested; any other open obligation returns the typed `open_work_obligations` result |
| `search` | `ls` over every lifecycle |
| `handoff` | `offer`, `accept`, or `cancel` the unique checkpoint-coupled handoff |
| `remember` | Create or explicitly revise an attributed episode under one permanent key; retain history |
| `memories` | List/search current rows or read one current/historical revision of a live key |
| `forget` | Append an attributed terminal tombstone; never erase or reuse the key |

#### Read-only mode

`engram mcp --read-only`, with the same identity arguments, serves a host
child that must not write. It lists only the read words `next`, `ls`,
`search`, `show` and `memories`, and admits only their reading forms:

- `next` only with `peek` exactly `true`, which stages no delivery and
  records nothing;
- `memories` only without `context_generation`: the argument present at all,
  even as `null`, is refused, because a listing that carries one records it;
- each tool only with the arguments it declares.

Any other call, a tool that is not a read word, a writing form of a read
word, or an undeclared argument, is refused before the tool runs, as an MCP
tool error (`isError: true`) whose JSON reads
`{"error": {"code": "mcp_read_only_refused", "message": "MCP read-only mode
refused TOOL: …", "details": {"mode": "read_only", "tool": TOOL,
"restriction": R}, "reminders": […], "next": […]}}`, where `R` is
`tool_not_admitted`, `argument_not_admitted`, `next_without_peek` or
`memories_with_context_generation`; the code is stable. Like every tool
error, it carries `reminders`, one line naming the restriction, and `next`,
the read to make instead: `engram work next --peek` after a tool that is not
a read word or a `next` without the peek, `engram work memories` after a
listing with a generation, and nothing after undeclared arguments. The read words open
the store read-only for each call, and the connection never opens or holds
the writable one, so nothing it does writes the database or its WAL; SQLite
may still coordinate through the shared-memory file. A missing or
uninitialized store refuses and is not created. Ordinary `engram mcp` is
unchanged: it lists all fifteen tools and marks none of them read-only,
since `next` and `memories` have writing forms there. The text a read word
returns may still suggest a writing command; in this mode such a call is
refused. `initialize` gives a read-only connection its own instructions,
naming the five read words and their admitted forms.

A read-only child cannot record a memories listing, so when its host passes
a context generation, the peek's direction to run `memories` with that
generation never settles: the child lists `memories` without it and reads
the entries it needs, and the direction stays standing.

Every agent tool result keeps its structured shape and adds two fields.
`reminders` holds words only, derived by a fixed table from the readiness
`obligations` strings, open `obligation_page` items, active blockers, and the
claim holder. `next` holds literal `engram work …` commands derived by a fixed
table from `allowed_next`: at most one dead-prerequisite `--drop-after`
recovery followed by lifecycle moves in priority order (`handoff --accept`,
`claim`, `note`, `done`, and for an item's only active blocker the exact
`update REF --unblock --blocker SELECTOR`, or `show REF` when several are
active), with three commands total and
one trailing `show REF`, so no receipt lists more than four. Other planning
edits (`--blocked`, `--release`, `handoff --to`, `add --under`, `--title`,
`--cancel`, `--after`, `--waive`, `--supersede-with`) are not synthesized as
general next commands; their tags stay in `allowed_next` on the structured
receipt. A successful `add --under PARENT --optional` beneath a foreign-held
parent lists `show CHILD` and `show PARENT` as inspection guidance and does
not suggest execution; own-parent and root `add` receipts keep the
ordinary claim guidance. A required-child completion refusal supplies the exact waiver command.
The host-only reopen operation remains structured-only. A holder-only mutation
against completed work instead names `note` as the late-finding path and never
suggests reopening merely to record evidence.
Errors keep their stable code and details and add the same two fields. The
shell prints a one-line receipt followed by `reminders:` and `next:`; `--json`
prints the structured receipt plus `effective_session_id` only for a successful
default-session mutation. In the baseline `add → claim → done` lifecycle,
text output contains no full record id (32 or 64 lowercase hex), fence number,
or idempotency key. `scripts/parity.test.mjs` checks that on a fresh store and
counts three commands and at most three agent-supplied fields. Scoped
note/detail navigation and evidence locators may expose full record ids,
including read commands for clipped status notes.

`ls --mine` returns items assigned to the actor plus the session's focused
item when this session holds it; claims on other items are visible through
`show`. `add --under` selects the parent and submits one required child
through `work_propose:decompose`; adding `--optional` instead records an
optional child that is shown as such and does not gate parent completion (a
decomposition admits one through 16 children). Either form then focuses that
child exactly as a root `add` focuses the new root; a bare word that follows
while this session holds other work is refused, as described under reading
and focus above. On open work, `note`
records evidence and then checkpoints the run's current evidence set. On
completed work, `note` records only attributed late evidence after the frozen
completion cut; it creates no checkpoint, does not reopen or reseal the run,
and cannot enter the existing `CompletionSeal`. Repeated commands follow the
[session and intent retry rule](local-work-system.md#agent-native-protocol);
not every keyless repeat is a replay.
An identical child `add --under` in the same session replays the original
creation after decomposition's own parent revision advance, including its
proven restored-run bootstrap. Changed child intent creates new work; other
parent or authority changes refuse instead of silently creating another child.
The `work_decomposition_retry_conflict` refusal names the parent and its changed
state without exposing a server key. Inspect the parent and its existing
children, reuse an already-created child when present, and add new work only
for a genuinely different child intent.

### Work protocol contract

The lifecycle words use the six ambient work operations: `work_next`,
`work_focus`, `work_propose`, `work_update`, `work_complete`, and
`work_handoff`. `remember`, `memories`, and `forget` are a separate thin
project-memory surface, not new work-core operations, and `evaluate` is the
service's separate evaluation entry (`work_evaluate_on`), also not a core
operation. Hosts and operators may call the six work operations directly
through `engram work core`; they are not additional MCP tools, and `evaluate`
is reachable only as a word. Session binding supplies
project, actor, current work,
and cursors, so update/complete/handoff do not repeatedly shuttle ids. Ambient
state contains no authority token; each mutation rechecks the project, item,
claim, and fence state. `work_next` returns only the selected
`focus`, `ready`, `catalog`, `changes`, `memories`, `assigned`, and/or
`participated` sections; omitting `sections` selects all seven. Core CLI callers use
`--sections focus,ready,catalog,changes,memories,assigned,participated` and
MCP callers pass a string array. Selecting no `changes` section never stages or
advances project delivery, including when a prior page remains pending.
Ready and catalog candidates are filtered and limited by maintained SQLite
projections before their compact item rows are decoded. Assignment and label
filters use NFC plus full Unicode case folding, and catalog text search uses a
trigram index over the short reference, title, outcome, labels, and active
blocker detail. These views remain advisory; lifecycle mutations revalidate
their canonical work-event basis under the write lock.
The `--blocked`/`blocked_only` filter is independent of derived availability:
it returns work with an active blocker or incomplete prerequisite even when
the item is deferred or its lifecycle is closed.
Source changes retain dense positions and explicit compact summaries instead
of canonical work snapshots or memory bodies. A change's `object_id` is the
id of the source record the summary was projected from; it names that record
and says nothing about the summary's content. Restricted
work memory, and work memory outside the session's currently focused verified
root, is replaced by a typed `omission` marker at its original dense position;
the protected body and structured fields never cross the agent boundary. The
largest dense prefix fitting the fixed change budget is staged durably; each
call returns the changes since the session's previous call, and the previous
page counts as delivered when the session asks again. A response lost between
Engram and the agent is therefore not redelivered; the section is advisory and
canonical state stays readable through focus and catalog views. A host that
needs exact delivery acknowledges explicitly by returning the exact
`delivered_through` value and opaque `delivery_token` as `acknowledge_through`
and `acknowledge_token`; both fields are absent when changes were not
delivered. A pair matching neither the pending page and token nor the already
confirmed cursor is refused. To recover after an invalid ACK or lost response,
serialize this session's delivery and focus-changing calls, then run
`engram work core next --sections focus` without ACK fields. It reports
`session.confirmed_project_cursor` and `session.pending_delivery` without
staging or acknowledging a page (not necessarily without session writes).
Run `engram work core next --sections changes --acknowledge-through
<confirmed_project_cursor>` with that cursor and no token: acknowledging the
confirmed cursor is a no-op, so any retained page is returned with the same
change payload, `delivered_through` and `delivery_token`, even if later data
exists. Dynamic advisory fields are not part of that exact replay. After
delivering the page, acknowledge its returned pair. This may stage the next
page; repeating the previous ACK does not acknowledge that next page.

Do not drop ACK fields on a changes call to recover: that implicitly
acknowledges the pending page. The whole recovery sequence requires host-side
serialization for the same session; a stale cursor may refuse and require a
fresh cursor read. No exact-replay guarantee covers concurrent advancement or
a focus change discarding the page. Agent `peek` is a different interface,
not the host cursor read. See the complete
[host recovery recipe](../host-checklist.md#recover-a-host-delivery-after-an-invalid-ack).
Concurrent appends wait for the next page. Every successful work response is at
most 12,288 serialized JSON bytes; typed `omissions` report advisory sections
shortened by count or byte budget. A `staged` changes omission instead means
those dense entries remain unconsumed for the next page; it is not a
byte-budget discard. `work_focus` is navigation only and never claims/releases
as a side effect. Its host-only core result returns an exact history count with
a bounded newest-event summary tail, the latest run even after completion, and
a body-free actor-filtered memory index. The `show` word projects that result
into the safe agent detail view described above; event summaries name what
happened instead of repeating only the transition kind and title. A staged
page never blocks a focus change or a mutation; changing focus discards the
un-delivered page and the next call recomputes the same interval under the new
focus.
`work_propose` atomically handles roots and bounded decomposition. `work_update`
carries a typed transition such as claim/release, checkpoint, blocker, cancel,
supersede, deferral, assignment, revision, or prerequisite change. Update and
handoff success responses contain only a compact receipt, one bounded
`obligation_page`, generic readiness `obligations`, and `allowed_next`;
their size does not grow with item history. A successful holder note, update,
evidence, checkpoint, or handoff on open work advances claim expiry to at
least one hour after that mutation without shortening a longer explicit TTL;
successful completion terminalizes it. A completed `note` or `gate` instead
appends attributed evidence without a live claim, claim renewal, checkpoint,
reopen, or reseal. A holder mutation on a lapsed claim is
refused with exactly one next command: `engram work claim <ref>`. Once the work
is ready, that ordinary claim command retakes the same holder's claim, advances
the fence, preserves an active run, and needs
no recovery reason. Every handoff offer expires no later than its source claim.
A different prior holder still requires the explicit recovery path. Each entry
names the exact tool and tagged operation. For example,
`allowed_next: ["work_update:claim(recovery_reason_required)"]` directs the
agent to submit the `work_update` claim variant with an attributed
`recovery_reason`; ordinary
claiming is `work_update:claim`. A successful claim receipt includes
`control_binding { root_execution_id, work_id, run_id, work_revision,
claim_id, claim_fence }`, ready to pass unchanged to host-private
`session_bind`. The same live tuple appears as `focus.control_binding`;
`focus.run` also exposes `root_execution_id` and `work_id`, while
`focus.claim` exposes the claim and fence components. `work_revision` is the
focused work item's revision—the claim receipt's top-level `revision`—not the
claim projection's own revision counter. A binding appears only when
`session_bind` would accept it. The calling session's live claim proposes the
tuple, and the same validation bind runs decides. A claim with a pending
handoff offer therefore shows no binding, though `focus.claim` still shows the
claim; so would a claim under a closed ancestor or outside the active root
execution. No binding while `focus.claim` shows the calling session's live
claim means the session still holds the claim but bind would refuse it now;
`session_bind` with an earlier binding for it is refused as stale until a later
read shows the binding again. This holds when an answer is built: a claim
retried with the same idempotency key returns the stored original receipt,
binding included, so a host reads the current binding with `work core inspect`,
not from a replayed receipt.

A host that must read a claim's binding without moving the agent uses
`work core inspect <ref>`. It returns the same bounded view as `work core
focus`, `control_binding` included, read in one snapshot on a read-only
connection: it refuses a missing or uninitialized store rather than creating
one, selects no focus, stages or discards no delivery page, appends nothing,
registers no session, and carries no focus-bound memory index. Selecting a
different item with `work core focus` is not a side-effect-free read: it
discards a staged delivery page and changes the item that bare agent words and
the control context act on. Inspect always carries `control_binding`: the
binding when the calling session holds the item's live claim and
`session_bind` would accept it, and an explicit `null` otherwise, never a
missing key. Because only the calling session's
claim yields a binding, the host must use the agent's own session id.
`work core focus` omits the key when there is no binding; both forms mean no
binding. The binding goes stale when the holder's planning update changes the item
revision or when a lapsed claim is retaken with a new fence; the host then reads
it again and rebinds between turns. A pending handoff offer hides the binding.
Cancelling the offer restores the same binding, and so does letting it expire
while the claim is still live. An offer never outlives its claim, so the two
can lapse together; then no binding returns, and retaking the claim gives a new
fence and a new binding. Accepting the offer moves the claim to the recipient
under a new fence, so the offering session gets no binding back and the
recipient gets a new one.

A host that must choose which held claim to bind uses `work core held`. It
lists every item in the project on which the calling session holds a live
claim: `work_id`, `short_ref`, `claim_id`, `claim_fence`, `expires_at`,
`claimed_at` (when this session acquired the claim, by claiming it or by
accepting a handoff; renewals leave it unchanged), `focused` (whether the item
is the session's focus), and `control_binding`, the binding `session_bind`
would accept, computed as focus and inspect compute it, or an explicit `null`.
A row with a null binding is still a held claim, for example one with a
pending handoff offer. Only the holder may revise a claimed item, and its
revision re-accepts the claim at the new revision, so the row shows a fresh
binding and an earlier one is stale. Rows come newest claim
first, then by work id, at most 16 of them; `total` counts every held claim,
and `omitted` counts those left out. `focused_work_id` names the session's
focus, or `null`, whether or not it is held. Like inspect, it reads one
snapshot on a read-only connection: it refuses a missing or uninitialized store,
selects no focus, touches no delivery page, appends nothing, and registers no
session.

`work_complete` can consume evidence/checkpoint state created through explicit
`work_update` calls, or accept `capture { summary, refs }` to record evidence,
checkpoint its exact evidence set, and seal in one model-level call. Completion
is refused while blockers, prerequisites, required child seals or explicit
completion waivers, live handoffs, capture requirements, or run obligations
remain unresolved. Open obligations are not an MCP error envelope.
When a satisfied bound criterion is contradicted by a newer relevant failed
or indeterminate check, or its newest check fails the existing verification
freshness rule, completion returns `WorkBoundVerificationRefused`. Its
`work_completion_refused` code, human message, CLI exit 1 and MCP error status
are unchanged. Native CLI JSON and MCP `error.details` add `cause`, containing
the one-based `criterion`, `requirement` (check kind and optional command
fingerprint), `mismatch`, selected `verification`, original `satisfied_by`,
`producer_observation`, actual `result`, and typed `remedy`. A
`stale_source_revision` mismatch adds `cause.stale_source`, the record that
decided it, in the shape the verification detail uses. A word reminder after
the remedy names it as one sentence. The message keeps its words.
The remedy is `run_current_check` for a matcher mismatch or
`run_passing_check_after` for a non-passing result. `error.details.remedy`
and the word reminders format that action from the cause, without parsing
the reason. Candidate selection, the completion-only recording-order
fallback, and admissible bindings and waivers retain their existing rules.
This is an error, not an owed-result completion receipt. The storage
completion transaction rolls back temporary waivers and all completion
effects on refusal; service preparation such as target focus, capture and
checkpoint remains recorded as before.

`work_complete` returns a typed `open_work_obligations` result with the same
`obligation_page` used by `work_focus`, nested `work_next.focus`, and
`work_update`. A completed receipt also returns that page reconstructed from
the exact terminal obligation ids bound into the seal. Each page is count-
and byte-bounded, reports an explicit `omitted_count`, and carries immutable
obligation/definition identities, the required exact rule-set id, state,
rule, requirement, trigger, terminal
resolution/evidence when present, and deterministic typed guidance. An open
verification requirement directs the caller to record matching host
verification, checkpoint it, then complete, or request a host/operator waiver.
Trimming retains open obligations before satisfied, waived or displaced
history and keeps
deterministic trigger/resolution ordering within those state groups. In focus
evidence, a visible verification summary keeps its referenced environment
summary ahead of it. Count and byte trimming remove
unrelated or dependent evidence before breaking that visible typed closure.
Generic readiness strings remain a separate compatibility field. A successful
completion result is durably replayable under the same idempotency key;
recoverable refusals are recomputed from current state. The pending attempt
retains the original request and work target, so a lost refusal cannot redirect
the same caller key after focus moves. An interrupted
capture-backed completion reuses committed evidence, and reuses an exact
checkpoint while its acknowledged run-feed cut remains current. If the feed
advances, the retry writes a new checkpoint under a cut-derived substep key.
Cut selection and that checkpoint append share one SQLite write transaction.

For a cancelled or superseded required child without a seal, qualifying
[sibling successor resolution](local-work-system.md#gates-prerequisites-supersession-and-project-memories),
or waiver, the refusal names that lifecycle and returns the runnable agent command
`engram work update PARENT --waive CHILD --reason "why"`. The matching MCP
update uses `action: "waive"`, `child`, and `reason`; both translate into the
existing typed `work_update:waive_required_child` operation. The project-bound
session records the reason-attributed, audited waiver, after which retrying
`done` re-evaluates the current completion barrier.

Recoverable completion refusals add `recovery { cause, item, command }` to the
receipt. `cause` is a tagged value for `open_obligation`,
`required_child_unsealed`, `missing_contribution`, or `missing_acceptance`,
including the exact blocker identity. `item` carries the
affected full id, short ref, title, and lifecycle-backed state. `command` is
deliberately a single next command, and the `done` verb exposes exactly that
one entry in its `next` list. Beside an `open_obligation` cause, never inside
it, `recovery.open_obligation_check` names the check the obligation waits for.
It is read at the completion cut:
- `none_followed` when no passed check of its kind was recorded after the
  obligation opened;
- otherwise `newest`, with that newest passed check's `verification` record
  and `position`, and its `mismatch` or `left_out` reason. A stale mismatch
  adds the `stale_source` that decided it.
A reminder after the cause's unchanged words says the same. For a `newest`
check, `next` adds `show REF --note RECORD`, which reads that check's
detail. Recovery guidance is not a replayable result: it
is rebuilt from a coherent current snapshot so a retry observes a child,
contribution, obligation, or acceptance barrier that moved. Native `done` and
the fifteen-tool MCP surface return this as a typed refusal receipt. The JSON
core prints the same typed refusal receipt on stdout and exits with status 1;
it does not wrap the refusal in an error envelope. Short-ref ambiguity likewise
returns a stable
`work_reference_ambiguous` error with up to eight ordered candidates, an exact
`more` count, and full-id retry guidance on every JSON front door.

The host-only JSON core schemas for `work_propose`, `work_update`,
`work_complete`, and `work_handoff` use typed discriminated inputs.
Each accepts an optional `work_ref`; the target is resolved and bound inside
the mutation, so a concurrent focus change by the same session cannot redirect
it, and it becomes the ambient focus as a side effect. `idempotency_key` is
optional on every mutating branch; the
[session and intent retry rule](local-work-system.md#agent-native-protocol)
defines automatic keys, explicit overrides, and current replay limits.
Durable attempts bind caller intent separately
from the current focused work/claim/handoff basis. A pending refused completion
may refresh its live claim basis only after the original target binding is
verified; committed successes replay, and an interrupted attempt cannot mutate
a newly focused item. Omitting
`work_complete.acceptance` asserts every current criterion with the note
`accepted by <actor_id> via work done` (or the supplied `note`) and leaves each
criterion's evidence empty unless explicit `links` and `link_basis` bind
existing records to selected positions. Those links cannot be combined with
explicit `acceptance`. Explicit per-criterion citations pass through and
must belong to the completion evidence set; work-level evidence is never
automatically assigned to individual criteria. Omitting
`work_update:checkpoint.evidence` acknowledges every evidence object already on
the live run.

Work search/lifecycle filters, paged catalog results, and item history ship in
the ambient query/focus views. Stats, stale/orphan diagnostics, approval
decisions, import/export, and report publication remain administrative tools
over the same core. Successful model responses are terse; refusals return a
stable code plus a satisfiable remedy. Full durable receipts go to the host. No
replayable control-plane turn grant token appears in model-visible MCP
output. Agent-facing work itself has no grant token.

One capture powers peer context and the ordered feed. The host may use a
mailbox as a doorbell, but must not relay full state or make the agent repeat
the same fact into another status ledger.

### Shipped host-private turn channel

`engram control` is a long-lived stdio process. It accepts one JSON object per
line and returns one `{ "status": "ok", "result": ... }` or typed error line.
The runtime session and asserted actor are fixed by process arguments:
`--actor-id`, `--session-id`, and the optional `--actor-context` (or
`ENGRAM_ACTOR_CONTEXT`) that the work words and the MCP server also accept. A
host passes one context to every channel of a session; the control connection
normalizes it the same way and records it on its attribution, never on the
principal. The shipped operations are:

| Operation | Durable effect |
| --- | --- |
| `session_bind` | Resolve a shared control anchor by project and external reference, optionally bind an exact live `WorkRun` claim, rotate a routing token, reset to `ready`. Binding creates no task or join event. Its result carries the session status. |
| `session_status` | Read current phase, epochs, mediation declaration, optional work binding, revision, `open_grant_id` plus `open_grant_state`, and, for a work-bound session, the claim's `named_root` state |
| `turn_evaluate` | Derive membership, phase, policy and work binding from SQLite and persist a decision plus optional grant |
| `turn_begin` | Recheck the grant's basis, then consume the issued grant; for a work-bound session the receipt carries the claim's `named_root` state |
| `turn_checkpoint` | Atomically append bound execution observations, complete the grant, and append a canonical control checkpoint event |
| `named_root_bind` | Append a claim's named source-root event, `bound` or `ended`, to the project, root and run feeds |
| `named_root_read` | None. Read one claim's named root on its run, for any run and claim of the project: identities, run and claim lifecycle, the derived `named_root` state, the newest root event by its real id, and the run-feed cut, from one snapshot |
| `execution_observe` | Append one record of execution the host observed without admission (a turn seen after it started, a change between turns, or a check inside such a turn) to the project, root and run feeds. It creates no grant, begin or turn status, renews no claim and credits no check. Under `account_if_eligible`, naming the project's current policy, an eligible reported change accounts at its own position and opens the selected rule set's obligations; an ineligible one is kept as `audit_only` with its reason; see [Record execution observed without admission](behavioral-control-plane.md#5b-record-execution-observed-without-admission) |
| `acceptance_binding_read` | None. Read, for one item on its active run, what satisfied each bound criterion: the obligation completion selects, its recorded resolution, and the original verification with its producer, in pages pinned to the run-feed cut the first page captures; see [Reading what satisfied a bound criterion](acceptance-evaluation.md#reading-what-satisfied-a-bound-criterion) |
| `acceptance_verification_read` | None. List, for one criterion of an item on its active run, every host verification of its bound kind up to a caller-named cut that must be the run's head, with each record's id, kind, fingerprint, result, source basis, position and producer, in run-feed order and in pages with exact counts; it judges nothing; see [Listing the candidate verifications of a criterion](acceptance-evaluation.md#listing-the-candidate-verifications-of-a-criterion) |

The `named_root` state, `none`, `bound` or `unbound_by_release`, is the
authoritative read a host uses to decide when to name a root again; see
[Bind a named source root](behavioral-control-plane.md#5a-bind-a-named-source-root).

The bind response supplies the `routing_token` used on later calls. A grant
carries no delivery page, and there are no recovery turns: the work context an
agent sees comes from `next`, which keeps its own staged delivery. A freshly
bound session is `ready`, and its first turn is granted at once. While hosts
move off the old fields, `turn_evaluate.purpose` may be `ordinary` or absent
(any other value is an `invalid_request`), and `turn_begin.delivery_tokens` may
be `[]` or absent; a non-empty list is refused with `grant_scope_mismatch`.
The checkpoint receipt's `confirmed_cursor` repeats its `cursor`, the position
of the checkpoint event in the task's write-only audit index. Exact retry
evidence remains canonical across process restart, but a newly opened control
connection invalidates unbegun grants and returns the session to `ready`; old
results never resurrect authority. The new connection also fences a still-live predecessor, whose next
operation fails with `control_connection_superseded`. A begun grant is not
silently replayed or discarded: `session_status.open_grant_id` identifies the
required checkpoint and `open_grant_state` distinguishes `issued` from
`begun`. A fresh `turn_evaluate` key atomically supersedes an
issued-but-unbegun grant and records an immutable transition bound to the
replacement decision; an already-begun grant instead refuses with
`turn_already_open`. A begun turn exposes no replayable prompt because its
outcome may be uncertain; the host closes it with a report. Reusing a key for a
different intent fails.

A native local-work bind supplies `work_binding` with
`root_execution_id`, `work_id`, `run_id`, `work_revision`, `claim_id`, and
`claim_fence`. Storage verifies that exact tuple against the session's live
claim and copies it into every turn-grant basis. Ownership means
`claim.holder == session_id` for the MCP actor session that claimed the run;
the asserted `actor_id` is audit context and never substitutes for that holder
check. A malformed, cross-project, or currently peer-owned bind fails as
`work_claim_mismatch`. A tuple that canonical history proves belonged to this
session but whose revision, fence, claim, handoff, run, root execution, or
expiry moved before bind fails as `stale_fence`, telling the adapter to reread
and rebind. The same movement after bind refuses evaluation or begin with
`stale_fence`. Omitting `work_binding` binds only the shared control scope,
without a work-claim binding; that session cannot append run execution observations.

`turn_checkpoint.observations` accepts at most 64 host facts containing
`observation_id`, `action_fingerprint`, `effect`, `outcome`, and
`source_changed`. An observation may also carry
`source_basis { workspace_id, source_revision }` and `observed_at`, and,
with `source_changed=true`, `reported_source_change`: how the host
established the change, `content_comparison`, `assumed_missing_baseline`
or `watcher_only`, as the [checkpoint section](behavioral-control-plane.md#5-checkpoint-the-turn)
defines them. The checkpoint is refused when the field comes with
`source_changed=false`, when `content_comparison` or
`assumed_missing_baseline` comes without `source_basis`, when `watcher_only`
comes with one, or when the value is not one of the three, which is refused
by name. A host that does not say leaves the field out.
`source_revision` is the host's fingerprint of the full relevant content,
including committed and dirty bytes. `workspace_id` is retained for audit but
does not participate in anti-stale equality. Engram supplies the authoritative
project, frozen work binding, session, grant, actor, and recording timestamp,
then appends each canonical observation to the project, root-work, and
run-execution feeds in the same transaction as the control checkpoint.

`turn_checkpoint.verification_evidence` accepts at most 16 host-minted checks.
Each entry supplies `producer_observation` as either
`{ kind: "object_id", object_id }` or
`{ kind: "observation_id", observation_id }`, plus `check_kind`, optional
`summary`, and bounded `refs`. Storage derives the check fingerprint, outcome,
source/run/session binding, and timestamps from the producer; an unknown
producer returns `verification_producer_not_found`. Up to four
`environment_evidence` entries bind an environment identity to an exact source
basis. The opaque form supplies only a host-produced fingerprint.
The component form adds
`components { toolchain, sandbox?, workspace_id, capability_map_revision }`;
Engram derives the canonical component fingerprint, requires the component
workspace to equal the source workspace, and requires the capability-map
revision to equal the bound session. Component strings are trimmed,
nonempty, limited to 256 bytes, inspected by the configured redactor, and are
asserted host context rather than attestation. Do not place secrets in them.

A verification may cite an environment as
`{ kind: "object_id", object_id }` or
`{ kind: "index", index }`, where the index addresses the same request's
ordered environment list. The referenced object must belong to the same run
and source revision. The current built-in test requirement does not require a
particular environment, but its optional link is retained for audit and future
typed policies. Mismatched derived bytes, a missing reference, or a run/source/
session basis mismatch return `environment_fingerprint_mismatch`,
`environment_evidence_not_found`, or `environment_basis_mismatch`.

The receipt returns all three typed record-id lists. Begin and checkpoint keys
are each scoped to the exact grant and canonical request intent. An exact retry
returns those ids without another feed append; changing any ordered list under
the same checkpoint key fails with `control_operation_idempotency_conflict`.

Agent-facing `work_update:evidence` retains its generic form. It also
accepts the attach-only form
`{ kind: "evidence", attach: { evidence: <typed-record-id> }, idempotency_key }`.
Attach validates that the id names verification/environment evidence on the
focused run and does not mint another canonical object or feed entry. Generic
evidence can be cited for context and completion, but cannot satisfy a typed
verification requirement.

Every work-bound observation with `source_changed: true` atomically opens one
built-in test obligation, irrespective of `outcome` and irrespective of
whether `source_basis` is present. The recorded `source_changed` is the
core's reading of the host's report, not the host's literal flag;
observations stored before this rule keep the host's flag and are read as
stored. It is true when the host reported a change, unless the reported
`source_revision` equals the revision of the run's newest recorded source
change and no host record on the run since that change (observation or
environment evidence) carried another revision, in any
workspace. Such a repeat is recorded as `source_changed: false` and opens
nothing, so writes under git-ignored paths that leave the content unchanged
do not count, while a move seen only by a check or an environment capture,
even one that later came back, still counts at the next change. When the
report or that newest change carries no revision, the host's report stands, so a
later change with a revision still re-anchors obligations that a
revision-less change left waiver-only. For a claim without a named root, a
passed typed test satisfies open obligations only against the newest mutation
source revision at the evaluated run-feed cut. Thus a newest basisless
mutation makes the open set waiver-only until a later basis-bearing mutation
plus passed test arrives; that later test may satisfy both the earlier and
newer definitions. For the stock rule that waiver comes at `done`: completion
records each still-open stock obligation as an untested change instead of
refusing. Under a named root, a fresh check in the root that ran after a
basisless change accounts for it, and `done` refuses the changes a named root
holds open, now or since it ended or the claim was released, instead of
recording them as untested (see
[the host binding](behavioral-control-plane.md#5a-bind-a-named-source-root)).
`work_focus` exposes the
canonical bounded `obligation_page`, the same field appears inside
`work_next.focus`, and `work_next` deltas use
`obligation_opened`, `obligation_satisfied`, `obligation_waived`, or, for a
waived stock obligation, `untested_source_change` naming the change and its
source revision, or, for a displaced one, `foreign_workspace_change` naming
its workspace, without leaking host authority. A host's named-root events
arrive as `source_root_named` and `source_root_ended`, naming the workspace
and generation. The page's `untested_total` counts every untested change on
the run, and its items name those that fit; likewise `displaced_total` counts
every displaced change, and an item's `displaced_change` names its
observation id, workspace and source revision.
Its `open_total` counts every open obligation on the run before count and byte
trimming, and is 0 for an item that has no run yet, such as one restored from
a work-graph snapshot and not yet claimed. When it exceeds the open
items shown, an open obligation was left out, and the agent reminder says more
obligations are open than shown. Only a page stored before this count existed
lacks it, and that page keeps its original reminder.

A page whose run completed, or that a completion seal binds, carries
`historical: true`; the field is absent otherwise. Its rows are history and
owe nothing. A row whose state is `open` there is no longer actionable: on a
live read it was opened after the run finished, for example by an observation
a host recorded late; in a stored receipt replayed after the run finished, it
is the row as the receipt recorded it while the run was live. `historical`
alone does not say when a row was opened. Such a page's `open_total` is 0,
every item's guidance is `{"action": "none"}`, and no reminder, evaluation
guidance or mutation `obligations.open` count offers it as owed work,
including a stored receipt replayed after its run finished. `show` says
"historical (run completed)" and reports how many such rows the bounded page
shows as `historical_open_obligations`. A completion's sealed page lists only
the obligations its seal binds, read back at the seal's cut: a row the seal
does not bind is accepted only when it was opened after that cut, and never
breaks the readback.

Every new completion seal declares obligation schema V1 and freezes the exact
definition/resolution id pairs applicable at its dense pre-seal cut. The
final checkpoint must acknowledge the matching typed verification evidence.
A later `note` or `gate` from any project-bound session is marked in the
evidence actor's existing provenance chain and appended after that cut. It is
visible in `show` notes and peer `next` changes but never changes the frozen
seal or adds a completion barrier.
A new seal also declares environment schema V1 and cites the sorted, distinct
environment-evidence ids at or before that cut. It refuses more than 64
environment records and never copies the component payload into the seal. A
parent verifies required child seals transitively; every accepted seal carries
the current obligation and environment schema bindings.

An observation effect absent from the frozen grant is a request error with
code `observation_scope_mismatch`. Checkpointing an issued-but-unbegun grant
returns `grant_not_begun` with host transition guidance: bind/recover the
runtime as needed, evaluate a fresh turn, begin that exact grant, and only then
checkpoint it. `grant_scope_mismatch` remains the general frozen-basis mismatch.
Checkpointing an already-begun grant compares its frozen session/grant binding
but deliberately does not recheck claim expiry or live ownership: begin already
consumed authority, and checkpoint records what happened rather than granting a
new action.
This control operation is deliberately named `turn_checkpoint`; the local-work
lifecycle operation `checkpoint_work` remains the separate run-progress and
evidence checkpoint.

The project `required_assurance` floor constrains turn decisions for bound
control sessions. The host is responsible for enforcing its declared mediation.
Ordinary CLI/MCP work words do not require a control grant, so the policy value
alone does not establish that all access to the store is gated.

The built-in policy grants `observe`, `communicate`, and turn-gated
`mutate_local`. A session must meet the project and effect assurance floors,
and declare mediation covering the requested effect. The bind receipt exposes
`effective_mediated_effects`, capped by the host's assurance. Internal
`coordinate` is not a model-turn capability. Shared/external/lifecycle effects
and `action_gated` bindings remain unavailable.

Resource leases and the host `obligation_waive` operation are removed.
`turn_evaluate` still accepts `resource_intents: []`; supplied resource subjects
are project-bound and normalized, but do not acquire exclusive ownership.
Host/user authority still governs file and external mutation. The only turn
purposes are `ordinary` and `recovery`; purpose `finalizer` and session phase
`finalizer_open` are removed and not accepted.

For path resource intents, the core rejects a different embedded project id and
NFC-normalizes every segment. Path-bearing host commands (`init`, `doctor`, `control`, `authority`,
`control-policy`, `readiness`, `control-session-inspect`) resolve the project root's filesystem identity before
opening the store: `--host-path-policy case_fold|case_sensitive`
(or `ENGRAM_HOST_PATH_POLICY`) when the host knows it, otherwise a probe that
looks the project file up under the opposite ASCII case: nothing there means
the root tells the spellings apart, and when something is there the root's
listing says whether it is the same entry. The probe only reads. A project
file name with no ASCII letter (or not Unicode text), a name that reaches the
file through a wider alias than ASCII case (such as a non-ASCII case variant,
a short 8.3 name, a trailing dot or space, or another Unicode normalization),
a project file that changes while it is probed, or a failed
lookup or listing leaves the identity unresolved. Agent work words, MCP
startup, graph, backup, restore and import do not run that probe; they still
perform their ordinary store and file I/O. The first resolved writable opener
persists that policy; read-only [readiness](host-readiness.md) never binds it
and explicitly reports an unbound or unresolved identity. Later resolved
openers must present the same one, and a mismatch names both. An opener that
could not resolve the identity (a project file the probe cannot test, or a
lookup that fails) still
reads and tracks work, but path-bearing control requests are refused with
`host_path_identity_unresolved` instead of guessing. Windows alias rules
(reserved names, alternate data stream syntax, trailing-dot/space aliases,
known 8.3 aliases) follow the running operating system. `doctor` reports the
persisted and resolved policy.

Standalone delivery acknowledgement, heartbeat, and independent exit are not
built. Action authorization, begin and completion are designed and deferred:
see [action gates](action-gates.md#when-to-build-it).

Hooks can integrate the shipped turn boundary. Full action gating needs a wrapper,
gateway, or native host integration around every declared material tool. If a
shell or network path remains unmediated, the session must not claim
`action_gated` assurance. See the
[control-plane host contract](behavioral-control-plane.md#host-integration-contract).

### Host configuration

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
        "--actor-context",
        "model=opus-4.1;reasoning=high",
        "--source-skill",
        "engram-repo"
      ]
    }
  }
}
```

The proprietary runtime supplies actor/session/tool/skill instruction context.
This process exposes exactly the fifteen MCP tools. V1 records host
context with `asserted` assurance; configuration text is not authentication.
Distinct concurrent sessions need distinct `--session-id` values. The database
is shared; the MCP processes are not.

### Dogfood contract

`scripts/parity.test.mjs` runs the real binary against a fresh home with
`engram init` as host setup outside the count,
then drives `add → claim → done` and fails if the agent needed more than three
commands or three supplied fields, typed JSON, or saw a full record id
(32 or 64 lowercase hex), fence, or key in text output. It also checks that an
unheld `note` records a marked observation without execution credit, while an
unnoted `done` supplies its resolving command even when observations exist.

`scripts/mcp-dogfood.test.mjs` launches real stdio MCP processes against a
fresh home. Its main lifecycle uses only the agent-facing MCP tools: one session
creates, claims, blocks/unblocks, notes, and offers a root; a peer accepts the
checkpoint-coupled handoff, notes, and seals it with `done`. Keyless replay,
`reminders`/`next` derivation, catalog and search filters, cancellation,
compact completion, child creation under a parent, and field revision are
asserted along the way. The receipt checks in this lifecycle reject full
record ids (32 or 64 lowercase hex), fences, and keys in `reminders` and
`next`; separate status-recovery tests preserve the scoped note-locator
exceptions. The CLI path drives the same lifecycle through the words in
text and `--json` modes and keeps one
`engram work core focus` call. Both scripts are part of `scripts/check.sh`.

The shared [test launcher](../development.md#test-launcher) drives these gates;
`scripts/test-launcher.test.mjs` checks its execution, diagnostics and completion
delivery behavior alongside the review-fingerprint suite.

`scripts/control-dogfood.test.mjs` launches the real bounded JSON-lines service,
bootstraps an advisory policy, activates a turn-gated successor through the
operator CLI, verifies both versions through `doctor`, binds, evaluates,
restarts before begin, proves the old grant cannot begin,
resynchronizes, checkpoints, checks mutation denial, and probes a wrong
routing token. Host action control,
report finalization/publication, review
actions, history, explicit contradiction resolution, and the remaining
administrative CLI remain planned surfaces. They must reuse this core rather
than fork its semantics.
