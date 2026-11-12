use super::*;

type ProjectedMember = (String, String, String, i64, String, String, String);

fn projected_members(store: &SqliteStore) -> Vec<ProjectedMember> {
    store
        .connection
        .prepare(
            "SELECT project_id,origin_id,feed,position,object_kind,object_id,segment_id
             FROM work_task_sync_evidence_feed_members
             ORDER BY project_id,origin_id,feed,position",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

fn retain_witness_fixture(store: &mut SqliteStore) -> ObjectId {
    let (id, enrolled) = origin(None);
    let (other_id, other_enrolled) = origin(None);
    let fact = evidence(&id, "qualified historical evidence");
    let segment_id = ObjectId::mint();
    let segment = OriginFeedSegment {
        source: source(&id),
        from_exclusive: 0,
        through: 1,
        predecessor: None,
        anchor: None,
        entries: vec![member(1, &fact)],
    };
    let cut = OriginFeedCut {
        source: source(&id),
        through: 1,
        endpoint: Some(member(1, &fact)),
        segments: vec![segment_id.clone()],
    };
    retain(
        store,
        &pack(
            &id,
            vec![
                enrollment(&id, &enrolled),
                enrollment(&other_id, &other_enrolled),
                fact,
                envelope(&id, &segment_id, RecordKind::OriginFeedSegment, &segment),
                envelope(&id, &ObjectId::mint(), RecordKind::OriginFeedCut, &cut),
            ],
        ),
    );
    other_id
}

#[test]
fn doctor_rejects_wrong_scope_duplicates_and_repair_preserves_original_members() {
    for changed_scope in ["project", "origin", "feed"] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("witness-scope.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let other_id = retain_witness_fixture(&mut store);
        healthy(&store);
        let canonical = originals(&store);
        let expected = projected_members(&store);
        assert_eq!(expected.len(), 1);
        let (mut project_key, mut origin_key, mut feed_key, ..) = expected[0].clone();
        match changed_scope {
            "project" => project_key = "other-evidence-project".into(),
            "origin" => origin_key = other_id.to_string(),
            "feed" => {
                feed_key = serde_json::to_string(&crate::FeedId::Project(ProjectId(
                    "other-evidence-feed".into(),
                )))
                .unwrap();
            }
            _ => unreachable!(),
        }
        // Keep the valid row and its entry/segment ids; add only another scope.
        assert_eq!(
            store
                .connection
                .execute(
                    "INSERT INTO work_task_sync_evidence_feed_members
                 (project_id,origin_id,feed,position,object_kind,object_id,segment_id)
                 SELECT ?1,?2,?3,position,object_kind,object_id,segment_id
                 FROM work_task_sync_evidence_feed_members
                 WHERE project_id=?4 AND origin_id=?5 AND feed=?6",
                    params![
                        project_key,
                        origin_key,
                        feed_key,
                        expected[0].0,
                        expected[0].1,
                        expected[0].2
                    ],
                )
                .unwrap(),
            1,
        );
        assert_eq!(projected_members(&store).len(), 2);
        assert_eq!(originals(&store), canonical);
        let report = store.verify_all().unwrap();
        assert!(!report.is_healthy(), "{changed_scope}: {report:?}");
        assert!(super::super::witnesses::verify_counts(&store.connection).is_err());
        drop(store);

        assert!(
            SqliteStore::repair_rebuildable_projections(&database)
                .unwrap()
                .is_healthy()
        );
        let store = SqliteStore::open(&database).unwrap();
        assert_eq!(projected_members(&store), expected, "{changed_scope}");
        assert_eq!(originals(&store), canonical, "{changed_scope}");
        healthy(&store);
    }
}
