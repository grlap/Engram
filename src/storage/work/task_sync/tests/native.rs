use super::*;
use crate::storage::work::test_support::{checkpoint, complete, delegated, evidence};

fn native_tasks(source: &mut SqliteStore, project: &ProjectId) -> Vec<WorkItem> {
    let redactor = crate::DevelopmentNoopRedactor;
    let mut works = Vec::new();
    for name in ["completed", "blocked", "cancelled", "superseded", "bound"] {
        let mut request = root_request(&project.0, name, 0);
        request.title = name.into();
        request.kind = if name == "blocked" {
            WorkItemKind::Bug
        } else {
            WorkItemKind::Task
        };
        if name == "bound" {
            request.acceptance_bindings = vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: Some(ObjectId::from_canonical_bytes(b"native-check")),
                },
            }];
        }
        works.push(source.create_work(&request, &redactor).unwrap());
    }
    let completed = &works[0];
    let live = claim(source, completed, "native-completer", "claim", 1, 100);
    let evidence = evidence(source, completed, &live, "native-completer", "evidence", 2);
    checkpoint(
        source,
        completed,
        &live,
        "native-completer",
        "checkpoint",
        3,
        std::slice::from_ref(&evidence),
    );
    complete(
        source,
        completed,
        &live,
        "native-completer",
        &evidence,
        "complete",
        4,
    )
    .unwrap();
    works
}

#[test]
fn task_sync_native_history_blockers_disposals_and_bindings_round_trip() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("native-sync".into());
    let redactor = crate::DevelopmentNoopRedactor;
    let works = native_tasks(&mut source, &project);
    let completed = &works[0];
    let blocker = source
        .add_work_blocker(
            &crate::domain::AddWorkBlockerRequest {
                work_id: works[1].work_id,
                expected_work_revision: works[1].revision,
                kind: WorkBlockerKind::ExternalInput,
                detail: "native input missing".into(),
                authority: delegated(&project.0, "native-blocker"),
                actor: actor("native-blocker"),
                idempotency_key: "block".into(),
                blocked_at: at(5),
            },
            &redactor,
        )
        .unwrap();
    for (index, disposition) in [
        (2, crate::WorkDisposition::Cancelled),
        (3, crate::WorkDisposition::Superseded),
    ] {
        source
            .dispose_work(
                &crate::domain::DisposeWorkRequest {
                    work_id: works[index].work_id,
                    expected_work_revision: works[index].revision,
                    disposition,
                    replacement_id: (index == 3).then_some(works[4].work_id),
                    reason: format!("native disposal {index}"),
                    actor: actor("native-disposer"),
                    idempotency_key: format!("dispose-{index}"),
                    disposed_at: at(6),
                },
                &redactor,
            )
            .unwrap();
    }
    let tree = source.export_task_tree(&project).unwrap();
    let task = |id| tree.tasks.iter().find(|task| task.id == id).unwrap();
    assert_eq!(
        task(completed.work_id)
            .completion
            .as_ref()
            .unwrap()
            .actor
            .actor_id,
        "native-completer"
    );
    assert_eq!(
        task(works[1].work_id).blockers[0].blocker_id,
        blocker.blocker_id
    );
    assert_eq!(
        task(works[1].work_id).blockers[0].created_by.actor_id,
        "native-blocker"
    );
    assert_eq!(
        task(works[2].work_id).disposal_reason.as_deref(),
        Some("native disposal 2")
    );
    assert_eq!(task(works[3].work_id).superseded_by, Some(works[4].work_id));
    assert_eq!(
        task(works[4].work_id).acceptance_bindings,
        works[4].acceptance_bindings
    );
    let bytes = tree.documents().unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &TaskTree::from_documents(&bytes).unwrap());
    assert_eq!(
        target
            .export_task_tree(&project)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );
    assert!(target.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_external_imported_origin_is_refused_without_side_effects() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("external".into());
    let input = crate::domain::WorkImportInput {
        snapshot: crate::WorkSourceSnapshot {
            schema_version: crate::schema::SCHEMA_VERSION,
            adapter_kind: "planner".into(),
            canonical_ref: "plan/item".into(),
            projected: crate::WorkSourceProjection {
                title: Some("external".into()),
                body: None,
                status: None,
                owner: None,
            },
            captured_at: at(0),
            source_revision: None,
            fingerprint: "source revision".into(),
            canonical_url: None,
            payload_hash: ObjectId::from_canonical_bytes(b"external plan"),
            raw: std::collections::BTreeMap::new(),
        },
        draft: Some(crate::domain::WorkImportDraft {
            title: "authored".into(),
            outcome: "outcome".into(),
            acceptance: Vec::new(),
        }),
    };
    let preview = source.preview_work_import(&project, &input, at(0)).unwrap();
    source
        .apply_work_import(
            &project,
            &input,
            &preview.preview_token,
            &actor("author"),
            at(1),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&source);
    let error = source.export_task_tree(&project).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("external imported origin and source snapshot")
    );
    assert_eq!(refusal_code(&error), "task_sync_origin_mismatch");
    assert_eq!(state(&source), before);
    let mut tree = candidate();
    tree.project.project_id = project.clone();
    for dry_run in [true, false] {
        let error = source
            .import_task_tree(&project, &actor("importer"), &tree, dry_run, at(5))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("existing native execution state")
        );
        assert_eq!(state(&source), before);
    }
}

#[test]
fn task_sync_native_history_preserves_untrimmed_actor_principals() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("historical-actors".into());
    let redactor = crate::DevelopmentNoopRedactor;
    let works = native_tasks(&mut source, &project);
    let mut historical = actor("native-blocker-session");
    historical.actor_id = " historical actor \n".into();
    historical.actor_kind = " test_agent ".into();
    let work = &works[2];
    let live = claim(
        &mut source,
        work,
        "completion-session",
        "historical-claim",
        5,
        100,
    );
    let proof = evidence(
        &mut source,
        work,
        &live,
        "completion-session",
        "historical-evidence",
        6,
    );
    checkpoint(
        &mut source,
        work,
        &live,
        "completion-session",
        "historical-checkpoint",
        7,
        std::slice::from_ref(&proof),
    );
    let mut completion = crate::storage::work::test_support::completion_request(
        work,
        &live,
        "completion-session",
        &proof,
        "historical-completion",
        8,
    );
    completion.actor.actor_id.clone_from(&historical.actor_id);
    completion
        .actor
        .actor_kind
        .clone_from(&historical.actor_kind);
    source.complete_work(&completion, &redactor).unwrap();
    source
        .add_work_blocker(
            &crate::domain::AddWorkBlockerRequest {
                work_id: works[1].work_id,
                expected_work_revision: works[1].revision,
                kind: WorkBlockerKind::ExternalInput,
                detail: "historically attributed blocker".into(),
                authority: delegated(&project.0, "native-blocker-session"),
                actor: historical.clone(),
                idempotency_key: "historical-blocker".into(),
                blocked_at: at(5),
            },
            &redactor,
        )
        .unwrap();
    let tree = source.export_task_tree(&project).unwrap();
    let blocker = &tree
        .tasks
        .iter()
        .find(|task| task.id == works[1].work_id)
        .unwrap()
        .blockers[0];
    assert_eq!(blocker.created_by, SharedActor::from_actor(&historical));
    assert_eq!(
        tree.tasks
            .iter()
            .find(|task| task.id == work.work_id)
            .unwrap()
            .completion
            .as_ref()
            .unwrap()
            .actor,
        SharedActor::from_actor(&historical)
    );
    let bytes = tree.documents().unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &TaskTree::from_documents(&bytes).unwrap());
    assert!(target.verify_all().unwrap().is_healthy());
    assert_eq!(
        target
            .export_task_tree(&project)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );

    let mut reported = candidate();
    let completion = reported
        .tasks
        .iter_mut()
        .find_map(|task| task.completion.as_mut())
        .unwrap();
    completion.actor = SharedActor::from_actor(&historical);
    let bytes = reported.documents().unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &reported);
    assert!(target.verify_all().unwrap().is_healthy());
    assert_eq!(
        target
            .export_task_tree(&reported.project.project_id)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );
}

#[test]
fn task_sync_imported_task_supports_native_planning_and_completion() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("post-import".into());
    let redactor = crate::DevelopmentNoopRedactor;
    let source_item = source
        .create_work(&root_request(&project.0, "source", 0), &redactor)
        .unwrap();
    let original = source.export_task_tree(&project).unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &original);
    let item = query::load_work_item(&target.connection, source_item.work_id).unwrap();
    assert!(item.restored && item.active_run_id.is_none());
    let imported_actor = item.created_by.clone();
    let item = target
        .revise_work(
            &crate::domain::ReviseWorkRequest {
                work_id: item.work_id,
                expected_revision: item.revision,
                patch: crate::domain::WorkRevisionPatch {
                    title: Some("native planning after import".into()),
                    labels: Some(vec!["native-change".into()]),
                    ..Default::default()
                },
                authority: delegated(&project.0, "native-executor"),
                actor: actor("native-executor"),
                idempotency_key: "post-import-revise".into(),
                updated_at: at(6),
            },
            &redactor,
        )
        .unwrap();
    assert_eq!(item.created_by, imported_actor);
    assert!(target.verify_all().unwrap().is_healthy());
    let live = target
        .claim_work(
            &crate::domain::ClaimWorkRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                expected_run_id: item.active_run_id,
                holder: crate::SessionId("native-executor".into()),
                ttl_seconds: 100,
                recovery_reason: None,
                actor: actor("native-executor"),
                idempotency_key: "post-import-claim".into(),
                claimed_at: at(7),
            },
            &redactor,
        )
        .unwrap();
    let item = query::load_work_item(&target.connection, item.work_id).unwrap();
    assert_eq!(item.active_run_id, Some(live.run_id));
    let proof = evidence(
        &mut target,
        &item,
        &live,
        "native-executor",
        "post-import-evidence",
        8,
    );
    checkpoint(
        &mut target,
        &item,
        &live,
        "native-executor",
        "post-import-checkpoint",
        9,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut target,
        &item,
        &live,
        "native-executor",
        &proof,
        "post-import-complete",
        10,
    )
    .unwrap();
    super::super::origin::DECODES.with(|count| count.set(0));
    let completed = query::load_work_item(&target.connection, item.work_id).unwrap();
    assert_eq!(
        super::super::origin::DECODES.with(std::cell::Cell::get),
        0,
        "native event reads do not decode the import origin"
    );
    assert_eq!(completed.lifecycle, WorkLifecycle::Completed);
    assert_eq!(completed.created_by, imported_actor);
    let health = target.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    target
        .save_work_graph_snapshot(
            &project,
            &actor("snapshot-operator"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(11),
            &redactor,
        )
        .unwrap();
    let exported = target.export_task_tree(&project).unwrap();
    let task = &exported.tasks[0];
    assert_eq!(task.id, source_item.work_id);
    assert_eq!(task.title, "native planning after import");
    assert_eq!(task.labels, ["native-change"]);
    assert_eq!(task.lifecycle, WorkLifecycle::Completed);
    assert_eq!(
        task.completion.as_ref().unwrap().actor.actor_id,
        "native-executor"
    );
    assert_ne!(exported.documents().unwrap(), original.documents().unwrap());
    let before = state(&target);
    for dry_run in [true, false] {
        let error = target
            .import_task_tree(&project, &actor("importer"), &exported, dry_run, at(12))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("existing native execution state")
        );
        assert_eq!(state(&target), before);
    }
}
