use super::*;

/// Equal content revisions checked in two workspaces, followed by a source
/// change in a third workspace at that same revision. The plain note has no
/// verification facts; the large second summary needs a window placeholder.
pub(crate) fn verification_workspace_notes_fixture(
    database: &std::path::Path,
    project: &str,
    second_workspace: &str,
) -> (String, [ObjectId; 3]) {
    let mut store = SqliteStore::open(database).expect("store");
    let work = store
        .create_work(
            &root_request(project, "verification-workspaces", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "workspace-claim", 2, 36_000);
    let plain = evidence(&mut store, &work, &claim, "runner", "plain", 3);
    let basis = |workspace: &str| ExecutionSourceBasis {
        git_tree: None,
        workspace_id: workspace.into(),
        source_revision: "A3".into(),
        source_root_generation: None,
        source_root_state: None,
    };
    let summary = "large verification summary ".repeat(700);
    let mut checks = Vec::new();
    for (offset, key, workspace, summary) in [
        (0, "first-check", "workspace-A", "first check"),
        (1, "second-check", second_workspace, summary.as_str()),
    ] {
        checks.push(host_verification_with_outcome(
            &mut store,
            &work,
            &claim,
            "runner",
            HostCheck {
                key,
                kind: VerificationKind::Test,
                outcome: ExecutionOutcome::Succeeded,
                result: VerificationResult::Passed,
                summary,
            },
            4 + offset,
            basis(workspace),
        ));
    }
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "moved-root",
        6,
        Some(basis("workspace-current")),
        None,
    );
    (work.short_ref, [plain, checks.remove(0), checks.remove(0)])
}
