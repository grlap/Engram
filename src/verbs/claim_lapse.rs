//! The reminder a read word carries when the reader's own claim lapsed.
//!
//! A read finds it before it fits its receipt, fits its own content within
//! the budget less the reminder's cost, then puts the reminder first. The
//! reminder only states the lapse and the renewal command: a read never
//! renews, reclaims or changes any claim.

use super::{AgentVerbs, DateTime, Receipt, Utc, VerbError, json};
use crate::work_service::CLAIM_LAPSE_UNAVAILABLE;

/// Bytes the reminder can add to a receipt's JSON or terminal text: the
/// reminder itself, its quotes and separator, and a reminders header.
pub(super) fn reserve(reminder: Option<&String>) -> usize {
    reminder.map_or(0, |reminder| reminder.len() + 32)
}

/// Puts the reminder first among the receipt's reminders.
pub(super) fn prepend(mut receipt: Receipt, reminder: Option<String>) -> Receipt {
    if let Some(reminder) = reminder {
        receipt.reminders.insert(0, reminder);
        receipt.value["reminders"] = json!(receipt.reminders);
    }
    receipt
}

impl AgentVerbs {
    /// This session's own lapsed claim, as the reminder a read carries.
    pub(super) fn claim_lapse_reminder(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Option<String>, VerbError> {
        Ok(self.service.own_claim_lapse_reminder(now)?)
    }

    /// Runs a read that fits its receipt to `budget`, within the protocol
    /// budget less the reminder's cost, and puts the reminder first. The
    /// lookup and the read share one read-only connection and one snapshot.
    /// The lookup is advisory: when it fails, the read still answers, with a
    /// reminder that the check was unavailable, and a refusal of the read
    /// itself is what the caller sees.
    pub(super) fn with_claim_lapse(
        &self,
        now: DateTime<Utc>,
        budget: usize,
        read: impl FnOnce(usize) -> Result<Receipt, VerbError>,
    ) -> Result<Receipt, VerbError> {
        self.service.one_read_connection(|| {
            let reminder = self
                .claim_lapse_reminder(now)
                .unwrap_or_else(|_| Some(CLAIM_LAPSE_UNAVAILABLE.into()));
            // Every read carries the reminder. Fitted reads made room for it;
            // a full memory stored before the reserve may run past the
            // protocol ceiling by at most READ_REMINDER_RESERVE; the explicit
            // complete reads stay unbounded, as before.
            let receipt = read(budget.saturating_sub(reserve(reminder.as_ref())))?;
            Ok(prepend(receipt, reminder))
        })
    }
}
