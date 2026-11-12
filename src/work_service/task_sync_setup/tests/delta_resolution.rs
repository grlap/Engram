use super::*;
use crate::task_sync::{delta::*, exchange, ledger::*};

fn input(service: &LocalWorkService) -> exchange::InspectedDeltaResolution {
    let original = delta_export_request(service);
    let binding = super::delta_commit::binding(service, &original);
    let commit = RetainedCommit {
        oid: original.producer.to_string(),
        tree: original.producer.to_string(),
        parents: Vec::new(),
        retention_ref: format!(
            "refs/engram/task-sync/exchange/{}/commit",
            original.operation
        ),
        raw_bytes_fingerprint: original.producer.clone(),
    };
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: service.project_id.clone(),
        operation: original.operation,
        output: original.candidate.output.clone(),
        documents: std::collections::BTreeMap::new(),
    };
    exchange::InspectedDeltaResolution {
        original: exchange::delta::InspectedDelta {
            binding_id: original.local_cut.binding.clone(),
            binding,
            intent: original.clone(),
            payload: payload.clone(),
        },
        selection: DeltaCommitSelection {
            prepared: crate::ObjectId::mint(),
            commit: commit.clone(),
        },
        recipe: OutgoingReconciliation::DeltaResolved {
            failure: OutgoingFailure::AncestryChanged {
                fetched_tip: commit.oid.clone(),
            },
            resolved: Box::new(original),
            payload: Box::new(payload),
            commit: Box::new(commit),
        },
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delta_resolution_service_refuses_before_store_or_git_for_all_three_routes() {
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
        "resolved_actor",
        "resolved_session",
        "resolved_feed",
        "resolved_binding",
        "resolved_project",
        "missing_recipe",
        "invalid_store",
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
                fs::write(
                    database.parent().unwrap().join("task-sync-setting.json"),
                    if case == "malformed" {
                        b"not JSON".to_vec()
                    } else {
                        vec![b' '; 128 * 1024]
                    },
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
        if case == "invalid_store" {
            fs::write(&database, b"not a SQLite database").unwrap();
            // This admitted scope reaches ordinary store opening, which keeps
            // its sidecar so every opener locks the same file. Include that
            // persistent guard in the fixture before checking unchanged bytes.
            fs::write(database.with_added_extension("open-lock"), b"").unwrap();
        }
        let mut input = input(&service);
        match case {
            "project" => input.original.binding.project = ProjectId("another/project".into()),
            "feed" => {
                input.original.intent.local_cut.position.feed =
                    crate::FeedId::Project(ProjectId("another/project".into()));
            }
            "binding" => input.original.binding_id = crate::ObjectId::mint(),
            "actor" => input.original.intent.actor.actor_id = "another actor".into(),
            "session" => {
                input.original.intent.actor.session_id = Some(SessionId("another session".into()));
            }
            "missing_recipe" => {
                input.recipe = OutgoingReconciliation::PushStarted {
                    reconciliation: crate::ObjectId::mint(),
                }
            }
            _ => (),
        }
        if let OutgoingReconciliation::DeltaResolved {
            resolved, payload, ..
        } = &mut input.recipe
        {
            match case {
                "resolved_actor" => resolved.actor.actor_id = "another actor".into(),
                "resolved_session" => {
                    resolved.actor.session_id = Some(SessionId("another session".into()));
                }
                "resolved_feed" => {
                    resolved.local_cut.position.feed =
                        crate::FeedId::Project(ProjectId("another/project".into()));
                }
                "resolved_binding" => resolved.local_cut.binding = crate::ObjectId::mint(),
                "resolved_project" => payload.project_id = ProjectId("another/project".into()),
                _ => (),
            }
        }
        let before = entries(owned.path());
        exchange::test_support::LAUNCHES.with(|count| count.set(0));
        let limits = exchange::RunLimits::default();
        let cancel = exchange::Cancellation::default();
        let errors = [
            service
                .record_task_sync_delta_resolution(&input, limits, &cancel)
                .await
                .unwrap_err(),
            service
                .record_task_sync_delta_retry_progress(
                    &input,
                    &OutgoingProgress::PushStarted,
                    limits,
                    &cancel,
                )
                .await
                .unwrap_err(),
            Box::pin(service.publish_task_sync_delta_retry(&input, limits, &cancel))
                .await
                .unwrap_err(),
        ];
        for error in errors {
            let expected = match case {
                "read_only" => "read-only",
                "absent" | "disabled" | "malformed" | "oversized" => "task sync is not enabled",
                "missing_recipe" => "resolved delta recipe missing",
                "invalid_store" => "not a database",
                _ => "belongs to another project, binding, actor or session",
            };
            assert!(error.to_string().contains(expected), "{case}: {error}");
        }
        assert_eq!(entries(owned.path()), before, "{case}");
        assert_eq!(database.exists(), case == "invalid_store", "{case}");
        exchange::test_support::LAUNCHES.with(|count| assert_eq!(count.get(), 0, "{case}"));
    }
}
