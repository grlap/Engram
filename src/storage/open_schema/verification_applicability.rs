//! The declared schema barrier preventing older readers from ignoring restrictions.

use super::{Connection, Path, SchemaDurability, SchemaOwner, SqliteStore, StoreError};
use crate::storage::{
    VERIFICATION_APPLICABILITY_DDL, VERIFICATION_APPLICABILITY_INDEX,
    current_schema_definition_issue, current_schema_reference, normalized_schema_definition,
    stored_schema_definition,
};

impl SqliteStore {
    /// Whether this connection holds the exact current applicability barrier.
    ///
    /// # Errors
    /// Returns schema inspection errors; a wrong definition never advertises support.
    pub fn verification_git_tree_applicability_available(&self) -> Result<bool, StoreError> {
        Self::verification_applicability_available_on(&self.connection)
    }

    pub(in crate::storage) fn verification_applicability_available_on(
        connection: &Connection,
    ) -> Result<bool, StoreError> {
        let reference = current_schema_reference()?;
        let expected = reference
            .iter()
            .find(|d| d.name == VERIFICATION_APPLICABILITY_INDEX);
        let actual = stored_schema_definition(connection, VERIFICATION_APPLICABILITY_INDEX)?;
        Ok(expected
            .zip(actual)
            .is_some_and(|(expected, (kind, name, sql))| {
                expected.object_type == kind
                    && expected.name == name
                    && expected.sql == normalized_schema_definition(&sql)
            }))
    }

    pub(in crate::storage) fn require_verification_applicability_on(
        connection: &Connection,
    ) -> Result<(), StoreError> {
        if !Self::verification_applicability_available_on(connection)? {
            return Err(StoreError::InvalidControlProjection(
                "verification applicability requires objects_verification_git_tree_applicability_v1".into(),
            ));
        }
        Ok(())
    }

    /// Establish only the verification barrier, after older consumers are gone.
    ///
    /// # Errors
    /// Conflicting schemas and incompatible verification bodies roll back the step.
    pub fn build_verification_applicability_index(path: &Path) -> Result<usize, StoreError> {
        let connection = Self::open_projection_maintenance(path, false)?;
        let transaction = rusqlite::Transaction::new_unchecked(
            &connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        Self::preflight_projection_maintenance(&transaction)?;
        if !Self::verification_applicability_available_on(&transaction)? {
            // No IF NOT EXISTS: a conflicting same-named object is a refusal.
            transaction.execute_batch(VERIFICATION_APPLICABILITY_DDL)?;
        }
        let checked = super::super::doctor::census_verification_evidence_on(&transaction)?;
        for owner in [SchemaOwner::Core, SchemaOwner::Work] {
            for durability in [SchemaDurability::Durable, SchemaDurability::Rebuildable] {
                if let Some(issue) =
                    current_schema_definition_issue(&transaction, owner, durability)?
                {
                    return Err(StoreError::InvalidControlProjection(issue));
                }
            }
        }
        transaction.commit()?;
        Ok(checked)
    }
}
