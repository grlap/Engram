//! `update` word handler: translates the flat agent-facing update actions
//! into the typed core update request and its receipt line.

use super::{
    AgentVerbs, DateTime, Receipt, StoreError, UpdateAction, UpdateInput, Utc, VerbError,
    WorkBlockerKind, WorkRevisionPatch, WorkUpdateInput, held_suffix, nonempty, parse_bindings,
    parse_supplied_evaluation_mode, short, trimmed, validate_priority,
};

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
        } else {
            format!("{line}{}", held_suffix(self.holder(&after, now), now))
        };
        let guidance = self.guidance(&after, "update", now);
        Ok(self.finish_mutation(Receipt::assemble(
            vec![line],
            guidance,
            serde_json::to_value(&result)?,
            false,
        )))
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
            UpdateAction::Release { reason } => (
                WorkUpdateInput::Release {
                    reason: reason
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "released".into()),
                    waiver_reason: None,
                    idempotency_key: String::new(),
                },
                format!("released {work_ref} \"{title}\""),
            ),
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
            UpdateAction::Unblock => (
                WorkUpdateInput::Unblock {
                    blocker_id: None,
                    idempotency_key: String::new(),
                },
                format!("unblocked {work_ref} \"{title}\""),
            ),
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
