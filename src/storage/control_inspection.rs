//! Bounded presence evidence, not control admission or a reconciliation fence.

use super::{Connection, HostPathPolicy, SqliteStore, StoreError};

/// Presence checks over one current-schema read snapshot. Any presence blocks
/// absence-based host recovery, regardless of project, grant state or payload.
/// This is not an inventory of all records for the id. Unchecked data includes
/// connection tokens in `control_connections` and advisory decisions in
/// `control_observations`. Reusing an id/key can replay an old observation or
/// conflict; absence here never authorizes id recycling. The host must
/// separately retain and recheck connection ownership.
#[derive(Debug)]
pub struct ControlSessionInspection {
    pub stored_host_path_policy: HostPathPolicy,
    pub session_present: bool,
    pub session_grants_present: bool,
    pub retained_grant_present: bool,
}

impl SqliteStore {
    pub(super) fn control_session_presence_on(
        connection: &Connection,
        session_id: &str,
        retained_grant_id: &str,
        stored_host_path_policy: HostPathPolicy,
    ) -> Result<ControlSessionInspection, StoreError> {
        // No project/state joins or payload decoding: even an orphan,
        // cross-project or malformed grant is present, never clearance.
        Ok(connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM control_sessions WHERE session_id = ?1),
                    EXISTS(SELECT 1 FROM control_turn_grants WHERE session_id = ?1),
                    EXISTS(SELECT 1 FROM control_turn_grants WHERE grant_id = ?2)",
            [session_id, retained_grant_id],
            |row| {
                Ok(ControlSessionInspection {
                    stored_host_path_policy,
                    session_present: row.get(0)?,
                    session_grants_present: row.get(1)?,
                    retained_grant_present: row.get(2)?,
                })
            },
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn policy() -> HostPathPolicy {
        HostPathPolicy::host_default()
    }

    fn inspect(path: &Path) -> Result<ControlSessionInspection, StoreError> {
        SqliteStore::inspect_control_session(path, Some(policy()), "target", "retained")
    }

    fn grant(connection: &Connection, id: &str, session: &str, state: &str) {
        connection
            .execute(
                "INSERT INTO control_turn_grants
             (grant_id, session_id, task_id, request_key, grant_hash, grant_json,
              state, issued_at_ms, expires_at_ms)
             VALUES (?1, ?2, 'missing-task', ?1, 'opaque', x'ff', ?3, 0, 1)",
                [id, session, state],
            )
            .unwrap();
    }

    fn bytes(path: &Path) -> Vec<Option<Vec<u8>>> {
        ["", "-wal"]
            .iter()
            .map(
                |suffix| match std::fs::read(format!("{}{suffix}", path.display())) {
                    Ok(bytes) => Some(bytes),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => panic!("fixture read: {error}"),
                },
            )
            .collect()
    }

    #[test]
    fn control_inspection_absence_is_read_only_and_does_not_create_connection() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("store.db");
        let store = SqliteStore::open(&path).unwrap();
        let before = bytes(&path);
        let report = inspect(&path).unwrap();
        assert!(
            !report.session_present
                && !report.session_grants_present
                && !report.retained_grant_present
        );
        assert!(bytes(&path) == before, "database/WAL bytes changed");
        let count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM control_connections", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn control_inspection_counts_orphans_all_states_and_cross_session_retained_grant() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("store.db");
        let store = SqliteStore::open(&path).unwrap();
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        for state in [
            "issued",
            "begun",
            "completed",
            "expired",
            "unknown-malformed",
        ] {
            grant(&store.connection, "other", "target", state);
            let before = bytes(&path);
            let report = inspect(&path).unwrap();
            assert!(!report.session_present);
            assert!(report.session_grants_present, "{state}");
            assert!(!report.retained_grant_present);
            assert!(bytes(&path) == before, "database/WAL bytes changed");
            store
                .connection
                .execute("DELETE FROM control_turn_grants", [])
                .unwrap();
        }
        grant(&store.connection, "retained", "another-session", "begun");
        let report = inspect(&path).unwrap();
        assert!(!report.session_present && !report.session_grants_present);
        assert!(report.retained_grant_present);
    }

    #[test]
    fn control_inspection_does_not_filter_binding_by_project_or_decode_payload() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("store.db");
        let store = SqliteStore::open(&path).unwrap();
        store
            .connection
            .execute_batch(
                "PRAGMA foreign_keys=OFF;
            INSERT INTO control_sessions (
                session_id,project_id,task_id,routing_token,actor_json,bind_key,
                bind_intent_hash,bind_intent_json,phase,assurance,mediated_effects_json,
                confirmed_cursor,project_policy_epoch,task_admission_epoch,
                blocking_watermark,capability_map_revision,revision,updated_at_ms)
            VALUES ('target','different-project','missing-task','private-token',x'ff','k',
                'opaque',x'ff','invalid','invalid','invalid',0,0,0,0,0,0,0);",
            )
            .unwrap();
        let before = bytes(&path);
        let report = inspect(&path).unwrap();
        assert!(report.session_present);
        assert!(!report.session_grants_present && !report.retained_grant_present);
        assert!(bytes(&path) == before, "database/WAL bytes changed");
    }

    #[test]
    fn control_inspection_presence_uses_pinned_snapshot() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("store.db");
        let writer = SqliteStore::open(&path).unwrap();
        writer
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        let reader = SqliteStore::open_existing_read_only(&path).unwrap();
        let snapshot = reader.connection.unchecked_transaction().unwrap();
        let read = || {
            SqliteStore::control_session_presence_on(&snapshot, "target", "retained", policy())
                .unwrap()
        };
        assert!(!read().session_grants_present);
        grant(&writer.connection, "retained", "target", "begun");
        assert!(!read().session_grants_present);
        snapshot.commit().unwrap();
        let report = inspect(&path).unwrap();
        assert!(report.session_grants_present && report.retained_grant_present);
    }

    #[test]
    fn control_inspection_refuses_missing_schema_unbound_and_mismatched_policy() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("absent").join("store.db");
        assert!(inspect(&path).is_err());
        assert!(!path.parent().unwrap().exists());
        let path = home.path().join("store.db");
        let unbound = SqliteStore::open_unresolved(&path).unwrap();
        let before = bytes(&path);
        assert!(inspect(&path).is_err());
        assert!(bytes(&path) == before, "database/WAL bytes changed");
        drop(unbound);
        let store = SqliteStore::open(&path).unwrap();
        assert!(SqliteStore::inspect_control_session(&path, None, "target", "retained").is_err());
        let mut other = policy();
        other.case_fold_paths = !other.case_fold_paths;
        assert!(
            SqliteStore::inspect_control_session(&path, Some(other), "target", "retained").is_err()
        );
        for invalid in [
            String::new(),
            "a".repeat(crate::MAX_SESSION_ID_BYTES + 1),
            "bad\nid".into(),
        ] {
            assert!(
                SqliteStore::inspect_control_session(&path, Some(policy()), &invalid, "retained")
                    .is_err()
            );
            assert!(
                SqliteStore::inspect_control_session(&path, Some(policy()), "target", &invalid)
                    .is_err()
            );
        }
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE control_turn_grants")
            .unwrap();
        let before = bytes(&path);
        assert!(matches!(
            inspect(&path),
            Err(StoreError::DifferentBuildSchema)
        ));
        assert!(bytes(&path) == before, "database/WAL bytes changed");
    }

    #[test]
    fn control_inspection_selectors_use_shared_byte_bound() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("store.db");
        let _store = SqliteStore::open(&path).unwrap();
        let before = bytes(&path);
        let exact = "a".repeat(crate::MAX_SESSION_ID_BYTES);
        let report =
            SqliteStore::inspect_control_session(&path, Some(policy()), &exact, &exact).unwrap();
        assert!(
            !report.session_present
                && !report.session_grants_present
                && !report.retained_grant_present
        );
        let too_long = format!("{exact}a");
        for (session, grant) in [(&too_long, &exact), (&exact, &too_long)] {
            assert!(matches!(
                SqliteStore::inspect_control_session(&path, Some(policy()), session, grant),
                Err(StoreError::InvalidControlSession(_))
            ));
        }
        assert!(bytes(&path) == before, "database/WAL bytes changed");
    }

    #[test]
    fn control_inspection_snapshot_projection_loss_matches_ordinary_admission() {
        for statement in [
            "DROP INDEX memory_heads_work_scope",
            "DROP INDEX work_items_ready",
        ] {
            let home = crate::test_support::temp_home().unwrap();
            let path = home.path().join("store.db");
            let writer = SqliteStore::open(&path).unwrap();
            let reader = SqliteStore::open_existing_read_only(&path).unwrap();
            // Damage after successful open, before the first snapshot read.
            writer.connection.execute_batch(statement).unwrap();
            let before = bytes(&path);
            let snapshot = reader.connection.unchecked_transaction().unwrap();
            let error =
                SqliteStore::inspect_control_session_on(&snapshot, policy(), "target", "retained")
                    .unwrap_err();
            assert_eq!(
                crate::storage::store_open_refusal_kind(&error),
                crate::storage::StoreOpenRefusalKind::ProjectionRepairRequired,
                "{statement}: {error}"
            );
            assert!(error.to_string().contains("--repair-projections"));
            snapshot.commit().unwrap();
            for error in [
                SqliteStore::open(&path)
                    .err()
                    .expect("ordinary open must refuse"),
                SqliteStore::readiness(&path, Some(policy())).unwrap_err(),
                inspect(&path).unwrap_err(),
            ] {
                assert_eq!(
                    crate::storage::store_open_refusal_kind(&error),
                    crate::storage::StoreOpenRefusalKind::ProjectionRepairRequired,
                    "{statement}: {error}"
                );
            }
            assert!(bytes(&path) == before, "database/WAL bytes changed");
        }
    }

    #[test]
    fn control_inspection_busy_read_refuses_without_clearance() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("store.db");
        drop(SqliteStore::open(&path).unwrap());
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
            .unwrap();
        let error = inspect(&path).unwrap_err();
        assert_eq!(
            crate::storage::store_open_refusal_kind(&error),
            crate::storage::StoreOpenRefusalKind::Busy
        );
        writer.execute_batch("ROLLBACK").unwrap();
    }
}
