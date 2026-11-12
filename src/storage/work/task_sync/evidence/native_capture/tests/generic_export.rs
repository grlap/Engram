use super::disclosure::assert_generic_refused;
use super::*;
use crate::storage::work::task_sync::evidence::{PackCursor, load_retained_pack_on};
use crate::task_sync::evidence::{EvidencePack, RecordEnvelope, RecordKind};

#[test]
fn generic_foreign_relay_refuses_native_publishers_member_origins_and_originals() {
    for native_part in ["publisher", "member_origin", "original"] {
        let mut local = fixture();
        let mut remote = fixture();
        let captured = capture(&mut remote);
        let page = remote
            .store
            .export_native_evidence_capture(
                &project(),
                &captured.cursor,
                32,
                &remote.request.profile,
            )
            .unwrap();
        assert!(page.next.is_none());
        let admissions = page
            .packs
            .iter()
            .map(|_| ObjectId::mint())
            .collect::<Vec<_>>();
        local
            .store
            .retain_evidence_batch(
                &project(),
                &page.packs.iter().zip(&admissions).collect::<Vec<_>>(),
            )
            .unwrap();
        let foreign = local
            .store
            .capture_evidence_packs(&project())
            .unwrap()
            .unwrap();
        let relayed = local.store.export_evidence_packs(&foreign).unwrap();
        assert!(relayed.next.is_none());
        assert_eq!(relayed.packs, page.packs);
        assert!(local.store.get_work_item(remote.work).is_err());

        // The publisher and member namespace can both be foreign while the
        // original already belongs to this store's native execution.
        let (id, bytes) = if native_part == "original" {
            (&local.original, &local.bytes)
        } else {
            (&remote.original, &remote.bytes)
        };
        let publisher = if native_part == "publisher" {
            &local.request.origin
        } else {
            &remote.request.origin
        };
        let origin = if native_part == "member_origin" {
            &local.request.origin
        } else {
            &remote.request.origin
        };
        let record = RecordEnvelope::from_canonical(
            origin.clone(),
            id.clone(),
            RecordKind::WorkEvidence.as_str(),
            bytes.clone(),
        )
        .unwrap();
        let repacked =
            EvidencePack::prepare(ObjectId::mint(), project(), publisher.clone(), vec![record])
                .unwrap();
        local
            .store
            .retain_evidence_pack(&project(), &repacked, &ObjectId::mint())
            .unwrap();
        let mixed = local
            .store
            .capture_evidence_packs(&project())
            .unwrap()
            .unwrap();
        let resumed = PackCursor {
            after: Some(foreign.capture.clone()),
            ..mixed.clone()
        };
        let before = test_database_shape_snapshot(&local.store.connection).unwrap();
        assert_generic_refused(&local.store, &mixed);
        assert_generic_refused(&local.store, &resumed);
        // A prior foreign-only interval remains usable after a native pack is
        // appended; an exporter cannot silently skip the refused catalog slot.
        assert_eq!(
            local.store.export_evidence_packs(&foreign).unwrap().packs,
            page.packs
        );
        assert_eq!(
            load_retained_pack_on(&local.store.connection, &project(), repacked.pack_id())
                .unwrap()
                .to_bytes()
                .unwrap(),
            repacked.to_bytes().unwrap()
        );
        assert_eq!(
            test_database_shape_snapshot(&local.store.connection).unwrap(),
            before
        );
        assert!(local.store.verify_all().unwrap().is_healthy());

        crate::storage::work::task_sync::evidence::schema::rebuild(&local.store.connection)
            .unwrap();
        assert_generic_refused(&local.store, &mixed);
        assert_generic_refused(&local.store, &resumed);
        assert_eq!(
            local.store.export_evidence_packs(&foreign).unwrap().packs,
            page.packs
        );
        assert!(local.store.verify_all().unwrap().is_healthy());
    }
}
