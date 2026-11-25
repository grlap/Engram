---
name: evidence-based-delivery
description: Package and close engineering work from reusable evidence. Use for READY handoffs, acceptance evaluation, landing evidence, test-rerun decisions, or workspace recovery that appears to invalidate an existing result.
---

# Evidence-based delivery

Use the project's evidence format, validation rules, and authority. Tie each
delivery decision to the original evidence for the source and behavior at issue.

## Keep one READY manifest

Maintain one manifest in the existing work record or handoff package. Use it
through integration and closure. Include:

- Work item and acceptance revision.
- Exact input identity as defined by the project.
- Each criterion, its status, and links to the original evidence.
- Each check's host, command or mode, scope, result, and eligibility for credit.
- Required review results and finding dispositions.
- Outstanding actions, their owners, and any evidence gaps.

Send READY through the project's handoff route with a pointer to the manifest.
The integration owner reads the original evidence and records the package's
disposition in the manifest. Recover the existing package from the work
record or queue when a handoff is interrupted.

## Reuse evidence after workspace recovery

A result belongs to its measured input and scope. After a workspace rename,
recovery, or claim transfer, verify the recorded input still matches and
recover the original result through the supported evidence route. Apply any
host or freshness requirement the criterion actually carries. If the host
refuses a retained result, record the exact refusal and the gap it creates.

## Decide whether to rerun

Recover missing citations and results from the original runs before deciding
what is uncovered. Check whether the evidence covers the required input,
behavior, platform, and scope and meets any explicit host, registration, or
freshness requirement. Check for a relevant newer failure or source change.
Reuse evidence that still qualifies. For a remaining gap, name the missing
proof and run the narrowest project-supported check that supplies it.

## Write atomic acceptance criteria

Give each criterion one independently verifiable outcome. State its observable
result and any required platform, timing, or authority. Split implementation
behavior from validation, publication, live operation, or cleanup when those
need different evidence or owners. For a one-time installation, describe the
event to verify and the artifact installed.

## Close against the evidence

Give the evaluator or closer the manifest and its original evidence. Use the
project's evaluation route when the required evidence is available. On a
refusal, identify the unmet criterion and what would satisfy it. If work is
pending, record its owner and release condition. After landing or deployment,
supply evidence for the criteria whose state changed and follow the project's
completion procedure. Keep any artifacts still needed for that decision.
