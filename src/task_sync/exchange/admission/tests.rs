use super::*;
use crate::{
    ActorContext, ProjectId, SessionId,
    domain::AssuranceLevel,
    storage::SqliteStore,
    task_sync::{
        FORMAT, ProjectDocument,
        ledger::{Direction, RepositoryBasis, StoreBasis, TargetBasis},
        notes::NoteOmissions,
        progress::{ExportCut, ExportProfile, PreparedCandidate, PreparedProvenance},
    },
};

async fn git(repository: &std::path::Path, args: &[&str]) -> Vec<u8> {
    local(
        repository,
        args,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap()
}

async fn commit(repository: &std::path::Path) -> String {
    git(repository, &["add", "--", ".engram/project.json"]).await;
    git(
        repository,
        &[
            "-c",
            "user.name=Exchange Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--no-gpg-sign",
            "-m",
            "retained fixture task text",
        ],
    )
    .await;
    String::from_utf8(git(repository, &["rev-parse", "HEAD"]).await)
        .unwrap()
        .trim_end()
        .to_owned()
}

#[tokio::test(flavor = "current_thread")]
async fn inspected_exchange_matches_exact_git_blobs_and_refuses_branch_target_and_tree_drift() {
    let home = crate::test_support::temp_home().unwrap();
    let repository = home.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    git(&repository, &["init", "--initial-branch=main", "."]).await;
    git(
        &repository,
        &[
            "config",
            "--local",
            "remote.origin.url",
            "https://example.invalid/tasks.git",
        ],
    )
    .await;
    git(&repository, &["config", "--local", "credential.helper", ""]).await;
    let project = ProjectId("inspection-fixture".into());
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included: false,
        },
        tasks: Vec::new(),
    };
    std::fs::create_dir(repository.join(".engram")).unwrap();
    std::fs::write(
        repository.join(".engram/project.json"),
        &tree.documents().unwrap()["project.json"],
    )
    .unwrap();
    let oid = commit(&repository).await;
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let actor = ActorContext {
        actor_id: "fixture".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        session_id: Some(SessionId("fixture".into())),
        run_id: None,
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "local exchange fixture".into(),
    };
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let target = ExchangeTarget::capture(
        &TargetSelection {
            repository: repository.clone(),
            remote: "origin".into(),
            reference: "refs/heads/main".into(),
        },
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let now = chrono::Utc::now();
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database: home.path().join("work.db"),
            origin: None,
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: repository.clone(),
            git_directory: repository.join(".git"),
        },
        branch: "refs/heads/main".into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/main".into(),
            configuration_fingerprint: target.fingerprint().unwrap(),
        },
        profile: profile.clone(),
        supersedes: None,
        superseded_tail: None,
        actor: actor.clone(),
        created_at: now,
    };
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let producer = crate::CanonicalObject::freeze(&"inspection fixture producer")
        .unwrap()
        .key()
        .clone();
    let prepared = PreparedCandidate {
        documents: tree.documents().unwrap(),
        tree: tree.clone(),
        provenance: PreparedProvenance {
            sequence: 1,
            operation: uuid::Uuid::new_v4(),
            predecessor: None,
            store: binding.store.database.clone(),
            destination: home.path().join("prepared"),
            project,
            profile,
            producer: producer.clone(),
            cut: binding.store.cut.clone(),
            omissions: NoteOmissions::default(),
            activity_omissions: NoteOmissions::default(),
        },
    };
    let operation = uuid::Uuid::new_v4();
    let candidate =
        retention::retain_prepared(&binding.repository.git_directory, operation, &prepared)
            .unwrap();
    let retained = crate::task_sync::exchange::retain_commit(
        &repository,
        operation,
        &oid,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    let before = crate::task_sync::progress::documents_fingerprint(&tree.documents().unwrap())
        .parse()
        .unwrap();
    let intent = Intent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor,
        producer,
        expected_head: Some(oid),
        common_commit: None,
        candidate,
        commit: retained,
        local_planning_basis: before,
    };
    let inspected = inspect_exchange(
        &id,
        &binding,
        &intent,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    refuses_git_drift_after_inspection(&repository, &mut store, &id, &inspected, now).await;
    let receipt = apply_exchange(
        &mut store,
        &inspected,
        false,
        now,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        receipt.tasks,
        [] as [crate::task_sync::ledger::ImportedTask; 0]
    );
    assert!(store.verify_all().unwrap().is_healthy());
    let mut changed_binding = binding.clone();
    changed_binding.branch = "refs/heads/other".into();
    assert!(
        inspect_exchange(
            &id,
            &changed_binding,
            &intent,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("branch changed")
    );
    git(
        &repository,
        &[
            "config",
            "--local",
            "remote.origin.url",
            "https://example.invalid/changed.git",
        ],
    )
    .await;
    assert!(
        inspect_exchange(
            &id,
            &binding,
            &intent,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("target configuration changed")
    );
    git(
        &repository,
        &[
            "config",
            "--local",
            "remote.origin.url",
            "https://example.invalid/tasks.git",
        ],
    )
    .await;
    let mut changed_tree = tree;
    changed_tree.project.project_id = ProjectId("different-project".into());
    std::fs::write(
        repository.join(".engram/project.json"),
        &changed_tree.documents().unwrap()["project.json"],
    )
    .unwrap();
    let changed_oid = commit(&repository).await;
    let mut changed_intent = intent;
    changed_intent.operation = uuid::Uuid::new_v4();
    changed_intent.key = changed_intent.operation.to_string();
    changed_intent.expected_head = Some(changed_oid.clone());
    changed_intent.candidate = retention::retain_prepared(
        &binding.repository.git_directory,
        changed_intent.operation,
        &prepared,
    )
    .unwrap();
    changed_intent.commit = crate::task_sync::exchange::retain_commit(
        &repository,
        changed_intent.operation,
        &changed_oid,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    assert!(
        inspect_exchange(
            &id,
            &binding,
            &changed_intent,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("task blob differs")
    );
}

/// Git state that changes after inspection is observed again before every
/// durable transition: each drift refuses and records nothing, and a restored
/// state is admitted. Leaves the request prepared, its pin in place.
async fn refuses_git_drift_after_inspection(
    repository: &std::path::Path,
    store: &mut SqliteStore,
    id: &ObjectId,
    inspected: &InspectedExchange,
    now: chrono::DateTime<chrono::Utc>,
) {
    let pin = format!(
        "refs/engram/task-sync/exchange/{}/commit",
        inspected.intent.operation
    );
    let ledger_before = store.task_sync_exchange_events(id).unwrap();
    git(repository, &["update-ref", "-d", &pin]).await;
    assert!(
        prepare_exchange(
            store,
            inspected,
            now,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("retained commit ref is missing or changed")
    );
    let absent = "0".repeat(inspected.intent.commit.oid.len());
    git(
        repository,
        &["update-ref", &pin, &inspected.intent.commit.oid, &absent],
    )
    .await;
    git(repository, &["symbolic-ref", "HEAD", "refs/heads/other"]).await;
    assert!(
        prepare_exchange(
            store,
            inspected,
            now,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("selected branch changed")
    );
    git(repository, &["symbolic-ref", "HEAD", "refs/heads/main"]).await;
    git(
        repository,
        &[
            "config",
            "--local",
            "remote.origin.url",
            "https://example.invalid/moved.git",
        ],
    )
    .await;
    assert!(
        prepare_exchange(
            store,
            inspected,
            now,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("target configuration changed")
    );
    git(
        repository,
        &[
            "config",
            "--local",
            "remote.origin.url",
            "https://example.invalid/tasks.git",
        ],
    )
    .await;
    // The selected branch itself moving off the inspected head also refuses.
    let inspected_head = inspected.intent.expected_head.clone().unwrap();
    git(
        repository,
        &[
            "-c",
            "user.name=Exchange Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "--no-gpg-sign",
            "-m",
            "branch moved after inspection",
        ],
    )
    .await;
    assert!(
        prepare_exchange(
            store,
            inspected,
            now,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("selected branch head changed")
    );
    git(
        repository,
        &["update-ref", "refs/heads/main", &inspected_head],
    )
    .await;
    assert_eq!(store.task_sync_exchange_events(id).unwrap(), ledger_before);
    prepare_exchange(
        store,
        inspected,
        now,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap();
    git(repository, &["update-ref", "-d", &pin]).await;
    assert!(
        apply_exchange(
            store,
            inspected,
            false,
            now,
            RunLimits::default(),
            &Cancellation::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("retained commit ref is missing or changed")
    );
    git(
        repository,
        &["update-ref", &pin, &inspected.intent.commit.oid, &absent],
    )
    .await;
}
