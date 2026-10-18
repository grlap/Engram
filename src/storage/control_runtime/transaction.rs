use super::request_budget::{self, ControlRequestBudget};
use super::{Connection, StoreError, Transaction};
use rusqlite::TransactionBehavior;
use std::time::Duration;

pub(super) fn control_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    budgeted_transaction(connection, apply, true)
}

/// Other control operations retain their existing generic storage errors;
/// only the new allowance certificate is shared with turn operations.
pub(in crate::storage) fn control_lifecycle_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    budgeted_transaction(connection, apply, false)
}

fn budgeted_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    certify_busy: bool,
) -> Result<T, StoreError> {
    let budget = request_budget::current();
    check_budget(budget)?;
    let policy = BusyPolicy::cap(connection, budget)?;
    let result = apply_transaction(connection, apply, budget, certify_busy);
    policy.finish(result)
}

fn check_budget(budget: Option<ControlRequestBudget>) -> Result<(), StoreError> {
    budget.map_or(Ok(()), ControlRequestBudget::check)
}

fn apply_transaction<T>(
    connection: &Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    budget: Option<ControlRequestBudget>,
    certify_busy: bool,
) -> Result<T, StoreError> {
    // Reading/restoring the busy policy must not give a delayed frame a fresh
    // opportunity to enter its transaction.
    check_budget(budget)?;
    // The caller lends its connection exclusively to this wrapper. The
    // shared reborrow lets a failed BEGIN be checked for autocommit without
    // retaining a failed mutable transaction borrow.
    let transaction = match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => {
            let error = StoreError::Sqlite(error);
            return Err(if certify_busy && connection.is_autocommit() {
                classify_busy(error)
            } else {
                error
            });
        }
    };
    if let Err(error) = check_budget(budget) {
        transaction.rollback()?;
        return Err(error);
    }
    match apply(&transaction) {
        Ok(result) => {
            if let Err(error) = check_budget(budget) {
                transaction.rollback()?;
                return Err(error);
            }
            // A failed COMMIT never certifies the no-application guarantee.
            // Once COMMIT starts, its late result is settled and replayed;
            // no subsequent clock check may turn it into an expiry refusal.
            transaction.commit()?;
            Ok(result)
        }
        Err(error)
            if (certify_busy && is_busy(&error))
                || matches!(error, StoreError::ControlRequestExpired) =>
        {
            // Drop discards rollback errors. Observe an explicit rollback
            // before telling the host that this attempt applied nothing.
            transaction.rollback()?;
            Err(if certify_busy {
                classify_busy(error)
            } else {
                error
            })
        }
        Err(error) => Err(error),
    }
}

/// Control hosts use an ordinary busy timeout. Cap its current configured
/// value, then restore that actual value on success, failure and unwinding.
/// Native sleeps and filesystem calls remain cooperative, not preemptible.
struct BusyPolicy<'a> {
    connection: &'a Connection,
    previous: Option<Duration>,
}

impl<'a> BusyPolicy<'a> {
    fn cap(
        connection: &'a Connection,
        budget: Option<ControlRequestBudget>,
    ) -> Result<Self, StoreError> {
        let mut guard = Self {
            connection,
            previous: None,
        };
        if let Some(budget) = budget {
            let milliseconds: u32 =
                connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let previous = Duration::from_millis(u64::from(milliseconds));
            guard.previous = Some(previous);
            connection.busy_timeout(previous.min(budget.remaining()))?;
        }
        Ok(guard)
    }

    fn finish<T>(mut self, result: Result<T, StoreError>) -> Result<T, StoreError> {
        if let Some(previous) = self.previous.take() {
            self.connection.busy_timeout(previous)?;
        }
        result
    }
}

impl Drop for BusyPolicy<'_> {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            let _ = self.connection.busy_timeout(previous);
        }
    }
}

fn is_busy(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(source)
            if matches!(
                source.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
    )
}

fn classify_busy(error: StoreError) -> StoreError {
    if !is_busy(&error) {
        return error;
    }
    match error {
        StoreError::Sqlite(source) => StoreError::ControlStoreBusy {
            source: Box::new(source),
        },
        error => error,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget_tests;
