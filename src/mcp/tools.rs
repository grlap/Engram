//! Tool routing and MCP response construction over the shared agent words.

#[cfg(test)]
use super::prose_sweep;
use super::{
    McpServer,
    arguments::{
        AddArgs, DoneArgs, EvaluateArgs, ForgetArgs, GateArgs, HandoffActionArg, HandoffArgs,
        LsArgs, MemoriesArgs, NextArgs, NoteArgs, RememberArgs, ShowArgs, UpdateActionArg,
        UpdateArgs, WorkClaimArgs, WorkSearchArgs,
    },
    parameters::Parameters,
};
use crate::{
    AddInput, AgentVerbs, ClaimInput, ClaimUnderInput, DoneInput, EvaluateInput, ForgetInput,
    GateInput, HandoffAction, HandoffInput, LsInput, MemoriesInput, NextInput, NoteInput, Receipt,
    RememberInput, UpdateAction, UpdateInput, VerbError, parse_defer_date, store_error_value,
};
use chrono::Utc;
use rmcp::{model::CallToolResult, tool, tool_router};
use serde_json::json;

#[tool_router(router = agent_tool_router, vis = "pub(super)")]
impl McpServer {
    /// What is ready, what this session holds, and what changed.
    #[tool(
        name = "next",
        description = "What is ready, what you hold, and what changed; peek=true reads orientation without staging or advancing delivery, focus or memory advertisement"
    )]
    pub(super) fn next(&self, Parameters(args): Parameters<NextArgs>) -> CallToolResult {
        self.verb(self.verbs().next(
            &NextInput {
                limit: args.limit,
                peek: args.peek.unwrap_or(false),
                verbose: args.verbose.unwrap_or(false),
                context_generation: args.context_generation,
            },
            Utc::now(),
        ))
    }

    /// List open work with flat filters.
    #[tool(
        name = "ls",
        description = "List open work; search, ready, blocked, mine, all, label, and under with optional/required narrow it; after continues the same listing"
    )]
    pub(super) fn ls(&self, Parameters(args): Parameters<LsArgs>) -> CallToolResult {
        self.verb(self.verbs().ls(
            &LsInput {
                search: args.search,
                blocked: args.blocked.unwrap_or(false),
                ready: args.ready.unwrap_or(false),
                mine: args.mine.unwrap_or(false),
                all: args.all.unwrap_or(false),
                label: args.label,
                under: args.under,
                optional: args.optional.unwrap_or(false),
                required: args.required.unwrap_or(false),
                after: args.after,
                limit: args.limit,
                verbose: args.verbose.unwrap_or(false),
            },
            Utc::now(),
        ))
    }

    /// Inspect one item without changing focus or claims.
    #[tool(
        name = "show",
        description = "One item with display-only peer labels: outcome, acceptance, holder, blockers, reminders; criterion_links traverses one frozen seal in recorded order, with after retaining that historical seal; reading changes neither focus nor claims. Rich verbose next/ls may expose raw identity and integrity metadata."
    )]
    pub(super) fn show(&self, Parameters(args): Parameters<ShowArgs>) -> CallToolResult {
        self.verb(self.verbs().show_records(
            &args.work_ref,
            &crate::verbs::ShowInput {
                notes: args.notes.unwrap_or(false),
                gates: args.gates.unwrap_or(false),
                history: args.history.unwrap_or(false),
                after: args.after,
                note: args.note,
                full: args.full.unwrap_or(false),
                evaluations: args.evaluations.unwrap_or(false),
                evaluation: args.evaluation,
                observations: args.observations.unwrap_or(false),
                criterion_links: args.criterion_links.unwrap_or(false),
            },
            Utc::now(),
        ))
    }

    /// Create a root or one required/optional child.
    #[tool(
        name = "add",
        description = "Create work from a title; under adds a child and optional makes it non-blocking"
    )]
    pub(super) fn add(&self, Parameters(args): Parameters<AddArgs>) -> CallToolResult {
        self.verb(self.verbs().add(
            AddInput {
                external: args.external,
                notes: args.notes.unwrap_or_default(),
                title: args.title,
                outcome: args.outcome,
                acceptance: args.acceptance.unwrap_or_default(),
                bindings: args.bindings.unwrap_or_default(),
                under: args.under,
                optional: args.optional.unwrap_or(false),
                priority: args.priority,
                labels: args.labels.unwrap_or_default(),
                assignee: args.assignee,
                kind: args.kind,
                evaluation_mode: args.evaluation_mode,
            },
            Utc::now(),
        ))
    }

    /// Hold an item, or a parent's next ready child.
    #[tool(
        name = "claim",
        description = "Hold an item before changing anything; later calls default to it. With under instead of work_ref, hold that parent's next ready child, chosen in ls --ready order and claimed in the same transaction"
    )]
    pub(super) fn claim(&self, Parameters(args): Parameters<WorkClaimArgs>) -> CallToolResult {
        let outcome = match (args.work_ref, args.under) {
            (None, Some(under)) => self.verbs().claim_under(
                ClaimUnderInput {
                    under,
                    ttl_seconds: args.ttl_seconds,
                    recover: args.recover,
                },
                Utc::now(),
            ),
            (Some(work_ref), None) => self.verbs().claim(
                ClaimInput {
                    work_ref,
                    ttl_seconds: args.ttl_seconds,
                    recover: args.recover,
                },
                Utc::now(),
            ),
            (None, None) => {
                Err(crate::StoreError::InvalidWork("claim needs work_ref or under".into()).into())
            }
            (Some(_), Some(_)) => Err(crate::StoreError::InvalidWork(
                "claim takes work_ref or under, not both".into(),
            )
            .into()),
        };
        self.verb(outcome)
    }

    /// Apply exactly one planning or claim action.
    #[tool(
        name = "update",
        description = "One action: release, blocked, unblock, revise, cancel, reject (required child plus reason; parent and ancestors must be open; atomically cancels and waives), after/drop_after (prerequisite), waive (child plus reason), supersede (replacement plus reason), or detach (stranded child plus reason)"
    )]
    pub(super) fn update(&self, Parameters(args): Parameters<UpdateArgs>) -> CallToolResult {
        if args.external.is_some() && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument("external", "external reference requires action revise");
        }
        if args.clear_external && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument(
                "clear_external",
                "clearing the external reference requires action revise",
            );
        }
        if args.acceptance.is_some() && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument(
                "acceptance",
                "acceptance replacement requires action revise",
            );
        }
        if args.bindings.is_some() && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument("bindings", "verification bindings require action revise");
        }
        if args.blocker.is_some() && !matches!(args.action, UpdateActionArg::Unblock) {
            return invalid_argument("blocker", "a blocker selector requires action unblock");
        }
        if args.evaluation_mode.is_some() && !matches!(args.action, UpdateActionArg::EvaluationMode)
        {
            return invalid_argument(
                "evaluation_mode",
                "an evaluation mode requires action evaluation_mode; it is ignored by no other action",
            );
        }
        let action = match args.action {
            UpdateActionArg::Release => UpdateAction::Release {
                reason: args.reason,
            },
            UpdateActionArg::Blocked => UpdateAction::Blocked {
                detail: args.text.unwrap_or_default(),
            },
            UpdateActionArg::Unblock => UpdateAction::Unblock {
                blocker: args.blocker,
            },
            UpdateActionArg::Revise => {
                let defer = match args.defer.as_deref().map(parse_defer_date).transpose() {
                    Ok(defer) => defer,
                    Err(message) => return invalid_argument("defer", &message),
                };
                UpdateAction::Revise {
                    external: args.external,
                    clear_external: args.clear_external,
                    title: args.title,
                    outcome: args.outcome,
                    acceptance: args.acceptance,
                    bindings: args.bindings,
                    assignee: args.assignee,
                    priority: args.priority,
                    defer,
                    kind: args.kind,
                    labels: args.labels.unwrap_or_default(),
                    unlabels: args.unlabels.unwrap_or_default(),
                }
            }
            UpdateActionArg::EvaluationMode => UpdateAction::EvaluationMode {
                mode: args.evaluation_mode,
            },
            UpdateActionArg::Cancel => UpdateAction::Cancel {
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::Reject => UpdateAction::Reject {
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::After => UpdateAction::After {
                prerequisite: args.prerequisite.unwrap_or_default(),
            },
            UpdateActionArg::DropAfter => UpdateAction::DropAfter {
                prerequisite: args.prerequisite.unwrap_or_default(),
            },
            UpdateActionArg::Waive => UpdateAction::WaiveRequiredChild {
                child: args.child.unwrap_or_default(),
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::Detach => UpdateAction::Detach {
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::Supersede => UpdateAction::Supersede {
                replacement: args.replacement.unwrap_or_default(),
                reason: args.reason.unwrap_or_default(),
            },
        };
        self.verb(self.verbs().update(
            UpdateInput {
                work_ref: args.work_ref,
                action,
            },
            Utc::now(),
        ))
    }

    /// Record a gate on held open work or a late finding on completed work.
    #[tool(
        name = "gate",
        description = "Record a gate on held open work or an attributed late finding on completed work; work_ref defaults to focus and evidence_ref is opaque"
    )]
    pub(super) fn gate(&self, Parameters(args): Parameters<GateArgs>) -> CallToolResult {
        self.verb(self.verbs().gate(
            GateInput {
                work_ref: args.work_ref,
                name: args.name,
                failed: args.failed.unwrap_or_default(),
                evidence_ref: args.evidence_ref,
            },
            Utc::now(),
        ))
    }

    /// Record one attributed acceptance evaluation on an item's active run.
    #[tool(
        name = "evaluate",
        description = "Record one immutable acceptance evaluation on the targeted item's active run: one verdict per criterion with basis, rationale, and run-evidence citations; work_ref defaults to focus and acceptance_basis is the revision printed by show"
    )]
    pub(super) fn evaluate(&self, Parameters(args): Parameters<EvaluateArgs>) -> CallToolResult {
        self.verb(self.verbs().evaluate(
            EvaluateInput {
                work_ref: args.work_ref,
                mode: args.mode,
                acceptance_basis: args.acceptance_basis,
                evidence_basis: args.evidence_basis,
                verdicts: args.verdicts,
                attempt: args.attempt,
                source_fingerprint: args.source_fingerprint,
                model: args.model,
                execution_identity: args.execution_identity,
                parent_session: args.parent_session,
                supersedes: args.supersedes,
            },
            Utc::now(),
        ))
    }

    /// Store one attributed project memory.
    #[tool(
        name = "remember",
        description = "Store an attributed project note; revise an existing key with retained history. Optional expected_revision refuses stale writes; no focus or claim changes"
    )]
    pub(super) fn remember(&self, Parameters(args): Parameters<RememberArgs>) -> CallToolResult {
        self.verb(self.verbs().remember(
            RememberInput {
                text: args.text,
                key: args.key,
                revise: args.revise.unwrap_or(false),
                expected_revision: args.expected_revision,
                retires_with: args.retires_with,
                clear_retires_with: args.clear_retires_with.unwrap_or(false),
                append: args.append.unwrap_or(false),
                section: args.section,
            },
            Utc::now(),
        ))
    }

    /// List, search, or fully read project memories.
    #[tool(
        name = "memories",
        description = "List or search current project memories; full with an exact key reads one body, with optional revision for attributed history; records nothing unless context_generation is given"
    )]
    pub(super) fn memories(&self, Parameters(args): Parameters<MemoriesArgs>) -> CallToolResult {
        self.verb(self.verbs().memories(
            &MemoriesInput {
                query: args.query,
                after: args.after,
                full: args.full.unwrap_or(false),
                revision: args.revision,
                context_generation: args.context_generation,
            },
            Utc::now(),
        ))
    }

    /// Permanently retire one project-memory key.
    #[tool(
        name = "forget",
        description = "Append an attributed tombstone for one project-memory key; this is not erasure"
    )]
    pub(super) fn forget(&self, Parameters(args): Parameters<ForgetArgs>) -> CallToolResult {
        self.verb(
            self.verbs()
                .forget(ForgetInput { key: args.key }, Utc::now()),
        )
    }

    /// Record one finding once.
    #[tool(
        name = "note",
        description = "Record an attributed note on open or blocked work without claiming, including children of a completed parent; work_ref selects the target. Only a live holder checkpoints; completed notes never change a frozen seal"
    )]
    pub(super) fn note(&self, Parameters(args): Parameters<NoteArgs>) -> CallToolResult {
        self.verb(self.verbs().note(
            &NoteInput {
                status: args.status.unwrap_or(false),
                work_ref: args.work_ref,
                text: args.text,
                refs: args.refs.unwrap_or_default(),
            },
            Utc::now(),
        ))
    }

    /// Complete the held item and disclose absent per-criterion evidence links.
    #[tool(
        name = "done",
        description = "Complete the item you hold; optional links cite existing note/gate evidence with the required link_basis from show, author linkage not verification. Success discloses criteria with no evidence linked to this criterion, without refusing completion for that absence; a refusal says what is still owed and the command that resolves it"
    )]
    pub(super) fn done(&self, Parameters(args): Parameters<DoneArgs>) -> CallToolResult {
        self.verb(self.verbs().done(
            DoneInput {
                links: args.links.unwrap_or_default(),
                link_basis: args.link_basis,
                work_ref: args.work_ref,
                summary: args.summary,
                note: args.note,
                source_fingerprint: args.source_fingerprint,
                landing: args.landing,
            },
            Utc::now(),
        ))
    }

    /// Search every item by text.
    #[tool(
        name = "search",
        description = "Search every item, including closed ones, by text"
    )]
    pub(super) fn search(&self, Parameters(args): Parameters<WorkSearchArgs>) -> CallToolResult {
        self.verb(self.verbs().search(&args.query, args.limit, Utc::now()))
    }

    /// Offer, accept, or cancel a transfer.
    #[tool(
        name = "handoff",
        description = "Offer the item you hold to another session, accept an offer made to you, or cancel yours"
    )]
    pub(super) fn handoff(&self, Parameters(args): Parameters<HandoffArgs>) -> CallToolResult {
        let action = match args.action {
            HandoffActionArg::Offer => HandoffAction::Offer {
                to: args.to.unwrap_or_default(),
                summary: args.summary,
                ttl_seconds: args.ttl_seconds,
            },
            HandoffActionArg::Accept => HandoffAction::Accept,
            HandoffActionArg::Cancel => HandoffAction::Cancel {
                reason: args.reason.unwrap_or_default(),
            },
        };
        self.verb(self.verbs().handoff(
            HandoffInput {
                work_ref: args.work_ref,
                action,
            },
            Utc::now(),
        ))
    }
}

pub(super) fn verb(outcome: Result<Receipt, VerbError>, words: &AgentVerbs) -> CallToolResult {
    let value = match outcome {
        Ok(receipt) => Ok(words.spell_receipt(receipt).value),
        Err(error) => {
            let guidance = words.error_guidance(&error);
            let mut value = words.project_error(&error, store_error_value(&error.error));
            value["error"]["reminders"] = json!(guidance.reminders);
            value["error"]["next"] = json!(guidance.next);
            Err(value)
        }
    };
    // Every MCP answer a test produces is swept for a CLI flag in its prose.
    #[cfg(test)]
    match &value {
        Ok(value) | Err(value) => prose_sweep::assert_prose_names_fields(value),
    }
    let started = crate::phase_trace::start();
    let result = match value {
        Ok(value) => CallToolResult::structured(value),
        Err(value) => CallToolResult::structured_error(value),
    };
    crate::phase_trace::finish(crate::phase_trace::Phase::ReceiptSerialize, started);
    result
}

/// An argument combination a tool refuses before it runs. Like every tool
/// error Engram itself returns, it carries `reminders` and `next`: the
/// reason, and no command, as a verb error with no specific remedy does. An
/// argument the MCP library rejects first carries text only.
fn invalid_argument(field: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": {
            "code": "invalid_argument",
            "message": message,
            "details": { "field": field },
            "reminders": [message],
            "next": [],
        }
    }))
}
