use super::*;
use crate::domain::{ControlWorkBinding, ExecutionSourceBasis, VerificationEvidence};
use crate::storage::work::test_support::host_check_producer;
use crate::task_sync::evidence::{EvidencePack, RecordKind};

fn proof_fixture(summary: &str, refs: Vec<String>, author: &str) -> Fixture {
    let mut f = fixture();
    // A separate run has no ordinary note that could mask a proof-only bypass.
    let item = f
        .store
        .create_work(
            &root_request(&project().0, "proof-only", 5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut f.store, &item, "runner", "proof-claim", 6, 600);
    let run = crate::storage::work::query::load_work_run(&f.store.connection, held.run_id).unwrap();
    let basis = ExecutionSourceBasis {
        workspace_id: "proof-workspace".into(),
        source_revision: "observed-source".into(),
        git_tree: None,
        source_root_generation: None,
        source_root_state: None,
    };
    let tx = f.store.connection.transaction().unwrap();
    let producer =
        host_check_producer(&tx, &item, &held, "runner", "proof-check", 7, basis.clone());
    let verification = VerificationEvidence {
        applicability: None,
        schema_version: crate::domain::SCHEMA_VERSION,
        project_id: project(),
        binding: ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: item.work_id,
            run_id: run.run_id,
            work_revision: held.accepted_work_revision,
            claim_id: held.claim_id,
            claim_fence: held.fence,
        },
        session_id: SessionId("runner".into()),
        producer_observation: producer,
        source_basis: basis.into(),
        environment: None,
        check_kind: crate::domain::VerificationKind::Test,
        check_fingerprint: crate::storage::work::test_support::check_fingerprint("proof-check"),
        result: crate::domain::VerificationResult::Passed,
        completed_at: at(7),
        summary: summary.into(),
        refs,
        actor: actor(author),
        recorded_at: at(7),
        bound_from: None,
    };
    let id = crate::storage::work::completion::append_control_verification_evidence_on(
        &tx,
        &verification,
    )
    .unwrap();
    tx.commit().unwrap();
    f.work = item.work_id;
    f.original = id.clone();
    f.bytes = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    f.request.roots = vec![id];
    let feed = FeedId::RunExecution(run.run_id);
    let through = f.store.work_feed_head(&feed).unwrap();
    f.request.feeds = vec![NativeEvidenceFeed { feed, through }];
    f
}

// Recreate a capture retained by an earlier build that did not check proof prose.
// Original objects are appended once through their writers, never edited.
fn retain_historical_capture(f: &mut Fixture) -> super::super::NativeEvidenceCapture {
    use super::super::{Capture, Selection, context, prepare_packs};
    let project = project();
    let mut selected = Selection::new(&f.store.connection, &project, &f.request);
    selected.pending.extend(f.request.roots.clone());
    let feed_cuts = context::feeds(&mut selected).unwrap();
    selected.close().unwrap();
    let governing = context::governing(&mut selected).unwrap();
    selected.close().unwrap();
    let packs = prepare_packs(&project, &f.request.origin, selected.records.values(), 1).unwrap();
    let created = selected.created;
    let mut tx = f.store.connection.transaction().unwrap();
    for record in &created {
        super::super::super::records::insert_on(&tx, record).unwrap();
    }
    let ids = packs.iter().map(|_| ObjectId::mint()).collect::<Vec<_>>();
    super::super::super::retain_batch_on(
        &mut tx,
        &project,
        &packs.iter().zip(&ids).collect::<Vec<_>>(),
    )
    .unwrap();
    let captured = Capture {
        project,
        request: f.request.clone(),
        actor: actor("collector"),
        packs: packs.iter().map(|p| p.pack_id().clone()).collect(),
        feed_cuts,
        governing,
        disclosure_labels: std::collections::BTreeMap::new(),
    };
    let object = crate::CanonicalObject::identified(&f.request.capture, &captured).unwrap();
    SqliteStore::insert_object(&tx, super::super::CAPTURE_KIND, &object).unwrap();
    tx.commit().unwrap();
    captured.receipt(false)
}

#[test]
fn proof_disclosure_refuses_capture_and_historical_export_atomically() {
    for (notes, summary, refs, author) in [
        (false, "Safe proof", vec![], "runner"),
        (true, "See src/private.rs", vec![], "runner"),
        (true, "Safe proof", vec![], "src/private.rs"),
        (true, "Safe proof", vec!["command:private".into()], "runner"),
    ] {
        let mut f = proof_fixture(summary, refs, author);
        f.request.profile.include_notes = notes;
        let before = test_database_shape_snapshot(&f.store.connection).unwrap();
        let error = f
            .store
            .capture_native_evidence(
                &project(),
                &SessionId("collector".into()),
                &actor("collector"),
                &f.request,
                &DevelopmentNoopRedactor,
            )
            .expect_err("proof disclosure must refuse capture");
        assert!(error.to_string().contains("disclosure refused"), "{error}");
        assert_eq!(
            test_database_shape_snapshot(&f.store.connection).unwrap(),
            before
        );
        assert!(
            f.store
                .capture_evidence_packs(&project())
                .unwrap()
                .is_none()
        );

        let historical = retain_historical_capture(&mut f);
        let before = test_database_shape_snapshot(&f.store.connection).unwrap();
        let mut proof_page = false;
        for after in 0..historical.packs.len() {
            let pack = super::super::super::load_retained_pack_on(
                &f.store.connection,
                &project(),
                &historical.packs[after],
            )
            .unwrap();
            if pack
                .records()
                .iter()
                .any(|r| r.kind() == RecordKind::VerificationEvidence)
            {
                proof_page = true;
                let cursor = super::super::NativeEvidenceCursor {
                    after,
                    ..historical.cursor.clone()
                };
                let error = f
                    .store
                    .export_native_evidence_capture(&project(), &cursor, 1, &f.request.profile)
                    .err()
                    .expect("proof-only page must refuse export");
                assert!(error.to_string().contains("disclosure refused"), "{error}");
            }
        }
        assert!(proof_page);
        assert_eq!(
            test_database_shape_snapshot(&f.store.connection).unwrap(),
            before
        );
        let bytes: Vec<u8> = f
            .store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [f.original.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bytes, f.bytes);
    }
}

#[test]
fn explicit_proof_disclosure_preserves_original_bytes_and_replay() {
    let mut f = proof_fixture("The observed checks passed", vec![], "runner");
    let captured = capture(&mut f);
    let page = f
        .store
        .export_native_evidence_capture(&project(), &captured.cursor, 32, &f.request.profile)
        .unwrap();
    let original = page
        .packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|r| r.object_id() == &f.original)
        .unwrap();
    assert_eq!(original.canonical_bytes(), f.bytes);
    let before = test_database_shape_snapshot(&f.store.connection).unwrap();
    assert!(capture(&mut f).replayed);
    assert_eq!(
        test_database_shape_snapshot(&f.store.connection).unwrap(),
        before
    );
}
