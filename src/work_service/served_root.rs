//! The named root a completed item's claim used, read after completion so
//! the receipt can tell the agent that workspace has served the item.

use super::{DateTime, LocalWorkService, ObjectId, StoreError, Utc};
use crate::storage::ServedNamedRoot;

#[cfg(test)]
thread_local! {
    /// Makes this thread's next read fail, as an unreadable store would, so a
    /// test can show the failure never reaches the completion.
    pub(crate) static FAIL_NEXT_SERVED_ROOT_READ: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

impl LocalWorkService {
    /// The named root `seal`'s claim had bound when it completed, if any, and
    /// whether another live claim of the same session still names it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store or the seal's records cannot be
    /// read.
    pub(crate) fn served_named_root(
        &self,
        seal: &ObjectId,
        now: DateTime<Utc>,
    ) -> Result<Option<ServedNamedRoot>, StoreError> {
        #[cfg(test)]
        if FAIL_NEXT_SERVED_ROOT_READ.with(|fail| fail.replace(false)) {
            return Err(StoreError::InvalidWorkProjection(
                "injected served-root read failure".into(),
            ));
        }
        self.read_store_at(now)?.served_named_root(seal, now)
    }
}
