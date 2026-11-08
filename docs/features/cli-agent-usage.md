# Agent Use of the CLI and MCP

[CLI and MCP index](cli-and-mcp.md) · [Architecture](../architecture.md)

## History and evidence receipt facts

Evidence kinds use the same words in structured and rendered output:
`generic`, `verification`, and `environment`. The `notes` family and `note`
command remain names for navigation, independent of a row's evidence kind.
Lifecycle words are `proposed`, `open`, `completed`, `cancelled`, and
`superseded`; derived availability remains a separate fact.

Terse show notes, note windows (including body placeholders), and note detail
retain an optional typed `gate: {name, passed}`. Ordinary prose does not create
that field. Gate rows remain in the notes window with the `gates` family,
unchanged totals and locators; the binding-evidence page has its own unchanged
shape.

Native event history renders from private canonical event facts. Selectors,
child references, counts, lifecycle and reasons precede an optional item title
within the 192-byte UTF-8 summary ceiling. Single-item history omits the repeated
title. Stored delivery summaries and staged payload bytes are unchanged;
replay hydrates the private facts again. Restored history keeps its deterministic
generation/member presentation order. Byte-pressure omission removes earlier
restored entries first, retaining the newest tail with exact omission counts.

Completed show and done distinguish recording identity from the verdict basis:
`recorded under asserted identity` describes the evaluator's recorded identity.
Criterion `observed` or `judgment` bases remain separate facts. Structured
acceptance provenance retains its existing field names and values.

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
locator, by}`. `by` is `you` for this actor and session as an agent, or a stable
project-scoped `peer-…` display label for another session, including a session
of the same actor. Actor-only records use a distinct `peer-actor-…` label.
When the status was recorded as an agent by the reader's own actor in another
session, and both that record and the reader carry an asserted actor id rather
than a shell default, the status adds
`by_relation: "same_actor_other_session"` and text appends
`(same asserted actor, another session)` inside the bracket after the `peer-…`
label. The relation compares asserted actor ids, not authenticated identity,
and grants nothing: the other session's claim, focus and authority stay its own.
Text follows the parent line
on `show` and is indented beneath `next` rows. Bodies start with a 768-byte
UTF-8 cap; larger bodies show a bounded first nonblank line.
Compact peek starts with a 192-byte status preview, preserving whitespace in
short bodies and carrying the exact note read for clipped bodies. Final text/JSON
fitting may shorten either preview further before shedding resume rows,
setting `complete: false` with explicit omission and a note-detail command.
Compact `next` renders each identified status capture and latest note head
once, with `context_ref` on repeated discovery rows and references in change
summaries; ordinary verbose `next` retains the exact staged page and full
change summaries. Verbose peek instead carries a bounded unstaged preview.
References require the same immutable capture, never text similarity; absent
capture identity keeps the body. References retain change kind and actor
attribution, and note session markers precede untrusted note text. Human text
escapes a note body's leading `[` as `\[` so the body cannot supply a session
marker; structured note bodies retain their original bytes.
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
authority continue to use the unchanged actor and session ids. For work and
control operations, context never refuses the session: Engram replaces each
unsafe-control run with one space,
trims the value, and cuts it at a UTF-8 boundary to 256 bytes; altered input
receives an explicit `actor_context:normalized` provenance marker, and an
empty result is absent. It is excluded from retry/idempotency identity, so a
replay retains the original operation's attribution instead of duplicating or
refusing it.

Task-sync setup preparation and materialization preserve the asserted actor
context verbatim under their separate validation contract: when present, it
must be nonblank, at most 4096 UTF-8 bytes, and contain no control characters.
An exactly empty context is absent; whitespace-only context is refused.
Invalid context refuses the setup operation before creating state. Their
immutable private records retain the first caller's attribution on retry;
see [setup preparation](task-planning-text.md#private-git-setup-preparation).
A local shell that omits either principal value remains usable: actor derives
from the first nonblank conventional OS-user environment variable and session
defaults to one stable id for that `engram` process. The actor derivation is
asserted context, not an authenticated OS identity; if no conventional user
variable exists, Engram uses a synthetic process actor instead of refusing.
Durable actor provenance distinguishes `defaulted:os_user_environment`,
`defaulted:process_actor`, and `defaulted:process_session`. Explicit actor and
session ids are recorded verbatim. A live caller, planning-actor,
or handoff-recipient actor session id is
admitted only when it is at most 64 UTF-8 bytes; longer values refuse before
store, session, focus, attempt, offer, planning-write, or task-bind effects,
with a bounded error that does not echo the rejected id. The same live
length-only admit applies to a graph-snapshot save or load operator actor, a
control-policy administrator actor session, and project-memory remember,
forget, full, or list caller
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

Focus is navigation, not execution ownership: a claim is ownership. Agent
`show`, and the first page of its notes and history windows, states both as
separate facts. The headline names the item's holder,
and `session_focus` (text `session focus: …`) names the reading session's
selected item, read in the same cut: `this item`, another ref with `not this
item`, or `none`. `next` names the focus row and the held rows separately; a
focused item the session does not hold is never shown as held. A completed,
cancelled or superseded item stays the selected focus until a focus-setting
word moves it. Receipts name its terminal lifecycle apart from ownership, no
successor is selected automatically, and its ended run supplies no live
control binding (`work core held` lists no row for it).

Ambient focus follows the last committed focus-setting operation of the
session. Implicit targets are therefore unsafe when one session is used
concurrently, as an MCP server may run several words at once: each call's
receipt names its own result, but a later bare word acts on whichever
focus-setting call committed last. Concurrent callers should name the item
explicitly. A word given an explicit ref acts on that ref and never consults
the focus, and reads (`show`, `ls`, `search`, `next`, `memories`) never move
it.

`add` and `add --under` do move focus, to the item they create, and their
receipt says so with `focus_change`, as described below. So a bare
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
  needs one, or `show` where nothing else applies. On open work an explicit
  `note` is offered, as an observation; on completed work an explicit
  `note` and a late `gate NAME --work-ref` are offered, as late findings.
  `not_open` also covers cancelled, superseded and proposed work, which
  admits no note or gate from this session, so it is offered `show` alone.

Repeating the word with the item named acts as before. A bare `handoff` keeps
the focus without this check: its recipient accepts an item it does not hold
yet, and an offer or a cancel already needs the claim.

`done` and `evaluate` record a verdict on one item, so a bare one also needs
that item to be certain. When the focus is an item this session holds and it
holds another live claim beside it, a bare `done` or `evaluate` is refused
with `work_bare_target_ambiguous`; a focus it does not hold keeps the refusal
above, which offers the focus:
- nothing is recorded, and a bare `evaluate` is refused before its attempt
  replay is consulted;
- `details.held_refs` lists up to three items the session holds, in ref
  order and always including the focus, which `details.focused_ref` names;
  `details.more` counts the held items not listed, and the message gives the
  total;
- `next` offers the explicit command for each listed item.

Naming the item acts as before, and an explicit retry replays as before.
With exactly one live claim, a bare `done` or `evaluate` acts on it as
before. A bare `note`, `gate` or `handoff` is unchanged.

An `add`, `claim` (with or without `--under`), `note`, `gate`, `evaluate`,
`done`, `update` or `handoff` that actually moves this session's focus says
so. Its receipt carries a
top-level `focus_change` and one text line after the headline, for example
`focus moved from w-… to w-…; future observations use w-…'s claim`. The object holds `from` and `to` (work refs, either `null`
when there was or is no focus). When the target has a live claim of this
session that the work-session operations would accept, as captured in this word's own
transactions, it adds `claim_id` and `claim_fence`. When that claim has a
bound named source root, it adds `workspace_id` and `generation`. A
workspace name longer than 192 bytes as JSON or as escaped shell text is
given instead as `workspace_id_omitted_bytes`, its UTF-8 length. Shell text
never shows the fence.
Without such a claim the line ends `w-… has no live claim to bind`. After
`done`, a release, or a cancel, supersede or reject, the item's claim has
ended, so no binding is named; after an accepted handoff the claim it
transferred to this session is named.

The change is what this invocation did, computed when the word finishes and
never stored. A call that leaves focus where it was says nothing, and so does
one that ends where it started. `claim --under` ends on its selected child,
and `add --under` on the child it creates. A peer's `note`, `gate` or
`evaluate` moves nothing, and an exact retry reports only a move that retry
itself made.

Mutation admission resolves the target without selecting it. A refused verb
leaves focus and staged delivery unchanged and carries no `focus_change`,
whether it refuses malformed input, stored state, authority, or a live
handoff offer. This includes a typed `done` refusal returned in a successful
protocol envelope. Existing protocol attempts, capture evidence or checkpoints
from before a refusal may remain; this is not database-wide rollback.
Successful operations apply their navigation policy after admission, capturing
claim and named-root fields in that focus transaction. Root creation and its
committed-core recovery select the created root before projecting its work
memory, whose access guard requires that persisted focus.
A fresh `done` with malformed input on completed work still refuses, while
an exact resend of an admitted completion still returns its seal.

Business writes, navigation and agent receipt construction are separate
steps. A receipt or projection error after a successful service committed its
focus is partial success, not an admission refusal: CLI JSON and MCP carry
the actual `focus_change` beside `error`, and CLI text puts its line after
the message. The journal never hides a committed move or restores an earlier
focus over another concurrent call's selection. Name explicit targets when
sharing a session; later successful navigation still selects its pinned item.

The claiming turn keeps the binding the host admitted it with. The new
target is only a candidate for the host's next admission, and the receipt
never says the host has rebound: whether and when the host binds the new
claim and root, and how it says so, is the host's to report.

The host's core operations follow the same rule when they name no item: a
decomposition under the focus, and every `work core update` form, claims,
checkpoints and evidence capture included. They are refused the same way
before any claim or attempt is written, from one read of the focus, its claim
and the held claims. A root creation or an atomic plan names no focus and is
not affected. A retry of an act already admitted is exempt, so a lost
response is answered even after the focus moved: a caller key under which
this session began an attempt of the same intent that finished, or whose
core write committed before the attempt could finish. The same key with
another payload, or an attempt that wrote nothing, is checked like a new act.

`work core complete` without `--work-ref` follows the bare `done` rule, both
parts of it. It is refused while the focus is not an item this session holds
and the session holds other work (`work_implicit_target_conflict`), and while
another live claim of this session stands beside its held focus
(`work_bare_target_ambiguous`, with `operation` `work_complete`). Only this
session's active, unexpired claims in this project count. The check runs
before the completion attempt is written, so the completion records nothing
(opening the store can still register a process-default session). A
completion keeps its own replay rule: a repeat of a completion this session
already admitted on the focus is answered from its receipt, whether that
attempt finished or was cut short after its run was sealed, but only while
that item is the focus again or is named. On another focus the same key is
refused: by the bare rule above when it applies there, otherwise, as when
the item is named, as an idempotency conflict. The same key with another
payload, or an attempt whose run is not sealed, is checked like a new
completion.

These are the reads that record nothing:

- `next --peek` (MCP `next` with `peek: true`);
- `ls` in every form, and MCP `search` (CLI `ls --search`);
- `show` in every form: plain, `--full`, `--notes` and `--notes --gates`,
  `--history`, `--note LOCATOR`, `--evaluations`, `--evaluation RECORD_ID`,
  `--observations`, `--criterion-links`, and their continuations;
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
engram work ls --all --verbose --json --limit 200 --page-bytes 1048576  # a program's larger JSON pages
engram work show REF [--notes [--gates] | --history] [--after CURSOR]
engram work show REF --note ID[:INDEX | :event-INDEX | :completion]  # complete immutable note or inherited member detail
engram work show REF --evaluations [--after CURSOR]  # the run's evaluation records, oldest to newest
engram work show REF --evaluation RECORD_ID  # one evaluation record complete
engram work show REF --observations [--after CURSOR]  # the run's source observations, oldest to newest
engram work show REF --criterion-links [--after CURSOR]  # every association in one frozen native seal, in recorded order
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
Printed commands escape unsafe characters but preserve every safe literal byte,
including repeated spaces inside quoted arguments. The same prefix-free
`terminal_error_line` / `terminal_error_command` helpers serve those receipt fields
and CLI error lines; they are not a store or JSON sanitizer.
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
Selection decides which rows the bounded page keeps: the newest execution
evidence by dense run-feed position, verifications first, each with the
environment evidence it links to however old that is; a verification whose
environment no longer fits is left out with it, never shown alone. On a full
page the latest note replaces the least-priority selected note. `notes` then
emits every kept row in the item's dense root-work feed order, whatever its
asserted timestamp,
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
`you` identifies this session, not every session using its actor. On a
record's author (notes, history, statuses, changes, evaluations and `next`'s
note rows) it also needs the record to be made as the agent this session is: a
record of this session and actor made as another kind of actor, such as a host
operator, is labelled by its actor alone. `next`'s changes leave out the
reader's own changes, those whose author would be labelled `you`: the agent
words' own records, and records the host-control channel makes for this
session as the agent. A change of this session made as another kind of actor,
or by another actor, is listed with its label, and a change not visible from
the reader's focus is listed as not visible, whoever made it. Holder and other session
labels name the session only. Another
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
exposes its full minted record id as `evaluation` in JSON; the text evaluation
summary uses a 12-character prefix. Current `show` uses
`acceptance_evaluation.evaluation`, `show --full` uses
`work.evaluation.evaluation`, and `evaluate` uses `evaluation.evaluation`.
Current and full `show`, normal/minimal `evaluate` receipts (including replay),
complete evaluation detail/windows, and compact `next` use `evaluation` only
for this record id; they emit no `hash` alias. The rollout first added the
preferred key, then landed and ran TermAl's evaluation-preferring reader before
Engram removed the alias. TermAl's separately tracked fallback removal follows
the final Engram producer landing. The id is not a content hash, and canonical
historical records, receipts and links are unchanged.
The evaluated work revision and any source fingerprint
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
raw identifiers.
Agent refusals that concern one work item name it by short reference in the
same way: `work_not_found`, a revision conflict, `work_not_open`, an
ancestor-blocked claim (`work_invalid`), a refused peer decomposition, an
already satisfied prerequisite, a refused detach, a
claim mismatch or lapse, a release that needs a reason, a refused completion
or bound verification, a completion that needs recovery, missing acceptance
criteria, refused, refused-at-admission, carried-failure and moved-basis
evaluations, and open work obligations. Where the core envelope's `details`
carry `work_id`, the agent ones carry `work_ref` in its place (a refused
evaluation and open work obligations have no details object, and keep none).
The message names the affected item by its `w-…` reference, including the
ancestor-blocked claim whose core message names only the ancestor. A recovery
with cause `required_child_unsealed` names its child by short reference in
the agent message, reminder and `details.cause.child`; the core envelope keeps
the raw child id. Reminders that repeat a work id from the message use its
reference too, on CLI text, CLI JSON and MCP alike. The code, remaining details
and scoped record ids (evaluations, seals, evidence, observations) are unchanged.
Caller-supplied text, such as a reason or a criterion, is never rewritten,
even when it quotes the item's raw id. An ambiguous
short reference keeps its candidates' full work ids, in the message, in
`details.candidates` and in each offered `show` command, since a colliding
short reference cannot tell them apart.
A refusal, remedy or reminder that tells the caller to pass an argument names
it as that caller passes it. The CLI, its text and its JSON keep the flags,
such as `--revise requires --key`, while MCP names the fields, such as
`revise requires key`; this applies to the message, `details.reason`,
`details.remedy` and the reminders of a refusal, and to the reminders, a
listing's `hint`, a completion refusal's `remedy` and the
`memory_retirement.instruction` of a receipt. A text that carries its own
values, such as a memory key, a retirement target or a replacement item, is
built in the caller's spelling. A fixed sentence is written in both spellings
and replaced whole only where it ends the text, so an arbitrary `--flag`, and
a caller's reason or citation before such a sentence, are never rewritten.
Runnable `engram work …` commands in `next` and elsewhere stay CLI syntax on
every surface, and the host/core envelope keeps the CLI spelling.
Project-memory attribution, caller-owned process-default session notices and
encoded continuation context also retain their documented contracts. Compact
rows retain up to 80 UTF-8 bytes of title. Their hybrid `state` is availability
for open work and lifecycle for proposed, completed, cancelled or superseded
work; redundant lifecycle and blocked fields are omitted. Empty labels are
omitted, nonempty labels are capped, and `labels_omitted` counts the rest.
Truncation reserves three UTF-8 bytes for `…` and cuts the remaining prefix at
a character boundary. When fitting an oversized
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
pair binds verbose `next`, `ls`, and note/history windows; only CLI
`ls --json --page-bytes` may choose a larger listing budget, described with `ls` below. `done` refits its
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
  effective cap, clamped to 1..5 and reduced to one for compact peek with a
  live-held focus, even if fewer rows fit. Text
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
  `--blocked`. `--ready` selects open work only, since only open work is
  ready, so `--all` adds nothing to it: `ls --all --ready` lists exactly what
  `ls --ready` lists, in the same order. `--blocked` selects work with an
  active blocker or an unmet prerequisite, excluding completed, cancelled
  and superseded work even with `--all`. Historical blockers remain stored
  and inspectable through `show` and unfiltered `ls --all`. Proposed work
  keeps its existing treatment. Compact rows omit the constant plain-ready
  sentence and keep additional reasons such as prior-claim recovery. Show claim reminders and
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
  Compact peek is a recovery packet: the focus includes its bounded objective,
  exact acceptance count (criteria omitted), qualified current status and bounded
  unresolved-dependency facts from this snapshot. The same holder capture is
  shown once, beside the focus; other live-held and unresolved assigned duties
  remain visible. With a live-held focus, at most one ready candidate is shown;
  otherwise the usual five-candidate cap remains. Before byte fitting, up to
  four changes are selected from the retained feed preview whose structured
  subject is the focus, another held item, a shown assigned duty, or a shown
  incoming handoff, keeping feed order. Later fitting may omit a duty or handoff
  row while retaining a change for its subject.
  Cited captures keep exact full-note commands; ordinary notes are not promoted
  to current status.
  Broad participation and unrelated or overflow change bodies are omitted with
  counts, non-advancement disclosure and the inspection
  command `engram work next --peek --verbose` (`details` in JSON), which is
  itself a bounded preview. `engram work ls --all --limit 20` (`catalog_detail`)
  traverses omitted work through its continuations; `show REF --notes` reads
  each item's records. These commands are new reads. Ready
  continuation still starts after the last shown candidate. Full contract,
  evidence and note commands are new reads; they do not pin historical contracts.
  The bounded preview does not guarantee that every undelivered relevant change
  was scanned. Read each duty's notes window and follow its continuations for
  complete recorded constraints. Lifecycle commands precede catalog navigation
  in the bounded `next` list; omitted guidance remains in the verbose read.
  A clipped status carries its exact full-note locator and must be read in full
  before acting on approval or STOP conditions. Missing qualified status is not
  substituted with a former holder's note. Advancing `next`, verbose peek and
  the core wire retain their existing shapes and delivery behavior.
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
- Claimless advancing `next` and verbose peek include nonempty `assigned` and `participated` sections
  between held and ready work, at most five rows each with exact omitted counts.
  Full rows name the work, title, holder word, and first line of this session's
  latest own note when present. For a note the reader's actor made as an
  agent, compact rows use `note_by: "you"` and text prints
  `[note session you]` before the body.
  Rich verbose JSON retains the original `note_session_id` instead.
  After this session's own rows, within the same five, `participated` lists
  Open work the reader's actor noted, observed or gated as an agent from
  another session, which this session neither holds nor took part in. Such a
  row carries `continuity: "same_actor_other_session"`, the first line of that
  actor's latest such note, and text prints
  `[note by same asserted actor, another session]` before the body; it never
  says `you` or names the other session. Both the record and the reader must
  carry an asserted actor id rather than a shell default, so a defaulted actor
  gets no continuity rows. Matching compares asserted actor ids, not
  authenticated identity. The row is navigation only: it inherits no claim,
  focus or authority, and a claim held by the other session stays its own.
  Handoff offers and `stranded_children` remain session-based.
  Compact repeated rows instead contain only `{ref, context_ref}`; the
  presence of `context_ref` is the discriminator. It names the retained
  `focus REF`, `held REF`, `assigned REF`, or `participated REF` primary row containing
  the full projection. Do not read title, holder, status or note from a
  reference row. Verbose rows retain the full shape.
  Another actor's session field is omitted. This is asserted attribution,
  not authenticated identity. This is recent-work discovery, not a review
  obligation or claim; keep owed decisions on a claimed coordination item.
  See the [resume discovery contract](local-work-system.md#agent-native-protocol).
  Compact peek keeps assignment but omits broad participation with an exact
  omitted-row count and `details` inspection route. Current incoming offers
  are read separately from live, unexpired recipient state in the same snapshot:
  `incoming_handoffs` contains at most five `items` with `ref`, bounded `title`,
  `expires_at` and a `detail` command to `show REF`, plus an exact `omitted` count
  and `catalog_detail` route. Inspect omitted catalog items with `show` for
  current offer state and the admitted accept command. Final byte fitting may
  shed offer rows while keeping their count and route. Cancelled, accepted and
  expired offers do not count as pending; navigation grants no claim.
- `next` and `next --peek` also show `stranded_children`, at most five Open
  direct children beneath Completed parents in which this session participated,
  even without a prior child read or claim. Rows contain `{ref, parent_ref,
  child_requirement, title, blocked_reason, remedy}`. The remedy is the
  currently admitted detach command, or the current detach obstacle's exact
  remedy. Restored history attributes participation to its original session.
  `stranded_children_omitted` counts all unshown rows, including whole rows
  removed to fit the strict 12 KiB JSON and terminal-text budgets. When rows
  are omitted, `stranded_children_next` provides the broader
  `engram work ls --blocked` listing, even if zero rows fit. Empty arrays and
  zero counts are absent. This snapshot advice never changes focus or claims;
  mutation rechecks its current admission. Core callers select it with the
  existing `participated` section. See the
  [resume discovery contract](local-work-system.md#agent-native-protocol).
  Unexpected failure of this advisory group emits no partial rows, omission
  count or navigation. It instead sets `stranded_children_unavailable: true`
  and a fixed `stranded_children_error_class`, rendered as
  `stranded children: unavailable (<class>)` in text. The marker survives byte
  fitting in both next modes. Other sections remain usable; their failures
  and mutation admission remain strict. Healthy empty results and budget cuts
  do not emit this marker. Discovery uses indexed Open children and stops at
  one canonical session anchor per Completed parent; unrelated closed history
  is excluded, while unmatched candidate-parent history can still be scanned.
  Parent and child identities give a stable order for paging and probe reuse,
  with no chronological meaning.
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
  the actual blocking ancestor ref and lifecycle, and exact detach command
  when admitted. `next`
  does so when the child is already focused; reading it does not select focus.
  Inspect the old child's successor after an uncertain response; a keyless
  repeat after supersession refuses without creating another root. See
  [detached follow-ups](local-work-system.md#gates-prerequisites-supersession-and-project-memories).
- `show CHILD` names its direct parent with `parent_ref`, `parent_title`, and
  `parent_lifecycle`; `status.work.child_requirement` is always `required` or
  `optional` for a child. Text includes `parent: REF "title" (lifecycle),
  required|optional` and `next` offers `engram work show PARENT` once, including
  when that parent is also the blocking ancestor. Admitted recovery remains
  first and history navigation follows parent inspection. This relationship
  survives acceptance/note trimming. Roots print `parent: root` and omit
  parent fields and child requirement. The safe focus read loads one bounded
  parent row in its existing snapshot; its private carrier does not change the
  ambient/core wire. CLI JSON and MCP agree.
- Open work blocked by a non-open ancestor exposes `blocking_ancestor` with
  deliberately different shapes across receipts:

  | Receipt | Ancestor fields |
  | --- | --- |
  | `show`, compact and verbose native `ls`, compact native `next` focus/held/ready rows, refusal `details.blocking_ancestor` | `{ ref, lifecycle }` |
  | Core `focus`/`inspect`, core `next` focus/ready/catalog, verbose native `next` focus/ready rows | `{ work_id, short_ref, lifecycle }` |

  `work_id` is the typed ancestor identity; `short_ref` and `ref` name that same
  ancestor. Native verbose `ls` keeps the compact ancestor object even though
  its work row includes raw identity. This is a deliberate receipt distinction,
  not a global confidentiality boundary. Verbose native `next` held rows have
  no `blocking_ancestor`; native `next` does not request a catalog section.
  Ready rows normally omit the field because ancestor-blocked work is excluded.
  The nearest non-open ancestor is
  selected, including a proposed ancestor. The catalog's `blocking_parent`
  is that ancestor's lifecycle, not necessarily the direct parent's lifecycle.
  Direct parent fields remain separate. Roots, closed items and items with
  all ancestors open omit `blocking_ancestor`; independent blockers and
  prerequisites do not manufacture an ancestor.
  Claim keeps the `work_invalid` code, names the affected item in
  `details.work_ref`, and adds `details.blocking_ancestor`,
  with `show CHILD` and `show ANCESTOR` navigation. Execution stays refused;
  only the existing detach admission supplies a detach command in show.
  Explicit blockers and prerequisites remain separate from this cause.
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
  Each inherited generation lists its notes, then its events, then its
  completion, each in the order the record stores them; ordinary show's
  `restored_history` uses the same order. This is a deterministic
  presentation, not a chronology: a snapshot carries no position shared
  across those families, and carried timestamps are shown as data and never
  reorder rows.
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
  not a display ordinal. An inherited event row uses `RECORD_ID:event-INDEX`
  and an inherited completion `RECORD_ID:completion`. A history window row
  whose summary was shortened keeps the original `body_bytes`, sets
  `summary_truncated` and offers that locator's `detail` command. That detail
  returns the whole summary and the complete `member`, every field as the
  record stores it (for an event: kind, work revision, reason, lifecycle,
  related item and revision, and time), except that its `actor` is the row's
  `by` display label, with its `member_bytes` as compact JSON; text frames
  the member as data, and `next` returns to the `--history` window. `show REF --note LOCATOR` (MCP `note: LOCATOR`)
  returns the complete body and references, with `body_bytes` in UTF-8, and
  deliberately may exceed 12 KiB. Ambiguous or wrong-item references refuse
  with `work_note_reference_invalid` and candidate locators when available.
  A body too large for a window is represented by `body_omitted: true`, its
  byte size and a `detail` command instead of `summary`/`refs`; its member
  still advances continuation, so it cannot hide older notes. Bodies and
  reference lines are framed as data in text; JSON preserves exact content.
  These existing canonical locators are read-navigation exceptions, not
  execution tokens. No per-note identity is invented for inherited members.
- Native verification rows in the notes window and full-note detail carry
  `verification` with `result`, `check_kind`, `source_revision`, and
  `producer_outcome`. Its additive `source_workspace` is the recorded
  `source_basis.workspace_id`, copied verbatim beside `source_revision` in
  CLI JSON and MCP, even when another workspace holds the same revision or
  the current root has moved. Native verification evidence requires a source
  basis, so both source fields are present together. Generic and restored
  notes without typed verification facts carry neither field. The field
  does not change rendered text or note ordering.
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
  Text shows it as one line under the row. The detail's `assessment` is the
  summary (`view: "summary"`), with the record's `record_position`, its
  `cut_position`, the `check_kind` and the `total` of candidates:
  - `counts` gives exact counts by `status` and `reason` (the `mismatch` code
    or `left_out` reason) over every candidate;
  - `must_show` holds, in full and in trigger order, every row a reader must
    act on: one that matches or mismatches, or whose obligation is still
    recorded open; every other row left out before matching appears only in
    `counts`. Whole rows fill a 6 KiB page, with `must_show_total`,
    `must_show_earlier` and `must_show_remaining`. When rows remain, its
    `continuation` command shows the next page of them, and text says "more
    must-show rows";
  - `history` is the command to the full history at the same cut
    (`view: "history"`), which pages every candidate in trigger order, at most
    eight `rows` a page, with exact `total`, `shown`, `earlier` and `omitted`
    counts and a `continuation` to the next page.
  Each command is `show REF --note LOCATOR --after CURSOR` (MCP `note` with
  `after`). A continued page carries the assessment alone, and every
  continuation refuses once the run has moved on. See the
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
  minted passing verification evidence of that kind (recorded as registered
  verification; a host that does not run Execution Observability can only
  revise or waive a bound requirement); a newer failed check of
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
  `limit` and `byte_budget` (12288 bytes, less the bytes reserved for a read
  reminder such as a lapsed own claim when one is carried). A program reading
  the CLI JSON may pass `--page-bytes BYTES`, from 12288 through 1048576, to fit
  each page to that budget instead, so a `--limit 200` page can hold 200 rows;
  `byte_budget` then reports it, less the same read-reminder reserve. Rows keep
  their usual projection, so a larger budget fits more rows but does not
  guarantee every requested one. It is accepted only with `--json` and refused
  out of range; MCP `ls` has no such option. Rows, order, `after` tokens and
  continuation are unchanged, and a token from either budget continues with
  either. The printed `next` command does not repeat `--page-bytes`, so pass it
  again on each page. A continuation is refused only
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
  Search matches refs, titles, outcomes and labels, plus complete canonical
  note summaries, note refs, and structured gate names and failures. This
  includes peer observations, inherited notes and restored late evidence;
  history-only events, private scratch, identities and raw environment payloads
  are excluded. Matching uses NFC and full Unicode case folding with literal
  substring semantics, including short queries, punctuation and paths.
  An item matching a note carries one deterministic `note_match` with its
  full `locator` and `family`; use `show REF --note LOCATOR` to read it.
  Counts, membership and pages share one snapshot and one transient note
  match set. A new matching item invalidates the continuation; another match
  within an existing member does not. Search uses a rebuildable trigram FTS5
  note index, normalized incrementally in the same transaction as each write.
  Candidates are verified literally against individual indexed public fields;
  queries shorter than three characters or containing NUL scan those projection
  fields only.
  Search trusts this projection and does not validate canonical note records
  or their producers. Explicit show/detail reads and doctor retain canonical
  validation. Projection repair rebuilds and verifies the index from canonical
  records; ordinary writes never trigger a full rebuild.
  Every storage catalog search, including a stateful `next` with search,
  shares a finite allowance for that scan: 10 million SQL progress steps
  (one-step callbacks, with clock polling every 1,024 steps), 100,000 visited
  index/result records, 384 MiB of cumulative charged work bytes, and a
  ten-second cooperative deadline. Record visits count selected catalog and
  note-index entries, retained matches, relation and ancestor rows, handoff
  projections/events, evidence-binding projections, root headers/members and
  waiver entries. Repeated visits consume the same cumulative allowance,
  including availability guidance and successor enrichment. Charged bytes
  include canonical blobs before copying, text before normalization, selected
  text/blob fields and a conservative 512-byte charge per counted entry.
  Canonical validation reads charge their input bytes even when they do not
  count as another index/result entry. Single-identifier, position, existence
  and aggregate navigation lookups are exempt from the cumulative record/byte
  counters; their SQL, including joins, FTS candidates and recursive SQL
  traversal, remains subject to the VM-step, SQLite length and elapsed limits.
  Derived values and clones of already charged input are not separately
  counted allocations. This is work accounting, not peak heap usage.
  SQLite strings, blobs and rows are capped at 1 MiB inside the catalog scope;
  this bounds SQLite input units, not every Rust allocation. Exhaustion returns `search_budget_exhausted` with
  the dimension, limit and observed lower bound, without partial matches,
  counts or a continuation. Narrow the parent or other filters to reduce work.
  A direct storage query starts its allowance after connection admission.
  MCP `search` and search-bearing `ls` start it before blocking-worker
  submission and share it through read opening and the complete word.
  Store admission precedes the catalog scope and retains its existing limits.
  The ordinary claim-lapse prelude also precedes that scope: like other read
  verbs, it checks at most eight lapsed items. It is exempt from the catalog's
  cumulative, VM-step and SQLite length caps; eight items is not a byte limit.
  The outer MCP request checks elapsed time and cancellation before and after
  the word, including admission and that prelude, but does not interrupt them
  mid-read. Keeping this ordinary prelude outside the catalog allowance avoids
  changing claim-lapse guidance when a search budget is exhausted.
  Each MCP server admits one search worker, with no waiting queue; another
  search receives `search_busy` while it is occupied. Ordinary verbs retain
  their independent execution path. Search uses the existing read-only
  snapshot connection, without holding the service's writable-store mutex
  during the scan. Request cancellation reports `search_cancelled` when a
  response can still be delivered. Caller destruction signals cancellation;
  the server retains the worker handle until joined. Process shutdown closes
  search admission, cancels and joins the worker before releasing the store
  owner. A failed worker reports `search_worker_failed` without partial data.
  Native/filesystem stalls and
  the existing SQLite busy wait must return before a cooperative check can
  run, so ten seconds is not a hard response-time guarantee. The synchronous
  query stops before returning a refusal. Non-search reads retain their
  existing limits, and existing stored oversized notes are not rewritten.
  Core `next` with search remains stateful: a refusal does not undo session
  registration, acknowledgement or delivery transactions committed before
  the catalog scan. MCP `next` exposes no search argument.
- Shell notes take the target positionally: `engram work note REF "text"`.
  MCP uses `note { work_ref: REF, text: TEXT }`; the shell has no `note
  --work-ref` flag (that flag belongs to `gate`).
- An MCP word refuses an argument its input schema does not list, before it
  reads or changes anything, and the refusal names that argument and every
  accepted one: `accept` on `add` is refused and points to `acceptance`,
  rather than being ignored while a defaulted criterion is recorded. Each
  word's input schema says so with `additionalProperties: false`.
- MCP `update` and `handoff` also refuse declared fields that the selected
  action does not consume, before any work mutation. The structured
  `invalid_argument` refusal names the field and action; explicit `null`,
  `false` and empty lists still count as supplied fields. For example,
  `update` with action `cancel` refuses `title`, and `handoff` with action
  `accept` refuses `to`, `summary`, `reason` and `ttl_seconds`. Schema and
  type errors keep their existing text-only refusal.
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
  The parent and every ancestor must be open. A non-open direct parent
  receives the generic `work_reject_refused` before cancellation.
  Above an open parent, the nearest non-open ancestor, including a
  Proposed ancestor, refuses with the same code; its remedy names that
  ancestor's ref and lifecycle. Its ordered `next` commands inspect the
  ancestor, inspect the child for admitted detach or resolve-first guidance,
  and offer an independent root follow-up with `add` and an acceptance
  criterion. A Proposed ancestor alone does not admit detach; inspect its
  resolve-first guidance. Another terminal ancestor in the chain may admit
  detach, subject to the existing checks.
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
  can retain fewer. `show` has the same summary cap; use the separate
  `show REF --criterion-links` read to traverse the complete frozen mapping.
  Original note detail is not a mapping continuation.
  Exact same-session linked intent replays its committed receipt. An identical
  request from a different session is not that author's retry; it refuses,
  as do new or late links attempting to amend the frozen seal.
  Open items with no criteria advertise no link basis. Hidden criteria continue
  the visible one-based numbering; omission does not renumber them.
  Core explicit acceptance cannot be combined with these positional links.
  Neither a link nor a passing gate verifies that
  the criterion is satisfied: satisfaction remains the author's assertion.
- `show REF --criterion-links` (MCP `criterion_links: true`) reads recorded
  associations from the canonical native completion seal, never the summary's
  prefix. It is exclusive of other show modes; only `after` may accompany it.
  Each `criterion_links` row has one-based sealed `criterion` and
  `evidence_member` positions, its `locator`, a complete-note `detail` command,
  and an optional bounded `preview` or diagnostic `preview_error_class`.
  Order and repeats are preserved as stored. Empty criteria contribute no
  rows. Links are evidence associations, not verification or eligibility.
  `criterion_links_window` gives the explicit frozen `basis` (`project_id`,
  `work_id`, `run_id`, `seal_id`), `order: "recorded"`, exact `total`, `earlier`,
  `shown`, `remaining`, `after`, and `byte_budget`. Total counts associations,
  including repeats, and equals earlier + shown + remaining.
  Each complete terminal representation and compact application-receipt JSON,
  including guidance, cursor and any lapsed-claim reminder, stays strictly
  below 12288 bytes. Previews are shed before tail rows. Continuation advances
  from the last emitted association, never an inspected but omitted row.
  A true empty mapping has zero counts and no cursor; an unavailable or corrupt
  native seal, run, or completion binding refuses rather than appearing empty.
  Unreadable cited evidence retains its association and locator, with
  `preview_error_class` instead of a preview. Restored-record-only
  completion has no native mapping. An evaluated seal can have more than the
  explicit input's 64-link limit; traversal has no such total limit.
  The readable unsigned `cl1-` cursor binds project, item, historical run,
  seal and the last member's positions and locator. It provides navigation,
  not authority. A valid edited seek is allowed; foreign, malformed or
  mismatched cursors refuse with `work_show_cursor_invalid`. No mutable
  revision, policy, expiry or current head enters the cursor. A continuation
  keeps its historical seal after reopen and later completion; fresh reads
  of open work report no current frozen mapping. Historical numbering never
  attaches the current criteria text or status. Reads create no store or
  default session and change no focus, claim, delivery, record or seal.
- Unlinked criteria are named before the seal, while linking is still
  possible. Under a self-asserted policy, the holder's `show` of an open item
  reports `unlinked_criteria: {count, positions}` in JSON: the criteria that
  carry no evidence link yet. That is every unbound criterion, and every
  bound one whose newest binding obligation is not satisfied, because a
  satisfied binding is the one link completion adds on its own, decided the
  same way the seal decides it. Positions are capped at eight; the count
  stays exact, and every criterion linked reads `{count: 0, positions: []}`.
  While the count is above zero, the holder's `show` and `gate` receipt add
  one reminder after the status guidance and before the reminders of open
  obligations, the same on CLI and MCP: `criteria 1, 3 have no evidence link
  yet; link evidence in done` (for one criterion, `criterion N has …`), with
  `and K more` past eight positions. While any of the unlinked criteria, listed
  or past the eighth, has a binding whose newest obligation is still open, the
  line ends `, or pass the bound check first`, since `done` refuses an open
  obligation and passing its check links the criterion; an unbound criterion,
  or one whose binding was waived, has no check to pass and needs a link in
  `done`. The line states what is linked now, not a
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
- Completed `show` and `next` also expose the derived
  [sealed source diagnostic](acceptance-evaluation.md#later-source-observations-after-completion).
  A later matching host sighting can differ from the evaluated source even
  when its `source_changed` flag is false. Later endpoint agreement never
  hides an earlier discrepancy. Terse views carry status, counts, and an exact
  observation detail command, with no raw record ids or source fingerprints.
  The explicit observation window retains the compared source, record ids,
  and evaluation detail command. Its historical observation
  cursor stays with the original seal/run across appends and reopen.
  Unadmitted/audit-only labels remain, and recording after the seal's cut
  does not establish a write after completion or the source at `done`.
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
- Every answer a word gives ends with `reminders` (what is owed, in words)
  and `next` (commands you can run now). In ordinary `engram mcp`, arguments
  a word's input schema rejects, such as an undeclared field, a value of the
  wrong type or an action word that does not exist, never reach the word:
  Engram's argument extractor refuses them first, and the MCP library
  returns the refusal as a tool error (`isError: true`) whose only content
  is text describing the problem, with no structured JSON, `reminders` or
  `next`. After the `failed to deserialize parameters:` prefix the library
  routes by, the text names the field it concerns, a nested one by
  its path (``field `peek` ``, ``field `action` ``,
  ``field `verdicts[0].criterion` ``),
  then what was wrong: the expected type, the unknown value or, for an
  undeclared field, the accepted ones. A missing required field is named in
  that message itself. Correct the arguments against the word's input schema
  and call again; the connection stays usable. The [read-only mode](cli-mcp-and-work-protocols.md#read-only-mode)
  refuses such arguments to its read words itself, before the library, as a
  structured `mcp_read_only_refused` error. Ordinary mutation words never
  ask for fences or idempotency keys. Words accept record ids as inputs only
  for scoped evidence citations and note-detail navigation. Structured receipts also return record
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
  `carried_failure_self_acknowledged` or `nothing_to_supersede`. A newest
  `fail`, `insufficient_evidence` or `needs_human` stands until a note, a
  gate, a host check or a source change is recorded after its evidence basis:
  an evaluation whose basis includes none is refused with
  `acceptance_evaluation_refused` (CLI exit 1, MCP error), and
  `details.cause` reads `{"kind": "reroll", "mismatch":
  "blocking_evaluation_stands", "evaluation": RECORD_ID, "feed": {"kind":
  "run_execution", "id": RUN_ID}, "after_position": A, "through_position":
  T, "criterion": N, "verdict": VERDICT, "remedy":
  "record_new_evidence_then_evaluate"}`. Qualifying evidence must lie after
  position `A` (exclusive, the blocking evaluation's evidence basis) and at
  or before `T` (inclusive, the submitted basis); `N` is the one-based
  position of the blocking criterion (the first `fail`, else the first
  `insufficient_evidence`, else the first `needs_human`), and `VERDICT` its
  verdict word. Before any evaluator is started, `show`
  (`acceptance_evaluation.reroll`), `show --full` (`work.evaluation.reroll`)
  and `next` (`focus.evaluation.reroll`) carry the same object, assessed
  through the active run's feed head (`T` is that head), and omit it when
  nothing stands. The record transaction assesses again at its own basis, so
  an absent field admits nothing and a present one is cleared by recording
  new evidence. The receipt and
  ordinary `show` carry a bounded prefix of verdict rows with an exact
  `verdicts_omitted` count and the decision facts (passed count, first
  blocking verdict, evaluator label, freshness, recorded source
  fingerprint); `next` prints one `evaluation:`
  line under the focused evaluated item; `show REF --full` returns the
  complete newest evaluation with every rationale and citation. It is
  refused until an operator enables the
  [acceptance evaluation](acceptance-evaluation.md) policy, and under that
  policy `done [--source-fingerprint F | --source-id-file PATH]` consumes the newest fresh, all-pass
  evaluation instead of the author's self-assertion, presenting the
  host-measured identity when the policy requires source freshness. An
  independent evaluator that later takes the run cannot consume its own
  pass. Under that policy `done` on an item with no acceptance criteria
  answers the error code `acceptance_criteria_required`: add a criterion
  with `update REF --accept "criterion"`, have the host evaluate it (the
  host refuses to evaluate an item without criteria), then run `done` again.
  `done` and the completed item's `show` say where the sealed
  acceptance came from (`evaluated (<mode>; recorded under <assurance> identity) by <evaluator>` or
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
`forget`) with the same flat arguments, plus `search` — fifteen work tools.
The separate `task_sync_settings` configuration tool reads or changes the
[host-local project opt-in](task-planning-text.md#project-opt-in-setting).
The
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
