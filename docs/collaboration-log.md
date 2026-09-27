# Working together: Engram and TermAl

This is an evolving retrospective of collaboration between Greg and the agents
developing Engram and TermAl. Advisor records what works, what causes friction,
and what happens after we try to improve it. Greg assigned this role on
2026-09-27 and clarified that the subject is our experience of working together,
rather than another description of operating rules.

Each dated entry describes an observed situation, its effect on the work, and
what we learned. Later entries will document changes and whether they helped.
The account includes Advisor's own mistakes. It is qualitative: we do not yet
have a measured baseline for coordination cost or improvement over time.
Dates and times use UTC. The separate [workflow document](agent-pair-workflow.md)
contains adopted coordination guidance and an unadopted pilot proposal.

## 2026-09-27 — Foundations, with too much friction

Recorded by Engram::Advisor at 22:17 UTC. Migration and council status in this
entry reflect peer reports through 22:07 UTC; later reports appear below.

### What the collaboration looks like in practice

Engram and TermAl agents are both builders and users of each other's work.
TermAl hosts their sessions and execution; Engram keeps work and memory. Moving
TermAl's tasks from Beads to Engram made the integration a real working problem:
one project could prepare an import while the other repaired a missing behavior
that the import or its acceptance checks exposed.

The agents exchanged findings and artifacts through mailboxes. Implementers
made corrections, reviewers inspected them, and Advisor assembled readiness
reports for Greg. Across session replacements, durable notes and handovers
helped carry the work forward. Yet Greg still had to ask repeatedly for status,
what everyone was focused on, and when he could restart TermAl. The coordination
existed, but its results were not consistently clear to the person waiting.

### What worked well

**The projects found real defects for each other.** Migration preparation
exposed missing acceptance bindings in Engram and gaps in the migration
verifier. The findings reached owners who corrected them. This is more useful
than planning improvements in isolation: the need came from another agent
trying to do actual work.

**Review produced concrete corrections.** The verifier was exercised with valid
data, deliberately invalid data and restored data. Independent inspection found
specific omissions, including accounting for known items outside the import.
The corrected preparation was accepted on those artifacts. That demonstrated
progress without pretending that a successful rehearsal completed the live
migration. Sources: Termal::Fable's migration reports and Advisor's reviews on
2026-09-27, followed by Termal::Fable2's handover confirmation.

**Some knowledge survived handovers.** Successor agents received the migration
artifacts, outstanding gaps and named owners. Termal::Fable2 could continue
with the accepted verifier rather than rebuilding it from scratch. This is a
useful result of durable notes and handover material, though it does not prove
that recovery is consistently cheap or complete.

**Peer reports could distinguish partial success from completion.** At 22:07
UTC, Termal::Fable2 reported that a test in the selected worktree passed while
Engram still held no verification evidence for the run. She supplied the
execution conditions and a report, and identified Termal::Opus2 as the fix
owner. The collaboration exposed a real integration gap instead of treating
the passing test as sufficient. At this entry, the correction and repeat of
the same live scenario are still pending.

### What worked poorly

**Greg had to pull information out of the system.** The retained conversation
contains repeated status requests and questions about restart timing. Advisor
was receiving detailed peer updates, but that did not reliably become a short
answer about what was ready, what was blocked and what Greg needed to do.
My assessment: reporting internal activity consumed attention without giving
enough visibility of the outcome.

**Already-given authority was reopened during handovers.** Greg had confirmed
the same standing commit principle for both projects. Agents still debated
whether it applied to a replacement session, and reported older written
guidance suggesting fresh confirmation. In Advisor's assessment, this created
avoidable coordination and pressure to ask Greg again. The source of the
decision and its scope needed
to travel together; preserving a rule-shaped summary was not enough.
Sources: Greg's confirmation is recorded in
[Authority and Git](../AGENTS.md#authority-and-git) and its identical
[CLAUDE.md section](../CLAUDE.md#authority-and-git); the practical friction was
reported in Termal::Opus2 and Termal::Fable2's handover exchanges on 2026-09-27.

**Successful parts did not yet make a successful whole.** Rehearsals, installed
builds, restarts and project configuration each answered different questions.
Live evidence collection still failed in the scenario Fable2 reported. This
made readiness difficult to explain and meant the migration remained unfinished
despite substantial completed preparation. The peer report assigns the
operational gap to TermAl; making its implications understandable belongs to
coordination too.

**Advisor initially misunderstood this documentation task.** I produced a
document dominated by roles, rules and process, then answered with another
migration status update. Greg clarified that he wanted an account of the
collaboration: what works well, what works poorly, and later improvements.
This rewrite corrects that mismatch. More complete process documentation would
not have answered the question he actually asked.

### Changes started, with results still to observe

Greg directed agents to work autonomously and take disputed points first to
a council of Codex and Fable from both projects. He asked for a joint account
of product direction and clear information when TermAl needs a restart.
The instruction was distributed; Engram::Fable reported at 22:05 UTC that she
was consolidating council contributions. The joint report had not yet arrived
at this baseline. We cannot yet say whether this arrangement reduces delays
or repeated requests to Greg. This records the change, not new authority or
an exception to either project's execution requirements.

Advisor is also changing the documentation itself: this file now starts with
experience and effects. Subsequent entries will compare what happened after
an improvement with the problem it was meant to address. We should be able
to see whether Greg needed fewer repeated explanations, whether successors
recovered without his reconstruction, and whether a reported defect recurred
after deployment. These are questions for observation, not claimed metrics.

### What this says about the improvement loop

There is a working path from a real problem to a shared finding, an owned
correction and independent checking. Migration preparation provides concrete
examples. The weaker parts are coordination overhead and following a deployed
change back into real use to establish that it helped.

The live test that passed without producing evidence is a useful case to
follow: the symptom and owner are known, but the outcome is still open. A later
entry should say what changed, whether the same scenario then worked, and
whether the extra coordination was proportionate. Until that observation,
calling the whole loop complete would go beyond what we have seen.

## 2026-09-27 — Council synthesis and an incomplete recovery improvement

Recorded by Engram::Advisor at 22:22 UTC after reading the council report and
host diagnosis.
Sources: Engram::Fable's joint report delivered to Advisor at 22:12 UTC and
Termal::Opus2's reviewer-access diagnosis at 22:17 UTC. These are attributed
peer reports; Advisor has not independently measured their runtime results.

**A useful consolidation happened.** Engram::Fable gathered all four council
members' contributions into one report. It identified shared concerns about
missing evidence, source freshness and recovery, alongside proposed priorities.
This delivered the synthesis that was still pending in the earlier entry.
It does not yet show that consultation shortened work or improved either
product after deployment.

**The authority disagreement is part of the account too.** The council report
records a disagreement between Advisor and Termal::Fable about relayed
authorization, with a settlement proposed by Termal::Fable2. Advisor's position
was that Greg's newer standing instruction already covered the authorized
acts across handovers. That is Advisor's position in the disagreement, not a
claim that all participants had accepted it. During this follow-up, Advisor
read TermAl's `CLAUDE.md` section "Never commit or push without explicit
permission": it still said "ask first, every time". The proposed settlement
was not yet reflected there. This records the disagreement and the observed
text; it does not adopt the council's recommendation or change authority.

**A proposed improvement is now concrete enough to follow.** The council
recommended making every withheld check credit or lost report explain its
reason to the agent, and preserving a minimal reproduction for verification
after deployment. That connects a coordination symptom (agents having to
notice unexplained absence) to a product change with an observable result.
The recommendation and its effectiveness are separate: implementation and
post-deployment observation are still owed for the reported missing-credit case.

**Our own review exposed a mismatch between instruction and capability.**
A Claude reviewer reported being unable to read Engram memories. Advisor
forwarded the exact refusal and context to TermAl. Opus2 matched it to an
existing defect: according to his code inspection, the newer reviewer guidance
allowed recovery reads, but Claude's tool-permission enforcement still refused
the MCP calls. The Codex reviewer's round-two structured result for this
retrospective records successful Engram MCP recovery: a peek, three memory-list
pages and full reads of thirteen relevant entries. This is the reviewer's
reported execution, not Advisor's reproduction of it. The intended recovery
improvement therefore did not work equally across
the two runtimes. The diagnosis reached an owner; a verified fix has not yet
been reported.

Advisor's lesson from these two exchanges: collecting viewpoints and changing
instructions are useful intermediate results. We still need to observe the
effect in the actual agent workflow before describing them as improvements
that worked. The common failure is a missing connection between parts that
each look reasonable in isolation.

## Continuing the account

Advisor adds a dated, timed and author-attributed entry when there is something
substantive to learn; a successor names their authorship too. Briefly connect
the earlier problem, the attempted improvement, the observed effect and any
remaining uncertainty. A change that did not help belongs here too. Keep prior
observations dated and correct mistakes explicitly. Detailed execution records
and task status stay in the tracker; this file preserves the experience and
the lessons rather than every message or every task.
