use super::*;

fn block(verbs: &AgentVerbs, work: &str) {
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.into()),
                action: UpdateAction::Blocked {
                    detail: "retained historical obstacle".into(),
                },
            },
            at(20),
        )
        .unwrap();
}

#[test]
fn blocked_listing_excludes_native_and_restored_ended_work_before_counting_and_paging() {
    let (directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let mut children = Vec::new();
    for title in [
        "Open first",
        "Open second",
        "Restored completed",
        "Native cancelled",
        "Native superseded",
        "Proposed blocked",
        "Proposed clear",
    ] {
        let child = verbs
            .add(
                AddInput {
                    title: format!("Matching {title}"),
                    under: Some(parent.clone()),
                    optional: true,
                    labels: vec!["debt".into()],
                    assignee: Some("agent".into()),
                    ..AddInput::default()
                },
                at(1),
            )
            .unwrap()
            .value["work"]["short_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        if title != "Proposed clear" {
            block(&verbs, &child);
        }
        children.push(child);
    }
    let historical = children[2..5]
        .iter()
        .map(|child| verbs.show(child, at(21)).unwrap().value["blockers"].clone())
        .collect::<Vec<_>>();
    super::super::terminalize(&verbs, &children[3], WorkLifecycle::Cancelled);
    super::super::terminalize(&verbs, &children[4], WorkLifecycle::Superseded);
    let native = verbs
        .ls(
            &LsInput {
                all: true,
                blocked: true,
                under: Some(parent.clone()),
                ..LsInput::default()
            },
            at(30),
        )
        .unwrap();
    assert_eq!(native.value["total"], 4);
    for child in &children[3..5] {
        assert!(
            native.value["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["ref"] != *child)
        );
    }

    // A completed source needs its real completion proof. Preserve the prior
    // blocker in the exported history shape, which the loader admits, without
    // weakening native completion or editing stored rows.
    let prior = super::super::review::snapshot(&path, &project, &parent);
    let completed_id = prior
        .body
        .items
        .iter()
        .find(|item| item.short_ref == children[2])
        .unwrap()
        .work_id;
    let historical_blocker = prior
        .body
        .blockers
        .iter()
        .find(|blocker| blocker.work_id == completed_id)
        .unwrap()
        .clone();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(children[2].clone()),
                action: UpdateAction::Unblock { blocker: None },
            },
            at(31),
        )
        .unwrap();
    super::super::terminalize(&verbs, &children[2], WorkLifecycle::Completed);
    let mut document = super::super::review::snapshot(&path, &project, &parent);
    document.body.blockers.push(historical_blocker);
    document.body.blockers.sort_by_key(|blocker| {
        (
            document
                .body
                .items
                .iter()
                .find(|item| item.work_id == blocker.work_id)
                .unwrap()
                .short_ref
                .clone(),
            blocker.blocker_id.clone(),
        )
    });
    document.body.summary.section_counts.blockers = document.body.blockers.len();
    document.manifest.summary = document.body.summary.clone();
    for (child, lifecycle) in [
        (&children[5], WorkLifecycle::Proposed),
        (&children[6], WorkLifecycle::Proposed),
    ] {
        document
            .body
            .items
            .iter_mut()
            .find(|item| item.short_ref == *child)
            .unwrap()
            .lifecycle = lifecycle;
    }
    document.manifest.body_sha256 = crate::CanonicalObject::freeze(&document.body)
        .unwrap()
        .key()
        .clone();
    let mut expected = document
        .body
        .items
        .iter()
        .filter(|item| [&children[0], &children[1], &children[5]].contains(&&item.short_ref))
        .map(|item| (item.work_id.0, item.short_ref.clone()))
        .collect::<Vec<_>>();
    expected.sort_by_key(|item| item.0);
    let expected = expected.into_iter().map(|item| item.1).collect::<Vec<_>>();
    let (restored, _store, restored_path) = super::super::review::load(directory.path(), &document);
    let connection = rusqlite::Connection::open(&restored_path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for verbose in [false, true] {
        let mut input = LsInput {
            all: true,
            blocked: true,
            under: Some(parent.clone()),
            optional: true,
            search: Some("MATCHING".into()),
            label: Some("DEBT".into()),
            mine: true,
            limit: Some(1),
            verbose,
            ..LsInput::default()
        };
        let mut collected = Vec::new();
        loop {
            let receipt = restored.ls(&input, at(110)).unwrap();
            assert_eq!(receipt.value["total"], expected.len());
            assert_eq!(receipt.value["shown_before"], collected.len());
            let rows = receipt.value["items"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            collected.push(
                if verbose {
                    &row["work"]["short_ref"]
                } else {
                    &row["ref"]
                }
                .as_str()
                .unwrap()
                .to_owned(),
            );
            assert_eq!(receipt.value["omitted"], expected.len() - collected.len());
            assert_eq!(receipt.value["more"], collected.len() < expected.len());
            let Some(token) = receipt.value["after"].as_str() else {
                break;
            };
            assert!(collected.len() < expected.len());
            input.after = Some(token.into());
        }
        assert_eq!(collected, expected);
        let empty = restored
            .ls(
                &LsInput {
                    search: Some("Restored completed".into()),
                    after: None,
                    ..input
                },
                at(110),
            )
            .unwrap();
        assert_eq!(empty.value["items"], json!([]));
        assert_eq!(empty.value["total"], 0);
        assert_eq!(empty.value["omitted"], 0);
        assert_eq!(empty.value["more"], false);
        assert!(empty.value["after"].is_null());
    }
    let plain = restored
        .ls(
            &LsInput {
                blocked: true,
                under: Some(parent.clone()),
                ..LsInput::default()
            },
            at(110),
        )
        .unwrap();
    assert_eq!(
        plain.value["total"], 2,
        "Proposed retains the existing all-only treatment"
    );
    let all = restored
        .ls(
            &LsInput {
                all: true,
                under: Some(parent),
                ..LsInput::default()
            },
            at(110),
        )
        .unwrap();
    assert_eq!(all.value["total"], children.len());
    for (child, blockers) in children[2..5].iter().zip(historical) {
        assert_eq!(
            restored.show(child, at(110)).unwrap().value["blockers"],
            blockers
        );
        assert!(
            all.value["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["ref"] == *child)
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn blocked_continuation_tracks_surviving_membership_not_ended_history() {
    let (_directory, verbs, _, _) = fixture();
    let first = add(&verbs, "First", None, false, 0);
    let second = add(&verbs, "Second", None, false, 1);
    let ended = add(&verbs, "Ended history", None, false, 2);
    let completed = add(&verbs, "Completed history", None, false, 3);
    for work in [&first, &second, &ended] {
        block(&verbs, work);
    }
    super::super::terminalize(&verbs, &ended, WorkLifecycle::Cancelled);
    super::super::terminalize(&verbs, &completed, WorkLifecycle::Completed);
    let input = LsInput {
        all: true,
        blocked: true,
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&input, at(30)).unwrap();
    assert_eq!(page.value["total"], 2);
    let continued = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..input
    };
    note(&verbs, &completed, "Historical observation", 31);
    let tail = verbs.ls(&continued, at(32)).unwrap();
    assert_eq!(tail.value["total"], 2);
    assert_eq!(tail.value["shown_before"], 1);
    assert_eq!(tail.value["omitted"], 0);
    let remaining = tail.value["items"][0]["ref"].as_str().unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(remaining.into()),
                action: UpdateAction::Cancel {
                    reason: "Ended after listing".into(),
                },
            },
            at(33),
        )
        .unwrap();
    assert!(matches!(
        verbs.ls(&continued, at(34)).unwrap_err().error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
}
