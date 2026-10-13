# Team strategy

The TermAl and Engram coordinators agreed this strategy on 2026-10-13. It records
the existing operating decision; repository landing and authority rules still
govern. Parent-owned validation and landing follow the existing
[development and review workflow](development.md).
This page grants and changes no commit, push, tracker, approval, restart or
deploy authority, and changes no gate or review rule. Where another page
restates this strategy, the wording here governs Engram roots.

1. **Roots:** one independent ready package per coding root, with its own claim,
   worktree, evidence and landing under its project's rule. Engram implementers
   land under the [Authority and Git](../AGENTS.md#authority-and-git) rules;
   TermAl producers hand off to the integration owner. Codex `gpt-6.1-sol` is
   the coordinators' default model for new coding roots, with exceptions
   recorded. A root is created through a route documented on TermAl's
   [root-session page](https://github.com/grlap/TermAl/blob/536c17a16aaa1df6e5f6d6089bde4884b5d46280/docs/features/root-coding-sessions.md#choosing-a-route),
   which owns that contract. The coordinator who creates a root records
   creator, project and cwd, explicit model, purpose and creation receipt on
   the package's item.
2. **Delegations:** bounded coding subwork whose parent integrates, verifies and
   lands it with the package, and keeps the package claim. Delegations do not
   land independently.
3. **Count:** active coding roots per project do not exceed the ready, disjoint
   packages or host capacity. Host-capacity scheduling, with gates and review
   rules unchanged: one full gate of either project runs on the machine at a
   time, and while one project's full gate runs, the other project runs at
   most one filtered test command. Read Windows commit charge as a share of
   the commit limit before creating a root or starting WSL. Above about 75%,
   create no new root and start no WSL.
4. **Idle:** a root that holds no package and finds no ready package for more
   than about two hours still counts toward item 3's limit, and is reused first
   when a package becomes ready, before any new root is created. A held
   package that waits on its gate or reviews is not idle.
5. **Reassess:** daily, or after a load incident, use landings per hour, gate
   failures by class, peak commit charge as a share of the commit limit, review
   rounds and idle root-hours. Each coordinator records these measurements in
   one note, on the item that records this strategy, at each reassessment.
