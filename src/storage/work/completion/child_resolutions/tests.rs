use super::super::*;
use crate::RequiredChildResolution;
use crate::verbs::{AddInput, AgentVerbs, ClaimInput, DoneInput, UpdateAction, UpdateInput};

#[test]
fn successor_seal_accounting_verifies_each_immutable_binding_and_disjointness() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("work.db");
    let project = crate::ProjectId("successor-proof".into());
    let verbs = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let at = |seconds| DateTime::<Utc>::from_timestamp(seconds, 0).unwrap();
    let add = |title: &str, parent: Option<&str>, seconds| {
        verbs
            .add(
                AddInput {
                    title: title.into(),
                    under: parent.map(str::to_owned),
                    ..AddInput::default()
                },
                at(seconds),
            )
            .unwrap()
            .value["work"]["short_ref"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let parent = add("Parent", None, 0);
    let child = add("Original", Some(&parent), 1);
    let successor = add("Successor", Some(&parent), 2);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child),
                action: UpdateAction::Supersede {
                    replacement: successor.clone(),
                    reason: "Replacement owns this requirement".into(),
                },
            },
            at(3),
        )
        .unwrap();
    for (work, seconds) in [(&successor, 4), (&parent, 6)] {
        verbs
            .claim(
                ClaimInput {
                    work_ref: work.clone(),
                    ttl_seconds: None,
                    recover: None,
                },
                at(seconds),
            )
            .unwrap();
        assert!(
            !verbs
                .done(
                    DoneInput {
                        work_ref: Some(work.clone()),
                        summary: Some("Delivered".into()),
                        note: None
                    },
                    at(seconds + 1)
                )
                .unwrap()
                .owed
        );
    }
    let store = SqliteStore::open(&path).unwrap();
    let id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let hash = store
        .latest_work_run(id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let seal: CompletionSeal = store.get(&hash).unwrap().unwrap();
    let connection = Connection::open(&path).unwrap();
    assert!(validate_completion_seal_children_on(&connection, &seal, 0).is_ok());
    let snapshot = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for fault in [
        "revision",
        "child",
        "successor",
        "supersession",
        "seal",
        "generation",
        "duplicate",
        "waived",
        "uncited",
    ] {
        let mut damaged = seal.clone();
        let RequiredChildResolution::ResolvedBySuccessor {
            work_id,
            work_revision,
            successor,
            supersession,
            successor_seal,
        } = &mut damaged.required_child_resolutions[0];
        match fault {
            "revision" => *work_revision += 1,
            "child" => *work_id = id,
            "successor" => *successor = id,
            "supersession" => *supersession = hash.clone(),
            "seal" => *successor_seal = hash.clone(),
            "generation" => damaged.root_execution_id = RootExecutionId::new(),
            "duplicate" => damaged
                .required_child_resolutions
                .push(damaged.required_child_resolutions[0].clone()),
            "waived" => damaged.required_child_waivers.push(RequiredChildWaiver {
                work_id: *work_id,
                work_revision: *work_revision,
                waived_by: "agent".into(),
                reason: "Duplicate accounting".into(),
            }),
            "uncited" => damaged.required_child_seals.clear(),
            _ => unreachable!(),
        }
        assert!(
            validate_completion_seal_children_on(&connection, &damaged, 0).is_err(),
            "{fault}"
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        snapshot
    );
    assert!(store.verify_all().unwrap().is_healthy());
}
