//! Orientation, listing, and show word handlers.

use super::{
    AgentVerbs, ChildRequirement, DEFAULT_LIMIT, DateTime, Holder, LsInput,
    MAX_AGENT_WORK_RESPONSE_BYTES, MAX_COMPACT_CHANGE_ITEMS, MAX_NEXT_PAGES, NextInput, Receipt,
    StoreError, Utc, VerbError, WorkAvailability, WorkFocusView, WorkLifecycle, WorkNextQuery,
    WorkNextSection, changes_not_delivered, compact_next_lines, compact_next_receipt,
    compact_next_value, fit_show_receipt, item_line, json, ready_line,
    record_verbose_next_omission, section_word, show_lines, show_receipt_value,
};

impl AgentVerbs {
    /// `next`: focus, ready candidates, and the changes since the last call.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the core cannot read or stage the view.
    pub fn next(&self, input: &NextInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.next_with_verbose_budget(input, now, MAX_AGENT_WORK_RESPONSE_BYTES)
    }

    // Production always uses the protocol budget. Tests may inject a tighter
    // verbose overlay ceiling; compact `next` ignores this parameter.
    #[allow(
        clippy::too_many_lines,
        reason = "focus, held, ready, changes, and guidance are assembled in one readable pass"
    )]
    pub(in crate::verbs) fn next_with_verbose_budget(
        &self,
        input: &NextInput,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let ready_limit = if input.verbose {
            limit
        } else {
            limit.clamp(1, super::super::MAX_NEXT_READY_CANDIDATES)
        };
        let change_limit = if input.verbose {
            limit
        } else {
            limit.min(MAX_COMPACT_CHANGE_ITEMS)
        };
        let query = WorkNextQuery {
            sections: vec![
                WorkNextSection::Focus,
                WorkNextSection::Changes,
                WorkNextSection::Memories,
                WorkNextSection::Assigned,
                WorkNextSection::Participated,
            ],
            context_generation: input.context_generation.clone(),
            ..WorkNextQuery::default()
        };
        let mut view = if input.peek {
            self.service
                .work_next_peek_for_agent(
                    change_limit,
                    ready_limit,
                    input.verbose,
                    query,
                    now,
                    |changes| {
                        !super::super::collapsed_changes(changes, self.service.display_identity())
                            .is_empty()
                    },
                )
                .map_err(|error| {
                    VerbError::from(error)
                        .retrying(super::super::receipts::ReadRetry::Next { peek: true })
                })?
        } else {
            self.service
                .work_next_for_agent(change_limit, ready_limit, input.verbose, query, now)
                .map_err(|error| {
                    VerbError::from(error)
                        .retrying(super::super::receipts::ReadRetry::Next { peek: false })
                })?
        };
        view.backup_reminder = self.service.backup_reminder();
        let lists = view.agent_lists.take().ok_or_else(|| {
            StoreError::InvalidWorkProjection("agent next has no advisory list snapshot".into())
        })?;
        let mut held = lists.held;
        let mut ready = lists.ready;
        let mut compact_changes = super::super::collapsed_changes(
            view.changes.as_deref().unwrap_or_default(),
            self.service.display_identity(),
        );
        let mut not_delivered = changes_not_delivered(&view);
        // Compact output may drain own-session-only pages within its bound.
        // Verbose output exposes the original exact page and its cursor, so
        // it must not acknowledge additional pages behind that receipt.
        let mut pages = 1;
        while !input.verbose
            && !input.peek
            && compact_changes.is_empty()
            && not_delivered > 0
            && pages < MAX_NEXT_PAGES
        {
            let more = self.service.work_next(
                change_limit,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                now,
            )?;
            compact_changes = super::super::collapsed_changes(
                more.changes.as_deref().unwrap_or_default(),
                self.service.display_identity(),
            );
            not_delivered = changes_not_delivered(&more);
            pages += 1;
        }
        let mut guidance = view
            .focus
            .as_ref()
            .map(|focus| self.guidance(focus, "next", now))
            .unwrap_or_default();
        if let Some(first) = ready.iter().find(|item| {
            item.availability == WorkAvailability::Ready
                && view
                    .focus
                    .as_ref()
                    .is_none_or(|focus| focus.status.work.work_id != item.work.work_id)
        }) {
            // Catalog cards do not carry session-specific `allowed_next`.
            // Resolve the exact ordinary-vs-recovery claim action via `show`.
            let command = format!("engram work show {}", first.work.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.insert(0, command);
            }
        }
        if let Some((item, _)) = held.iter().find(|(item, _)| {
            view.focus
                .as_ref()
                .is_none_or(|focus| focus.status.work.work_id != item.work.work_id)
        }) {
            let command = format!("engram work show {}", item.work.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        if let Some(row) = view
            .discovery
            .assigned
            .first()
            .or_else(|| view.discovery.participated.first())
        {
            let command = format!("engram work show {}", row.work_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        if guidance.next.is_empty() {
            guidance.next.push("engram work add \"…\"".into());
        }
        if input.peek {
            guidance.next.insert(
                0,
                super::super::memory_recovery::listing_command(
                    view.peek.as_ref(),
                    view.context_generation.as_deref(),
                ),
            );
        }
        let (lines, value, guidance) = if input.verbose {
            // The direction to list memories stays first among the reminders
            // and is never shed; its command is the first next command, which
            // the loop below keeps. The backup reminder follows it and is
            // never shed either.
            let recovery = super::super::memory_recovery::reminder(view.peek.as_ref());
            if let Some(recovery) = &recovery {
                guidance.reminders.insert(0, recovery.clone());
            }
            let mut kept_reminders = usize::from(recovery.is_some());
            if let Some(backup) = &view.backup_reminder {
                guidance.reminders.insert(kept_reminders, backup.clone());
                kept_reminders += 1;
            }
            // The session's own lapsed claim follows, never shed either.
            if let Some(lapse) = &view.claim_lapse_reminder {
                guidance.reminders.insert(kept_reminders, lapse.clone());
                kept_reminders += 1;
            }
            let mut peek_omissions: Vec<super::super::receipts::CompactSectionOmission> =
                Vec::new();
            let mut agent_omissions: Vec<super::super::receipts::CompactSectionOmission> =
                Vec::new();
            let mut evaluation_obligations = view.focus.as_ref().and_then(|focus| {
                focus
                    .evaluated_policy
                    .then(|| {
                        super::super::evaluation_guidance::EvaluationObligations::from_page(
                            &focus.obligation_page,
                            focus.evidence_basis,
                            focus.evaluation_obligation_rows_visible,
                        )
                    })
                    .flatten()
            });
            if evaluation_obligations.is_some() {
                guidance.reminders.retain(|reminder| {
                    !reminder.starts_with("open obligations: ")
                        && !reminder.starts_with("open obligation ")
                        && !reminder.starts_with("resolve obligations needing action, ")
                });
            }
            loop {
                let changes = compact_changes
                    .iter()
                    .map(|change| change.line.clone())
                    .collect::<Vec<_>>();
                let mut lines = super::super::memory_recovery::opening_lines(
                    view.peek.as_ref(),
                    view.context_generation.as_deref(),
                );
                match &view.focus {
                    Some(focus) => {
                        lines.push(format!(
                            "focus: {}",
                            item_line(&focus.status, self.holder(focus, now), now)
                        ));
                        if let Some(status) = &focus.acceptance_evaluation {
                            lines.push(format!(
                                "  evaluation: {}",
                                super::super::show::evaluation_summary(status)
                            ));
                        }
                    }
                    None if peek_omissions
                        .iter()
                        .any(|omission| omission.section == "focus")
                        || agent_omissions
                            .iter()
                            .any(|omission| omission.section == "focus") =>
                    {
                        lines.push("focus: omitted (byte budget)".into());
                    }
                    None => lines.push("focus: none".into()),
                }
                if let Some(advisory) = &evaluation_obligations {
                    lines.extend(advisory.reminder_lines());
                    if advisory.requires_action() {
                        lines.push("resolve obligations needing action, then request a fresh acceptance evaluation before done".into());
                    }
                }
                lines.push(format!("held by you ({}):", held.len()));
                for (item, expires_at) in &held {
                    lines.push(format!(
                        "  {}",
                        item_line(item, Holder::You(*expires_at), now)
                    ));
                }
                super::super::receipts::append_discovery_lines(&mut lines, &view.discovery);
                lines.push(format!("ready ({}):", ready.len()));
                for item in &ready {
                    lines.push(format!("  {}", ready_line(item)));
                }
                super::super::receipts::append_next_changes_lines(
                    &mut lines,
                    &changes,
                    not_delivered,
                    view.peek.as_ref(),
                );
                if let Some(memories) = &view.memories {
                    lines.push(format!(
                        "memories: {} retained{}",
                        memories.count,
                        if memories.changed { " (changed)" } else { "" }
                    ));
                }
                if input.peek {
                    super::super::receipts::append_peek_disclosure(
                        &mut lines,
                        view.peek.as_ref(),
                        view.context_generation.as_deref(),
                    );
                }
                for omission in view
                    .omissions
                    .iter()
                    .filter(|omission| omission.section != WorkNextSection::Changes)
                {
                    lines.push(format!(
                        "  ({} more {} not shown)",
                        omission.omitted_count,
                        section_word(omission.section)
                    ));
                }
                let mut value = serde_json::to_value(&view)?;
                if let Some(advisory) = &evaluation_obligations {
                    value["evaluation_obligations"] = json!(advisory);
                }
                if input.peek {
                    value["memories_detail"] =
                        json!(super::super::memory_recovery::listing_command(
                            view.peek.as_ref(),
                            view.context_generation.as_deref(),
                        ));
                    value["preview_omissions"] = json!(peek_omissions);
                    for omission in &peek_omissions {
                        lines.push(format!(
                            "  ({} {} rows omitted from this preview)",
                            omission.omitted_count, omission.section
                        ));
                    }
                } else if !agent_omissions.is_empty() {
                    value["agent_omissions"] = json!(agent_omissions);
                    for omission in &agent_omissions {
                        lines.push(format!(
                            "  ({} {} omitted to fit this receipt)",
                            omission.omitted_count, omission.section
                        ));
                    }
                }
                value["ready"] = serde_json::to_value(&ready)?;
                value["changes_by_others"] = json!(changes);
                value["held"] = serde_json::to_value(
                    held.iter()
                        .map(|(item, expires_at)| {
                            let mut row = json!({ "work": item.work, "expires_at": expires_at });
                            if let Some(status) = &item.work.current_status {
                                row["current_status"] = json!(status);
                            }
                            if let Some(peer) = &item.work.status_observation {
                                row["status_observation"] = json!(peer);
                            }
                            row
                        })
                        .collect::<Vec<_>>(),
                )?;
                let receipt =
                    Receipt::assemble(lines.clone(), guidance.clone(), value.clone(), false)
                        .with_build_identity(&view.read_cut, view.context_generation.as_deref());
                if super::super::receipts::agent_receipt_fits(&receipt, budget)? {
                    break (lines, value, guidance.clone());
                }
                if view.discovery.shorten_status_previews()
                    || held.iter_mut().rev().any(|(item, _)| {
                        crate::work_service::shorten_status_previews(
                            &mut item.work.current_status,
                            &mut item.work.status_observation,
                        )
                    })
                    || crate::work_service::shed_work_next_focus(&mut view)
                {
                    if view.focus.is_none() {
                        evaluation_obligations = None;
                    }
                    continue;
                }
                // Non-peek verbose delivery keeps the exact staged page. Peek
                // may omit raw rows from the preview only; it never acknowledges
                // them. Progress is monotone in remaining raw row count, not
                // rendered bytes: removing completion can reveal its longer
                // checkpoint. Re-collapse and keep shedding until it fits.
                let section = if view.discovery.shed_one() {
                    continue;
                } else if input.peek
                    && view
                        .changes
                        .as_mut()
                        .is_some_and(|rows| rows.pop().is_some())
                {
                    compact_changes = super::super::collapsed_changes(
                        view.changes.as_deref().unwrap_or_default(),
                        self.service.display_identity(),
                    );
                    if let Some(peek) = &mut view.peek {
                        peek.more_changes_available = true;
                    }
                    "changes"
                } else if ready.pop().is_some() {
                    "ready"
                } else if held.pop().is_some() {
                    "held"
                } else if evaluation_obligations
                    .as_mut()
                    .is_some_and(super::super::evaluation_guidance::EvaluationObligations::omit_one)
                {
                    continue;
                } else if guidance.reminders.len() > kept_reminders {
                    guidance.reminders.pop();
                    "reminders"
                } else if guidance.next.len() > 1 {
                    guidance.next.pop();
                    "next"
                } else if view.focus.take().is_some() {
                    evaluation_obligations = None;
                    "focus"
                } else {
                    break (lines, value, guidance.clone());
                };
                if input.peek {
                    super::super::receipts::record_compact_omission(
                        &mut peek_omissions,
                        section,
                        1,
                    );
                } else if section == "ready" {
                    record_verbose_next_omission(&mut view.omissions, 1);
                } else {
                    // Whole-focus removal is not a trim-step Focus count.
                    super::super::receipts::record_compact_omission(
                        &mut agent_omissions,
                        section,
                        1,
                    );
                }
            }
        } else {
            let claims = lists
                .claims
                .into_iter()
                .map(|(id, holder, expiry)| {
                    (
                        id,
                        (self.service.display_identity().session(&holder), expiry),
                    )
                })
                .collect();
            let compact = compact_next_receipt(
                &view,
                &held,
                &ready,
                &compact_changes,
                &claims,
                &guidance,
                lists.ready_navigation,
            )?;
            let lines = compact_next_lines(&compact);
            let value = compact_next_value(&compact);
            (lines, value, compact.guidance)
        };
        if !input.peek
            && value
                .get("memories")
                .is_some_and(|memories| !memories.is_null())
        {
            self.service.acknowledge_work_next_memories(&view, now);
        }
        Ok(Receipt::assemble(lines, guidance, value, false)
            .with_build_identity(&view.read_cut, view.context_generation.as_deref()))
    }

    /// `ls`: open items by default; `search` is `ls` over every lifecycle.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the catalog cannot be read.
    pub fn ls(&self, input: &LsInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.with_claim_lapse(now, MAX_AGENT_WORK_RESPONSE_BYTES, |budget| {
            self.ls_with_budget(input, now, budget)
        })
    }

    /// `ls` for a program reading its JSON: each page fits `page_bytes`
    /// instead of the agent protocol budget. Rows, order, cursors and
    /// continuation are those of `ls`; only how many rows fit a page differs.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when `page_bytes` is outside the protocol budget
    /// through [`crate::work_service::MAX_LS_PAGE_BYTES`], or the catalog
    /// cannot be read.
    pub fn ls_with_page_bytes(
        &self,
        input: &LsInput,
        now: DateTime<Utc>,
        page_bytes: usize,
    ) -> Result<Receipt, VerbError> {
        if !(MAX_AGENT_WORK_RESPONSE_BYTES..=crate::work_service::MAX_LS_PAGE_BYTES)
            .contains(&page_bytes)
        {
            return Err(StoreError::InvalidWork(format!(
                "page bytes {page_bytes} is outside {MAX_AGENT_WORK_RESPONSE_BYTES}..={}; choose a budget in that range",
                crate::work_service::MAX_LS_PAGE_BYTES
            ))
            .into());
        }
        self.with_claim_lapse(now, page_bytes, |budget| {
            self.ls_with_budget(input, now, budget)
        })
    }

    // Agent surfaces use the protocol budget; a JSON-reading program may ask
    // for a larger one, and tests can exercise the zero-row boundary without
    // bypassing the bounded item projection.
    pub(in crate::verbs) fn ls_with_budget(
        &self,
        input: &LsInput,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1_000);
        input.validate_listing()?;
        let command = input.list_command();
        let page = self
            .service
            .work_catalog_page(
                &crate::domain::WorkCatalogQuery {
                    search: input.search.clone(),
                    lifecycles: if input.all {
                        Vec::new()
                    } else {
                        vec![WorkLifecycle::Open]
                    },
                    blocked_only: input.blocked,
                    availabilities: if input.ready {
                        vec![WorkAvailability::Ready]
                    } else {
                        Vec::new()
                    },
                    ready_priority_order: input.ready,
                    assigned_to: input.mine.then(|| self.actor_id.clone()),
                    held_by: input.mine.then(|| self.session_id.clone()),
                    label: input.label.clone(),
                    child_requirement: if input.optional {
                        Some(ChildRequirement::Optional)
                    } else if input.required {
                        Some(ChildRequirement::Required)
                    } else {
                        None
                    },
                    limit,
                    ..crate::domain::WorkCatalogQuery::default()
                },
                input.under.as_deref(),
                input.after.as_deref(),
                now,
            )
            .map_err(|error| VerbError::for_listing(error, &command))?;
        super::super::listing::fit_list_receipt(
            input,
            &page,
            self.service.display_identity(),
            budget,
        )
        .map_err(|error| VerbError::for_listing(error.error, &command))
    }

    /// `show`: one item in agent detail without changing ambient focus or
    /// claiming. Host authority and integrity fields remain on `work core
    /// focus`.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the reference is unknown.
    pub fn show(&self, work_ref: &str, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.with_claim_lapse(now, MAX_AGENT_WORK_RESPONSE_BYTES, |budget| {
            self.show_within(work_ref, now, budget)
        })
    }

    pub(in crate::verbs) fn show_within(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let view = self
            .service
            .work_focus_for_agent(work_ref, now)
            .map_err(|error| VerbError::at(error, work_ref))?;
        fit_show_receipt(view, |view| self.render_show(view, now), budget)
    }

    pub(in crate::verbs) fn render_show(
        &self,
        view: &WorkFocusView,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        if view.status.work.parent_id.is_some() && view.parent.is_none() {
            return Err(StoreError::InvalidWorkProjection(
                "safe show is missing direct parent context for a child".into(),
            )
            .into());
        }
        let holder = self.holder(view, now);
        let lines = show_lines(view, holder, self.service.display_identity(), now);
        let mut guidance = self.guidance(view, "show", now);
        if let Some(parent) = &view.parent {
            // Keep actionable recovery first; parent navigation precedes optional history.
            let command = format!("engram work show {}", parent.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        if view.history.total + view.restored_history.total > 0 {
            guidance.next.push(format!(
                "engram work show {} --history",
                view.status.work.short_ref
            ));
        }
        if super::super::mutation::needs_full_contract(view) {
            let command = super::super::mutation::full_contract(&view.status.work.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(show_receipt_value(
                view,
                holder,
                self.service.display_identity(),
                now,
            ))?,
            false,
        ))
    }

    /// Like `show`, optionally returning the newest complete-note window.
    ///
    /// # Errors
    /// Returns [`VerbError`] for unknown work, invalid notes or oversized metadata.
    pub fn show_with_notes(
        &self,
        work_ref: &str,
        notes: bool,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        self.show_records(
            work_ref,
            &super::super::ShowInput {
                notes,
                ..super::super::ShowInput::default()
            },
            now,
        )
    }
}
