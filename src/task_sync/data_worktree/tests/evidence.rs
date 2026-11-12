use super::*;
use crate::task_sync::{
    ProjectDocument, TaskTree,
    delta::OriginQualifiedCut,
    evidence::{EvidencePack, RecordEnvelope},
    ledger::{Binding, Direction, EvidenceIntent, EvidencePackSelection, StoreBasis, TargetBasis},
    progress::{ExportCut, ExportProfile},
};
use crate::{ActorContext, CanonicalObject, FeedId, FeedPosition, ObjectId, ProjectId};
use chrono::{TimeZone, Utc};
use sha2::{Digest, Sha256};

pub(super) fn stage_pack(project: &ProjectId) -> EvidencePack {
    let actor: ActorContext = serde_json::from_value(serde_json::json!({
        "actor_id":"fixture","actor_kind":"agent","assurance":"asserted","run_id":null,
        "session_id":"fixture","source_tool":null,"source_skill":null,"provenance_chain":[],"reason":"fixture"
    })).unwrap();
    let body = CanonicalObject::mint(&crate::domain::WorkEvidence {
        schema_version: crate::schema::SCHEMA_VERSION,
        work_id: crate::WorkId::new(),
        run_id: crate::WorkRunId::new(),
        claim_id: crate::WorkClaimId::new(),
        claim_fence: 1,
        summary: "retained evidence".into(),
        refs: Vec::new(),
        gate: None,
        actor,
        created_at: Utc::now(),
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
    EvidencePack::prepare(ObjectId::mint(), project.clone(), origin, vec![record]).unwrap()
}

#[test]
fn evidence_page_staging_replays_exact_bytes_and_preserves_code_source() {
    let fixture = Fixture::new();
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let project = ProjectId("data/fixture".into());
    let pack = stage_pack(&project);
    let before = fixture.code_snapshot();
    let source = crate::source::inspect(&fixture.code, Duration::from_secs(30)).source_id;
    assert!(source.is_some());
    let files = selected
        .stage_evidence_packs(&project, std::slice::from_ref(&pack))
        .unwrap();
    assert_eq!(
        files,
        selected
            .stage_evidence_packs(&project, std::slice::from_ref(&pack))
            .unwrap()
    );
    assert_eq!(files[0].path, pack.relative_path());
    assert_eq!(
        fs::read(fixture.data.join(&files[0].path)).unwrap(),
        pack.to_bytes().unwrap()
    );
    assert_eq!(fixture.code_snapshot(), before);
    assert!(
        crate::source::inspect(&fixture.code, Duration::from_secs(30))
            .source_id
            .eq(&source)
    );
    // Never replace changed/partial bytes under the same immutable pack name.
    fs::write(fixture.data.join(&files[0].path), b"partial").unwrap();
    assert!(selected.stage_evidence_packs(&project, &[pack]).is_err());
    assert_eq!(
        fs::read(fixture.data.join(&files[0].path)).unwrap(),
        b"partial"
    );
}

#[test]
fn foreign_and_linked_evidence_destinations_refuse_without_pack_publication() {
    let fixture = Fixture::new();
    let selected = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let project = ProjectId("data/fixture".into());
    let native = stage_pack(&project);
    let foreign = stage_pack(&ProjectId("foreign".into()));
    assert!(
        selected
            .stage_evidence_packs(&project, &[native, foreign])
            .is_err()
    );
    assert!(!fixture.data.join(".engram").exists());
    #[cfg(unix)]
    {
        fs::create_dir(fixture.data.join(".engram")).unwrap();
        let unrelated = fixture.code.join("target/unrelated");
        fs::create_dir(&unrelated).unwrap();
        std::os::unix::fs::symlink(&unrelated, fixture.data.join(".engram/evidence")).unwrap();
        assert!(
            selected
                .stage_evidence_packs(&project, &[stage_pack(&project)])
                .is_err()
        );
        assert_eq!(fs::read_dir(unrelated).unwrap().count(), 0);
    }
}

#[test]
fn selected_evidence_reads_large_raw_blob_without_changing_code_and_refuses_changed_selection() {
    let f = Fixture::new();
    let before = f.code_snapshot();
    let project = ProjectId("data/fixture".into());
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    crate::task_sync::directory::write_folder(
        &f.data.join(".engram"),
        &TaskTree {
            project: ProjectDocument {
                format: crate::task_sync::FORMAT.into(),
                project_id: project.clone(),
                notes_included: false,
            },
            tasks: Vec::new(),
        },
        &profile,
    )
    .unwrap();
    let actor: ActorContext = serde_json::from_value(serde_json::json!({
        "actor_id":"fixture","actor_kind":"agent","assurance":"asserted","run_id":null,
        "session_id":"fixture","source_tool":null,"source_skill":null,"provenance_chain":[],"reason":"fixture"
    })).unwrap();
    let origin = ObjectId::mint();
    let body = CanonicalObject::mint(&crate::domain::WorkEvidence {
        schema_version: crate::schema::SCHEMA_VERSION,
        work_id: crate::WorkId::new(),
        run_id: crate::WorkRunId::new(),
        claim_id: crate::WorkClaimId::new(),
        claim_fence: 1,
        summary: "e".repeat(1024 * 1024 + 512),
        refs: Vec::new(),
        gate: None,
        actor: actor.clone(),
        created_at: Utc::now(),
    })
    .unwrap();
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
    let bytes = pack.to_bytes().unwrap();
    assert!(bytes.len() > 1024 * 1024);
    let path = f.data.join(pack.relative_path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, &bytes).unwrap();
    run(&f.data, &["add", ".engram"]);
    let selected = DataWorktree::observe(&f.code, &f.data, "engram-data").unwrap();
    let operation = uuid::Uuid::new_v4();
    let source = selected
        .create_retained(&StagedCommitRequest {
            operation,
            tree: oid_line(&run(&f.data, &["write-tree"])).unwrap(),
            expected_head: None,
            parents: Vec::new(),
            author_name: "Evidence author".into(),
            author_email: "evidence@example.invalid".into(),
            authored_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            message: "Retain exact evidence carrier\n".into(),
        })
        .unwrap();
    let binding_id = ObjectId::mint();
    let local_origin = ObjectId::mint();
    let binding = Binding {
        schema_version: 1,
        project: project.clone(),
        store: StoreBasis {
            database: f.home.path().join("store.db"),
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
            reference: "refs/heads/engram-data".into(),
            configuration_fingerprint: CanonicalObject::freeze(&"configuration")
                .unwrap()
                .key()
                .clone(),
        },
        profile,
        supersedes: None,
        superseded_tail: None,
        actor: actor.clone(),
        created_at: Utc::now(),
    };
    let mut intent = EvidenceIntent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor,
        producer: CanonicalObject::freeze(&"encoder").unwrap().key().clone(),
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
        packs: std::collections::BTreeMap::from([(
            pack.pack_id().clone(),
            EvidencePackSelection {
                publishing_origin: origin,
                admission: ObjectId::mint(),
                content_fingerprint: format!("{:x}", Sha256::digest(&bytes)).parse().unwrap(),
                encoded_bytes: bytes.len(),
                records: 1,
            },
        )]),
    };
    let index = fs::read(selected.git_directory.join("index")).unwrap();
    for seconds in [0, 1, 30, 59] {
        super::super::budget_probe::install(None);
        let error = git::read_selected_evidence(
            &binding_id,
            &binding,
            &intent,
            Duration::from_secs(seconds),
        )
        .err()
        .expect("a short public request must refuse before Git");
        let (queries, stalled) = super::super::budget_probe::finish();
        assert!(
            error.to_string().contains("at least 60 seconds"),
            "{error:#}"
        );
        assert!(queries.is_empty() && !stalled);
    }
    let read = git::read_selected_evidence(&binding_id, &binding, &intent, Duration::from_secs(60))
        .unwrap();
    assert_eq!(read.packs[0].to_bytes().unwrap(), bytes);
    assert_eq!(read.commit.oid, intent.source.oid);
    assert_eq!(f.code_snapshot(), before);
    assert_eq!(
        fs::read(selected.git_directory.join("index")).unwrap(),
        index
    );
    assert_eq!(
        DataWorktree::observe(&f.code, &f.data, "engram-data")
            .unwrap()
            .head,
        None
    );
    selected_evidence_observations_spend_the_read_deadline(&f, &binding_id, &binding, &intent);
    assert_eq!(f.code_snapshot(), before);
    assert_eq!(
        fs::read(selected.git_directory.join("index")).unwrap(),
        index
    );
    intent
        .packs
        .get_mut(pack.pack_id())
        .unwrap()
        .content_fingerprint = CanonicalObject::freeze(&"different selected bytes")
        .unwrap()
        .key()
        .clone();
    assert!(
        git::read_selected_evidence(&binding_id, &binding, &intent, Duration::from_secs(60))
            .is_err()
    );
    assert_eq!(f.code_snapshot(), before);
    assert_eq!(
        fs::read(selected.git_directory.join("index")).unwrap(),
        index
    );
}

/// The data-worktree observations before and after the bounded blob reads
/// spend the caller's one deadline: a slow first or last query refuses with a
/// deadline instead of starting a fresh fixed bound, and a read with budget
/// left succeeds with every query capped at what remains.
fn selected_evidence_observations_spend_the_read_deadline(
    f: &Fixture,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
) {
    use super::super::budget_probe;
    let deadline = Duration::from_secs(60);
    let read = || git::read_selected_evidence(binding_id, binding, intent, deadline);
    let is_deadline = |error: &anyhow::Error| {
        error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<git::QueryFailure>(),
                Some(git::QueryFailure::Deadline)
            )
        })
    };

    budget_probe::install(None);
    read().unwrap();
    let (bounds, _) = budget_probe::finish();
    assert_ne!(bounds, [] as [Duration; 0]);
    assert!(bounds.iter().all(|bound| *bound < deadline), "{bounds:?}");
    assert!(
        bounds.windows(2).all(|pair| pair[1] <= pair[0]),
        "{bounds:?}"
    );

    // An exchange phase's admitted remainder is allowed below the public floor.
    let warning = crate::operation_time::Warning::start("selected evidence budget test");
    let remaining = Duration::from_secs(40);
    budget_probe::install(None);
    git::read_selected_evidence_remaining(binding_id, binding, intent, remaining, &warning)
        .unwrap();
    let (internal_bounds, _) = budget_probe::finish();
    assert_eq!(internal_bounds.len(), bounds.len());
    assert!(internal_bounds.iter().all(|bound| *bound < remaining));

    // The initial observation's first query takes longer than the whole read.
    budget_probe::install(Some((0, deadline)));
    let Err(error) = read() else {
        panic!("a spent budget must refuse the first query")
    };
    let (refused, stalled) = budget_probe::finish();
    assert!(
        stalled && refused.is_empty() && is_deadline(&error),
        "{error:#}"
    );

    // The final revalidation's last query does.
    budget_probe::install(Some((bounds.len() - 1, deadline)));
    let Err(error) = read() else {
        panic!("a spent budget must refuse the final revalidation")
    };
    let (refused, stalled) = budget_probe::finish();
    assert!(stalled && is_deadline(&error), "{error:#}");
    assert_eq!(refused.len(), bounds.len() - 1);

    // A slow first query that leaves budget lets the read finish within it.
    let spent = Duration::from_secs(20);
    budget_probe::install(Some((0, spent)));
    read().unwrap();
    let (later, stalled) = budget_probe::finish();
    assert!(stalled && later.len() == bounds.len());
    assert!(
        later
            .iter()
            .all(|bound| *bound <= deadline.checked_sub(spent).unwrap()),
        "{later:?}"
    );
    assert_eq!(
        DataWorktree::observe(&f.code, &f.data, "engram-data")
            .unwrap()
            .head,
        None
    );
}
