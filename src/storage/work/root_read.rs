//! Materialization reuse inside an explicitly read-only advisory call graph.
//! Selection and authority are checked by their callers on every use.

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use rusqlite::{Connection, OptionalExtension, TransactionState};

use super::{query, root_state};
use crate::{
    RootExecution, RootExecutionId, WorkId,
    domain::RootExecutionRef,
    storage::{SqliteStore, StoreError},
};

#[cfg(test)]
mod tests;

struct MaterializedRoot {
    value: Rc<RootExecution>,
    address: RootExecutionRef,
}

/// Private construction confines the materializations to one read closure.
/// This is never passed into mutation or retained on the store. In particular,
/// neither a selection result nor a claim-validity Boolean is cached here.
pub(crate) struct RootReadScope<'a> {
    connection: &'a Connection,
    roots: RefCell<HashMap<RootExecutionId, Rc<MaterializedRoot>>>,
}

impl SqliteStore {
    pub(crate) fn work_root_read_snapshot<T>(
        &self,
        read: impl FnOnce(&RootReadScope<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        query::on_one_snapshot(&self.connection, |connection| {
            let scope = RootReadScope {
                connection,
                roots: RefCell::new(HashMap::new()),
            };
            scope.check_connection(&self.connection)?;
            read(&scope)
        })
    }
}

impl RootReadScope<'_> {
    pub(in crate::storage) fn check_connection(
        &self,
        connection: &Connection,
    ) -> Result<(), StoreError> {
        if !std::ptr::eq(self.connection, connection)
            || connection.is_autocommit()
            || connection.transaction_state::<&str>(None)? == TransactionState::Write
        {
            return Err(StoreError::InvalidWorkProjection(
                "root read scope requires its unchanged read transaction".into(),
            ));
        }
        Ok(())
    }

    fn projected(&self, id: RootExecutionId) -> Result<Rc<MaterializedRoot>, StoreError> {
        self.check_connection(self.connection)?;
        // This also pins a previously deferred transaction's read snapshot.
        let head: String = self.connection.query_row(
            "SELECT head_id FROM work_root_executions WHERE root_execution_id = ?1",
            [id.0.to_string()],
            |row| row.get(0),
        )?;
        if let Some(root) = self.roots.borrow().get(&id) {
            if root.address.head.as_str() != head {
                return Err(StoreError::InvalidWorkProjection(
                    "root read scope head changed".into(),
                ));
            }
            return Ok(Rc::clone(root));
        }
        let (value, address) = root_state::projected(self.connection, id)?;
        let root = Rc::new(MaterializedRoot {
            value: Rc::new(value),
            address,
        });
        self.roots.borrow_mut().insert(id, Rc::clone(&root));
        Ok(root)
    }

    pub(in crate::storage) fn current(
        &self,
        id: RootExecutionId,
    ) -> Result<Rc<RootExecution>, StoreError> {
        let root = self.projected(id)?;
        query::verify_root_execution_reference_on(self.connection, &root.address)?;
        Ok(Rc::clone(&root.value))
    }

    pub(in crate::storage) fn active_optional(
        &self,
        root_id: WorkId,
    ) -> Result<Option<Rc<RootExecution>>, StoreError> {
        self.check_connection(self.connection)?;
        let id: Option<String> = self.connection.query_row(
            "SELECT root_execution_id FROM work_root_executions WHERE root_id = ?1 AND state = 'active'",
            [root_id.0.to_string()], |row| row.get(0),
        ).optional()?;
        let Some(id) = id else {
            return Ok(None);
        };
        let root = self.projected(query::parse_root_execution_id(&id)?)?;
        if root.value.root_id != root_id {
            return Err(StoreError::InvalidWorkProjection(
                "active root execution differs from its root binding".into(),
            ));
        }
        query::verify_root_execution_reference_on(self.connection, &root.address)?;
        Ok(Some(Rc::clone(&root.value)))
    }

    pub(in crate::storage) fn active(
        &self,
        root_id: WorkId,
    ) -> Result<Rc<RootExecution>, StoreError> {
        self.active_optional(root_id)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "root work {root_id:?} has no active execution"
            ))
        })
    }

    pub(in crate::storage) fn retained(
        &self,
        id: RootExecutionId,
    ) -> Result<Rc<RootExecution>, StoreError> {
        let root = self.projected(id)?;
        query::verify_retained_root_execution_reference_on(
            self.connection,
            &root.value,
            &root.address,
        )?;
        Ok(Rc::clone(&root.value))
    }
}
