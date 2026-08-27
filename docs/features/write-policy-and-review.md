# Write Policy & Review

> Normative reference: [spec §5–6](../spec.md#5-write-path). Related briefs:
> [typed memory model](typed-memory-model.md),
> [security & trust](security-and-trust.md).

Anyone — human or agent — can assert a memory. Whether it activates
immediately or waits as `proposed` depends on **origin and authority**, never
on who asked nicely.

## The promotion matrix

| Origin | soft | firm | hard |
| --- | --- | --- | --- |
| Human, explicit | active | active | active |
| Agent, with machine-verifiable evidence | active | proposed | proposed |
| Agent, unsupported claim | proposed † | proposed | proposed |
| Session-end distillation | proposed | proposed | proposed |

† `episode` records activate directly regardless of evidence — attributed
observations, on-demand delivery only, decaying by default.

Every promotion is its own attributed audit event. Writes pass the `Redactor`
port before persistence ([security & trust](security-and-trust.md)).

## Capture without ceremony

`engram note <prose>` and `memory_note` are the common write path. They infer
kind, authority, delivery, and scope from the active task and asserted host
context, then return those choices in the write receipt. A caller supplies
flags only to override or resolve genuine ambiguity. The explicit `assert`
surface remains available, and inference never bypasses the promotion matrix.

Task scope is the default for execution findings; agent scope is reserved for
genuinely private scratch. The captured object drives peer deltas, handoff
material, and report assembly so the same fact is not recorded in several
systems.

Task-scoped assertions from a joined session activate immediately because
they are attributed execution state, not promoted project truth. They remain
local and publication-gated. Agent-private scratch activates only for its
owner. Moving either into project memory or a published report crosses a
separate review boundary; project-scoped agent assertions continue to follow
the promotion matrix above. This distinction keeps shared task memory usable
without weakening long-lived knowledge policy.

## Distillation proposes, never writes

Session-end distillation drafts candidate memories from what happened and
files them as `proposed` — automation's capture rate with review's quality
gate. It deduplicates first: exact duplicates no-op; semantic near-duplicates
are proposed *linked* to the existing memory, never auto-merged. Distillation
touches only local working memory; optional publication to an external target
happens solely through explicit report finalization and a separately
authorized publication intent
([local tasks & reports](local-tasks-and-reports.md)).

## Review lifecycle

Two separate clocks:

- `review_by` — epistemic. Past it, the memory is `stale`: still delivered,
  flagged, queued in `engram review` for re-affirmation, demotion, or
  retirement.
- `valid_until` — factual. Past it, the memory is `expired`: excluded unless
  explicitly requested.

"Review overdue" does not mean "false"; the states behave differently on
purpose. Individual hard constraints may be configured to fail closed on
overdue review.

Every context packet shows proposed and stale counts, preventing useful agent
findings from disappearing into an invisible review queue.

## Conflicts

`engram conflicts` lists everything contested: concurrent heads and
unresolved `contradicts` edges. Resolution is a new version citing all
conflicting parents with recorded rationale — timestamps never decide.

## Forgetting

`engram forget` writes a tombstone: excluded everywhere, retained so sync can
never resurrect it. Physical purge is an exceptional runbook, not a CLI verb
— see [spec §6.5](../spec.md#65-forgetting-vs-purging).
