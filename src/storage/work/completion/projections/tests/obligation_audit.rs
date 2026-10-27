use super::*;

fn fixture(changes: usize) -> (SqliteStore, Vec<WorkRunId>) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut runs = Vec::new();
    for index in 0..3 {
        let key = format!("audit-{index}");
        let work = store
            .create_work(&root_request("audit", &key, 0), &DevelopmentNoopRedactor)
            .unwrap();
        let held = claim(&mut store, &work, "holder", &key, 1, 3600);
        runs.push(held.run_id);
        for change in 0..changes {
            source_mutation(
                &mut store,
                &work,
                &held,
                "holder",
                &format!("{key}-{change}"),
                2 + i64::try_from(change).unwrap(),
                Some(&format!("revision-{change}")),
            );
        }
        let second = 3 + i64::try_from(changes).unwrap();
        let proof = evidence(&mut store, &work, &held, "holder", &key, second);
        checkpoint(
            &mut store,
            &work,
            &held,
            "holder",
            &key,
            second + 1,
            std::slice::from_ref(&proof),
        );
        complete(&mut store, &work, &held, "holder", &proof, &key, second + 2).unwrap();
    }
    (store, runs)
}

fn report(store: &SqliteStore, reuse: bool) -> (usize, Vec<String>) {
    let snapshot = store
        .connection
        .is_autocommit()
        .then(|| store.connection.unchecked_transaction().unwrap());
    let connection = snapshot.as_deref().unwrap_or(&store.connection);
    let mut obligations = if reuse {
        ObligationAudit::new(connection)
    } else {
        ObligationAudit::without_reuse(connection)
    };
    let mut report =
        SqliteStore::verify_work_projections_with_obligations_on(connection, &mut obligations)
            .unwrap();
    // Existing unrelated report sections use HashMaps. Compare every finding,
    // including duplicates, without imposing an order on those sections.
    report.1.sort();
    if let Some(snapshot) = snapshot {
        snapshot.rollback().unwrap();
    }
    report
}

#[test]
fn obligation_audit_loads_each_successful_run_once_across_both_traversals() {
    for changes in [1, 8] {
        let (store, runs) = fixture(changes);
        super::super::super::super::cost::start();
        let optimized = report(&store, true);
        let costs = super::super::super::super::cost::finish();
        assert_eq!(optimized, report(&store, false));
        assert_eq!(optimized.1, Vec::<String>::new(), "{optimized:?}");
        let loaded = costs["total"]["obligation_audit_runs"].as_object().unwrap();
        assert_eq!(loaded.len(), runs.len());
        for run in runs {
            assert_eq!(
                loaded[&run.0.to_string()],
                serde_json::json!({"successful": 1, "failed": 0}),
                "one strict run load across obligation rows and all feed occurrences"
            );
        }
    }
}

#[test]
fn obligation_audit_preserves_every_corrupt_finding_and_checked_count() {
    let (store, runs) = fixture(2);
    let records =
        super::super::super::load_work_obligation_records_on(&store.connection, runs[0], None)
            .unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(report(&store, true).1, Vec::<String>::new());
    let corruptions = [
        "UPDATE work_run_obligations SET work_revision = work_revision + 1
         WHERE obligation_id = ?1",
        "DELETE FROM work_run_obligations WHERE obligation_id = ?1",
        "UPDATE work_run_obligations SET state = 'open', resolution_id = NULL,
         resolution_kind = NULL, evidence_id = NULL, resolved_at_ms = NULL
         WHERE obligation_id = ?1",
        "UPDATE objects SET canonical_json = CAST(json_set(canonical_json,
         '$.requirement.required_environment', NULL) AS BLOB) WHERE object_id = ?2",
        "UPDATE work_feed_entries SET feed_id = 'wrong-project'
         WHERE object_id = ?3 AND feed_kind = 'project'",
    ];
    for sql in corruptions {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt_audit")
            .unwrap();
        if sql.contains("wrong-project") {
            store
                .connection
                .execute(
                    "INSERT INTO work_feed_heads(feed_kind, feed_id, position)
                 VALUES ('project', 'wrong-project', 0)",
                    [],
                )
                .unwrap();
        }
        // Numbered parameters select the relevant identity for each corruption.
        let mut statement = store.connection.prepare(sql).unwrap();
        for (index, value) in [
            records[0].obligation.obligation_id.0.to_string(),
            records[0].definition_id.to_string(),
            records[0].resolution_id.as_ref().unwrap().to_string(),
        ]
        .iter()
        .enumerate()
        .take(statement.parameter_count())
        {
            statement.raw_bind_parameter(index + 1, value).unwrap();
        }
        assert_eq!(statement.raw_execute().unwrap(), 1, "{sql}");
        drop(statement);
        super::super::super::super::cost::start();
        let optimized = report(&store, true);
        let costs = super::super::super::super::cost::finish();
        assert_ne!(optimized.1, Vec::<String>::new(), "{sql}");
        assert_eq!(optimized, report(&store, false), "{sql}");
        if sql.contains("required_environment") {
            assert!(
                optimized
                    .1
                    .iter()
                    .any(|finding| finding.contains("required_environment"))
            );
        }
        let counts = &costs["total"]["obligation_audit_runs"][runs[0].0.to_string()];
        if counts["failed"].as_u64().unwrap() > 0 {
            // Failures are deliberately uncached: a damaged sibling still fails
            // every caller, with the original labels and multiplicity.
            assert!(counts["failed"].as_u64().unwrap() > 1, "{counts}");
            assert_eq!(counts["successful"], 0);
        } else {
            // A readable run is reused; per-occurrence checks still report
            // a lost resolution or wrong feed membership independently.
            assert_eq!(counts["successful"], 1);
        }
        if sql.contains("wrong-project") {
            assert!(
                optimized
                    .1
                    .iter()
                    .any(|finding| finding.ends_with(":wrong_membership"))
            );
        }
        store
            .connection
            .execute_batch("ROLLBACK TO corrupt_audit; RELEASE corrupt_audit")
            .unwrap();
    }
}

#[test]
fn obligation_audit_matches_one_shot_for_an_unmodeled_top_level_member() {
    let (store, _) = fixture(2);
    assert_eq!(
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = CAST(json_set(canonical_json,
         '$.unsupported_member', 'synthetic') AS BLOB)
         WHERE object_id = (SELECT definition_id FROM work_run_obligations
                           ORDER BY obligation_id LIMIT 1)",
                [],
            )
            .unwrap(),
        1
    );
    let baseline = report(&store, false);
    let reused = report(&store, true);
    println!(
        "UNMODELED_TOP_LEVEL_MEMBER one_shot_findings={} reused_findings={} checked={}",
        baseline.1.len(),
        reused.1.len(),
        baseline.0
    );
    // This tests reuse equivalence, without pinning acceptance of an unmodeled
    // field. A later decoder correction can reject it in both paths.
    assert_eq!(reused, baseline);
}
