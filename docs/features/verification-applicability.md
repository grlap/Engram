# Verification applicability

Stored verification evidence may carry an optional restriction:

```json
{"applicability":{"kind":"git_tree","tree":"<full Git tree object name>"}}
```

On `execution_observe` sightings and observed-check source bases,
`source_basis.git_tree` is an optional host measurement. Both tree fields accept exactly
40 or 64 lowercase ASCII hexadecimal characters. The host supplies it only
when that same capture found `git status --porcelain=v1 --untracked-files=all`
empty. A dirty capture omits it. Absence means no tree identity and is valid
input. A commit id and a content-v1 fingerprint are not tree identity.

`execution_observe` records source sightings and observed checks; it does not
mint native verification. `verification_record` creates native evidence from
registered terminal checks. Its current request has no `applicability` member,
and the evidence it creates has no applicability restriction. Engram sets
`source_basis.git_tree` from the declared `tree` of a `staged_tree` input; a
`commit` input has no tree field in its source basis, even for a clean checkout.
Eligibility requires `post_run.clean_before` and `clean_after`, a clean input
outcome and no dirty paths. These describe the registered input, including a
staged tree; they do not require porcelain status to be empty.

Historical restricted evidence retains its stored restriction and canonical
bytes; see [records from earlier builds](execution-observability.md#records-from-earlier-builds).
A present null, unsupported kind, malformed tree or undeclared member in a
restriction is refused without echoing values. Strict host request decoding
returns `invalid_request` for unknown fields, including an applicability member
on the current registered-verification request.

`verification_bind` preserves an original restriction when binding evidence to
another run. A missing or different target tree returns
`verification_bind_refused` with the typed reason `git_tree_missing` or
`git_tree_mismatch`. Core evidence ingestion also rejects a restricted record
whose producer tree does not match its restriction; that invariant does not
provide another host ingestion operation.

The shared core predicate compares each selected record's restriction with
the relevant host-measured judged sighting. Missing or different identity
leaves it attributed but non-qualifying, with `git_tree_missing` or
`git_tree_mismatch`. Equal trees qualify across different commits, subject to
all existing result, root, run, timestamp and freshness rules. The predicate
runs after the existing newest-candidate selection. A newer failed or
restricted candidate cannot be skipped to find a convenient older pass.
Unused historical restrictions do not veto a sufficient newer local check.

The rule applies to obligation satisfaction, completion's newest-check rule,
evaluation admission and consumption, selected observed mechanical passes,
and binding to another run. Bound copies preserve the original restriction
and the target's measured tree. Quiet tree changes with equal content-v1 are
still changes of applicability. Attachment remains attribution-only: its
restriction assessment never confers qualification.

## Schema barrier and cutover

The core declares the rebuildable partial index
`objects_verification_git_tree_applicability_v1` on verification object ids.
Older builds since the strict schema guard was introduced treat this
unrecognized ordinary index as an unexpected durable schema object and
refuse ordinary open with `different_build_schema` before mutation. This
does not retire connections that are already open.

The new build refuses ordinary open without the barrier as
`projection_repair_required`. Only the exact current schema definition counts.
Successful `work_session_read` results advertise
`verification_evidence_git_tree_applicability_v1: true` only while that
definition exists. The advertisement is specific to the active connection;
an absent, false, malformed or unknown advertisement grants no restricted
credit. Binding a copy of historical restricted evidence checks the actual
barrier in its write transaction; a missing or wrong barrier returns
`control_projection_invalid`. The copy preserves the original restriction;
a host cannot retry by supplying evidence with that restriction removed.

Follow the existing [host cutover procedure](../host-checklist.md). Verify the
new build refuses an unconverted store without writing before its conditional
installation immediately before Greg's agreed TermAl restart. After the
restart, once every pre-install Engram process is gone, run with the new build:

```text
engram doctor --build-verification-applicability-index --json
engram readiness --json
```

Run the step on every live store. It creates only the missing index in an
immediate transaction, strictly decodes every verification body, checks all
current schema definitions, and rolls back on any failure. A correct existing
barrier is a verified no-op. Its receipt names a verification census and does
not claim a full audit. Full projection repair may recreate the barrier within
its transaction, but must never commit its removal once restricted records
exist. Do not remove the barrier to permit an older build to open the store.
This procedure grants no host restart authority.

## Pre-commit census

Ordinary new-build doctor cannot audit an unconverted production path because
its missing barrier refuses open. Before commit, use the compatible installed
build's supported `engram backup --out PATH` to take a consistent SQLite
snapshot of each discovered live store under `target/`. The command uses
Engram's verified backup path; use no outside SQLite copying tool.

Place each snapshot at
`<snapshot-home>/projects/<project-directory>/engram.db`, retaining the
project directory resolved for that live store's stable project id. Use a
different isolated home for each snapshot and the same confirmed project
file for both builds. The command order is:

```text
<compatible-build> --home <live-home> --project-file <project-file> backup --out <snapshot-database>
<new-build> --home <snapshot-home> --project-file <project-file> doctor --build-verification-applicability-index --json
<new-build> --home <snapshot-home> --project-file <project-file> doctor --json
<new-build> --home <snapshot-home> --project-file <project-file> readiness --json
```

The compatible build must capture the source: the new build's ordinary open
refuses its missing barrier. `backup` uses SQLite's consistent `VACUUM INTO`
through Engram, settles and verifies the staged copy, then publishes it
without replacing an existing destination. A snapshot does not stop writers
on its live source.

With the new build, run the narrow barrier step on each isolated snapshot,
then `doctor --json` and `readiness`. Select the snapshot through an isolated
Engram home for that same project. Record each live source path, snapshot cut
or time, barrier outcome, named doctor findings, readiness and measured runtime.
An incompatible stored member stops the landing for the coordinator's
conversion decision. Never strip it or count an open refusal as a census.
Delete the snapshots after the census; retain the bounded results as evidence.

At the actual cutover, the narrow step's transactional strict census checks
again and rolls back on incompatible members added since the snapshot. That
backstop does not substitute for the pre-commit snapshot census. The ordinary
doctor remains refused on production until cutover. See also
[SQLite admission](sqlite-store.md), [backup](off-host-backup.md), and
[acceptance evaluation](acceptance-evaluation.md).
