use super::*;
use crate::task_sync::{
    data_worktree::{DataWorktree, StagedCommitRequest},
    delta::OriginQualifiedCut,
    evidence::{EvidencePack, RecordEnvelope},
    ledger::{Binding, Direction, EvidenceIntent, EvidencePackSelection, StoreBasis, TargetBasis},
};
use crate::{CanonicalObject, FeedId, FeedPosition, ObjectId};
use std::time::Duration;

pub(super) fn assert_project_envelope(
    retained: &[u8],
    published: &[u8],
    assertions: &super::super::super::Assertions,
) {
    let envelope: serde_json::Value = serde_json::from_slice(published).unwrap();
    assert_eq!(
        envelope["project"],
        serde_json::from_slice::<serde_json::Value>(retained).unwrap()
    );
    let basis = crate::task_sync::directory::folder_basis_from_bytes(published).unwrap();
    assert_eq!(basis.project_id, ProjectId("materialize/fixture".into()));
    assert_eq!(basis.profile.include_notes, assertions.include_notes);
    assert_eq!(basis.profile.origin_machine, assertions.origin_machine);
}

#[test]
fn managed_setup_stages_evidence_with_plain_git_add_and_reads_the_committed_pack() {
    let f = Fixture::new(false, 1);
    let project = ProjectId("materialize/fixture".into());
    let first = f.run();
    let selected = DataWorktree::observe(&f.code, &f.root, "ordinary").unwrap();
    let actor: crate::ActorContext = serde_json::from_value(serde_json::json!({
        "actor_id":"fixture", "actor_kind":"agent", "assurance":"asserted", "run_id":null,
        "session_id":"fixture", "source_tool":null, "source_skill":null,
        "provenance_chain":[], "reason":"exercise managed evidence staging"
    }))
    .unwrap();
    let body = CanonicalObject::mint(&crate::domain::WorkEvidence {
        schema_version: crate::schema::SCHEMA_VERSION,
        work_id: WorkId::new(),
        run_id: crate::WorkRunId::new(),
        claim_id: crate::WorkClaimId::new(),
        claim_fence: 1,
        summary: "Evidence beside managed planning data".into(),
        refs: Vec::new(),
        gate: None,
        actor: actor.clone(),
        created_at: chrono::Utc::now(),
    })
    .unwrap();
    let origin = ObjectId::mint();
    let record = RecordEnvelope::from_canonical(
        origin.clone(),
        body.key().clone(),
        "work_evidence",
        body.bytes().to_vec(),
    )
    .unwrap();
    let pack = EvidencePack::prepare(
        ObjectId::mint(),
        project.clone(),
        origin.clone(),
        vec![record],
    )
    .unwrap();
    let files = selected
        .stage_evidence_packs(&project, std::slice::from_ref(&pack))
        .unwrap();
    assert_eq!(
        selected
            .stage_evidence_packs(&project, std::slice::from_ref(&pack))
            .unwrap(),
        files
    );
    let bytes = pack.to_bytes().unwrap();
    assert_eq!(fs::read(f.root.join(&files[0].path)).unwrap(), bytes);

    // Public evidence is a sibling of planning documents, never part of the
    // materializer's private, exactly inventoried candidate or ownership pins.
    let private = f.root.join(".engram/private.db");
    fs::write(&private, b"private bytes stay local").unwrap();
    let origin_path = format!(".engram/evidence/{origin}");
    for path in [
        ".engram/evidence/private.json".to_owned(),
        ".engram/evidence/not-an-origin/ab/private.json".to_owned(),
        format!("{origin_path}/private.json"),
        format!("{origin_path}/zz/{}.json", pack.pack_id()),
        format!("{}.private", pack.relative_path()),
        format!("{origin_path}/ab/private.json"),
    ] {
        let path = f.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"not shared").unwrap();
    }
    let repeated = f.run();
    assert!(repeated.repeated);
    assert_eq!(first.operation, repeated.operation);
    assert_eq!(fs::read(&private).unwrap(), b"private bytes stay local");
    assert_eq!(
        candidate::read(&Directory::open(&f.private().join("raw")).unwrap()).unwrap(),
        f.documents
    );

    git(&f.root, &["add", ".engram"]);
    let mut actual: Vec<_> = git(&f.root, &["ls-files", "-z", "--", ".engram"])
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8(name.to_vec()).unwrap())
        .collect();
    let mut expected: Vec<_> = f
        .documents
        .keys()
        .map(|path| format!(".engram/{path}"))
        .collect();
    expected.extend([
        ".engram/.gitattributes".into(),
        ".engram/.gitignore".into(),
        pack.relative_path(),
    ]);
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    let staged = String::from_utf8(git(&f.root, &["write-tree"]))
        .unwrap()
        .trim()
        .to_owned();
    let operation = uuid::Uuid::new_v4();
    let source = selected
        .create_retained(&StagedCommitRequest {
            operation,
            tree: staged,
            expected_head: selected.head.clone(),
            parents: selected.head.iter().cloned().collect(),
            author_name: "Fixture".into(),
            author_email: "fixture@example.invalid".into(),
            authored_at: chrono::DateTime::from_timestamp(chrono::Utc::now().timestamp(), 0)
                .unwrap(),
            message: "Share managed planning and evidence\n".into(),
        })
        .unwrap();
    let selected = selected
        .advance_retained(&source, selected.head.as_deref())
        .unwrap();
    assert_eq!(selected.head.as_deref(), Some(source.oid.as_str()));
    let binding_id = ObjectId::mint();
    let local_origin = ObjectId::mint();
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database: f.store.clone(),
            origin: Some(local_origin.clone()),
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: selected.repository_basis(uuid::Uuid::new_v4()),
        branch: selected.reference.clone(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/ordinary".into(),
            configuration_fingerprint: CanonicalObject::freeze(&"fixture configuration")
                .unwrap()
                .key()
                .clone(),
        },
        profile: ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        supersedes: None,
        superseded_tail: None,
        actor: actor.clone(),
        created_at: chrono::Utc::now(),
    };
    let intent = EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor,
        producer: CanonicalObject::freeze(&"fixture encoder")
            .unwrap()
            .key()
            .clone(),
        expected_head: None,
        source,
        local_cut: OriginQualifiedCut {
            binding: binding_id.clone(),
            origin: local_origin,
            restore_occurrence: None,
            position: FeedPosition {
                feed: FeedId::Project(project),
                position: 0,
            },
            anchor: None,
        },
        packs: BTreeMap::from([(
            pack.pack_id().clone(),
            EvidencePackSelection {
                publishing_origin: origin,
                admission: ObjectId::mint(),
                content_fingerprint: files[0].content_fingerprint.clone(),
                encoded_bytes: bytes.len(),
                records: 1,
            },
        )]),
    };
    let index = fs::read(f.admin.join("index")).unwrap();
    let read = crate::task_sync::git::read_selected_evidence(
        &binding_id,
        &binding,
        &intent,
        Duration::from_secs(60),
    )
    .unwrap();
    assert_eq!(read.packs[0].to_bytes().unwrap(), bytes);
    assert_eq!(fs::read(f.admin.join("index")).unwrap(), index);
    assert_eq!(fs::read(private).unwrap(), b"private bytes stay local");
}
