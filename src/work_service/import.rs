//! Operator intake uses the same asserted attribution and canonical work store.

use super::{DateTime, LocalWorkService, SqliteStore, StoreError, Utc};
use crate::domain::{
    WorkImportInput, WorkImportPreview, WorkImportReceipt, WorkSourceDetail, WorkSourceKey,
};
use crate::{DevelopmentNoopRedactor, ObjectHash};

/// Maximum UTF-8 JSON input size for both direct parsing and the file reader.
pub const MAX_WORK_IMPORT_INPUT_BYTES: usize = 1024 * 1024;

/// Parses bounded intake JSON without silently accepting duplicate members.
///
/// # Errors
/// Returns an input refusal for duplicate/unknown fields or an invalid shape.
pub fn parse_work_import_input(bytes: &[u8]) -> Result<WorkImportInput, StoreError> {
    if bytes.len() > MAX_WORK_IMPORT_INPUT_BYTES {
        return Err(StoreError::InvalidWork(format!(
            "import input exceeds the {MAX_WORK_IMPORT_INPUT_BYTES}-byte limit"
        )));
    }
    let value = crate::graph_snapshot::json_input::parse(bytes).map_err(|error| match error {
        StoreError::InvalidGraphSnapshot(reason) => StoreError::InvalidWork(reason),
        other => other,
    })?;
    let input: WorkImportInput = serde_json::from_value(value.clone())?;
    let typed = serde_json::to_value(&input)?;
    for path in ["/snapshot", "/snapshot/projected"] {
        if let Some(fields) = value.pointer(path).and_then(serde_json::Value::as_object) {
            let expected = typed
                .pointer(path)
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| {
                    StoreError::InvalidWork("invalid source snapshot input shape".into())
                })?;
            if fields.keys().any(|name| !expected.contains_key(name)) {
                return Err(StoreError::InvalidWork(format!(
                    "unknown field in import {path}; preserve extension data under snapshot.raw"
                )));
            }
        }
    }
    Ok(input)
}

impl LocalWorkService {
    /// Previews file intake on an established, physically read-only connection.
    ///
    /// # Errors
    /// Returns the store or input refusal without creating or repairing a store.
    pub fn preview_work_import(
        &self,
        input: &WorkImportInput,
        now: DateTime<Utc>,
    ) -> Result<WorkImportPreview, StoreError> {
        self.validate_read_attribution(now)?;
        let store = SqliteStore::open_existing_read_only(&self.database)?;
        store.preview_work_import(&self.project_id, input, now)
    }

    /// Resolves exact source identity without selecting local focus.
    ///
    /// # Errors
    /// Returns a refusal when the source binding is ambiguous or invalid.
    pub fn lookup_work_source(
        &self,
        key: &WorkSourceKey,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkSourceDetail>, StoreError> {
        self.validate_read_attribution(now)?;
        let store = SqliteStore::open_existing_read_only(&self.database)?;
        store.work_source_detail(&self.project_id, key)
    }

    /// Commits a previewed creation or source-change notification, never a patch.
    ///
    /// # Errors
    /// Returns a refusal on changed preview basis, invalid input or store failure.
    pub fn apply_work_import(
        &self,
        input: &WorkImportInput,
        token: &ObjectHash,
        now: DateTime<Utc>,
    ) -> Result<WorkImportReceipt, StoreError> {
        self.store_at(now)?.apply_work_import(
            &self.project_id,
            input,
            token,
            &self.actor(
                "import",
                "explicit source snapshot intake; local revisions remain authored",
            ),
            now,
            &DevelopmentNoopRedactor,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_parser_refuses_input_above_one_mib_before_json_parsing() {
        let oversized = vec![b' '; MAX_WORK_IMPORT_INPUT_BYTES + 1];
        assert!(matches!(parse_work_import_input(&oversized),
            Err(StoreError::InvalidWork(message)) if message == "import input exceeds the 1048576-byte limit"));
        assert!(
            !matches!(parse_work_import_input(&oversized[..MAX_WORK_IMPORT_INPUT_BYTES]),
            Err(StoreError::InvalidWork(message)) if message.contains("exceeds"))
        );
    }

    #[test]
    fn import_retry_matches_real_service_default_attribution() {
        use crate::work_service::{WorkActorDefaultSource, WorkAttributionDefaults};
        let make = |defaults| {
            LocalWorkService::new_with_attribution(
                "unused-actor-test.db".into(),
                crate::ProjectId("actor-test".into()),
                "author".into(),
                crate::SessionId("session".into()),
                Some("skill".into()),
                Some("host context".into()),
                defaults,
            )
        };
        let explicit = make(WorkAttributionDefaults::default()).actor("import", "intent");
        for actor in [
            None,
            Some(WorkActorDefaultSource::OsUserEnvironment),
            Some(WorkActorDefaultSource::ProcessFallback),
        ] {
            for session in [false, true] {
                let captured =
                    make(WorkAttributionDefaults { actor, session }).actor("import", "intent");
                assert_eq!(captured.retry_stable(), explicit);
                if actor.is_some() || session {
                    assert_ne!(captured, explicit, "original audit records the defaults");
                }
            }
        }
    }
}
