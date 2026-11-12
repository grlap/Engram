//! Real cold Git and storage admission; the HTTPS workflow has its own Windows proof.

use super::*;
use crate::task_sync::delta::CustodyArtifactKind;
use crate::task_sync::{data_worktree::DataWorktree, directory, exchange};
use std::{fs, path::Path};

pub(super) fn git(root: &Path, args: &[&str]) -> String {
    let mut command = crate::task_sync::git::command(root, args);
    command
        .env("GIT_AUTHOR_NAME", "Bootstrap fixture")
        .env("GIT_AUTHOR_EMAIL", "bootstrap@example.invalid")
        .env("GIT_COMMITTER_NAME", "Bootstrap fixture")
        .env("GIT_COMMITTER_EMAIL", "bootstrap@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"));
    let bytes = crate::task_sync::git::run_query_command(
        command,
        args[0],
        false,
        None,
        std::time::Duration::from_secs(30),
        crate::task_sync::git::QueryStderrPolicy::ErrorOnly,
    )
    .unwrap();
    String::from_utf8(bytes).unwrap().trim().into()
}

pub(super) async fn fixture() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    ObjectId,
    CustodyAuditRequest,
) {
    let (home, mut store, mut binding, tree) = initial_fixture();
    let data = real_repository(home.path(), &tree, &mut binding);
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let commit = exchange::retain_commit(
        &data.checkout,
        request.operation,
        data.head.as_ref().unwrap(),
        exchange::RunLimits::default(),
        &exchange::Cancellation::default(),
    )
    .await
    .unwrap();
    request.seed = CustodySeedSelection::IncomingDataCommit { commit };
    (home, store, binding, id, request)
}

/// A real code checkout with its dedicated `.engram-data` worktree holding
/// `tree`, which `binding` then selects in place of its placeholder repository.
/// Call it before the binding is activated.
pub(super) fn real_repository(
    home: &Path,
    tree: &crate::task_sync::TaskTree,
    binding: &mut Binding,
) -> DataWorktree {
    real_repository_in(home, tree, binding, "sha1")
}

/// The same repository in an explicit Git object format, for a fixture whose
/// recorded commits use that format's object-id width.
pub(super) fn real_repository_in(
    home: &Path,
    tree: &crate::task_sync::TaskTree,
    binding: &mut Binding,
    object_format: &str,
) -> DataWorktree {
    let code = home.join("code");
    fs::create_dir(&code).unwrap();
    git(
        &code,
        &[
            "init",
            "--initial-branch=code",
            &format!("--object-format={object_format}"),
        ],
    );
    fs::write(code.join(".gitignore"), "/.engram-data/\n/target/\n").unwrap();
    fs::write(code.join("source.rs"), "fn main() {}\n").unwrap();
    git(&code, &["add", ".gitignore", "source.rs"]);
    git(
        &code,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "Seed code fixture",
        ],
    );
    let checkout = code.join(".engram-data");
    git(
        &code,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            checkout.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(
        &checkout,
        &["symbolic-ref", "HEAD", "refs/heads/engram-data"],
    );
    git(&checkout, &["read-tree", "--empty"]);
    directory::write_folder(&checkout.join(".engram"), tree, &binding.profile).unwrap();
    git(&checkout, &["add", ".engram"]);
    git(
        &checkout,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "Seed wrapped task data",
        ],
    );
    let data = DataWorktree::observe(&code, &checkout, "engram-data").unwrap();
    binding.repository = data.repository_basis(binding.repository.enrollment);
    binding.branch.clone_from(&data.reference);
    binding.target.reference.clone_from(&data.reference);
    data
}

#[tokio::test]
async fn incoming_data_bootstrap_owns_real_pin_without_legacy_preparation() {
    let (_home, mut store, binding, id, request) = fixture().await;
    let code = binding.repository.main_checkout.parent().unwrap();
    let code_head = git(code, &["rev-parse", "HEAD"]);
    let index = fs::read(code.join(".git/index")).unwrap();
    let before = store.export_task_tree(&binding.project).unwrap();
    let original = store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap();
    let event: Event =
        SqliteStore::get_typed_object_on(&store.connection, &original.checkpoint, EVENT_KIND)
            .unwrap()
            .unwrap();
    let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
        panic!("checkpoint expected")
    };
    assert!(checkpoint.data_commit().is_some());
    assert!(checkpoint.artifacts.iter().any(|artifact| artifact.kind
        == CustodyArtifactKind::IncomingDataCommit
        && artifact.record.is_none()));
    assert_eq!(store.export_task_tree(&binding.project).unwrap(), before);
    assert_eq!(git(code, &["rev-parse", "HEAD"]), code_head);
    assert_eq!(fs::read(code.join(".git/index")).unwrap(), index);
    let ledger = Ledger::load(&store.connection).unwrap();
    assert!(
        !ledger
            .events
            .values()
            .any(|event| matches!(event.transition, Transition::Prepared { .. }))
    );
    let next = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    store
        .audit_task_sync_custody(&next, &binding.actor, at(31))
        .unwrap();
    let repeated = store
        .audit_task_sync_custody(&request, &binding.actor, at(32))
        .unwrap();
    assert_eq!(repeated.checkpoint, original.checkpoint);
    assert!(repeated.repeated && repeated.acknowledged);
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    producing_origin::check_on(&transaction, &binding.project, &guard)
        .unwrap()
        .require_on(&transaction)
        .unwrap();
    drop(transaction);
    drop(guard);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn incoming_data_bootstrap_refuses_missing_pin_and_recovers_original_acknowledgement() {
    let (_home, mut store, binding, _, request) = fixture().await;
    let CustodySeedSelection::IncomingDataCommit { commit } = &request.seed else {
        unreachable!()
    };
    let reference = &commit.retention_ref;
    git(
        &binding.repository.main_checkout,
        &["update-ref", "-d", reference],
    );
    let before = state(&store);
    let error = store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("retention reference unavailable"),
        "{error}",
    );
    assert_eq!(state(&store), before);
    git(
        &binding.repository.main_checkout,
        &["update-ref", reference, &commit.oid],
    );
    super::super::super::audit::FAIL_STAGE.with(|failure| failure.set(Some("repository_ack")));
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(31))
            .is_err()
    );
    let pending =
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project)
            .unwrap()
            .audit
            .unwrap();
    let recovered = store
        .audit_task_sync_custody(&request, &binding.actor, at(32))
        .unwrap();
    assert_eq!(recovered.checkpoint, pending.event_id);
    assert!(recovered.repeated && recovered.acknowledged);
    assert!(store.verify_all().unwrap().is_healthy());
}
