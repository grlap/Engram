//! Imported diagnostic prose remains checked when native use closes over it.
use super::*;
use crate::storage::{NativeEvidenceCaptureRequest, NativeEvidenceFeed};
use crate::task_sync::{origin::EnrollRequest, progress::ExportProfile};

#[test]
fn native_capture_imported_remote_detail_obeys_disclosure() {
    for unsafe_detail in [false, true] {
        let root = repository();
        let home = crate::test_support::temp_home().unwrap();
        let store = crate::storage::work::task_sync::evidence::test_support::project_store(
            home.path(),
            &project(),
        );
        let mut origin = Fixture::new(root.path());
        let mut target = destination_in(&mut origin, root.path(), 11, store);
        target.observe(root.path(), 16);
        let (seal_id, seal) = closure::complete_origin(&mut origin, root.path());
        let check = seal.acceptance[0].evidence[0].clone();
        let (original, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, false);
        let (pack, altered_id, original_bytes) = with_detail(&original, unsafe_detail);
        retain(&mut target.store, &pack);
        super::typed_dependencies::retain_context(&origin, &mut target, &origin_id);
        let mut verification = intent(&target, check, origin_id);
        verification.origin_cut = cuts
            .into_iter()
            .find(|id| {
                let (_, body) = object_on(&target.store.connection, id).unwrap().unwrap();
                let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
                    serde_json::from_slice(&body).unwrap();
                cut.source.feed == FeedId::RunExecution(seal.run_id)
            })
            .unwrap();
        verification.criterion = Some(1);
        let local_actor = target.local_actor();
        target
            .store
            .associate_imported_verification(
                &project(),
                &session(),
                &local_actor,
                &verification,
                at(21),
            )
            .unwrap();
        let consumption = consume(&mut target, &verification.decision, false);
        let request = capture_request(&mut target, &consumption.consumption);
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        let result = target.store.capture_native_evidence(
            &project(),
            &SessionId("collector".into()),
            &actor("collector"),
            &request,
            &DevelopmentNoopRedactor,
        );
        if unsafe_detail {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("note disclosure refused")
            );
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap(),
                before
            );
        } else {
            let captured = result.unwrap();
            let page = target
                .store
                .export_native_evidence_capture(&project(), &captured.cursor, 128, &request.profile)
                .unwrap();
            assert!(page.next.is_none());
            let exported = page
                .packs
                .iter()
                .flat_map(EvidencePack::records)
                .find(|record| record.object_id() == &altered_id)
                .unwrap();
            assert_eq!(exported.canonical_bytes(), original_bytes);
        }
        assert_eq!(
            object_on(&target.store.connection, &altered_id)
                .unwrap()
                .unwrap()
                .1,
            original_bytes
        );
        healthy(&target.store);
    }
}

fn with_detail(original: &EvidencePack, unsafe_detail: bool) -> (EvidencePack, ObjectId, Vec<u8>) {
    let mut altered_id = None;
    let records = original
        .records()
        .iter()
        .map(|record| {
            if record.kind() != RecordKind::RemoteVerificationAssociation {
                return record.clone();
            }
            let mut value: RemoteVerificationAssociationRecord =
                serde_json::from_slice(record.canonical_bytes()).unwrap();
            value.qualification.detail = Some(if unsafe_detail {
                "See src/private.rs".into()
            } else {
                "The imported terminal matched".into()
            });
            altered_id = Some(record.object_id().clone());
            RecordEnvelope::from_canonical(
                record.origin().clone(),
                record.object_id().clone(),
                record.kind().as_str(),
                serde_json_canonicalizer::to_vec(&value).unwrap(),
            )
            .unwrap()
        })
        .collect();
    let pack = EvidencePack::prepare(
        ObjectId::mint(),
        project(),
        original.publishing_origin().clone(),
        records,
    )
    .unwrap();
    let altered_id = altered_id.unwrap();
    let original_bytes = pack
        .records()
        .iter()
        .find(|record| record.object_id() == &altered_id)
        .unwrap()
        .canonical_bytes()
        .to_vec();
    (pack, altered_id, original_bytes)
}

fn capture_request(target: &mut Fixture, consumption: &ObjectId) -> NativeEvidenceCaptureRequest {
    let local_origin = target
        .store
        .enroll_task_sync_origin(
            &project(),
            &actor("collector"),
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(24),
        )
        .unwrap();
    let feed = FeedId::RunExecution(target.binding.run_id);
    NativeEvidenceCaptureRequest {
        capture: ObjectId::mint(),
        origin: local_origin.origin_id,
        profile: ExportProfile {
            include_notes: true,
            origin_machine: Some("capture-host".into()),
        },
        roots: vec![consumption.clone()],
        feeds: vec![NativeEvidenceFeed {
            through: target.store.work_feed_head(&feed).unwrap(),
            feed,
        }],
        maximum_pack_records: 32,
    }
}
