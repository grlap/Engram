use super::*;
use crate::task_sync::{
    driver::{DeltaRequest, StepRequest},
    exchange::{Cancellation, RunLimits},
};

#[tokio::test]
async fn exchange_step_rejects_short_overrides_before_store_or_path_creation() {
    let owned = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(
        &owned.path().join("missing"),
        &ProjectId("service/timeout".into()),
    );
    let service = service(&database);
    let request = StepRequest::InspectTarget {
        project_file: owned.path().join("missing-marker"),
        branch: "engram-data".into(),
        remote: "origin".into(),
        reference: "refs/heads/engram-data".into(),
    };
    let before = entries(owned.path());
    for seconds in [1, 30, 59] {
        let error = service
            .task_sync_exchange_step(
                &request,
                RunLimits {
                    deadline: std::time::Duration::from_secs(seconds),
                    ..RunLimits::default()
                },
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<crate::task_sync::exchange::RunError>(),
            Some(&crate::task_sync::exchange::RunError::InvalidLimits),
        );
        assert_eq!(entries(owned.path()), before);
    }
    for seconds in [60, 120] {
        let error = service
            .task_sync_exchange_step(
                &request,
                RunLimits {
                    deadline: std::time::Duration::from_secs(seconds),
                    ..RunLimits::default()
                },
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("task sync is not enabled"),
            "{error:#}"
        );
        assert_eq!(entries(owned.path()), before);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_capture_and_export_refuse_before_store_or_path_mutation_without_opt_in() {
    let owned = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(
        &owned.path().join("missing"),
        &ProjectId("service/opt-in".into()),
    );
    let service = service(&database);
    let capture = crate::ObjectId::mint();
    for request in [
        StepRequest::CaptureNative {
            binding: crate::ObjectId::mint(),
            request: Box::new(crate::storage::NativeEvidenceCaptureRequest {
                capture: capture.clone(),
                origin: crate::ObjectId::mint(),
                profile: crate::task_sync::progress::ExportProfile {
                    include_notes: false,
                    origin_machine: None,
                },
                roots: Vec::new(),
                feeds: Vec::new(),
                maximum_pack_records: 1,
            }),
        },
        StepRequest::ExportNative {
            binding: crate::ObjectId::mint(),
            project_file: owned.path().join("missing-marker"),
            branch: "engram-data".into(),
            cursor: crate::storage::NativeEvidenceCursor { capture, after: 0 },
            maximum_packs: 1,
        },
    ] {
        let before = entries(owned.path());
        let error = service
            .task_sync_exchange_step(&request, RunLimits::default(), &Cancellation::default())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("task sync is not enabled"));
        assert_eq!(entries(owned.path()), before);
    }
}

fn imported_selections() -> Vec<StepRequest> {
    use crate::domain::{ControlWorkBinding, VerificationKind, VerificationRequirement};
    use crate::task_sync::evidence::{
        acceptance::{AcceptanceUseRequest, HistoricalProof},
        consumption::ConsumptionRequest,
        verification::ApplicabilityRequest,
    };
    let binding = ControlWorkBinding {
        root_execution_id: crate::RootExecutionId::new(),
        work_id: crate::WorkId::new(),
        run_id: crate::WorkRunId::new(),
        work_revision: 1,
        claim_id: crate::WorkClaimId::new(),
        claim_fence: 1,
    };
    let consumed = ConsumptionRequest {
        consumption: crate::ObjectId::mint(),
        decision: crate::ObjectId::mint(),
        binding: binding.clone(),
    };
    vec![
        StepRequest::QualifyVerification {
            request: Box::new(ApplicabilityRequest {
                original: crate::ObjectId::mint(),
                origin: crate::ObjectId::mint(),
                origin_cut: crate::ObjectId::mint(),
                binding: binding.clone(),
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
                criterion: None,
                decision: crate::ObjectId::mint(),
            }),
        },
        StepRequest::QualifyAcceptance {
            request: Box::new(AcceptanceUseRequest {
                decision: crate::ObjectId::mint(),
                origin: crate::ObjectId::mint(),
                proof: HistoricalProof::NativeCompletion {
                    seal: crate::ObjectId::mint(),
                    governing_witness: None,
                },
                origin_cuts: vec![crate::ObjectId::mint()],
                binding,
                verification_decisions: Vec::new(),
            }),
        },
        StepRequest::ConsumeVerification {
            request: Box::new(consumed.clone()),
        },
        StepRequest::ConsumeAcceptance {
            request: Box::new(consumed),
        },
    ]
}

#[tokio::test(flavor = "current_thread")]
async fn imported_steps_refuse_before_store_creation_without_opt_in_or_on_read_only_service() {
    let owned = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(
        &owned.path().join("missing"),
        &ProjectId("service/opt-in".into()),
    );
    for read_only in [false, true] {
        let mut service = service(&database);
        if read_only {
            service = service.into_read_only();
        }
        for request in imported_selections() {
            let error = service
                .task_sync_exchange_step(&request, RunLimits::default(), &Cancellation::default())
                .await
                .unwrap_err();
            let expected = if read_only {
                "read-only"
            } else {
                "task sync is not enabled"
            };
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!database.parent().unwrap().exists());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn exchange_step_refuses_mutations_before_store_and_repository_access() {
    for case in [
        "read_only",
        "opt_out",
        "foreign_actor",
        "foreign_feed",
        "unknown_binding",
    ] {
        let owned = crate::test_support::temp_home().unwrap();
        let database = crate::project_database_path(
            &owned.path().join("home"),
            &ProjectId("service/opt-in".into()),
        );
        let mut service = service(&database);
        if !matches!(case, "read_only" | "opt_out") {
            service
                .task_sync_settings(
                    &task_sync::settings::Request {
                        action: task_sync::settings::Action::Enable,
                        user_instruction: Some("Enable Git task sync for this fixture.".into()),
                    },
                    chrono::Utc::now(),
                )
                .unwrap();
        }
        if case == "read_only" {
            service = service.into_read_only();
        }
        let mut intent = delta_export_request(&service);
        if case == "foreign_actor" {
            intent.actor.actor_id = "foreign actor".into();
        }
        if case == "foreign_feed" {
            intent.local_cut.position.feed =
                crate::FeedId::Project(ProjectId("foreign project".into()));
        }
        let before = entries(owned.path());
        let request = DeltaRequest {
            binding: intent.local_cut.binding.clone(),
            intent,
        };
        for request in [
            StepRequest::ApplyDelta {
                request: Box::new(request.clone()),
            },
            StepRequest::RetainIncomingDelta {
                request: Box::new(request),
            },
        ] {
            let error = service
                .task_sync_exchange_step(&request, RunLimits::default(), &Cancellation::default())
                .await
                .unwrap_err();
            let expected = match case {
                "read_only" => "read-only",
                "opt_out" => "task sync is not enabled",
                "foreign_actor" => "another actor or session",
                "foreign_feed" => "another project or binding",
                _ => "not initialized",
            };
            assert!(error.to_string().contains(expected), "{case}: {error}");
            assert_eq!(entries(owned.path()), before, "{case}");
            assert!(!database.exists());
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn exchange_historical_read_does_not_enable_sync_or_create_a_store() {
    let owned = crate::test_support::temp_home().unwrap();
    let database = crate::project_database_path(
        &owned.path().join("missing"),
        &ProjectId("service/opt-in".into()),
    );
    let service = service(&database).into_read_only();
    for request in [
        StepRequest::ReadBinding {
            binding: crate::ObjectId::mint(),
        },
        StepRequest::VerificationSummary {
            decision: crate::ObjectId::mint(),
        },
        StepRequest::AcceptanceSummary {
            decision: crate::ObjectId::mint(),
        },
    ] {
        let error = service
            .task_sync_exchange_step(&request, RunLimits::default(), &Cancellation::default())
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<crate::storage::StoreError>(),
            Some(crate::storage::StoreError::StoreNotInitialized)
        ));
        assert!(!database.parent().unwrap().exists());
    }
}
