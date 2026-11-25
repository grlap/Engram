---
name: team-coordination
description: Coordinate multi-session engineering work through handoffs, shared runners, and integration. Use when dispatch, capacity, or stalled work needs coordination across owners.
---

# Team coordination

Use the repository's own instructions for authority, review and landing
requirements, and cross-host execution and data exchange. Read the
instructions for each repository involved. Use the supported runner,
communication, and task-state routes those instructions identify.
Acts reserved to the user wait for the user's word; explicit user pauses, build and capacity limits, install and restart rules, and landing gates stay in force.

Treat a coordinator as a scheduler and recovery owner. It must not become a
serial approval point. Scheduling the serial gate and the landing windows
stays its job; that is scheduling, not approval. Approval stays with the role
named by the repository.

## Keep work moving

Record each lane's owner and next action in the existing work record. For a
wait, name the exact dependency, what releases it, who or what wakes the
owner, and the recovery route. Use the host's completion notification where
available. On wake, verify the result in its original record and resume the
next action. Recover an existing run or handoff when delivery is interrupted.

At a scheduling decision, check current session and work status. Track active
work, blocked work and its release condition, and the next ready candidate.
Recover a dispatchable lane from its recorded next action. If the host and work
record disagree, report the discrepancy with the owner, last recorded action,
and observed state. Use existing automation for recurring coordination and
address concrete delivery gaps through the supported route. Keep routine scans
focused on work that changed or is pending.

## Schedule shared capacity

Dispatch independent work in separate workspaces when the project permits it.
Coordinate overlapping paths with the owners. Apply the project's build and
review limits to the jobs that use those resources. While a lane waits, keep
useful preparation separate from the input undergoing validation. Reviewers
inspect the frozen input in read-only mode.

Use the project's supported runner and handoff channel for cross-host work.
Give the runner the exact input and check scope. Follow each repository's
rules for transferring work records. Recover the terminal result, artifact
locations, and diagnostics relevant to the next action. See
[Coder test execution](../coder-test-execution/SKILL.md) for test handoffs.

## Move ready candidates through integration

Use the existing integration queue and the candidate's
[READY manifest](../evidence-based-delivery/SKILL.md#keep-one-ready-manifest).
Order eligible candidates by dependencies, overlap, and available capacity.
The integration owner checks for conflicts, records the exact input
being validated, and follows local rules for reusing reviews and checks.
Group independent changes when the project's validation can account for each.

The integration owner never edits product code, tests, fixtures, documentation
or configuration as a fix, even for a one-line correction. It returns merge
conflicts and defects to the implementation owner with the exact input,
affected paths and relevant diagnostic, and resumes only from the
worker-delivered result.

Assign implementation, including tests, documentation, and configuration, to
a coding model suited to the work. Follow the project's model and reasoning
guidance where provided. When transferring a repair because of a role or
model concern, verify the receiving session's effective settings. Involve an
architect for changes to durable meaning or unresolved design decisions
across component boundaries.

Keep responsibility through landing and any required live check or cleanup.
Name the owner and next action in the same work record when responsibility
changes.
