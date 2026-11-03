//! Work creation and child proposal word handlers.

use super::{
    AddInput, AgentVerbs, ChildRequirement, DateTime, Holder, Receipt, StoreError, Utc, VerbError,
    WorkChildInput, WorkProposeInput, WorkProposeResult, json, parse_bindings,
    parse_supplied_evaluation_mode, short, slug, trimmed, validate_priority, wording,
};

impl AgentVerbs {
    /// `add`: a root, or one required/optional child beneath `under`.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when input is empty or the core refuses admission.
    pub fn add(&self, mut input: AddInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        input.notes = crate::domain::normalize_initial_work_notes(&input.notes)
            .map_err(StoreError::InvalidWork)?;
        // `add --under` focuses the parent before proposing, so a note over
        // the size bound is refused here, with the text the append uses.
        for note in &input.notes {
            crate::storage::normalize_note_text(
                note,
                crate::storage::refusal_labels::NOTE_SUMMARY,
            )?;
        }
        if input
            .acceptance
            .iter()
            .any(|criterion| criterion.trim().is_empty())
        {
            return Err(
                StoreError::InvalidWork("acceptance criteria must not be blank".into()).into(),
            );
        }
        let reminder = input.acceptance.is_empty().then(|| {
            wording::DEFAULTED_ACCEPTANCE_REMINDER
                .spelled(self.argument_names)
                .to_owned()
        });
        let has_initial_notes = !input.notes.is_empty();
        // The new item becomes the focus; the receipt names that move from
        // this call's own journal, whatever other calls of the session do.
        let mut receipt =
            self.disclosing_focus(|| Ok(self.finish_mutation(self.add_inner(input, now)?)))?;
        if has_initial_notes {
            receipt = receipt.with_reminder(
                "initial observations (no execution credit) recorded at creation".into(),
            )?;
        }
        match reminder {
            Some(text) => receipt.with_reminder(text),
            None => Ok(receipt),
        }
    }

    fn add_inner(&self, input: AddInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let title =
            crate::storage::normalize_text(&input.title, crate::storage::refusal_labels::TITLE)?;
        let outcome = input
            .outcome
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| title.clone());
        let mut acceptance = input
            .acceptance
            .iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if acceptance.is_empty() {
            acceptance.push(format!("{title} is done"));
        }
        let priority = validate_priority(input.priority)?;
        let labels = trimmed(&input.labels);
        let assigned_to = input
            .assignee
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if input.optional && input.under.is_none() {
            return Err(
                StoreError::InvalidWork(wording::OPTIONAL_NEEDS_PARENT_REFUSAL.cli.into()).into(),
            );
        }
        let evaluation_mode = parse_supplied_evaluation_mode(input.evaluation_mode.as_deref())?;
        let acceptance_bindings = parse_bindings(&input.bindings)?;
        crate::storage::normalize_acceptance(&acceptance, &acceptance_bindings)?;
        crate::domain::normalize_external_reference(input.external.as_deref())
            .map_err(StoreError::InvalidWork)?;
        if let Some(under) = input.under.as_deref() {
            let requirement = if input.optional {
                ChildRequirement::Optional
            } else {
                ChildRequirement::Required
            };
            return self.add_child(
                under,
                WorkChildInput {
                    external_ref: input.external,
                    notes: input.notes,
                    key: slug(&title),
                    title,
                    outcome,
                    acceptance,
                    acceptance_bindings,
                    requirement: Some(requirement),
                    kind: input.kind,
                    priority,
                    labels,
                    assigned_to,
                    deferred_until: None,
                    evaluation_mode,
                },
                now,
            );
        }
        let result = self.service.work_propose(
            WorkProposeInput::Root {
                notes: input.notes,
                external_ref: input.external,
                title,
                outcome,
                acceptance,
                acceptance_bindings,
                work_kind: input.kind,
                priority,
                labels,
                assigned_to,
                deferred_until: None,
                evaluation_mode,
                idempotency_key: String::new(),
            },
            now,
        )?;
        let WorkProposeResult::Root { work, focus } = &result else {
            return Err(StoreError::InvalidWorkProjection(
                "root proposal returned a decomposition receipt".into(),
            )
            .into());
        };
        let guidance = self.guidance(focus, "add", now);
        let lines = vec![format!(
            "added {} \"{}\"",
            work.short_ref,
            short(&work.title)
        )];
        super::super::mutation::receipt(
            focus,
            "add",
            json!({"kind": "root"}),
            lines,
            guidance,
            self.holder(focus, now),
            false,
        )
    }

    /// One required or optional child through `work_propose:decompose`; it becomes
    /// the focus exactly as a new root does.
    fn add_child(
        &self,
        under: &str,
        child: WorkChildInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        // A pin the proposal would refuse is refused first. The parent is
        // only inspected here: the proposal binds it after its own checks,
        // so a refused child leaves focus and the session as they were.
        self.service
            .refuse_record_id_pins(&child.acceptance_bindings, now)?;
        let parent = self
            .service
            .inspect_work(under, now)
            .map_err(|error| VerbError::at(error, under))?;
        let parent_ref = parent.status.work.short_ref.clone();
        let result = self
            .service
            .work_propose_on(
                Some(&parent_ref),
                WorkProposeInput::Decompose {
                    children: vec![child],
                    prerequisites: Vec::new(),
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &parent_ref))?;
        let WorkProposeResult::Decomposition(summary) = &result else {
            return Err(StoreError::InvalidWorkProjection(
                "decomposition returned a root receipt".into(),
            )
            .into());
        };
        let child_ref = summary
            .children
            .first()
            .map(|child| child.short_ref.clone())
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection("decomposition created no child".into())
            })?;
        let focus = self
            .service
            .work_focus(&child_ref, now)
            .map_err(|error| VerbError::at(error, &child_ref))?;
        let peer_proposal = focus.status.work.child_requirement == ChildRequirement::Optional
            && matches!(self.holder(&parent, now), Holder::Other(..));
        let mut guidance = self.guidance(&focus, "add", now);
        if peer_proposal {
            guidance.next = vec![
                format!("engram work show {child_ref}"),
                format!("engram work show {parent_ref}"),
            ];
            guidance
                .reminders
                .retain(|reminder| reminder != "unclaimed: claim it before execution");
            let inspect = "inspect child and parent".to_owned();
            if !guidance.reminders.contains(&inspect) {
                guidance.reminders.push(inspect);
            }
        }
        let value = json!({"kind": "child", "parent_ref": parent_ref,
            "child_requirement": focus.status.work.child_requirement,
            "details_omitted": summary.details_omitted});
        let requirement = if focus.status.work.child_requirement == ChildRequirement::Optional {
            " optional"
        } else {
            ""
        };
        let lines = vec![format!(
            "added{requirement} {child_ref} \"{}\" under {parent_ref} \"{}\"",
            short(&focus.status.work.title),
            short(&parent.status.work.title)
        )];
        let mut receipt = super::super::mutation::receipt(
            &focus,
            "add",
            value,
            lines,
            guidance,
            self.holder(&focus, now),
            false,
        )?;
        if peer_proposal {
            // `mutation::receipt` drops `show CHILD` in favor of full_detail;
            // a peer proposal still needs both inspect commands in `next`.
            let next = vec![
                format!("engram work show {child_ref}"),
                format!("engram work show {parent_ref}"),
            ];
            receipt.next.clone_from(&next);
            receipt.value["next"] = json!(next);
        }
        Ok(receipt)
    }
}
