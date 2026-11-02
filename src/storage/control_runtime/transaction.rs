use super::request_budget::{self, ControlRequestBudget};
use super::{Connection, StoreError, Transaction};
use rusqlite::TransactionBehavior;
use std::time::Duration;

#[cfg(test)]
pub(in crate::storage) fn control_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    control_lifecycle_transaction(connection, apply)
}

/// Ordinary host operations share the request budget and keep their generic
/// storage errors. No execution-admission state participates in this writer.
pub(in crate::storage) fn control_lifecycle_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    budgeted_transaction(connection, apply)
}

fn budgeted_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let budget = request_budget::current();
    check_budget(budget)?;
    let policy = BusyPolicy::cap(connection, budget)?;
    let result = apply_transaction(connection, apply, budget, policy.diagnostic_budget);
    policy.finish(result)
}

fn check_budget(budget: Option<ControlRequestBudget>) -> Result<(), StoreError> {
    budget.map_or(Ok(()), ControlRequestBudget::check)
}

fn apply_transaction<T>(
    connection: &Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
    budget: Option<ControlRequestBudget>,
    diagnostic_budget: crate::diagnostics::transaction::Budget,
) -> Result<T, StoreError> {
    // Reading/restoring the busy policy must not give a delayed frame a fresh
    // opportunity to enter its transaction.
    check_budget(budget)?;
    // The caller lends its connection exclusively to this wrapper. The
    // shared reborrow lets a failed BEGIN be checked for autocommit without
    // retaining a failed mutable transaction borrow.
    let mut timing = crate::storage::transaction_timing::TransactionTiming::start(
        connection,
        crate::diagnostics::transaction::Boundary::ControlMutation,
        diagnostic_budget,
    );
    let transaction = match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => {
            timing.begin_failed(&error, connection.is_autocommit());
            let error = StoreError::Sqlite(error);
            return Err(error);
        }
    };
    timing.acquired();
    if let Err(error) = check_budget(budget) {
        rollback_observed(transaction, &mut timing)?;
        return Err(error);
    }
    match apply(&transaction) {
        Ok(result) => {
            if let Err(error) = check_budget(budget) {
                rollback_observed(transaction, &mut timing)?;
                return Err(error);
            }
            // A failed COMMIT never certifies the no-application guarantee.
            // Once COMMIT starts, its late result is settled and replayed;
            // no subsequent clock check may turn it into an expiry refusal.
            let committed = transaction.commit();
            timing.committed(committed.is_ok());
            committed?;
            Ok(result)
        }
        Err(error) if is_busy(&error) || matches!(error, StoreError::ControlRequestExpired) => {
            // Drop discards rollback errors. Preserve a rollback failure as
            // a native error; only expiry has a no-application wire code.
            rollback_observed(transaction, &mut timing)?;
            Err(error)
        }
        Err(error) => Err(error),
    }
}

fn rollback_observed(
    transaction: Transaction<'_>,
    timing: &mut crate::storage::transaction_timing::TransactionTiming<'_>,
) -> Result<(), StoreError> {
    let result = transaction.rollback();
    timing.rolled_back(result.is_ok());
    result.map_err(StoreError::Sqlite)
}

/// Control hosts use an ordinary busy timeout. Cap its current configured
/// value, then restore that actual value on success, failure and unwinding.
/// Native sleeps and filesystem calls remain cooperative, not preemptible.
struct BusyPolicy<'a> {
    connection: &'a Connection,
    previous: Option<Duration>,
    diagnostic_budget: crate::diagnostics::transaction::Budget,
}

impl<'a> BusyPolicy<'a> {
    fn cap(
        connection: &'a Connection,
        budget: Option<ControlRequestBudget>,
    ) -> Result<Self, StoreError> {
        let mut guard = Self {
            connection,
            previous: None,
            diagnostic_budget: crate::diagnostics::transaction::Budget::default(),
        };
        if let Some(budget) = budget {
            let milliseconds: u32 =
                connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            let previous = Duration::from_millis(u64::from(milliseconds));
            guard.previous = Some(previous);
            let allowance = previous.min(budget.remaining());
            guard.diagnostic_budget = crate::diagnostics::transaction::Budget {
                configured_ms: Some(u64::from(milliseconds)),
                allowance_ms: Some(u64::try_from(allowance.as_millis()).unwrap_or(u64::MAX)),
                ..Default::default()
            };
            connection.busy_timeout(allowance)?;
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget_tests;
