//! Receipt-only acknowledgement never substitutes for custody admission.

use super::*;
use crate::task_sync::ledger::CompletedExchange;
use crate::task_sync::origin::private;

#[cfg(windows)]
mod windows;

fn objects(store: &SqliteStore) -> Vec<(String, Vec<u8>)> {
    let mut statement = store
        .connection
        .prepare("SELECT object_id,canonical_json FROM objects ORDER BY object_id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

fn pending(
    stop: bool,
) -> (
    crate::test_support::TempHome,
    SqliteStore,
    InspectedEvidence,
    CompletedExchange,
) {
    let (home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = retained_input(&mut store, &binding, &id);
    let mut pending = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    let database = binding.store.database.clone();
    let project = binding.project.clone();
    let outcome = input.recipe.outcome_id.clone();
    let request = retention::evidence::request_fingerprint(&input.recipe).unwrap();
    super::super::super::super::evidence::after_commit::set(move || {
        let connection = rusqlite::Connection::open(&database).unwrap();
        let visible: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id=?1)",
                [outcome.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(visible);
        assert_eq!(
            private::read(&database, &project)
                .unwrap()
                .witness
                .unwrap()
                .pending_exchange,
            Some(request)
        );
    });
    let original = if stop {
        store
            .stop_task_sync_evidence_import(
                &input,
                &EvidenceStop::Refused {
                    reason: EvidenceRefusal::InvalidPayload,
                },
            )
            .unwrap()
    } else {
        store.apply_task_sync_evidence_import(&input).unwrap()
    };
    let after = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    pending.pending_exchange =
        Some(retention::evidence::request_fingerprint(&input.recipe).unwrap());
    // Model a lost private response after the real canonical effect committed.
    // The Windows case below reaches this state through the actual late fetch.
    private::advance(&binding.store.database, &after, &pending).unwrap();
    (home, store, input, original)
}

#[test]
fn committed_evidence_replay_finishes_only_its_private_acknowledgement_once() {
    for stopped in [false, true] {
        let (_home, mut store, input, original) = pending(stopped);
        let binding = &input.binding;
        let before = objects(&store);
        let inventory = retention::inventory(&binding.repository.git_directory).unwrap();
        let old = private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness
            .unwrap();
        assert!(old.pending_exchange.is_some());
        let replay = store.apply_task_sync_evidence_import(&input).unwrap();
        assert_eq!(replay, original);
        let after = private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness
            .unwrap();
        assert!(after.pending_exchange.is_none());
        assert_eq!(after.exchange.as_ref().unwrap().head, original.outcome);
        assert_eq!(objects(&store), before);
        assert_eq!(
            retention::inventory(&binding.repository.git_directory).unwrap(),
            inventory
        );
        assert_eq!(
            store.apply_task_sync_evidence_import(&input).unwrap(),
            original
        );
        assert_eq!(
            private::read(&binding.store.database, &binding.project)
                .unwrap()
                .witness,
            Some(after)
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn committed_ack_refuses_changed_private_or_canonical_proof_without_mutation() {
    for fault in [
        "request",
        "origin",
        "enrollment",
        "restore",
        "missing",
        "changed",
        "later",
    ] {
        let (_home, mut store, input, mut original) = pending(false);
        let binding = &input.binding;
        let database = &binding.store.database;
        let project = &binding.project;
        if matches!(fault, "request" | "origin" | "enrollment") {
            let old = private::read(database, project).unwrap().witness.unwrap();
            let mut changed = old.clone();
            match fault {
                "request" => {
                    changed.pending_exchange = Some(
                        CanonicalObject::freeze(&"different pending request")
                            .unwrap()
                            .key()
                            .clone(),
                    );
                }
                "origin" => changed.origin = ObjectId::mint(),
                "enrollment" => changed.enrollment_operation = uuid::Uuid::new_v4(),
                _ => unreachable!(),
            }
            if fault == "request" {
                private::advance(database, &old, &changed).unwrap();
            } else {
                // Only the fixture corrupts identity; the production advance
                // correctly refuses an origin/enrollment identity replacement.
                use crate::task_sync::settings::files::{Directory, PrivateFile};
                Directory::open(database.parent().unwrap(), false)
                    .map_err(|_| "fixture origin directory could not open")
                    .unwrap()
                    .unwrap()
                    .replace_slot(
                        PrivateFile::OriginRegistration,
                        &serde_json::to_vec(&changed).unwrap(),
                    )
                    .map_err(|_| "fixture origin identity could not be written")
                    .unwrap();
            }
        } else if fault == "restore" {
            use crate::backup::restore::{RestoreRecord, RestoreSource, RestoreState};
            use sha2::{Digest, Sha256};
            let home = crate::project_home_of(database, project).unwrap();
            let path = crate::backup::restore::restore_record_path(&home, project);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let record = RestoreRecord {
                format_version: crate::backup::target::RECORD_FORMAT_VERSION,
                occurrence_id: uuid::Uuid::new_v4(),
                project: project.0.clone(),
                source: RestoreSource::LocalFile {
                    path: database.to_string_lossy().into_owned(),
                    replace: true,
                },
                sha256: format!("{:x}", Sha256::digest(b"throwaway restored store")),
                staging: database
                    .with_extension("staging")
                    .to_string_lossy()
                    .into_owned(),
                state: RestoreState::Completed,
                pending_at: at(80),
                completed_at: Some(at(81)),
                previous_completed: None,
            };
            std::fs::write(path, crate::canonical::canonical_bytes(&record).unwrap()).unwrap();
        } else if fault == "missing" {
            // Model external corruption in this throwaway store; ordinary
            // writers correctly prevent deletion of the referenced outcome.
            store
                .connection
                .execute_batch("PRAGMA foreign_keys = OFF")
                .unwrap();
            store
                .connection
                .execute(
                    "DELETE FROM objects WHERE object_id=?1",
                    [original.outcome.as_str()],
                )
                .unwrap();
            store
                .connection
                .execute_batch("PRAGMA foreign_keys = ON")
                .unwrap();
            assert!(
                store
                    .connection
                    .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, bool>(0))
                    .unwrap()
            );
        } else if fault == "changed" {
            original.event.recorded_at = at(999);
        } else {
            let old = private::read(database, project).unwrap().witness.unwrap();
            store
                .acknowledge_committed_task_sync_evidence(
                    &input.binding_id,
                    input.recipe.intent().unwrap(),
                    &original,
                )
                .unwrap();
            audit_healthy(&mut store, binding, &input.binding_id);
            let after = private::read(database, project).unwrap().witness.unwrap();
            private::advance(database, &after, &old).unwrap();
        }
        let before = objects(&store);
        let private_before = private::read(database, project).unwrap();
        let inventory = retention::inventory(&binding.repository.git_directory).unwrap();
        assert!(
            store
                .acknowledge_committed_task_sync_evidence(
                    &input.binding_id,
                    input.recipe.intent().unwrap(),
                    &original,
                )
                .is_err(),
            "{fault}"
        );
        assert_eq!(objects(&store), before, "{fault}");
        assert_eq!(
            private::read(database, project).unwrap(),
            private_before,
            "{fault}"
        );
        assert_eq!(
            retention::inventory(&binding.repository.git_directory).unwrap(),
            inventory,
            "{fault}"
        );
    }
}

#[test]
fn copied_store_cannot_acknowledge_the_original_file_witness() {
    let (home, store, input, original) = pending(false);
    let copy = home.path().join("another-store.db");
    store
        .connection
        .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
        .unwrap();
    let mut other = SqliteStore::open(copy).unwrap();
    let before = objects(&other);
    let private_before =
        private::read(&input.binding.store.database, &input.binding.project).unwrap();
    assert!(
        other
            .acknowledge_committed_task_sync_evidence(
                &input.binding_id,
                input.recipe.intent().unwrap(),
                &original,
            )
            .is_err()
    );
    assert_eq!(objects(&other), before);
    assert_eq!(
        private::read(&input.binding.store.database, &input.binding.project).unwrap(),
        private_before
    );
}

#[test]
fn completed_lookup_stays_read_only_offline_and_mutating_replay_uses_canonical_proof() {
    let (_home, mut store, input, original) = pending(false);
    let root = &input.binding.repository.git_directory;
    let hidden = root.with_file_name("hidden-receipt-git");
    std::fs::rename(root, &hidden).unwrap();
    let before = objects(&store);
    let witness = private::read(&input.binding.store.database, &input.binding.project).unwrap();
    assert_eq!(
        store
            .completed_task_sync_evidence(&input.binding_id, input.recipe.intent().unwrap())
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        private::read(&input.binding.store.database, &input.binding.project).unwrap(),
        witness
    );
    assert_eq!(
        store.apply_task_sync_evidence_import(&input).unwrap(),
        original
    );
    assert_eq!(objects(&store), before);
    assert!(
        private::read(&input.binding.store.database, &input.binding.project)
            .unwrap()
            .witness
            .unwrap()
            .pending_exchange
            .is_none()
    );
    std::fs::rename(hidden, root).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}
