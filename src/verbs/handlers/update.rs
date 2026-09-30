//! `update` word handler: translates the flat agent-facing update actions
//! into the typed core update request and its receipt line.

use super::{
    AgentVerbs, DateTime, Receipt, StoreError, UpdateAction, UpdateInput, Utc, VerbError,
    WorkBlockerKind, WorkRevisionPatch, WorkUpdateInput, held_suffix, nonempty, parse_bindings,
    parse_supplied_evaluation_mode, short, trimmed, validate_priority,
};

/// The exact command that clears the blocker whose stored id is
/// `blocker_id` from `work_ref`.
pub(in crate::verbs) fn unblock_command(work_ref: &str, blocker_id: &str) -> String {
    format!(
        "engram work update {work_ref} --unblock --blocker {}",
        crate::work_service::blocker_selector::encode(blocker_id)
    )
}

/// What guidance offers for the item's active blockers: for one, the exact
/// command that clears it; for several, the read that lists each with its
/// own command, since only the agent knows which reason has gone, and nothing
/// from `show` itself, which already lists them.
pub(super) fn unblock_guidance(
    view: &crate::work_service::WorkFocusView,
    word: &str,
) -> Option<String> {
    let work_ref = &view.status.work.short_ref;
    match (
        view.blocker_count.max(view.blockers.len()),
        view.blockers.as_slice(),
    ) {
        (1, [blocker]) => Some(unblock_command(work_ref, &blocker.blocker_id)),
        (0, _) => None,
        _ => (word != "show").then(|| format!("engram work show {work_ref}")),
    }
}

/// A cleared blocker as the receipt names it: its selector, kind and detail
/// as it was recorded when it was raised.
fn cleared_blocker_words(blocker: &crate::WorkBlocker) -> String {
    format!(
        "blocker {} ({}) \"{}\"",
        crate::work_service::blocker_selector::encode(&blocker.blocker_id),
        crate::work_service::blocker_kind_word(blocker.kind),
        short(&blocker.detail)
    )
}

impl AgentVerbs {
    /// `update`: revise planning/lifecycle state or waive one disposed required child.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when no action applies or the core refuses it.
    pub fn update(&self, input: UpdateInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let prerequisite_target = match &input.action {
            UpdateAction::After { prerequisite } | UpdateAction::DropAfter { prerequisite }
                if !prerequisite.trim().is_empty() =>
            {
                let prerequisite = self
                    .service
                    .resolve_work_reference(prerequisite, now)
                    .map_err(|error| VerbError::at(error, prerequisite))?;
                Some((prerequisite.work_id, prerequisite.short_ref))
            }
            UpdateAction::WaiveRequiredChild { child, .. } if !child.trim().is_empty() => {
                self.service
                    .resolve_work_reference(child, now)
                    .map_err(|error| VerbError::at(error, child))?;
                None
            }
            _ => None,
        };
        let (core, line) = Self::update_translation(
            input.action,
            &work_ref,
            &title,
            !view.status.work.acceptance_bindings.is_empty(),
        )?;
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_update_on(Some(&target), core, now)
            .map_err(|error| {
                if let Some((prerequisite_id, prerequisite_ref)) = &prerequisite_target
                    && match &error {
                        StoreError::WorkNotOpen(closed)
                        | StoreError::WorkPrerequisiteAlreadySatisfied(closed)
                        | StoreError::WorkNotFound(closed) => closed == prerequisite_id,
                        _ => false,
                    }
                {
                    return VerbError::at(error, prerequisite_ref);
                }
                VerbError::at(error, &work_ref)
            })?;
        // What an unblock cleared is read from its committed clear, the one
        // that moved the item to the receipt's revision, so a replayed
        // answer names it as the first did, and a peer's change between this
        // word's read and the clear cannot make it name another blocker.
        let cleared_blocker = if result.operation == "unblock" {
            // The clear's own revision is its core result's; the receipt's
            // outer revision is the item's when the answer was shaped, which
            // a later change may already have moved on.
            let cleared_at = result
                .receipt
                .result
                .get("revision")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(result.receipt.revision);
            self.service
                .blocker_cleared_at(result.receipt.work_id, cleared_at, now)
                .map_err(|error| VerbError::at(error, &work_ref))?
        } else {
            None
        };
        let after = if result.operation == "detach" {
            self.target(Some(&result.receipt.work_ref), now)?
        } else {
            self.refreshed(&view, now)?
        };
        let line = if result.operation == "detach" {
            format!("{line} as independent root {}", after.status.work.short_ref)
        } else if result.operation == "reject" {
            let parent = result.receipt.result["parent_ref"]
                .as_str()
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "rejection receipt is missing its parent reference".into(),
                    )
                })?;
            format!("{line}; cancelled child and recorded required-child waiver on {parent}")
        } else if result.operation == "release"
            && result.receipt.result["waiver_recorded"].as_bool() == Some(true)
        {
            // A second, permanent statement on the root: say it was made.
            format!(
                "{line}; your reason is recorded as the waiver of this session's missing contribution{}",
                held_suffix(self.holder(&after, now), now)
            )
        } else if let Some(blocker) = &cleared_blocker {
            format!(
                "{line}: cleared {}; {} active blocker(s) remain{}",
                cleared_blocker_words(blocker),
                after.blocker_count.max(after.blockers.len()),
                held_suffix(self.holder(&after, now), now)
            )
        } else {
            format!("{line}{}", held_suffix(self.holder(&after, now), now))
        };
        let guidance = self.guidance(&after, "update", now);
        let mut value = serde_json::to_value(&result)?;
        if let Some(blocker) = &cleared_blocker {
            value["cleared_blocker"] = serde_json::Value::String(
                crate::work_service::blocker_selector::encode(&blocker.blocker_id),
            );
            value["blockers_remaining"] =
                serde_json::json!(after.blocker_count.max(after.blockers.len()));
        }
        let receipt = self.finish_mutation(Receipt::assemble(vec![line], guidance, value, false));
        // Memories naming this item as their retiring target are surfaced
        // when it leaves open work other than by completion: a cancel or a
        // rejection cancels it, a supersede or a detach replaces it.
        // The replacement is read from the recorded result, never from the
        // caller's input: a supersede records it as `superseded_by`, and a
        // detach's receipt names the new root.
        let action = match result.operation.as_str() {
            "cancel" | "reject" => {
                Some(super::super::memory_retirement::RetirementAction::Cancelled)
            }
            "supersede" => result
                .receipt
                .result
                .get("superseded_by")
                .cloned()
                .and_then(|value| serde_json::from_value::<crate::domain::WorkId>(value).ok())
                .map(
                    |replacement| super::super::memory_retirement::RetirementAction::Superseded {
                        replacement: self.command_work_ref(replacement, now),
                    },
                ),
            "detach" => Some(
                super::super::memory_retirement::RetirementAction::Superseded {
                    replacement: self.command_work_ref(result.receipt.work_id, now),
                },
            ),
            _ => None,
        };
        let Some(action) = action else {
            return Ok(receipt);
        };
        let candidates = self
            .service
            .project_memory_retirement_candidates(view.status.work.work_id, now);
        super::super::memory_retirement::append(
            &receipt,
            &candidates,
            &action,
            super::super::MAX_AGENT_WORK_RESPONSE_BYTES,
        )
    }

    /// The ref a suggested command should use for `work_id`: its short ref
    /// while that still names exactly this item, and otherwise the work id,
    /// which a short ref shared by several items would make ambiguous.
    fn command_work_ref(&self, work_id: crate::domain::WorkId, now: DateTime<Utc>) -> String {
        let full = work_id.0.to_string();
        let Ok(item) = self.service.resolve_work_reference(&full, now) else {
            return full;
        };
        match self.service.resolve_work_reference(&item.short_ref, now) {
            Ok(resolved) if resolved.work_id == work_id => item.short_ref,
            _ => full,
        }
    }

    /// Maps one flat `update` action onto the typed core update and the
    /// receipt line the shell prints.
    #[allow(
        clippy::too_many_lines,
        reason = "the flat update actions stay together so the agent-to-core mapping remains reviewable"
    )]
    fn update_translation(
        action: UpdateAction,
        work_ref: &str,
        title: &str,
        has_bindings: bool,
    ) -> Result<(WorkUpdateInput, String), VerbError> {
        Ok(match action {
            UpdateAction::Release { reason } => {
                // An explicit reason also waives a missing contribution. The
                // default never does: a waiver is text its holder wrote.
                let explicit = reason
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty());
                (
                    WorkUpdateInput::Release {
                        reason: explicit.clone().unwrap_or_else(|| "released".into()),
                        waiver_reason: explicit,
                        idempotency_key: String::new(),
                    },
                    format!("released {work_ref} \"{title}\""),
                )
            }
            UpdateAction::Reject { reason } => {
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the child is rejected".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Reject {
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("rejected {work_ref} \"{title}\": {}", short(&reason)),
                )
            }
            UpdateAction::Blocked { detail } => {
                let detail = detail.trim().to_owned();
                if detail.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the item is blocked".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Block {
                        blocker_kind: WorkBlockerKind::Manual,
                        detail: detail.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("blocked {work_ref} \"{title}\": {}", short(&detail)),
                )
            }
            UpdateAction::Unblock { blocker } => {
                // A selector is checked before anything is attempted: only
                // the exact spelling show prints names a blocker.
                let blocker_id = blocker
                    .map(|selector| {
                        if selector.trim().is_empty() {
                            return Err(StoreError::InvalidWork(
                                "the blocker selector is empty; omit it to clear the item's only blocker"
                                    .into(),
                            ));
                        }
                        crate::work_service::blocker_selector::decode(&selector).ok_or_else(|| {
                            StoreError::InvalidWork(
                                "that is not a blocker selector; use one exactly as show prints it"
                                    .into(),
                            )
                        })
                    })
                    .transpose()?;
                (
                    WorkUpdateInput::Unblock {
                        blocker_id,
                        idempotency_key: String::new(),
                    },
                    format!("unblocked {work_ref} \"{title}\""),
                )
            }
            UpdateAction::Revise {
                title: new_title,
                external,
                clear_external,
                outcome,
                acceptance,
                bindings,
                assignee,
                priority,
                defer,
                kind,
                labels,
                unlabels,
            } => {
                let add_labels = trimmed(&labels);
                let remove_labels = trimmed(&unlabels);
                let acceptance_bindings = bindings.as_deref().map(parse_bindings).transpose()?;
                let patch = WorkRevisionPatch {
                    acceptance_bindings,
                    external_ref: external,
                    clear_external,
                    title: nonempty(new_title),
                    outcome: nonempty(outcome),
                    acceptance,
                    kind,
                    priority: validate_priority(priority)?,
                    labels: None,
                    add_labels,
                    remove_labels,
                    assigned_to: nonempty(assignee),
                    clear_assignment: false,
                    deferred_until: defer,
                    clear_deferral: false,
                    evaluation_mode: None,
                    clear_evaluation_mode: false,
                };
                let mut fields = Vec::new();
                if patch.external_ref.is_some() || patch.clear_external {
                    fields.push("external reference");
                }
                if patch.title.is_some() {
                    fields.push("title");
                }
                if patch.outcome.is_some() {
                    fields.push("outcome");
                }
                if patch.acceptance.is_some() {
                    fields.push("acceptance");
                }
                if patch.acceptance_bindings.is_some() {
                    fields.push("verification bindings");
                }
                if patch.assigned_to.is_some() {
                    fields.push("assignee");
                }
                if patch.priority.is_some() {
                    fields.push("priority");
                }
                if patch.kind.is_some() {
                    fields.push("kind");
                }
                if !patch.add_labels.is_empty() || !patch.remove_labels.is_empty() {
                    fields.push("labels");
                }
                if patch.deferred_until.is_some() {
                    fields.push("deferral");
                }
                if fields.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "update needs one action: --release, --blocked, --unblock, --cancel, or a field to change"
                            .into(),
                    )
                    .into());
                }
                // Positions name the list that was replaced, so bindings the
                // revision did not restate are gone; say so where the agent
                // reads the receipt.
                let cleared_bindings = patch.acceptance.is_some()
                    && patch.acceptance_bindings.is_none()
                    && has_bindings;
                let mut text = format!("updated {work_ref} \"{title}\" ({})", fields.join(", "));
                if cleared_bindings {
                    text.push_str(
                        "; the replaced acceptance list drops its verification bindings, pass --bind again if they still apply",
                    );
                }
                (
                    WorkUpdateInput::Revise {
                        patch,
                        idempotency_key: String::new(),
                    },
                    text,
                )
            }
            UpdateAction::EvaluationMode { mode } => {
                // Only the explicit clear (no mode supplied) clears the pin; a
                // supplied blank or unknown word refuses before any effect.
                let selected = parse_supplied_evaluation_mode(mode.as_deref())?;
                let patch = WorkRevisionPatch {
                    acceptance_bindings: None,
                    external_ref: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: None,
                    kind: None,
                    priority: None,
                    labels: None,
                    add_labels: Vec::new(),
                    remove_labels: Vec::new(),
                    assigned_to: None,
                    clear_assignment: false,
                    deferred_until: None,
                    clear_deferral: false,
                    evaluation_mode: selected,
                    clear_evaluation_mode: selected.is_none(),
                };
                let text = match selected {
                    Some(mode) => format!(
                        "pinned evaluation mode {} on {work_ref} \"{title}\"",
                        mode.word()
                    ),
                    None => format!("cleared evaluation mode on {work_ref} \"{title}\""),
                };
                (
                    WorkUpdateInput::Revise {
                        patch,
                        idempotency_key: String::new(),
                    },
                    text,
                )
            }
            UpdateAction::Cancel { reason } => {
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the item is cancelled".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Cancel {
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("cancelled {work_ref} \"{title}\": {}", short(&reason)),
                )
            }
            UpdateAction::After { prerequisite } => {
                let prerequisite = prerequisite.trim().to_owned();
                if prerequisite.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "adding a prerequisite needs the prerequisite item ref".into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::AddPrerequisite {
                        prerequisite: prerequisite.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("made {work_ref} \"{title}\" wait for {prerequisite}"),
                )
            }
            UpdateAction::DropAfter { prerequisite } => {
                let prerequisite = prerequisite.trim().to_owned();
                if prerequisite.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "removing a prerequisite needs the prerequisite item ref".into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::RemovePrerequisite {
                        prerequisite: prerequisite.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("removed {prerequisite} as a prerequisite of {work_ref} \"{title}\""),
                )
            }
            UpdateAction::WaiveRequiredChild { child, reason } => {
                let child = child.trim().to_owned();
                let reason = reason.trim().to_owned();
                if child.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "a required-child waiver needs the child item ref".into(),
                    )
                    .into());
                }
                if reason.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "a required-child waiver needs a reason".into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::WaiveRequiredChild {
                        child: child.clone(),
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!(
                        "waived required child {child} for {work_ref} \"{title}\": {}",
                        short(&reason)
                    ),
                )
            }
            UpdateAction::Detach { reason } => {
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(StoreError::InvalidWork("detach needs a reason".into()).into());
                }
                (
                    WorkUpdateInput::Detach {
                        reason,
                        idempotency_key: String::new(),
                    },
                    format!("detached {work_ref} \"{title}\""),
                )
            }
            UpdateAction::Supersede {
                replacement,
                reason,
            } => {
                let replacement = replacement.trim().to_owned();
                let reason = reason.trim().to_owned();
                if replacement.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "a supersession needs the replacement item ref".into(),
                    )
                    .into());
                }
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("a supersession needs a reason".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Supersede {
                        replacement: replacement.clone(),
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!(
                        "superseded {work_ref} \"{title}\" with {replacement}: {}",
                        short(&reason)
                    ),
                )
            }
        })
    }
}
