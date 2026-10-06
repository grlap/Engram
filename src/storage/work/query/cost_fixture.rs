use super::super::planning::expect_root_contributor;
use super::*;

impl SqliteStore {
    #[cfg(test)]
    pub(crate) fn add_expected_root_contributor_fixture(
        &mut self,
        work_id: WorkId,
        participant: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.add_root_cost_fixture(work_id, std::slice::from_ref(participant), now)
    }

    #[cfg(test)]
    pub(crate) fn add_root_cost_fixture(
        &mut self,
        work_id: WorkId,
        participants: &[SessionId],
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, work_id)?;
        let mut event = latest_canonical_work_event_for_item(&transaction, work_id)?;
        if matches!(event.transition, WorkTransition::RequiredChildWaived { .. }) {
            return Err(StoreError::InvalidWorkProjection(
                "root cost fixture cannot duplicate a required-child waiver event".into(),
            ));
        }
        let run = active_run_snapshot(&transaction, &item)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection("fixture work has no active run".into())
        })?;
        let root_execution =
            super::super::root_state::update(&transaction, run.root_execution_id, |root| {
                let mut changed = participants.is_empty();
                for participant in participants {
                    changed |= expect_root_contributor(root, participant);
                }
                if changed {
                    root.revision += 1;
                    root.updated_at = now;
                }
            })?;
        event.created_at = now;
        let draft = WorkEventDraft::with_root_state(&event, Some(root_execution.value().clone()));
        super::super::feeds::append_work_event_with_root(&transaction, &draft, &root_execution)?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn add_root_benchmark_fixture(
        &mut self,
        work_id: WorkId,
        target_members: usize,
        target_bytes: usize,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, work_id)?;
        let mut event = latest_canonical_work_event_for_item(&transaction, work_id)?;
        if matches!(event.transition, WorkTransition::RequiredChildWaived { .. }) {
            return Err(StoreError::InvalidWorkProjection(
                "benchmark fixture cannot duplicate a required-child waiver event".into(),
            ));
        }
        let run = active_run_snapshot(&transaction, &item)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection("fixture work has no active run".into())
        })?;
        let root_execution =
            super::super::root_state::try_update(&transaction, run.root_execution_id, |root| {
                let members = root.run_ids.len()
                    + root.required_child_seals.len()
                    + root.required_child_waivers.len()
                    + root.expected_contributors.len()
                    + root.contributions.len()
                    + root.waivers.len();
                let remaining = target_members.checked_sub(members).ok_or_else(|| {
                    StoreError::InvalidWork("benchmark root already exceeds target".into())
                })?;
                for index in 0..remaining.div_ceil(2) {
                    let participant = SessionId(format!("benchmark-{index:08}"));
                    participant
                        .validate_admitted()
                        .expect("bounded fixture identity");
                    assert!(expect_root_contributor(root, &participant));
                    if index < remaining / 2 {
                        assert!(super::super::planning::waive_root_contributor(
                            root,
                            &participant,
                            "agent",
                            "Synthetic participant omission",
                        ));
                    }
                }
                root.revision += 1;
                root.updated_at = now;
                let existing_bytes = CanonicalObject::freeze(root)?.bytes().len();
                let padding = target_bytes.checked_sub(existing_bytes).ok_or_else(|| {
                    StoreError::InvalidWork("benchmark root already exceeds byte target".into())
                })?;
                let added_waivers = remaining / 2;
                if added_waivers == 0 {
                    return Err(StoreError::InvalidWork(
                        "benchmark needs waiver members".into(),
                    ));
                }
                // Fill attributed, printable reasons rather than creating invalid
                // oversized session identities. ASCII has an exact JCS byte cost.
                for (index, waiver) in root
                    .waivers
                    .iter_mut()
                    .filter(|waiver| waiver.participant.0.starts_with("benchmark-"))
                    .enumerate()
                {
                    waiver.reason.push_str(&"x".repeat(
                        padding / added_waivers + usize::from(index < padding % added_waivers),
                    ));
                }
                assert_eq!(CanonicalObject::freeze(root)?.bytes().len(), target_bytes);
                Ok(())
            })?;
        event.created_at = now;
        let draft = WorkEventDraft::with_root_state(&event, Some(root_execution.value().clone()));
        super::super::feeds::append_work_event_with_root(&transaction, &draft, &root_execution)?;
        transaction.commit()?;
        Ok(())
    }
}
