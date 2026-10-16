use super::{Connection, StoreError, Transaction, TransactionBehavior};

pub(super) fn control_transaction<T>(
    connection: &mut Connection,
    apply: impl FnOnce(&Transaction<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    // The caller lends its connection exclusively to this wrapper. The
    // shared reborrow lets a failed BEGIN be checked for autocommit without
    // retaining a failed mutable transaction borrow.
    let transaction = match Transaction::new_unchecked(connection, TransactionBehavior::Immediate) {
        Ok(transaction) => transaction,
        Err(error) => {
            let error = StoreError::Sqlite(error);
            return Err(if connection.is_autocommit() {
                classify_busy(error)
            } else {
                error
            });
        }
    };
    match apply(&transaction) {
        Ok(result) => {
            // A failed COMMIT never certifies the no-application guarantee.
            transaction.commit()?;
            Ok(result)
        }
        Err(error) if is_busy(&error) => {
            // Drop discards rollback errors. Observe an explicit rollback
            // before telling the host that this attempt applied nothing.
            transaction.rollback()?;
            Err(classify_busy(error))
        }
        Err(error) => Err(error),
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
