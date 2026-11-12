use super::*;
use crate::storage::work::{
    task_sync::{
        producing_origin,
        tests::{candidate, ledger::binding, state},
    },
    test_support::at,
};
use crate::task_sync::origin::{
    EnrollRequest, Enrollment, continuity::PreparedContinuity, private,
};
use crate::{ProjectId, project_database_path};

fn fixture() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    EnrollRequest,
) {
    let (home, mut store, mut binding, request) = unenrolled_fixture();
    let origin = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(1))
        .unwrap();
    binding.store.origin = Some(origin.origin_id);
    binding.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    (home, store, binding, request)
}

pub(super) fn unenrolled_fixture() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    EnrollRequest,
) {
    let home = crate::test_support::temp_home().unwrap();
    let mut binding = binding(home.path());
    binding.store.database = project_database_path(home.path(), &binding.project);
    std::fs::create_dir_all(binding.store.database.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&binding.repository.git_directory).unwrap();
    let store = SqliteStore::open(&binding.store.database).unwrap();
    let request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: None,
    };
    (home, store, binding, request)
}

fn assert_origin_join(store: &mut SqliteStore, binding: &Binding) {
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let checked = producing_origin::check_on(&tx, &binding.project, &guard).unwrap();
    checked.require_on(&tx).unwrap();
    assert_eq!(
        Some(&checked.origin().origin_id),
        binding.store.origin.as_ref()
    );
}

fn outgoing(store: &SqliteStore, id: &ObjectId, binding: &Binding) -> InspectedExchange {
    let mut tree = candidate();
    tree.tasks.clear();
    let mut input = super::tests::input(store, id, binding, tree);
    input.intent.direction = Direction::Outgoing;
    input
}

#[test]
fn origin_enrollment_survives_reopen_and_exact_retry_without_new_records() {
    let (_home, store, binding, request) = fixture();
    let original = store
        .task_sync_producing_origin(&binding.project)
        .unwrap()
        .unwrap();
    let before = state(&store);
    assert!(store.verify_all().unwrap().is_healthy());
    let changes = store
        .capture_task_changes(
            &binding.project,
            &crate::task_sync::progress::ExportCut {
                position: 0,
                anchor: None,
            },
        )
        .unwrap();
    assert_eq!(changes.cut, binding.store.cut);
    assert!(changes.changed.is_empty());
    drop(store);
    let mut reopened = SqliteStore::open(&binding.store.database).unwrap();
    assert_eq!(
        reopened
            .task_sync_producing_origin(&binding.project)
            .unwrap(),
        Some(original.clone())
    );
    assert_eq!(
        reopened
            .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(20))
            .unwrap(),
        original
    );
    assert_eq!(state(&reopened), before);
    let guard = PreparedContinuity::acquire(&binding.store.database, &binding.project).unwrap();
    let tx = reopened.begin_work_mutation().unwrap();
    let checked = producing_origin::check_on(&tx, &binding.project, &guard).unwrap();
    checked.require_on(&tx).unwrap();
    assert_eq!(checked.origin().origin_id, original.origin_id);
}

#[test]
fn origin_ordinary_open_and_read_never_enroll_or_create_private_files() {
    let home = crate::test_support::temp_home().unwrap();
    let project = ProjectId("unconfigured".into());
    let database = project_database_path(home.path(), &project);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let store = SqliteStore::open(&database).unwrap();
    let before = state(&store);
    assert_eq!(store.task_sync_producing_origin(&project).unwrap(), None);
    drop(store);
    let reopened = SqliteStore::open(&database).unwrap();
    assert_eq!(reopened.task_sync_producing_origin(&project).unwrap(), None);
    assert_eq!(state(&reopened), before);
    for name in [
        "task-sync-origin-registration.json",
        "task-sync-origin-pending.json",
        "prepare.lock",
    ] {
        assert!(!database.parent().unwrap().join(name).exists());
    }
}

#[test]
fn origin_copy_without_private_witness_is_quarantined_without_adoption() {
    let (home, store, binding, _) = fixture();
    let other = project_database_path(&home.path().join("copy"), &binding.project);
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    store.backup_to(&other).unwrap();
    let mut copy = SqliteStore::open(&other).unwrap();
    let before = state(&copy);
    let mut copied_binding = binding.clone();
    copied_binding.store.database = other.clone();
    assert!(
        copy.activate_task_sync_binding(&copied_binding)
            .unwrap_err()
            .to_string()
            .contains("witness missing")
    );
    assert_eq!(state(&copy), before);
    assert!(
        !other
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json")
            .exists()
    );
}

#[test]
fn origin_same_id_database_rewind_refuses_newer_exchange_witness() {
    let (home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let backup = home.path().join("older.db");
    store.backup_to(&backup).unwrap();
    let input = super::tests::input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let witness = std::fs::read(
        binding
            .store
            .database
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json"),
    )
    .unwrap();
    let origin = store.task_sync_producing_origin(&binding.project).unwrap();
    drop(store);
    // Restore the older coherent DB into a fresh project-home path, keeping
    // the later independent installation witness; the logical ID is unchanged.
    let restored = project_database_path(&home.path().join("restored"), &binding.project);
    std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
    std::fs::copy(backup, &restored).unwrap();
    std::fs::write(
        restored
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json"),
        witness,
    )
    .unwrap();
    let mut restored_store = SqliteStore::open(&restored).unwrap();
    assert_eq!(
        restored_store
            .task_sync_producing_origin(&binding.project)
            .unwrap(),
        origin
    );
    let before = state(&restored_store);
    let guard = PreparedContinuity::acquire(&restored, &binding.project).unwrap();
    let tx = restored_store.begin_work_mutation().unwrap();
    assert!(producing_origin::check_on(&tx, &binding.project, &guard).is_err());
    drop(tx);
    assert_eq!(state(&restored_store), before);
}

#[test]
fn origin_exact_committed_enrollment_recovers_missing_post_commit_witness() {
    let home = crate::test_support::temp_home().unwrap();
    let mut binding = binding(home.path());
    binding.store.database = project_database_path(home.path(), &binding.project);
    std::fs::create_dir_all(binding.store.database.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: None,
    };
    let pending = private::PendingEnrollment::new(
        Enrollment {
            project_id: binding.project.clone(),
            predecessor_origin_id: None,
            enrollment_operation_id: request.operation,
            actor: binding.actor.clone(),
            enrolled_at: at(1),
        },
        None,
    );
    private::retain(&binding.store.database, &pending).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let committed =
        producing_origin::insert_enrollment_on(&tx, &pending.object().unwrap()).unwrap();
    tx.commit().unwrap();
    let original = Some(committed);
    assert_eq!(
        Some(
            store
                .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(40))
                .unwrap()
        ),
        original
    );
    let before = state(&store);
    let changed = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: None,
    };
    assert!(
        store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &changed, at(41))
            .is_err()
    );
    assert_eq!(state(&store), before);
}

#[test]
fn origin_fork_activates_explicit_successor_of_settled_old_binding() {
    let (_home, mut store, mut binding, _) = fixture();
    let old_binding = store.activate_task_sync_binding(&binding).unwrap();
    let old_tail = store
        .task_sync_exchange_events(&old_binding)
        .unwrap()
        .last()
        .unwrap()
        .0
        .clone();
    let request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: binding.store.origin.clone(),
    };
    let fork = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7))
        .unwrap();
    binding.store.origin = Some(fork.origin_id);
    binding.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    binding.supersedes = Some(old_binding);
    binding.superseded_tail = Some(old_tail);
    binding.created_at = at(8);
    let new_binding = store.activate_task_sync_binding(&binding).unwrap();
    let witness = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    assert_eq!(witness.exchange.unwrap().binding, new_binding);
    assert_eq!(witness.pending_exchange, None);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn origin_exact_committed_fork_recovers_its_predecessor_witness() {
    let (_home, mut store, mut binding, _) = fixture();
    store.activate_task_sync_binding(&binding).unwrap();
    let prior = private::read(&binding.store.database, &binding.project).unwrap();
    let request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: binding.store.origin.clone(),
    };
    let pending = private::PendingEnrollment::new(
        Enrollment {
            project_id: binding.project.clone(),
            predecessor_origin_id: request.predecessor.clone(),
            enrollment_operation_id: request.operation,
            actor: binding.actor.clone(),
            enrolled_at: at(7),
        },
        prior.witness.clone(),
    );
    private::retain(&binding.store.database, &pending).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let committed =
        producing_origin::insert_enrollment_on(&tx, &pending.object().unwrap()).unwrap();
    tx.commit().unwrap();
    let before = state(&store);
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness,
        prior.witness
    );
    // Reopen as after process loss: the predecessor witness and exact fork
    // intent survive independently of the new canonical head.
    drop(store);
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let result = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(30))
        .unwrap();
    assert_eq!(result, committed);
    assert_eq!(state(&store), before);
    let recovered = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(recovered.pending.is_none());
    assert_eq!(recovered.witness.unwrap().origin, committed.origin_id);
    binding.store.origin = Some(committed.origin_id);
    assert_origin_join(&mut store, &binding);
    assert_eq!(
        store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(31))
            .unwrap(),
        result
    );
    assert_eq!(state(&store), before);
}

#[test]
fn origin_rejected_progress_releases_only_the_uncommitted_reservation() {
    use crate::task_sync::ledger::OutgoingProgress;
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = outgoing(&store, &id, &binding);
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let before = state(&store);
    let custody = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(
        store
            .record_task_sync_exchange_progress(&input, OutgoingProgress::PushStarted, at(7))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        custody
    );
    assert_origin_join(&mut store, &binding);
    // A lost cleanup after rollback is also releasable on a different next
    // request when every retained artifact is already known to this ledger.
    let witness = custody.witness.unwrap();
    let mut pending = witness.clone();
    pending.pending_exchange = Some(
        super::super::origin_request(
            "progress",
            &binding,
            Some(&id),
            Some(&input.intent),
            Some(&OutgoingProgress::PushStarted),
        )
        .unwrap(),
    );
    private::advance(&binding.store.database, &witness, &pending).unwrap();
    store
        .record_task_sync_exchange_progress(&input, OutgoingProgress::LocalCommitRecorded, at(8))
        .unwrap();
    store
        .record_task_sync_exchange_progress(&input, OutgoingProgress::PushStarted, at(9))
        .unwrap();
    assert_origin_join(&mut store, &binding);
}

#[test]
fn origin_refused_prepare_allows_a_fresh_basis_for_the_retained_candidate() {
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let fresh = super::tests::input(&store, &id, &binding, candidate());
    let mut stale = fresh.clone();
    stale.intent.local_planning_basis = CanonicalObject::freeze(&"earlier local planning")
        .unwrap()
        .key()
        .clone();
    let before = state(&store);
    let private_before = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(
        store
            .prepare_task_sync_exchange(&stale, at(6))
            .unwrap_err()
            .to_string()
            .contains("local planning basis changed")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    // The candidate remains retained. Reinspect it with a fresh basis using
    // the same operation; no import plan or publication uncertainty is erased.
    store.prepare_task_sync_exchange(&fresh, at(7)).unwrap();
    assert_origin_join(&mut store, &binding);
}

#[test]
fn origin_failed_effect_preserves_publication_custody_ahead_of_sql() {
    use crate::task_sync::ledger::OutgoingProgress;
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = outgoing(&store, &id, &binding);
    let prepared = store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    store
        .record_task_sync_exchange_progress(&input, OutgoingProgress::LocalCommitRecorded, at(7))
        .unwrap();
    let request = super::super::origin_request(
        "progress",
        &binding,
        Some(&id),
        Some(&input.intent),
        Some(&OutgoingProgress::PushStarted),
    )
    .unwrap();
    let before = state(&store);
    let error = producing_origin::with_exchange(
        &mut store,
        &binding.project,
        &binding.repository.git_directory,
        Some(input.intent.operation),
        &request,
        |store, guard| {
            let tx = store.begin_work_mutation()?;
            producing_origin::check_exchange_on(&tx, &binding.project, guard, &request)?;
            let mut ledger = Ledger::load(&tx)?;
            let object = append(
                &tx,
                &mut ledger,
                &input,
                Transition::PushStarted { prepared },
                at(8),
            )?;
            retention::retain_publication_witness(
                &binding.repository.git_directory,
                &retention::PublicationWitness {
                    operation: input.intent.operation,
                    event_id: object.key().clone(),
                    event: object.decode()?,
                },
            )
            .unwrap();
            // The real E1 ordering retains this witness before commit. Fail
            // here, leaving the same uncertainty as a failed SQL commit.
            Err::<(), _>(invalid("injected precommit loss"))
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("injected precommit loss"));
    assert_eq!(state(&store), before);
    let retained = private::read(&binding.store.database, &binding.project).unwrap();
    assert_eq!(retained.witness.unwrap().pending_exchange, Some(request));
    assert!(
        retention::read_publication_witness(
            &binding.repository.git_directory,
            input.intent.operation
        )
        .unwrap()
        .is_some()
    );
    assert!(
        store
            .record_task_sync_exchange_progress(
                &input,
                OutgoingProgress::LocalCommitRecorded,
                at(9)
            )
            .is_err()
    );
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    assert!(producing_origin::check_on(&tx, &binding.project, &guard).is_err());
}

#[test]
fn origin_second_repository_activation_refuses_before_commit_and_preserves_join() {
    let (home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut other = binding.clone();
    other.repository.enrollment = uuid::Uuid::new_v4();
    other.repository.main_checkout = home.path().join("other");
    other.repository.git_directory = other.repository.main_checkout.join(".git");
    std::fs::create_dir_all(&other.repository.git_directory).unwrap();
    let before = state(&store);
    let private_before = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(
        store
            .activate_task_sync_binding(&other)
            .unwrap_err()
            .to_string()
            .contains("origin has competing exchange bindings")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    assert_eq!(store.activate_task_sync_binding(&binding).unwrap(), id);
    assert_origin_join(&mut store, &binding);
}

#[test]
fn origin_first_enrollment_refuses_every_unsettled_legacy_exchange_stage() {
    use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};
    let (_home, mut store, mut binding, request) = unenrolled_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = outgoing(&store, &id, &binding);
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    for next in [
        OutgoingProgress::LocalCommitRecorded,
        OutgoingProgress::PushStarted,
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None }),
        OutgoingProgress::Observed(PublicationObservation::Confirmed {
            observed_tip: input.intent.commit.oid.clone(),
        }),
    ] {
        let before = state(&store);
        let custody = private::read(&binding.store.database, &binding.project).unwrap();
        assert!(
            store
                .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7))
                .unwrap_err()
                .to_string()
                .contains("unresolved exchange")
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            custody
        );
        assert!(custody.pending.is_none());
        // Refusal does not strand its original owner: it can still progress.
        store
            .record_task_sync_exchange_progress(&input, next, at(8))
            .unwrap();
    }
    let origin = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(9))
        .unwrap();
    binding.store.origin = Some(origin.origin_id);
    assert_origin_join(&mut store, &binding);
}

#[test]
fn origin_fork_preserves_unresolved_exchange_and_private_custody() {
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let before = state(&store);
    let private_before = private::read(&binding.store.database, &binding.project).unwrap();
    let fork = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: binding.store.origin.clone(),
    };
    assert!(
        store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &fork, at(8))
            .unwrap_err()
            .to_string()
            .contains("unresolved exchange")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
}

#[test]
fn origin_first_enrollment_preserves_an_unsettled_legacy_import() {
    let (_home, mut store, binding, request) = unenrolled_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let before = state(&store);
    let custody = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(
        store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7))
            .unwrap_err()
            .to_string()
            .contains("unresolved exchange")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        custody
    );
    store
        .apply_task_sync_exchange(&input, false, at(8))
        .unwrap();
    store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(9))
        .unwrap();
}

#[test]
fn origin_first_enrollment_refuses_unprepared_legacy_custody_without_retaining_intent() {
    let (_home, mut store, binding, request) = unenrolled_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, candidate());
    let before = state(&store);
    let custody = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(
        store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7))
            .unwrap_err()
            .to_string()
            .contains("retained operations are ahead of this ledger")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        custody
    );
    assert!(
        store
            .task_sync_producing_origin(&binding.project)
            .unwrap()
            .is_none()
    );
    // Both SQL and the private intent stay absent, so the legacy owner can
    // still account for its retained operation and settle it before enrollment.
    store.prepare_task_sync_exchange(&input, at(8)).unwrap();
    store
        .apply_task_sync_exchange(&input, false, at(9))
        .unwrap();
    store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(10))
        .unwrap();
}

#[test]
fn origin_retained_enrollment_retries_from_a_new_process_without_rewriting_attribution() {
    for fork in [false, true] {
        for committed in [false, true] {
            let (_home, mut store, binding, _) = if fork {
                fixture()
            } else {
                unenrolled_fixture()
            };
            let prior = private::read(&binding.store.database, &binding.project).unwrap();
            let mut original_actor = binding.actor.clone();
            original_actor.session_id =
                Some(crate::storage::work::test_support::process_default_session_at(100, at(1)));
            let request = EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: binding.store.origin.clone(),
            };
            let pending = private::PendingEnrollment::new(
                Enrollment {
                    project_id: binding.project.clone(),
                    predecessor_origin_id: request.predecessor.clone(),
                    enrollment_operation_id: request.operation,
                    actor: original_actor.clone(),
                    enrolled_at: at(1),
                },
                prior.witness,
            );
            private::retain(&binding.store.database, &pending).unwrap();
            if committed {
                let tx = store.begin_work_mutation().unwrap();
                producing_origin::insert_enrollment_on(&tx, &pending.object().unwrap()).unwrap();
                tx.commit().unwrap();
            }
            let mut retry_actor = original_actor.clone();
            retry_actor.session_id = Some(crate::SessionId(
                crate::new_process_default_work_session_id(),
            ));
            retry_actor.reason = "a fresh process retries the retained enrollment".into();
            assert_ne!(retry_actor.session_id, original_actor.session_id);
            let before = state(&store);
            let private_before = private::read(&binding.store.database, &binding.project).unwrap();
            let changed = EnrollRequest {
                operation: request.operation,
                predecessor: Some(ObjectId::mint()),
            };
            assert!(
                store
                    .enroll_task_sync_origin(&binding.project, &retry_actor, &changed, at(20))
                    .is_err()
            );
            assert_eq!(state(&store), before);
            assert_eq!(
                private::read(&binding.store.database, &binding.project).unwrap(),
                private_before
            );
            drop(store);
            let mut store = SqliteStore::open(&binding.store.database).unwrap();
            let result = store
                .enroll_task_sync_origin(&binding.project, &retry_actor, &request, at(30))
                .unwrap();
            assert_eq!(result.origin_id, pending.origin);
            assert_eq!(result.enrollment, pending.record);
            assert_eq!(result.enrollment.actor, original_actor);
            if committed {
                assert_eq!(state(&store), before);
            }
            let after = state(&store);
            assert_eq!(
                store
                    .enroll_task_sync_origin(&binding.project, &retry_actor, &request, at(40))
                    .unwrap(),
                result
            );
            assert_eq!(state(&store), after);
            let mut joined_binding = binding;
            joined_binding.store.origin = Some(result.origin_id);
            assert_origin_join(&mut store, &joined_binding);
        }
    }
}

#[test]
fn origin_writer_refusal_cleans_only_the_new_uncommitted_intent() {
    let (_home, mut store, binding, request) = unenrolled_fixture();
    store
        .connection
        .busy_timeout(std::time::Duration::ZERO)
        .unwrap();
    let mut other = SqliteStore::open(&binding.store.database).unwrap();
    let writer = other.begin_work_mutation().unwrap();
    let before = state(&store);
    let private_before = private::read(&binding.store.database, &binding.project).unwrap();
    assert!(matches!(
        store.enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7)),
        Err(StoreError::WorkWriterAdmissionRefused { .. })
    ));
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    drop(writer);
    let mut retry = binding.actor.clone();
    retry.session_id = Some(crate::SessionId(
        crate::new_process_default_work_session_id(),
    ));
    store
        .enroll_task_sync_origin(&binding.project, &retry, &request, at(8))
        .unwrap();
}

#[test]
fn origin_host_join_refuses_retained_operations_ahead_of_matching_database_and_witness() {
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let witness = private::read(&binding.store.database, &binding.project).unwrap();
    let before = state(&store);
    // Real E1 custody exists before SQL preparation, as it would after a rewind
    // of the DB and registration witness that left independent Git data intact.
    let input = super::tests::input(&store, &id, &binding, candidate());
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let error = producing_origin::check_on(&tx, &binding.project, &guard)
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("retained operations are ahead of this ledger")
    );
    drop(tx);
    drop(guard);
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        witness
    );
    // Only the exact E1 preparation can account for that retained operation.
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let checked = producing_origin::check_on(&tx, &binding.project, &guard).unwrap();
    checked.require_on(&tx).unwrap();
    assert_eq!(
        Some(&checked.origin().origin_id),
        binding.store.origin.as_ref()
    );
}

#[test]
fn origin_exact_e1_intent_recovers_commit_before_witness_advance() {
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let input = super::tests::input(&store, &id, &binding, candidate());
    let mut reserved = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    reserved.pending_exchange = Some(
        super::super::origin_request("prepare", &binding, Some(&id), Some(&input.intent), None)
            .unwrap(),
    );
    let prepared = store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let completed = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    // Inject the exact private bytes retained immediately before that commit.
    private::advance(&binding.store.database, &completed, &reserved).unwrap();
    let before = state(&store);
    assert!(
        store
            .apply_task_sync_exchange(&input, false, at(29))
            .unwrap_err()
            .to_string()
            .contains("retry that exact step")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness,
        Some(reserved)
    );
    assert_eq!(
        store.prepare_task_sync_exchange(&input, at(30)).unwrap(),
        prepared
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness,
        Some(completed)
    );
}

#[test]
fn origin_bogus_prospective_feed_cuts_refuse_before_sql_or_witness_changes() {
    for activation in [true, false] {
        let (_home, mut store, binding, _) = fixture();
        let mut bogus = binding.store.cut.clone();
        bogus.position += 1;
        let (before, witness, error) = if activation {
            let mut invalid_binding = binding.clone();
            invalid_binding.store.cut = bogus;
            invalid_binding.validate().unwrap(); // Well-formed, but not a local feed entry.
            let before = state(&store);
            let witness = private::read(&binding.store.database, &binding.project).unwrap();
            let error = store
                .activate_task_sync_binding(&invalid_binding)
                .unwrap_err();
            (before, witness, error)
        } else {
            let id = store.activate_task_sync_binding(&binding).unwrap();
            // Build real retained bytes with this same false cut. Changing
            // just the DTO would trigger an earlier artifact-mismatch refusal.
            let input =
                super::tests::input_with_cut(&store, &id, &binding, candidate(), bogus.clone());
            assert_eq!(input.intent.candidate.source_cut, bogus);
            let before = state(&store);
            let witness = private::read(&binding.store.database, &binding.project).unwrap();
            let error = store.prepare_task_sync_exchange(&input, at(8)).unwrap_err();
            (before, witness, error)
        };
        assert!(
            error
                .to_string()
                .contains("origin witness feed anchor missing or divergent"),
            "{error}"
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            witness
        );
        assert!(store.verify_all().unwrap().is_healthy());
        if activation {
            store.activate_task_sync_binding(&binding).unwrap();
        }
    }
}

#[test]
fn origin_completed_enrollment_retry_preserves_an_unrelated_pending_fork() {
    let (_home, mut store, binding, completed_request) = fixture();
    let original = store
        .task_sync_producing_origin(&binding.project)
        .unwrap()
        .unwrap();
    let prior = private::read(&binding.store.database, &binding.project).unwrap();
    let fork_request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: Some(original.origin_id.clone()),
    };
    let pending = private::PendingEnrollment::new(
        Enrollment {
            project_id: binding.project.clone(),
            predecessor_origin_id: fork_request.predecessor.clone(),
            enrollment_operation_id: fork_request.operation,
            actor: binding.actor.clone(),
            enrolled_at: at(7),
        },
        prior.witness,
    );
    private::retain(&binding.store.database, &pending).unwrap();
    let before = state(&store);
    let retained = private::read(&binding.store.database, &binding.project).unwrap();
    // Simulate process loss after retaining a fork but before its SQL write.
    // An exchange must not advance expected_witness and strand the exact retry.
    let error = store.activate_task_sync_binding(&binding).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("origin enrollment intent remains pending"),
        "{error}"
    );
    assert!(
        error
            .to_string()
            .contains(&fork_request.operation.to_string()),
        "{error}"
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        retained
    );
    assert_eq!(
        store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &completed_request, at(8))
            .unwrap(),
        original
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        retained
    );
    let fork = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &fork_request, at(9))
        .unwrap();
    assert_eq!(fork.origin_id, pending.origin);
    assert_eq!(fork.enrollment, pending.record);
}

#[test]
fn origin_export_import_preserves_record_ids_but_never_installs_private_witness() {
    let (home, store, binding, _) = fixture();
    let origin = store.task_sync_producing_origin(&binding.project).unwrap();
    let planning = store
        .export_task_tree(&binding.project)
        .unwrap()
        .documents()
        .unwrap();
    assert!(
        !String::from_utf8(serde_json::to_vec(&planning).unwrap())
            .unwrap()
            .contains("task_sync_producing_origin")
    );
    let export = home.path().join("store.jsonl");
    crate::storage::migration::export_json(&binding.store.database, &export).unwrap();
    let imported = project_database_path(&home.path().join("imported"), &binding.project);
    std::fs::create_dir_all(imported.parent().unwrap()).unwrap();
    // Migration's conservative Windows temporary-name guard is intentionally
    // stricter than the ordinary long-path SQLite opener. Import at a short
    // owned path, then make a coherent local copy into the standard layout.
    let migration_output = home.path().join("m.db");
    crate::storage::migration::import_json(&export, &migration_output).unwrap();
    SqliteStore::open(&migration_output)
        .unwrap()
        .backup_to(&imported)
        .unwrap();
    let mut restored = SqliteStore::open(&imported).unwrap();
    assert_eq!(
        restored
            .task_sync_producing_origin(&binding.project)
            .unwrap(),
        origin
    );
    assert!(restored.verify_all().unwrap().is_healthy());
    let guard = PreparedContinuity::acquire(&imported, &binding.project).unwrap();
    let tx = restored.begin_work_mutation().unwrap();
    assert!(producing_origin::check_on(&tx, &binding.project, &guard).is_err());
    assert!(
        !imported
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json")
            .exists()
    );
}

#[test]
fn origin_cancel_recovers_pending_enrollment_with_retained_candidate() {
    for fork in [false, true] {
        let (_home, mut store, binding, _) = if fork {
            fixture()
        } else {
            unenrolled_fixture()
        };
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let prior = private::read(&binding.store.database, &binding.project).unwrap();
        let request = EnrollRequest {
            operation: uuid::Uuid::new_v4(),
            predecessor: binding.store.origin.clone(),
        };
        let pending = private::PendingEnrollment::new(
            Enrollment {
                project_id: binding.project.clone(),
                predecessor_origin_id: request.predecessor.clone(),
                enrollment_operation_id: request.operation,
                actor: binding.actor.clone(),
                enrolled_at: at(7),
            },
            prior.witness.clone(),
        );
        private::retain(&binding.store.database, &pending).unwrap();
        // Process loss leaves the intent; the ordinary next action retains
        // candidate bytes before its writer discovers that pending enrollment.
        let input = super::tests::input(&store, &id, &binding, candidate());
        let before = state(&store);
        let artifacts = retention::inventory(&binding.repository.git_directory).unwrap();
        let prepare_error = store.prepare_task_sync_exchange(&input, at(8)).unwrap_err();
        for text in [
            "origin enrollment intent remains pending",
            "origin-enroll",
            "origin-cancel",
            &request.operation.to_string(),
        ] {
            assert!(prepare_error.to_string().contains(text), "{prepare_error}");
        }
        let competing = private::PendingEnrollment::new(
            Enrollment {
                enrollment_operation_id: uuid::Uuid::new_v4(),
                ..pending.record.clone()
            },
            pending.expected_witness.clone(),
        );
        let competing_error = private::retain(&binding.store.database, &competing).unwrap_err();
        assert_eq!(prepare_error.to_string(), competing_error.to_string());
        let error = store
            .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(9))
            .unwrap_err();
        assert!(
            error.to_string().contains("retained operations are ahead"),
            "{error}"
        );
        let wrong = EnrollRequest {
            operation: uuid::Uuid::new_v4(),
            ..request.clone()
        };
        assert!(
            store
                .cancel_task_sync_origin_enrollment(&binding.project, &binding.actor, &wrong)
                .is_err()
        );
        assert_eq!(
            private::read(&binding.store.database, &binding.project)
                .unwrap()
                .pending,
            Some(pending)
        );
        assert!(
            store
                .cancel_task_sync_origin_enrollment(&binding.project, &binding.actor, &request)
                .unwrap()
        );
        assert!(
            !store
                .cancel_task_sync_origin_enrollment(&binding.project, &binding.actor, &request)
                .unwrap()
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            prior
        );
        assert_eq!(
            retention::inventory(&binding.repository.git_directory).unwrap(),
            artifacts
        );
        store.prepare_task_sync_exchange(&input, at(10)).unwrap();
    }
}

#[test]
fn origin_enrollment_checks_exchange_reservation_before_retain_and_cancel_preserves_it() {
    let (_home, mut store, binding, _) = fixture();
    store.activate_task_sync_binding(&binding).unwrap();
    let prior = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    let mut reserved = prior.clone();
    reserved.pending_exchange =
        Some(super::super::origin_request("activate", &binding, None, None, None).unwrap());
    private::advance(&binding.store.database, &prior, &reserved).unwrap();
    let request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: binding.store.origin.clone(),
    };
    let before = state(&store);
    let custody = private::read(&binding.store.database, &binding.project).unwrap();
    let error = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("origin exchange intent remains pending"),
        "{error}"
    );
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        custody
    );
    assert_eq!(state(&store), before);
    // A previously interrupted caller may already have retained both slots.
    // Cancelling only the uncommitted enrollment preserves the exchange retry.
    let pending = private::PendingEnrollment::new(
        Enrollment {
            project_id: binding.project.clone(),
            predecessor_origin_id: request.predecessor.clone(),
            enrollment_operation_id: request.operation,
            actor: binding.actor.clone(),
            enrolled_at: at(7),
        },
        Some(reserved.clone()),
    );
    assert!(private::retain(&binding.store.database, &pending).is_err());
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        custody
    );
    // Inject the interrupted state the old pre-retention check could leave.
    std::fs::write(
        binding
            .store
            .database
            .parent()
            .unwrap()
            .join("task-sync-origin-pending.json"),
        serde_json::to_vec(&pending).unwrap(),
    )
    .unwrap();
    assert!(
        store
            .cancel_task_sync_origin_enrollment(&binding.project, &binding.actor, &request)
            .unwrap()
    );
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        custody
    );
    assert_eq!(state(&store), before);
    store.activate_task_sync_binding(&binding).unwrap();
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness
            .unwrap()
            .pending_exchange,
        None
    );
}

#[test]
fn origin_cancel_refuses_committed_changed_and_rewound_custody() {
    for case in ["committed", "witness", "feed", "exchange"] {
        let (_home, mut store, binding, _) = fixture();
        store.activate_task_sync_binding(&binding).unwrap();
        let prior = private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness
            .unwrap();
        let request = EnrollRequest {
            operation: uuid::Uuid::new_v4(),
            predecessor: binding.store.origin.clone(),
        };
        let pending = private::PendingEnrollment::new(
            Enrollment {
                project_id: binding.project.clone(),
                predecessor_origin_id: request.predecessor.clone(),
                enrollment_operation_id: request.operation,
                actor: binding.actor.clone(),
                enrolled_at: at(7),
            },
            Some(prior.clone()),
        );
        private::retain(&binding.store.database, &pending).unwrap();
        match case {
            "committed" => {
                let tx = store.begin_work_mutation().unwrap();
                producing_origin::insert_enrollment_on(&tx, &pending.object().unwrap()).unwrap();
                tx.commit().unwrap();
            }
            "witness" => {
                let mut changed = prior.clone();
                changed.pending_exchange = Some(ObjectId::mint());
                private::advance(&binding.store.database, &prior, &changed).unwrap();
            }
            "feed" => {
                store.connection.execute("DELETE FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1 AND position=?2",
                    rusqlite::params![binding.project.0, prior.cut.position]).unwrap();
            }
            "exchange" => {
                store
                    .connection
                    .execute(
                        "DELETE FROM work_task_sync_exchange_heads WHERE binding_id=?1",
                        [prior.exchange.as_ref().unwrap().binding.as_str()],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let before = state(&store);
        let custody = private::read(&binding.store.database, &binding.project).unwrap();
        assert!(
            store
                .cancel_task_sync_origin_enrollment(&binding.project, &binding.actor, &request)
                .is_err(),
            "{case}"
        );
        assert_eq!(state(&store), before, "{case}");
        assert_eq!(
            private::read(&binding.store.database, &binding.project).unwrap(),
            custody,
            "{case}"
        );
    }
}

#[test]
fn origin_explicit_fork_retains_original_record_and_rejects_competing_heads() {
    let (_home, mut store, binding, _) = fixture();
    let first = store
        .task_sync_producing_origin(&binding.project)
        .unwrap()
        .unwrap();
    let request = EnrollRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: Some(first.origin_id.clone()),
    };
    let next = store
        .enroll_task_sync_origin(&binding.project, &binding.actor, &request, at(7))
        .unwrap();
    assert_ne!(first.origin_id, next.origin_id);
    assert_eq!(
        next.enrollment.predecessor_origin_id.as_ref(),
        Some(&first.origin_id)
    );
    let record = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [first.origin_id.as_str()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<Enrollment>(&record).unwrap(),
        first.enrollment
    );
    let competing = CanonicalObject::mint(&Enrollment {
        predecessor_origin_id: None,
        enrollment_operation_id: uuid::Uuid::new_v4(),
        ..next.enrollment.clone()
    })
    .unwrap();
    SqliteStore::insert_object(
        &store.connection,
        crate::task_sync::origin::PRODUCING_ORIGIN_KIND,
        &competing,
    )
    .unwrap();
    assert!(store.task_sync_producing_origin(&binding.project).is_err());
    assert!(!store.verify_all().unwrap().is_healthy());
}

fn assert_indexed_anchor(
    store: &SqliteStore,
    binding: &Binding,
    cut: &crate::task_sync::progress::ExportCut,
) {
    let anchor = super::super::origin_anchor_on(
        &store.connection,
        &binding.project,
        binding.store.origin.as_ref().unwrap(),
    )
    .unwrap()
    .unwrap();
    let events = store.task_sync_exchange_events(&anchor.binding).unwrap();
    let (id, event) = events.last().unwrap();
    assert_eq!(&anchor.head, id);
    assert_eq!(anchor.sequence, event.sequence);
    assert_eq!(&anchor.cut, cut);
}

#[test]
fn origin_indexed_anchor_tracks_settled_and_pending_tails_and_refuses_stale_views() {
    use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};
    let (_home, mut store, binding, _) = fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    assert_indexed_anchor(&store, &binding, &binding.store.cut);
    // Enrollment already started this feed. Import-applied tails are covered
    // by the initial-import and reconciled historical fixtures.
    let outgoing = outgoing(&store, &id, &binding);
    store.prepare_task_sync_exchange(&outgoing, at(6)).unwrap();
    assert_indexed_anchor(&store, &binding, &outgoing.intent.candidate.source_cut);
    for damage in [
        "DELETE FROM work_task_sync_exchange_operations",
        "DELETE FROM work_task_sync_exchange_heads",
        "UPDATE work_task_sync_exchange_heads SET stage='settled',pending_prepared_id=NULL",
    ] {
        store.connection.execute_batch(damage).unwrap();
        let before = state(&store);
        assert!(
            super::super::origin_anchor_on(
                &store.connection,
                &binding.project,
                binding.store.origin.as_ref().unwrap()
            )
            .unwrap_err()
            .to_string()
            .contains("repair projections"),
            "{damage}"
        );
        assert_eq!(state(&store), before);
        let tx = store.begin_work_mutation().unwrap();
        super::super::view::rebuild(&tx).unwrap();
        tx.commit().unwrap();
    }
    for progress in [
        OutgoingProgress::LocalCommitRecorded,
        OutgoingProgress::PushStarted,
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None }),
        OutgoingProgress::Observed(PublicationObservation::Confirmed {
            observed_tip: outgoing.intent.commit.oid.clone(),
        }),
    ] {
        store
            .record_task_sync_exchange_progress(&outgoing, progress, at(9))
            .unwrap();
        assert_indexed_anchor(&store, &binding, &outgoing.intent.candidate.source_cut);
    }
    let pending: Option<String> = store
        .connection
        .query_row(
            "SELECT pending_prepared_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending, None);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn origin_indexed_custody_keeps_other_project_historical_publication_obligations() {
    use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};
    let (home, mut store, binding, _) = fixture();
    store.activate_task_sync_binding(&binding).unwrap();
    let mut other = binding.clone();
    other.project = ProjectId("other-project".into());
    other.store.origin = None;
    other.store.cut = producing_origin::cut_on(&store.connection, &other.project).unwrap();
    other.repository.enrollment = uuid::Uuid::new_v4();
    let other_id = store.activate_task_sync_binding(&other).unwrap();
    let mut tree = candidate();
    tree.project.project_id = other.project.clone();
    tree.tasks.clear();
    let mut input = super::tests::input(&store, &other_id, &other, tree);
    input.intent.direction = Direction::Outgoing;
    store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    for progress in [
        OutgoingProgress::LocalCommitRecorded,
        OutgoingProgress::PushStarted,
        OutgoingProgress::Observed(PublicationObservation::Confirmed {
            observed_tip: input.intent.commit.oid.clone(),
        }),
    ] {
        store
            .record_task_sync_exchange_progress(&input, progress, at(7))
            .unwrap();
    }
    assert_origin_join(&mut store, &binding);
    let before = state(&store);
    let witness = private::read(&binding.store.database, &binding.project).unwrap();
    let path = binding
        .repository
        .git_directory
        .join("engram-task-sync-publication-attempts")
        .join(format!("{}.json", input.intent.operation));
    assert!(path.starts_with(home.path()));
    std::fs::remove_file(path).unwrap();
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let error = producing_origin::check_on(&tx, &binding.project, &guard)
        .err()
        .unwrap();
    drop(tx);
    drop(guard);
    assert!(
        error.to_string().contains("publication witness is missing"),
        "{error}"
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project).unwrap(),
        witness
    );
}
