---
name: fix-bug
description: Diagnose and fix an Engram bug tracked in Beads.
metadata:
  termal:
    title:
      strategy: prefixFirstArgument
      prefix: Fix bug
---

Fix the Beads issue id supplied in `$ARGUMENTS`.

1. Run `bd show $ARGUMENTS`. If missing or closed, report and stop.
2. Claim it with `bd update $ARGUMENTS --claim` unless already assigned to the
   current implementer.
3. Read the referenced code, tests, docs, and relevant Engram skill sections.
   Confirm the defect and priority from evidence before editing.
4. If the report is a false positive, already fixed, or materially mis-scoped,
   explain why and ask for direction; do not silently close it.
5. Implement the smallest coherent fix. Preserve immutable versioning, frozen
   report retries, asserted-identity limits, local/external tracker separation,
   stable project identity, task-shared versus agent-private visibility,
   lease/cursor semantics, finalization barriers, and storage integrity.
6. Add or update behavioral tests that would fail without the fix.
7. Run the required gates from `AGENTS.md`, then invoke `/review-changes`.
8. Resolve Critical/High review findings and repeat gates/review. Present lower
   severities for user judgment.
9. Leave the bead `in_progress` with a completion and validation comment. Greg
   owns final closure. Never commit, push, or sync without explicit authority.

If `$ARGUMENTS` is omitted, run `bd ready` and ask which bug to take.
