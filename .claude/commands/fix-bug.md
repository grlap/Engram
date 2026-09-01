---
name: fix-bug
description: Diagnose and fix an Engram bug tracked in Engram.
metadata:
  termal:
    title:
      strategy: prefixFirstArgument
      prefix: Fix bug
---

Fix the Engram work item whose ref is supplied in `$ARGUMENTS`.

1. Run `engram work show $ARGUMENTS`. If missing or completed, report and
   stop.
2. Claim it with `engram work claim $ARGUMENTS`.
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
9. `engram work note` the validation evidence, then `engram work done` with
   what was delivered; it tells you if anything is still owed. Never commit,
   push, or sync without explicit authority.

If `$ARGUMENTS` is omitted, run `engram work next` and ask which bug to take.
