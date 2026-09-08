use super::{
    AgentNextOptions, DateTime, FeedId, LocalWorkService, MAX_CHANGE_SECTION_BYTES,
    ProjectMemorySignal, SqliteStore, StoreError, Utc, WorkChange, WorkNextPeek, WorkNextQuery,
    WorkNextSection, WorkNextView, agent_work_session, selected_work_next_sections,
    verified_bounded_work_changes, work_delivery_boundary,
};

// A bounded local scan, not a second persisted cursor or delivery stream.
const MAX_PEEK_PAGES: usize = 8;

impl LocalWorkService {
    /// Projects orientation without registering a session, staging a page, or
    /// acknowledging work or memories. The predicate belongs to the outer
    /// renderer and only decides whether a bounded page has visible rows.
    pub(crate) fn work_next_peek_for_agent(
        &self,
        limit: u32,
        list_limit: u32,
        verbose: bool,
        query: WorkNextQuery,
        now: DateTime<Utc>,
        show_page: impl Fn(&[WorkChange]) -> bool,
    ) -> Result<WorkNextView, StoreError> {
        crate::storage::validate_context_generation(query.context_generation.as_deref())?;
        self.validate_read_attribution(now)?;
        let store = SqliteStore::open_existing_read_only(&self.database)?;
        store.work_read_snapshot(|store| {
            let mut omissions = Vec::new();
            let advisory = self.next_advisory(
                store,
                limit,
                &query,
                now,
                Some(AgentNextOptions {
                    list_limit,
                    verbose,
                }),
                &mut omissions,
            )?;
            #[cfg(test)]
            if let Some(hook) = &self.advisory_read_hook {
                // The same test barrier as ordinary next, now after the peek
                // snapshot has been pinned and before memories/change reads.
                hook.entered.wait();
                hook.release.wait();
            }
            let session = store.work_session_state(&self.project_id, &self.session_id, now)?;
            let sections = selected_work_next_sections(&query.sections);
            let wants_changes = sections.contains(&WorkNextSection::Changes);
            let memories = if sections.contains(&WorkNextSection::Memories) {
                let advertisement = store.project_memory_advertisement_candidate(
                    &self.project_id,
                    &self.session_id,
                    query.context_generation.as_deref(),
                )?;
                Some(ProjectMemorySignal {
                    count: advertisement.count,
                    changed: advertisement.changed,
                })
            } else {
                None
            };
            let feed = FeedId::Project(self.project_id.clone());
            // A pending page is NOT implicitly delivered by looking. Start at
            // the confirmed cut and reproject using this read's authorization.
            let mut position = session.project_cursor;
            let mut changes = Vec::new();
            if wants_changes {
                let (root, task) = work_delivery_boundary(
                    store,
                    &self.project_id,
                    &self.session_id,
                    session.focused_work_id,
                )?;
                for _ in 0..if verbose { 1 } else { MAX_PEEK_PAGES } {
                    let entries = store.work_feed_after(&feed, position, limit)?;
                    changes = verified_bounded_work_changes(
                        store,
                        &self.project_id,
                        &self.session_id,
                        root,
                        task,
                        entries,
                        position,
                        MAX_CHANGE_SECTION_BYTES,
                    )?;
                    let Some(last) = changes.last() else { break };
                    position = last.entry.position.position;
                    if verbose
                        || show_page(&changes)
                        || position >= advisory.read_cut.project_position
                    {
                        break;
                    }
                }
            }
            Ok(WorkNextView {
                peek: Some(WorkNextPeek {
                    delivery_advanced: false,
                    more_changes_available: wants_changes
                        && position < advisory.read_cut.project_position,
                }),
                build_fingerprint: crate::build_identity::current().build_fingerprint.clone(),
                read_cut: advisory.read_cut,
                context_generation: query.context_generation,
                session: agent_work_session(&session),
                discovery: advisory.discovery,
                agent_lists: advisory.agent_lists,
                focus: advisory.focus,
                ready: advisory.ready,
                catalog: advisory.catalog,
                changes: wants_changes.then_some(changes),
                memories,
                delivered_through: None,
                delivery_token: None,
                omissions,
                // There must be no capability for the outer renderer to ack.
                memory_advertisement: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn peek_scan_cap_matches_advancing_next() {
        assert_eq!(super::MAX_PEEK_PAGES, crate::verbs::MAX_NEXT_PAGES);
    }
}
