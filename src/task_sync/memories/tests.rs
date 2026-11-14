use super::*;
use chrono::TimeZone;

fn revision(body: &str, previous: Option<ObjectId>) -> Revision {
    Revision {
        provenance: ImportProvenance {
            format: FORMAT.into(),
            project_id: ProjectId("memory-project".into()),
            key: "rule".into(),
            revision_id: ObjectId::mint(),
            previous,
            body_sha256: body_sha256(body),
            retiring_target: None,
            source_clear: false,
            source: Source {
                actor_id: "original-author".into(),
                actor_kind: "agent".into(),
                actor_context: Some("agent=codex;model=sol".into()),
                reason: "record a rule".into(),
                remembered_at: Utc.timestamp_millis_opt(1000).unwrap(),
            },
        },
        body: body.into(),
    }
}

#[test]
fn import_tag_is_bounded_strict_and_bound_to_capture() {
    let revision = revision("remember this", None);
    let tag = revision.provenance;
    let marked = tag.marked().unwrap();
    assert_eq!(ImportProvenance::parse(&marked).unwrap(), tag);
    tag.validate_capture(&tag.project_id, Some("rule"), &revision.body)
        .unwrap();
    assert!(
        tag.validate_capture(&tag.project_id, None, &revision.body)
            .is_err()
    );
    assert!(
        tag.validate_capture(&ProjectId("other".into()), Some("rule"), &revision.body)
            .is_err()
    );
    assert!(
        tag.validate_capture(&tag.project_id, Some("rule"), "different")
            .is_err()
    );
    assert!(ImportProvenance::parse("unmarked").is_err());
    let mut raw = serde_json::to_value(&tag).unwrap();
    raw["unknown"] = true.into();
    assert!(ImportProvenance::parse(&format!("{IMPORT_MARKER}:{raw}")).is_err());
    let mut oversized = tag;
    oversized.source.reason = "x".repeat(MAX_TAG_BYTES);
    assert!(oversized.marked().is_err());
}

#[test]
fn source_tag_survives_relay_without_inheriting_recorder_identity() {
    let tag = revision("rule", None).provenance;
    let mut actor = crate::ActorContext {
        actor_id: "local-recorder".into(),
        actor_kind: "agent".into(),
        assurance: crate::domain::AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(crate::SessionId("local-session".into())),
        source_tool: Some("remember".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "local receipt".into(),
    };
    tag.attach(&mut actor).unwrap();
    assert_eq!(actor.actor_id, "local-recorder");
    assert_eq!(actor.session_id.as_ref().unwrap().0, "local-session");
    assert_eq!(
        ImportProvenance::from_actor(&actor).unwrap(),
        Some(tag.clone())
    );
    assert!(tag.attach(&mut actor).is_err());
    actor.provenance_chain[0].reference = Some("malformed".into());
    assert!(ImportProvenance::from_actor(&actor).is_err());
}

#[test]
fn head_rejects_structurally_merged_branches_and_unreferenced_bodies() {
    let first = revision("original", None);
    let a = revision("side A", Some(first.provenance.revision_id.clone()));
    let b = revision("side B", Some(first.provenance.revision_id.clone()));
    let first_id = first.provenance.revision_id.clone();
    let a_id = a.provenance.revision_id.clone();
    let b_id = b.provenance.revision_id.clone();
    let mut history = KeyHistory {
        format: FORMAT.into(),
        project_id: first.provenance.project_id.clone(),
        key: "rule".into(),
        head: Head::Active {
            chain: vec![first_id.clone(), a_id.clone()],
        },
        revisions: BTreeMap::from([(first_id.clone(), first), (a_id.clone(), a)]),
    };
    history.validate().unwrap();
    history.revisions.insert(b_id.clone(), b);
    assert!(history.validate().is_err());
    history.head = Head::Active {
        chain: vec![first_id, a_id, b_id],
    };
    assert!(history.validate().is_err());
    history.head = Head::Tombstone {
        source: history
            .revisions
            .values()
            .next()
            .unwrap()
            .provenance
            .source
            .clone(),
    };
    assert!(history.validate().is_err());
    history.revisions.clear();
    history.validate().unwrap();
}

#[test]
fn seeded_anchor_validates_identity_and_content_without_replaying_prefix() {
    let first = revision("earlier", None);
    let seed = revision("seed body", Some(first.provenance.revision_id.clone()));
    let later = revision("later body", Some(seed.provenance.revision_id.clone()));
    let tag = seed.provenance.clone();
    let local_id = ObjectId::mint();
    let ids = [&first, &seed, &later].map(|r| r.provenance.revision_id.clone());
    let selected = KeyHistory {
        format: FORMAT.into(),
        project_id: tag.project_id.clone(),
        key: tag.key.clone(),
        head: Head::Active {
            chain: ids.to_vec(),
        },
        revisions: [first, seed, later]
            .into_iter()
            .map(|r| (r.provenance.revision_id.clone(), r))
            .collect(),
    };
    let manifest = SeedManifestEntry {
        project_id: tag.project_id.clone(),
        key: tag.key.clone(),
        local_version_id: local_id.clone(),
        shared_revision_id: tag.revision_id.clone(),
        body_sha256: tag.body_sha256.clone(),
    };
    for _ in 0..2 {
        assert_eq!(
            manifest
                .validate_anchor(&selected, &local_id, "seed body", Some(&tag))
                .unwrap(),
            1
        );
    }
    assert!(
        manifest
            .validate_anchor(&selected, &ObjectId::mint(), "seed body", Some(&tag))
            .is_err()
    );
    assert!(
        manifest
            .validate_anchor(&selected, &local_id, "changed body", Some(&tag))
            .is_err()
    );
    assert!(
        manifest
            .validate_anchor(&selected, &local_id, "seed body", None)
            .is_err()
    );
    let mut wrong_manifest = manifest.clone();
    wrong_manifest.shared_revision_id = ObjectId::mint();
    assert!(
        wrong_manifest
            .validate_anchor(&selected, &local_id, "seed body", Some(&tag))
            .is_err()
    );
    let mut wrong_tag = tag.clone();
    wrong_tag.revision_id = ObjectId::mint();
    assert!(selected.imported_anchor(&wrong_tag, "seed body").is_err());
    wrong_tag = tag;
    wrong_tag.source.reason = "rewritten attribution".into();
    assert!(selected.imported_anchor(&wrong_tag, "seed body").is_err());
}
