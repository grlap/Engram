use super::*;
use crate::task_sync::{delta::DeltaCommitSelection, exchange, ledger};

pub(super) fn binding(
    service: &LocalWorkService,
    intent: &task_sync::delta::DeltaIntent,
) -> ledger::Binding {
    ledger::Binding {
        schema_version: 1,
        project: service.project_id.clone(),
        store: ledger::StoreBasis {
            database: service.database.clone(),
            origin: Some(intent.local_cut.origin.clone()),
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: ledger::RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: service
                .database
                .parent()
                .unwrap()
                .join("missing-data-worktree"),
            git_directory: service.database.parent().unwrap().join("missing-git-admin"),
        },
        branch: "refs/heads/engram-data".into(),
        target: ledger::TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/engram-data".into(),
            configuration_fingerprint: intent.producer.clone(),
        },
        profile: empty_folder().profile,
        supersedes: None,
        superseded_tail: None,
        actor: intent.actor.clone(),
        created_at: chrono::Utc::now(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delta_commit_service_refuses_opt_out_read_only_and_foreign_scope_before_store_or_git() {
    for case in [
        "absent",
        "disabled",
        "malformed",
        "oversized",
        "read_only",
        "project",
        "feed",
        "binding",
        "actor",
        "session",
    ] {
        let owned = crate::test_support::temp_home().unwrap();
        let database = crate::project_database_path(
            &owned.path().join("home"),
            &ProjectId("service/opt-in".into()),
        );
        let mut service = service(&database);
        match case {
            "absent" | "read_only" => (),
            "malformed" | "oversized" => {
                fs::create_dir_all(database.parent().unwrap()).unwrap();
                let bytes = if case == "malformed" {
                    b"not JSON".to_vec()
                } else {
                    vec![b' '; 128 * 1024]
                };
                fs::write(
                    database.parent().unwrap().join("task-sync-setting.json"),
                    bytes,
                )
                .unwrap();
            }
            _ => {
                service
                    .task_sync_settings(
                        &task_sync::settings::Request {
                            action: if case == "disabled" {
                                task_sync::settings::Action::Disable
                            } else {
                                task_sync::settings::Action::Enable
                            },
                            user_instruction: (case != "disabled")
                                .then(|| "Enable Git task sync for this test fixture.".into()),
                        },
                        chrono::Utc::now(),
                    )
                    .unwrap();
            }
        }
        if case == "read_only" {
            service = service.into_read_only();
        }
        let mut intent = delta_export_request(&service);
        let mut bound = binding(&service, &intent);
        let mut binding_id = intent.local_cut.binding.clone();
        match case {
            "project" => bound.project = ProjectId("another/project".into()),
            "feed" => {
                intent.local_cut.position.feed =
                    crate::FeedId::Project(ProjectId("another/project".into()));
            }
            "binding" => binding_id = crate::ObjectId::mint(),
            "actor" => intent.actor.actor_id = "another actor".into(),
            "session" => intent.actor.session_id = Some(SessionId("another session".into())),
            _ => (),
        }
        let fingerprint = crate::CanonicalObject::freeze(&(intent.operation, "retained commit"))
            .unwrap()
            .key()
            .clone();
        let selection = DeltaCommitSelection {
            prepared: crate::ObjectId::mint(),
            commit: ledger::RetainedCommit {
                oid: fingerprint.to_string(),
                tree: fingerprint.to_string(),
                parents: Vec::new(),
                retention_ref: format!(
                    "refs/engram/task-sync/exchange/{}/commit",
                    intent.operation
                ),
                raw_bytes_fingerprint: fingerprint,
            },
        };
        let before = entries(owned.path());
        exchange::test_support::LAUNCHES.with(|count| count.set(0));
        let error = service
            .record_task_sync_delta_export_commit(
                &binding_id,
                &bound,
                &intent,
                &selection,
                exchange::RunLimits::default(),
                &exchange::Cancellation::default(),
            )
            .await
            .unwrap_err();
        let expected = match case {
            "read_only" => "read-only",
            "absent" | "disabled" | "malformed" | "oversized" => "task sync is not enabled",
            _ => "belongs to another project, binding, actor or session",
        };
        assert!(error.to_string().contains(expected), "{case}: {error}");
        let request = exchange::DeltaPublicationRequest {
            binding_id: &binding_id,
            binding: &bound,
            intent: &intent,
            selection: &selection,
            progress: ledger::OutgoingProgress::PushStarted,
        };
        let error = service
            .record_task_sync_delta_publication(
                &request,
                exchange::RunLimits::default(),
                &exchange::Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "publication {case}: {error}"
        );
        let outgoing = exchange::DeltaOutgoingSelection {
            binding_id: &binding_id,
            binding: &bound,
            intent: &intent,
            commit: &selection,
        };
        let error = service
            .publish_task_sync_delta_outgoing(
                &outgoing,
                exchange::RunLimits::default(),
                &exchange::Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains(expected),
            "dispatch {case}: {error}"
        );
        assert!(!database.exists(), "{case}");
        assert_eq!(entries(owned.path()), before, "{case}");
        exchange::test_support::LAUNCHES.with(|count| assert_eq!(count.get(), 0, "{case}"));
    }
}
