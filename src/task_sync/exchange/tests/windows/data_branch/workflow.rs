use super::*;
use crate::task_sync::{ProjectDocument, TaskTree, ledger::RetainedCommit};
use fixture::{BRANCH, Repositories, git, line};

pub(super) async fn target(code: &Path) -> ExchangeTarget {
    ExchangeTarget::capture(
        &TargetSelection {
            repository: code.join(".engram-data"),
            remote: "origin".into(),
            reference: BRANCH.into(),
        },
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap()
}

pub(super) async fn transport(
    target: &ExchangeTarget,
    operation: ExchangeOperation,
) -> Result<crate::task_sync::exchange::RunOutput, RunError> {
    crate::task_sync::exchange::run(
        target,
        &operation,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
}

pub(super) async fn fetch(
    code: &Path,
    target: &ExchangeTarget,
    commit: &RetainedCommit,
) -> RetainedCommit {
    fetch_operation(code, target, commit, uuid::Uuid::new_v4()).await
}

pub(super) async fn fetch_operation(
    code: &Path,
    target: &ExchangeTarget,
    commit: &RetainedCommit,
    operation: uuid::Uuid,
) -> RetainedCommit {
    let pin = format!("refs/engram-exchange/fixture/{operation}");
    transport(
        target,
        ExchangeOperation::Fetch {
            commit: commit.oid.clone(),
            retention_ref: pin.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        line(&code.join(".engram-data"), &["rev-parse", &pin]),
        commit.oid
    );
    crate::task_sync::exchange::retain_commit(
        &code.join(".engram-data"),
        operation,
        &commit.oid,
        RunLimits::default(),
        &Cancellation::default(),
    )
    .await
    .unwrap()
}

pub(super) fn stage(code: &Path, tree: &TaskTree) {
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    crate::task_sync::directory::write_folder(&code.join(".engram-data/.engram"), tree, &profile)
        .unwrap();
}

pub(super) fn empty_tree() -> TaskTree {
    TaskTree {
        project: ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: crate::ProjectId("https-data-fixture".into()),
            notes_included: false,
        },
        tasks: Vec::new(),
    }
}

pub(super) async fn incoming(root: &Path) {
    let repos = Repositories::new(root);
    let before_left = Repositories::code_snapshot(&repos.left);
    let before_right = Repositories::code_snapshot(&repos.right);
    let receiver = super::incoming::prepare(&repos).await;
    // Incoming admission has its own real carrier and hang guard. Outgoing
    // rejection/retry assertions run independently in the strict workflow.
    let (before, after) = Box::pin(super::outgoing::incoming_carrier(&repos)).await;
    let phase = super::timing::phase("incoming-workflow");
    Box::pin(super::incoming::exercise(&repos, &before, &after, receiver)).await;
    drop(phase);
    assert_eq!(Repositories::code_snapshot(&repos.left), before_left);
    assert_eq!(Repositories::code_snapshot(&repos.right), before_right);
    assert!(!root.join("server-errors").exists());
    println!("data-branch-workflow complete");
}

pub(super) async fn run(root: &Path) {
    let repos = Repositories::new(root);
    let before_left = Repositories::code_snapshot(&repos.left);
    let before_right = Repositories::code_snapshot(&repos.right);
    let tree = empty_tree();
    stage(&repos.left, &tree);
    let seed = Repositories::commit(
        &repos.left,
        uuid::Uuid::new_v4(),
        Vec::new(),
        "Seed orphan task metadata",
    );
    let left = Repositories::data(&repos.left);
    left.advance_retained(&seed, None).unwrap();
    let left_target = target(&repos.left).await;
    transport(
        &left_target,
        ExchangeOperation::Push {
            commit: seed.oid.clone(),
        },
    )
    .await
    .unwrap();
    let right_target = target(&repos.right).await;
    let received = fetch(&repos.right, &right_target, &seed).await;
    let right = Repositories::data(&repos.right);
    // Populate only this orphan data index/worktree before the public retained
    // fast-forward. This neither moves a ref nor touches either code index.
    git(
        &right.checkout,
        &["read-tree", "--reset", "-u", &received.oid],
    );
    right.advance_retained(&received, None).unwrap();
    assert_eq!(
        crate::task_sync::git::read_task_snapshot_bounded(
            &right.checkout,
            &tree.project.project_id,
            &received.oid,
            Duration::from_secs(30),
        )
        .unwrap()
        .tree,
        tree
    );
    assert_eq!(
        git(&left.checkout, &["cat-file", "commit", &seed.oid]),
        git(&right.checkout, &["cat-file", "commit", &received.oid])
    );

    let local = Repositories::commit(
        &repos.left,
        uuid::Uuid::new_v4(),
        vec![seed.oid.clone()],
        "Retain local data choice",
    );
    Repositories::data(&repos.left)
        .advance_retained(&local, Some(&seed.oid))
        .unwrap();
    let peer = Repositories::commit(
        &repos.right,
        uuid::Uuid::new_v4(),
        vec![seed.oid.clone()],
        "Retain concurrent data choice",
    );
    Repositories::data(&repos.right)
        .advance_retained(&peer, Some(&seed.oid))
        .unwrap();
    // The peer advances when this push asks for receive-pack advertisement,
    // after the caller selected its old basis. Real Git rejects non-FF and the
    // actual product parser classifies its complete porcelain response.
    fs::write(
        root.join("race.json"),
        serde_json::to_vec(&serde_json::json!({
            "repository": right.checkout, "commit": peer.oid,
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        transport(
            &left_target,
            ExchangeOperation::Push {
                commit: local.oid.clone()
            }
        )
        .await
        .unwrap_err(),
        RunError::NonFastForward
    );
    assert!(root.join("race-fired.json").is_file());
    let peer_on_left = fetch(&repos.left, &left_target, &peer).await;
    let resolved = Repositories::commit(
        &repos.left,
        uuid::Uuid::new_v4(),
        vec![local.oid.clone(), peer_on_left.oid.clone()],
        "Record agent-resolved task data",
    );
    Repositories::data(&repos.left)
        .advance_retained(&resolved, Some(&local.oid))
        .unwrap();
    transport(
        &left_target,
        ExchangeOperation::Push {
            commit: resolved.oid.clone(),
        },
    )
    .await
    .unwrap();
    let observed = transport(&right_target, ExchangeOperation::Observe)
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8(observed.stdout).unwrap(),
        format!("{}\t{BRANCH}\n", resolved.oid)
    );
    assert_eq!(resolved.parents, vec![local.oid, peer_on_left.oid]);
    assert_eq!(Repositories::code_snapshot(&repos.left), before_left);
    assert_eq!(Repositories::code_snapshot(&repos.right), before_right);
    assert!(!repos.left.join(".engram").exists() && !repos.right.join(".engram").exists());
    assert!(!root.join("server-errors").exists());
    let requests = fs::read_to_string(root.join("requests")).unwrap();
    assert!(requests.contains("POST /data.git/git-receive-pack"));
    assert!(requests.contains("POST /data.git/git-upload-pack"));
    println!("data-branch-workflow complete");
}
