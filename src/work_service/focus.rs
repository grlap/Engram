use super::{
    DateTime, LocalWorkService, MAX_AGENT_WORK_RESPONSE_BYTES, SessionId, StoreError, Utc,
    WorkFocusView, WorkId, WorkItem,
};

impl LocalWorkService {
    pub(crate) fn work_notes(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<crate::storage::WorkNotePage, StoreError> {
        let store = self.store_at(now)?;
        let item = store.resolve_work_ref(&self.project_id, work_ref)?;
        let mut page = store.work_notes(
            &self.project_id,
            item.work_id,
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )?;
        super::projection::project_full_notes(&mut page)?;
        Ok(page)
    }

    /// Makes `work_ref` the session's ambient focus without inspecting it, so a
    /// mutation can name its target in the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or outside the project.
    pub fn select_work(&self, work_ref: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        let mut store = self.store_at(now)?;
        self.bind_target(&mut store, Some(work_ref), now)?;
        Ok(())
    }

    /// The work this session holds under a live claim, with expiry, read from
    /// the claim projection without building any focus view.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store cannot be read.
    pub fn held_work(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, DateTime<Utc>)>, StoreError> {
        let store = self.store_at(now)?;
        store.work_held_by(&self.session_id, now)
    }

    /// Every live claim in this project, used only to annotate compact agent
    /// catalog rows without constructing one focus packet per item.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the live-claim projection is invalid.
    pub fn live_work_claims(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, SessionId, DateTime<Utc>)>, StoreError> {
        let store = self.store_at(now)?;
        store.live_work_claims(&self.project_id, now)
    }

    /// Inspects work by reference without changing ambient focus or staging
    /// any delivery.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or projections are invalid.
    pub fn inspect_work(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let store = self.store_at(now)?;
        let work = store.resolve_work_ref(&self.project_id, work_ref)?;
        self.focus_view(&store, work.work_id, false, true, now)
    }

    /// Resolves one work reference without projecting or changing ambient
    /// focus. Agent translations use this only to attribute core refusals.
    pub(crate) fn resolve_work_reference(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkItem, StoreError> {
        self.store_at(now)?
            .resolve_work_ref(&self.project_id, work_ref)
    }

    /// Selects and inspects ambient work without implicitly changing its claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or projections are invalid.
    pub fn work_focus(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let mut store = self.store_at(now)?;
        let item = store.resolve_work_ref(&self.project_id, work_ref)?;
        store.focus_work_session(&self.project_id, &self.session_id, item.work_id, now)?;
        self.focus_view(&store, item.work_id, true, true, now)
    }
}

#[cfg(test)]
mod tests;
