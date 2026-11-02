use super::*;
use crate::verbs::AgentVerbs;
use crate::work_service::{LocalWorkService, MAX_FOCUS_RELATIONS, WorkNextQuery};
use std::sync::Arc;

/// Record a real ordinary registered check, then retain a typed environment
/// link to exercise paging of historical evidence without a turn fixture.
fn record_checked_environment(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
) -> (ObjectId, ObjectId, ObjectId) {
    use crate::domain::{
        NamedRootBindingKind, RegisteredVerificationAssociation, RegisteredVerificationCheck,
        RegisteredVerificationInput, RegisteredVerificationInputOutcome,
        RegisteredVerificationOutcome, RegisteredVerificationPostRun,
        RegisteredVerificationRequest, RegisteredVerificationRoot, RegisteredVerificationRun,
    };
    use crate::storage::HostSessionAuthority;
    let run = load_work_run(&store.connection, claim.run_id).unwrap();
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let session = claim.holder.clone();
    let token = store.resume_work_connection(&session, at(10)).unwrap();
    let mut host = actor(&session.0);
    host.run_id = Some(run.run_id.0.to_string());
    store
        .bind_named_root(
            &work.project_id,
            &session,
            &token,
            HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            "workspace-page",
            1,
            at(10),
            NamedRootBindingKind::Bound,
            None,
            &mut host,
            "name-page",
            at(10),
        )
        .unwrap();
    let request = RegisteredVerificationRequest {
        operation_key: "termal-run:test-evidence-page".into(),
        registration: RegisteredVerificationAssociation {
            host_run_id: "test-evidence-page".into(),
            request_identity: "page-request".into(),
            registered_at: at(11),
            initiating_session_id: session.clone(),
            owning_session_id: None,
            binding: binding.clone(),
            root: RegisteredVerificationRoot {
                workspace_id: "workspace-page".into(),
                generation: 1,
                state: crate::domain::SourceRootState::Named,
            },
        },
        input: RegisteredVerificationInput::Commit {
            commit: "a".repeat(40),
        },
        run: RegisteredVerificationRun {
            mode: "focused".into(),
            command: "cargo test".into(),
            started_at: at(12),
            finished_at: at(15),
            outcome: RegisteredVerificationOutcome::Passed,
        },
        checks: vec![RegisteredVerificationCheck {
            stage: "focused".into(),
            check_kind: VerificationKind::Test,
            command: "cargo test".into(),
            check_fingerprint: ObjectId::from_canonical_bytes(b"cargo test"),
            started_at: Some(at(12)),
            completed_at: Some(at(15)),
            outcome: RegisteredVerificationOutcome::Passed,
            summary: Some("host registered the tests".into()),
            refs: vec!["command:cargo-test".into()],
            counts: None,
        }],
        post_run: RegisteredVerificationPostRun {
            outcome: RegisteredVerificationInputOutcome::Clean,
            clean_before: true,
            clean_after: true,
            dirty_paths: vec![],
            disposition: None,
        },
    };
    let receipt = store
        .record_registered_verification(
            &work.project_id,
            &session,
            &token,
            HostSessionAuthority::Work,
            &host,
            &request,
            at(16),
        )
        .unwrap();
    let original = receipt.verifications[0].record_id.clone();
    let mut verification: VerificationEvidence =
        super::super::super::feeds::load_typed_work_object(
            &store.connection,
            &original,
            "verification_evidence",
        )
        .unwrap();
    let components = EnvironmentComponents {
        toolchain: "rustc-test".into(),
        sandbox: None,
        workspace_id: "workspace-page".into(),
        capability_map_revision: 1,
    };
    let environment = crate::EnvironmentEvidence {
        schema_version: crate::domain::SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding,
        session_id: session,
        source_basis: verification.source_basis.clone(),
        environment_fingerprint: CanonicalObject::freeze(&components).unwrap().key().clone(),
        components: Some(components),
        observed_at: at(15),
        actor: host,
        recorded_at: at(17),
    };
    let transaction = store.connection.transaction().unwrap();
    let environment_id = super::super::super::completion::append_control_environment_evidence_on(
        &transaction,
        &environment,
    )
    .unwrap();
    verification.environment = Some(environment_id.clone());
    verification.recorded_at = at(17);
    let verification_id = super::super::super::completion::append_control_verification_evidence_on(
        &transaction,
        &verification,
    )
    .unwrap();
    transaction.commit().unwrap();
    (environment_id, verification_id, original)
}

// More records than the page holds: the page keeps the newest by run-feed
// position, so the oldest generic records drop out. The verification at the
// boundary keeps its environment, which lies beyond it, and both come first;
// show and rich next select the same rows, and the latest record is reported
// on its own.
#[test]
fn the_evidence_page_keeps_the_newest_records_and_each_verifications_environment() {
    let directory = crate::test_support::temp_home().expect("temporary home");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("project-evidence-page", "create", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create work");
    let claim = claim(&mut store, &work, "runner", "claim", 1, 3_600);
    let oldest = evidence(&mut store, &work, &claim, "runner", "oldest", 2);
    let (environment, verification, original_verification) =
        record_checked_environment(&mut store, &work, &claim);
    // Seven newer generic records leave the verification as the eighth
    // newest and its environment, recorded just before it, as the ninth.
    let newer = (0..7)
        .map(|index| {
            let current = store.get_work_item(work.work_id).expect("current item");
            evidence(
                &mut store,
                &current,
                &claim,
                "runner",
                &format!("newer-{index}"),
                20 + index,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        newer.len() + 1,
        MAX_FOCUS_RELATIONS,
        "the verification sits at the boundary"
    );
    let mut expected = vec![environment, verification];
    expected.extend(newer.iter().rev().take(MAX_FOCUS_RELATIONS - 2).cloned());
    let omitted = [oldest, original_verification, newer[0].clone()];
    assert!(expected.iter().all(|kept| !omitted.contains(kept)));
    drop(store);

    let service = Arc::new(LocalWorkService::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        claim.holder.clone(),
        None,
    ));
    let ids = |items: &[crate::work_service::WorkEvidenceSummary]| {
        items
            .iter()
            .map(|item| item.evidence.clone())
            .collect::<Vec<_>>()
    };

    // The default show reads this view.
    let shown = service
        .work_focus_for_agent(&work.short_ref, at(40))
        .expect("show view");
    assert_eq!(ids(&shown.evidence_items), expected);
    assert_eq!(shown.evidence_count, MAX_FOCUS_RELATIONS + omitted.len());
    assert_eq!(
        shown
            .latest_evidence_item
            .as_ref()
            .map(|item| &item.evidence),
        newer.last(),
        "the latest record is reported on its own"
    );

    // Rich next selects the same rows for the focused item.
    service
        .work_focus(&work.short_ref, at(41))
        .expect("focus the item");
    let next = service
        .work_next(5, WorkNextQuery::default(), at(42))
        .expect("rich next");
    let focus = next.focus.expect("next carries the focus");
    assert_eq!(ids(&focus.evidence_items), expected);
    assert_eq!(focus.evidence_count, MAX_FOCUS_RELATIONS + omitted.len());

    // The show word reports the two omitted records.
    let words = AgentVerbs::with_shared_service(service, "runner".into(), claim.holder.clone());
    let rendered = words.show(&work.short_ref, at(43)).expect("show word");
    assert_eq!(
        rendered.value["notes"].as_array().expect("notes").len(),
        MAX_FOCUS_RELATIONS
    );
    assert_eq!(rendered.value["notes_omitted"], omitted.len());
}

// A projected evidence row with no run-feed entry has no position to order
// by. It is refused, not silently left off the page, even when it is the
// oldest row and newer ones fill the page.
#[test]
fn run_evidence_without_a_run_feed_position_is_refused() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("project-evidence-position", "create", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create work");
    let claim = claim(&mut store, &work, "runner", "claim", 1, 3_600);
    let recorded = (0..=MAX_FOCUS_RELATIONS)
        .map(|index| {
            let current = store.get_work_item(work.work_id).expect("current item");
            evidence(
                &mut store,
                &current,
                &claim,
                "runner",
                &format!("evidence-{index}"),
                2 + i64::try_from(index).expect("small index"),
            )
        })
        .collect::<Vec<_>>();
    store
        .work_run_evidence_projection(claim.run_id, MAX_FOCUS_RELATIONS)
        .expect("an intact projection reads");
    store
        .connection
        .execute(
            "DELETE FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
            params![claim.run_id.0.to_string(), recorded[0].as_str()],
        )
        .expect("drop the oldest row's run-feed entry");
    assert!(matches!(
        store.work_run_evidence_projection(claim.run_id, MAX_FOCUS_RELATIONS),
        Err(StoreError::InvalidWorkProjection(message)) if message.contains("run-execution feed")
    ));
}
