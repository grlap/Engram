use super::*;
use crate::storage::{NativeEvidenceCapture, NativeEvidenceCaptureRequest, NativeEvidenceFeed};
use crate::task_sync::{origin::EnrollRequest, progress::ExportProfile};

fn capture(
    f: &mut Fixture,
    request: &NativeEvidenceCaptureRequest,
) -> Result<NativeEvidenceCapture, StoreError> {
    f.store
        .capture_native_evidence(
            &f.project,
            &SessionId("collector".into()),
            &actor("collector"),
            request,
            &DevelopmentNoopRedactor,
        )
        .map_err(Into::into)
}

fn assert_receipts(
    f: &Fixture,
    packs: &[crate::task_sync::evidence::EvidencePack],
    ids: &[&ObjectId],
) {
    for id in ids {
        let exported = packs
            .iter()
            .flat_map(crate::task_sync::evidence::EvidencePack::records)
            .find(|record| record.object_id() == *id)
            .unwrap();
        let stored: Vec<u8> = f
            .store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exported.canonical_bytes(), stored);
    }
}

fn assert_reason_prose_still_refused(f: &mut Fixture, origin: &ObjectId) {
    f.request.operation_key = "remote:unsafe-reason-for-capture".into();
    f.request.completion.result.reason = "See src/private.rs".into();
    let receipt = f.assert_uncredited(RemoteVerificationReason::AssociationStale);
    let feed = FeedId::Project(f.project.clone());
    let request = NativeEvidenceCaptureRequest {
        capture: ObjectId::mint(),
        origin: origin.clone(),
        profile: ExportProfile {
            include_notes: true,
            origin_machine: Some("capture-host".into()),
        },
        roots: vec![receipt.operation],
        feeds: vec![NativeEvidenceFeed {
            through: f.store.work_feed_head(&feed).unwrap(),
            feed,
        }],
        maximum_pack_records: 32,
    };
    assert!(
        capture(f, &request)
            .err()
            .unwrap()
            .to_string()
            .contains("note disclosure refused"),
        "free-form remote reason prose remains subject to disclosure"
    );
}

#[test]
fn remote_verification_native_capture_retains_diagnostic_metadata() {
    let directory = crate::test_support::temp_home().unwrap();
    let project = ProjectId("project-a".into());
    let path = crate::project_database_path(directory.path(), &project);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let store =
        SqliteStore::open_with_host_path_policy(&path, crate::HostPathPolicy::host_default())
            .unwrap();
    let mut f = fixture_in(store, 0, None);
    legacy_input(&mut f.request.completion.input).commit = Some(object_name("other commit"));
    let mismatch = f.assert_uncredited(RemoteVerificationReason::CommitMismatch);
    f.request.operation_key = "remote:stale-for-capture".into();
    f.request.association.binding.work_revision += 1;
    let stale = f.assert_uncredited(RemoteVerificationReason::AssociationStale);
    let origin = f
        .store
        .enroll_task_sync_origin(
            &f.project,
            &actor("collector"),
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(11),
        )
        .unwrap();
    let root = f
        .store
        .get_work_item(f.request.association.binding.work_id)
        .unwrap()
        .root_id;
    for feed in [FeedId::Project(f.project.clone()), FeedId::RootWork(root)] {
        let mut request = NativeEvidenceCaptureRequest {
            capture: ObjectId::mint(),
            origin: origin.origin_id.clone(),
            profile: ExportProfile {
                include_notes: true,
                origin_machine: Some("capture-host".into()),
            },
            roots: vec![mismatch.operation.clone(), stale.operation.clone()],
            feeds: vec![NativeEvidenceFeed {
                through: f.store.work_feed_head(&feed).unwrap(),
                feed,
            }],
            maximum_pack_records: 32,
        };
        let captured = capture(&mut f, &request).unwrap();
        let page = f
            .store
            .export_native_evidence_capture(&f.project, &captured.cursor, 32, &request.profile)
            .unwrap();
        assert_receipts(&f, &page.packs, &[&mismatch.operation, &stale.operation]);
        request.capture = ObjectId::mint();
        request.profile.include_notes = false;
        assert!(
            capture(&mut f, &request).is_err(),
            "diagnostic metadata does not bypass the notes opt-in"
        );
    }
    assert_reason_prose_still_refused(&mut f, &origin.origin_id);
    assert_eq!(f.count("verification_evidence"), 0);
    f.assert_healthy();
}
