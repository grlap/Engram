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
Claiming or explicitly targeting a mutation establishes focus; a bare mutation
keeps its existing target, not the item just read. These reads do not register
a fresh process-default session; registration waits for a stateful operation.

```bash
engram work next --peek [--verbose]  # orientation without advancing delivery
engram work next [--verbose]         # explicitly advance ordinary delivery
engram work ls [--search TEXT] [--ready | --blocked] [--mine] [--label L] [--all] [--under PARENT [--optional | --required]] [--limit N] [--after CURSOR] [--verbose]
engram work show REF [--notes [--gates] | --history] [--after CURSOR]
engram work show REF --note ID[:INDEX]  # complete immutable note detail
engram work add "Title" [--note "Initial finding"]... [--outcome "..."] [--accept "criterion"]... [--bind POSITION=KIND[:FINGERPRINT]]... [--under REF [--optional]] [--priority 0-4] [--kind KIND] [--label L]
engram work claim REF [--ttl SECONDS] [--recover "why"]   # same holder renews; --recover is for another prior holder
engram work claim --under PARENT [--ttl SECONDS] [--recover "why"]   # hold the parent's next ready child, chosen in ls --ready order and claimed in one transaction
engram work update REF [--release | --blocked "why" | --unblock | --cancel "why" | --reject "why" | --after OTHER | --drop-after OTHER | --waive CHILD --reason "why" | --supersede-with NEW --reason "why" | --assignee A | --priority N | --defer DATE | --accept "criterion"... | --bind POSITION=KIND[:FINGERPRINT]... | --title "..." | --kind KIND | --label L | --unlabel L]
engram work gate NAME [--work-ref REF] [--failed FAILURE]... [--ref opaque-reference]
engram work note [REF] "What you found or decided" [--ref path-or-url]
engram work done ["What was delivered"] [--link POSITION=LOCATOR --link-basis N]
engram work handoff REF --to SESSION | --accept | --cancel "why"
engram work remember ("Project note" | --text "Project note") [--key KEY [--revise [--expected-revision N]]]
engram work memories [QUERY] | engram work memories --after KEY | engram work memories KEY --full [--revision N]
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
The latest note is emitted last in `notes`; on a full page, it replaces the
least-priority selected note.
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
Raw actor/session metadata is not part of the ordinary terse show projection.
It otherwise omits canonical UUIDs and hashes, revision and fence counters,
and host-only run, claim, control-binding, obligation-page, and memory-version
fields. The scoped exceptions are note/detail locators, sealed evidence links,
and an open item's `acceptance_basis` when it has criteria to link; the basis
is a read-concurrency token, not execution authority. Humans and hosts that
need the rich projection use
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
ref and item revision from one read snapshot. JSON retains exact stored text;
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
`obligations.open` counts open entries on the source obligation page;
`obligations.omitted` retains its exact undisplayed count, not an assertion
that omitted entries are resolved. Actionable reminders, source omissions,
refusal `code`/`remedy`/`recovery`, and done's child-follow-up groups remain.
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
  (`engram work memories`). That command also remains in `next`. Memory
  `changed` compares the recorded advertisement, not whether notes were read
  or applied. Pure reads, including `memories`, do not acknowledge it;
  ordinary `next` retains its existing rendered-signal acknowledgement.

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
  as omitted notes in the default stream. Each row names its `family`.
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
  families over the combined stream, with total/shown/omitted for each.
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
  A cursor binds item, project, kind,
  immutable boundary/member, order and read cut. Mismatches, changed project
  feeds, reversed clocks and crossed time boundaries refuse with
  `work_show_cursor_invalid` and a fresh same-kind command. The cut uses the
  listing reader's conservative boundary-millisecond policy. Tokens encode
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
  they count the acceptance list as typed and are carried to the stored
  order; `--bind` alone on `update` counts the stored list, as `show` numbers
  it. `show` marks each bound criterion `[requires host KIND verification]`
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
  again.
- `update REF --accept "criterion"...` replaces the whole acceptance list in
  one attributed revision. Omission preserves it; empty lists and any blank
  criterion are refused. The core trims, sorts, and deduplicates criteria.
  History names the supplied fields, and prior canonical criteria remain in
  history. Completed work is immutable; `note` is the late-finding path.
- `ls` prints `showing X of N` and returns exact `total` and `omitted` counts
  beside `more`. Count and page share the same normalized filters and SQLite
  read transaction. `--mine` is assignment to this actor OR a live claim held
  by this session, counted once before limiting. The default limit is 20
  (explicit limits clamp to 1–1000). The complete text and JSON receipts,
  including footer and continuation, fit 12 KiB. The footer names the active
  `limit` and `byte_budget` (12288 bytes). Nonempty truncated pages include an
  `after` token and one `next` command repeating all filters and the
  active limit with `--after CURSOR`; MCP `ls` accepts the same `after` value.
  The cursor names the last row actually emitted, including after byte fitting.
  `shown_before` counts the prior prefix; `omitted` is the remaining total after
  that prefix plus this page, and `more` is true exactly when some remain.
  Ordinary listings stay ascending work id. `ls --ready` uses priority then
  work id, matching compact `next`. A changed project feed, expired time basis,
  reversed clock, malformed cursor, or different filters/project returns
  `work_catalog_cursor_invalid` with a fresh same-filter command, never a silent
  restart. Unrelated project notes also advance the feed; focus-only reads do
  not. Tokens are opaque to the caller, not confidential: they encode readable
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
- `add` needs only a title. Outcome and acceptance criteria are welcome; they
  are what `done` is checked against. When acceptance is omitted, text and JSON
  reminders say `acceptance defaulted to the title being done; set --accept`.
  This keeps the signal without repeating the item title; the final receipt
  includes the reminder in its response budget.
  Explicit acceptance suppresses that reminder; blank criteria are refused.
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
- `update --kind`, repeatable `--label`, and repeatable `--unlabel` revise
  indexed planning metadata through the existing audited planning path;
  unclaimed planning updates remain allowed.
- `note` is for decisions, findings, and evidence pointers. A holder note
  feeds peers, handoff, and the final report. A non-holder observation feeds
  project/root peers without a checkpoint, claim renewal, or run credit.
  Its immediate receipt marks `non_holder: true` and says
  `(observation, no run credit)`. A late note feeds peers but remains outside
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
  class never contains the underlying error body, path, hash, or actor text.
- Every answer ends with `reminders` (what is owed, in words) and `next`
  (commands you can run now). Ordinary mutation words never ask for hashes,
  fences, or idempotency keys. Optional criterion linking explicitly reuses
  note locators and the `acceptance_basis` read token; it grants no authority.
  Note-detail navigation is the other scoped locator exception, as above.
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
  Gate names follow the repository's
  [quality gates](../development.md#quality-gates).
  Classification stays your judgment: a product defect gets a required
  child through the ordinary `add … --accept "<test> passes" --kind bug
  --label gate`, and test or environment findings go into the durable note.
  `gate NAME` alone always records a pass. Every failure supplies at least one
  bounded `--failed` label; when no test id exists, use the check command or
  check name. A consecutive identical result replays;
  the same result after an intervening different result records a fresh gate
  transition.
- `evaluate` records one immutable acceptance evaluation on the targeted
  item's active run: `evaluate [REF] --mode MODE --acceptance-basis N
  --evidence-basis M --verdict POSITION=VERDICT[:BASIS] --rationale
  POSITION=TEXT [--evidence POSITION=LOCATOR]...`, where `show` prints both
  bases and `LOCATOR` is a note/gate locator exactly as `show --notes
  --gates` prints it (resolved as `done --link` resolves it) or the full
  record id of host-minted verification or environment evidence. The evaluator's
  own session is the attributed identity; a pass needs at least one
  run-evidence citation; the core validates structure and provenance, never
  relevance, and refuses a submission whose evidence basis a host-observed
  change has already overtaken. The receipt and ordinary `show` carry a
  bounded prefix of verdict rows with an exact `verdicts_omitted` count and
  the decision facts (passed count, first blocking verdict, evaluator label,
  freshness, recorded source fingerprint); `next` prints one `evaluation:`
  line under the focused evaluated item; `show REF --full` returns the
  complete newest evaluation with every rationale and citation. It is
  refused until an operator enables the
  [acceptance evaluation](acceptance-evaluation.md) policy, and under that
  policy `done [--source-fingerprint F]` consumes the newest fresh, all-pass
  evaluation instead of the author's self-assertion, presenting the
  host-measured fingerprint when the policy requires source freshness. An
  independent evaluator that later takes the run cannot consume its own
  pass. `done` and the completed item's `show` say where the sealed
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
SCHEMA12)`. Agent `next` ends its terminal text with one diagnostic line:
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
these additive diagnostic fields; its generation is asserted host context.
Compare the build token with a fresh CLI process after an
install to detect a stale, long-lived MCP child; restart the child to run the
new executable. The token is diagnostic, not an execution hash to copy into
commands, authenticated identity, or store-admission authority.

Every `doctor --json` mode and `readiness --json` includes `build` and
`build_fingerprint`. The build
object contains `package_version`, `executable_sha256` (SHA-256 of the running
executable's bytes), and `schema_reference` (SHA-256 of the RFC 8785 canonical
ordered, whitespace-normalized SQLite definitions used by ordinary schema
admission). The fingerprint hashes the RFC 8785 canonical build object. These
values are captured once per process: at MCP startup, or when a short-lived
CLI process emits diagnostics. Agent words other than `next` do not compute
identity. There is no Git metadata, build script, capability catalog, or
persisted last-writer row.
Equal inputs produce equal fingerprints; different executable bytes or schema
definitions distinguish builds even when their package versions agree.

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

# Host/operator boundary: activate a new immutable policy version. The
# optional expected policy id is the `id=` reported by doctor and prevents a stale
# operator from overwriting a concurrent policy update.
engram control-policy set-required-assurance turn_gated \
  --authorized-by host-operator \
  --idempotency-key enable-host-turn-mediation \
  --expected-policy-hash <active-policy-id>

# Select a bounded typed obligation set. The required environment must already
# be a canonical EnvironmentEvidence record id returned by a host checkpoint.
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

Actor context currently binds only the work/MCP service. The behavioral
control plane keeps its existing actor/session and environment-evidence
attribution contract.

The optional actor context can equivalently arrive through
`ENGRAM_ACTOR_CONTEXT`; it is fixed when the CLI invocation or MCP connection
binds its session. The MCP process retains one `LocalWorkService` and its
lazily opened SQLite connection for its lifetime. All fifteen MCP tools use
that service. A failed operation rolls back before the next call uses the
connection.

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
that storage uses. `check_fingerprint` compares a canonical check description;
`required_environment` names an exact canonical environment-evidence record.
Neither accepts a shell command or an environment description in place of that
fingerprint or id. Re-supplying the active set under a fresh key
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
`untested source change: ID (source revision REV); no matching passing test
followed it`, then `untested source changes: N more not shown (T in total)`
when the bounded obligation page names only some of them. With `--json`, the
named changes are in `untested_changes` and the count left out is in
`untested_changes_omitted`.
Before completion, the receipt reminder says tests have not run since the last
source change and that `done` records the change as untested without one. The
remaining text is unchanged. Criteria bound with `--bind` still refuse, and
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
| `update` | One `action`: `release`, `blocked`, `unblock`, `revise`, `cancel`, `reject`, `after`, `drop_after`, `waive`, `detach`, or `supersede` |
| `gate` | Record one bounded pass/fail observation; completed work accepts it as a late finding without a claim or reopen |
| `note` | Record evidence and checkpoint open work; completed work records only late evidence, both keyless |
| `done` | Complete the held item. A source change with no later matching passing test is recorded and disclosed as untested; any other open obligation returns the typed `open_work_obligations` result |
| `search` | `ls` over every lifecycle |
| `handoff` | `offer`, `accept`, or `cancel` the unique checkpoint-coupled handoff |
| `remember` | Create or explicitly revise an attributed episode under one permanent key; retain history |
| `memories` | List/search current rows or read one current/historical revision of a live key |
| `forget` | Append an attributed terminal tombstone; never erase or reuse the key |

Every agent tool result keeps its structured shape and adds two fields.
`reminders` holds words only, derived by a fixed table from the readiness
`obligations` strings, open `obligation_page` items, active blockers, and the
claim holder. `next` holds literal `engram work …` commands derived by a fixed
table from `allowed_next`: at most one dead-prerequisite `--drop-after`
recovery followed by lifecycle moves in priority order (`handoff --accept`,
`claim`, `note`, `done`, `update --unblock`), with three commands total and
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
default-session mutation. Text output never contains a 64-hex hash, fence
number, or idempotency key. `scripts/parity.test.mjs` checks that on a fresh
store and counts `add → claim → done` at three commands and at most three
agent-supplied fields.

`ls --mine` returns items assigned to the actor plus the session's focused
item when this session holds it; claims on other items are visible through
`show`. `add --under` selects the parent and submits one required child
through `work_propose:decompose`; adding `--optional` instead records an
optional child that is shown as such and does not gate parent completion (a
decomposition admits one through 16 children). Either form then focuses that
child exactly as a root `add` focuses the new root. On open work, `note`
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
Trimming retains open obligations before satisfied or waived history and keeps
deterministic trigger/resolution ordering within those state groups. Focus
evidence uses the same actionable-first rule: environments required by visible
open obligations are retained first, and a visible verification summary keeps
its referenced environment summary ahead of it. Count and byte trimming remove
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
one entry in its `next` list. Recovery guidance is not a replayable result: it
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
replayable control-plane turn/action grant token appears in model-visible MCP
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
| `session_bind` | Resolve a shared control anchor by project and external reference, optionally bind an exact live `WorkRun` claim, rotate a routing token, reset to `sync_required`; a re-bind to the same anchor keeps its confirmed position and moves past only contiguous events that session wrote itself, stopping before a peer event; a first bind or a bind to another anchor delivers its feed from the start. Binding creates no task or join event. |
| `session_status` | Read current phase, cursors, epochs, mediation declaration, optional work binding, revision, `open_grant_id` plus `open_grant_state`, and any safely redeliverable partial recovery grant |
| `turn_evaluate` | Derive membership/context/head/policy from SQLite and persist a decision plus optional grant |
| `turn_begin` | Recheck freshness and exact delivery token, then consume the issued grant |
| `turn_checkpoint` | Promote tentative delivery, atomically append bound execution observations, complete the grant, and append a canonical control checkpoint event |

The bind response supplies the `routing_token` used on later calls. A granted
turn carries an exact dense task delta under `grant.delivery.delta`. The final
page also carries the bounded context packet under `grant.delivery.context`;
earlier bounded pages set `context` to `null`, `has_more` to `true`, and grant
only an observe-only `recovery` turn. The host must inject the supplied payload
and cite `grant.delivery.page.delivery_token` in `turn_begin` before
dispatching the prompt. Checkpointing a partial page leaves the session
`sync_required`; finite recovery pages drain the backlog before an ordinary
turn can grant. A single canonical task event is size-limited at capture, so a
page always makes progress. Exact retry evidence remains canonical across
process restart, but a newly opened control connection invalidates unbegun
grants and returns the session to `sync_required`; old results never resurrect
authority. The new connection also fences a still-live predecessor, whose next
operation fails with `control_connection_superseded`. A begun grant is not
silently replayed or discarded: `session_status.open_grant_id` identifies the
required checkpoint and `open_grant_state` distinguishes `issued` from
`begun`. A fresh `turn_evaluate` key atomically supersedes an
issued-but-unbegun grant and records an immutable transition bound to the
replacement decision; an already-begun grant instead refuses with
`turn_already_open`. When the begun grant contains an observe-only partial
recovery page, `session_status.recoverable_grant` returns the exact canonical
grant and delivery bytes; the replacement host redelivers that payload and
then checkpoints the already-begun grant. The confirmed cursor does not move
until that checkpoint. Other begun turns expose no replayable prompt because
their outcome may be uncertain. Reusing a key for a different intent fails.

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
`source_basis { workspace_id, source_revision }` and `observed_at`.
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
whether `source_basis` is present. A passed typed test satisfies open
obligations only against the newest mutation source revision at the evaluated
run-feed cut. Thus a newest basisless mutation makes the open set waiver-only
until a later basis-bearing mutation plus passed test arrives; that later test
may satisfy both the earlier and newer definitions. For the stock rule that
waiver comes at `done`: completion records each still-open stock obligation as
an untested change instead of refusing. `work_focus` exposes the
canonical bounded `obligation_page`, the same field appears inside
`work_next.focus`, and `work_next` deltas use
`obligation_opened`, `obligation_satisfied`, `obligation_waived`, or, for a
waived stock obligation, `untested_source_change` naming the change and its
source revision, without leaking host authority. The page's `untested_total`
counts every untested change on the run, and its items name those that fit.
Its `open_total` counts every open obligation on the run before count and byte
trimming. When it exceeds the open items shown, an open obligation was left
out, and the agent reminder says more obligations are open than shown. A page
stored before this count existed lacks it and keeps its original reminder.

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
writes one uniquely named file into the project root and looks it up under
the opposite case. Agent work words, MCP startup, graph, backup, restore and
import do not run that probe; they still perform their ordinary store and
file I/O. The first resolved writable opener persists that policy;
read-only [readiness](host-readiness.md) never binds it and explicitly reports
an unbound or unresolved identity. Later
resolved openers must present the same one, and a mismatch names both. An
opener that could not resolve the identity (unwritable or missing root) still
reads and tracks work, but path-bearing control requests are refused with
`host_path_identity_unresolved` instead of guessing. Windows alias rules
(reserved names, alternate data stream syntax, trailing-dot/space aliases,
known 8.3 aliases) follow the running operating system. `doctor` reports the
persisted and resolved policy.

Action authorization/begin/completion, standalone delivery acknowledgement,
heartbeat, and independent exit remain planned protocol operations.

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
commands or three supplied fields, typed JSON, or saw a hash, fence, or key in
text output. It also checks that an unheld `note` records a marked observation
without execution credit, while an unnoted `done` supplies its resolving
command even when observations exist.

`scripts/mcp-dogfood.test.mjs` launches real stdio MCP processes against a
fresh home. Its main lifecycle uses only the agent-facing MCP tools: one session
creates, claims, blocks/unblocks, notes, and offers a root; a peer accepts the
checkpoint-coupled handoff, notes, and seals it with `done`. Keyless replay,
`reminders`/`next` derivation, catalog and search filters, cancellation,
compact completion, child creation under a parent, and field revision are
asserted along the way, and no `reminders` or `next` line ever carries a hash,
fence, or key. The CLI path drives the same lifecycle through the words in text
and `--json` modes and keeps one `engram work core focus` call. Both scripts are
part of `scripts/check.sh`.

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
