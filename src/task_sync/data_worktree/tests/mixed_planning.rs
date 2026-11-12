use super::*;
use crate::{
    ActorContext, CreateWorkRequest, DevelopmentNoopRedactor, ProjectId, SessionId, SqliteStore,
    domain::{AssuranceLevel, ChildRequirement, WorkItemKind, WorkOrigin},
    task_sync::{directory, progress::ExportProfile},
};

fn actor() -> ActorContext {
    ActorContext {
        actor_id: "planner".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId("planner".into())),
        source_tool: None,
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "exchange isolated mixed planning and evidence data".into(),
    }
}

fn create_task(store: &mut SqliteStore, project: &ProjectId) -> crate::domain::WorkItem {
    store
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Shared task".into(),
                outcome: "Exchange task text alongside inert evidence".into(),
                acceptance: vec!["Planning round trip succeeds".into()],
                kind: WorkItemKind::Task,
                priority: 1,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: actor(),
                idempotency_key: "shared".into(),
                created_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

// Only these disposable local clones use transport; production queries stay hardened.
fn exchange_git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = git::command(root, &["-c", "protocol.file.allow=always"]);
    command
        .args(args)
        .env("GIT_ALLOW_PROTOCOL", "file")
        .env("GIT_AUTHOR_NAME", "Data fixture")
        .env("GIT_AUTHOR_EMAIL", "data@example.invalid")
        .env("GIT_COMMITTER_NAME", "Data fixture")
        .env("GIT_COMMITTER_EMAIL", "data@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"));
    git::run_query_command(
        command,
        args[0],
        false,
        None,
        Duration::from_secs(30),
        git::QueryStderrPolicy::ErrorOnly,
    )
    .unwrap()
}

fn clone_peer(root: &Path, remote: &Path, name: &str) -> DataWorktree {
    exchange_git(root, &["clone", remote.to_str().unwrap(), name]);
    let code = root.join(name);
    let data = code.join(DIRECTORY_NAME);
    run(
        &code,
        &[
            "worktree",
            "add",
            "--detach",
            "--no-checkout",
            data.to_str().unwrap(),
            "HEAD",
        ],
    );
    run(&data, &["symbolic-ref", "HEAD", "refs/heads/engram-data"]);
    run(&data, &["read-tree", "--empty"]);
    DataWorktree::observe(&code, &data, "engram-data").unwrap()
}

fn commit_push(data: &DataWorktree) -> String {
    run(&data.checkout, &["add", "--all"]);
    run(
        &data.checkout,
        &["commit", "-m", "Exchange task and evidence data"],
    );
    exchange_git(
        &data.checkout,
        &["push", "origin", "HEAD:refs/heads/engram-data"],
    );
    oid_line(&run(&data.checkout, &["rev-parse", "HEAD"])).unwrap()
}

fn receive(data: &DataWorktree) -> DataWorktree {
    exchange_git(
        &data.checkout,
        &["fetch", "origin", "refs/heads/engram-data"],
    );
    // Both paths are disposable clones under this test's repository-owned home.
    run(&data.checkout, &["reset", "--hard", "FETCH_HEAD"]);
    DataWorktree::observe(&data.code_checkout, &data.checkout, "engram-data").unwrap()
}

fn round_trip(store: &mut SqliteStore, data: &DataWorktree, project: &ProjectId, key: &str) {
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let own = store.export_task_sync_folder(project, &profile).unwrap();
    let folder = directory::read_resolved_data_folder(data).unwrap();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project,
                actor: &actor(),
                tree: &folder.tree,
                export_basis: &own.export_basis,
                profile: &profile,
                complete: true,
                now: chrono::DateTime::from_timestamp(10, 0).unwrap(),
            },
            key,
        )
        .unwrap();
    let exported = store.export_task_sync_folder(project, &profile).unwrap();
    directory::write_data_folder(data, &exported.tree, &profile).unwrap();
    assert_eq!(
        directory::read_resolved_data_folder(data).unwrap().tree,
        exported.tree
    );
}

#[test]
fn two_clones_exchange_planning_with_evidence_on_the_same_orphan_data_branch() {
    let seed = Fixture::code_only(true);
    let root = seed.home.path();
    let remote = root.join("exchange.git");
    exchange_git(
        root,
        &[
            "clone",
            "--bare",
            seed.code.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let a = clone_peer(root, &remote, "clone-a");
    let b = clone_peer(root, &remote, "clone-b");
    let code_before = [&a, &b].map(|data| {
        (
            run(&data.code_checkout, &["rev-parse", "HEAD"]),
            fs::read(data.code_checkout.join(".git/index")).unwrap(),
        )
    });
    let project = ProjectId("data/fixture".into());
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let mut producer = SqliteStore::open_in_memory().unwrap();
    let work = create_task(&mut producer, &project);
    let exported = producer
        .export_task_sync_folder(&project, &profile)
        .unwrap();
    directory::write_data_folder(&a, &exported.tree, &profile).unwrap();
    let first_pack = super::evidence::stage_pack(&project);
    a.stage_evidence_packs(&project, std::slice::from_ref(&first_pack))
        .unwrap();
    // An exact planning export must leave the co-located original pack untouched.
    directory::write_data_folder(&a, &exported.tree, &profile).unwrap();
    // Both peers use planning imports; native execution remains with the producer.
    let mut source = SqliteStore::open_in_memory().unwrap();
    round_trip(&mut source, &a, &project, "bootstrap-a");
    assert!(source.get_work_item(work.work_id).unwrap().restored);
    let first = commit_push(&a);
    let b = receive(&b);
    assert_eq!(
        crate::task_sync::git::read_task_snapshot(&b.checkout, &project, &first)
            .unwrap()
            .tree,
        exported.tree
    );
    let mut destination = SqliteStore::open_in_memory().unwrap();
    round_trip(&mut destination, &b, &project, "incoming-a");
    assert_eq!(
        destination.get_work_item(work.work_id).unwrap().title,
        work.title
    );
    let second_pack = super::evidence::stage_pack(&project);
    b.stage_evidence_packs(&project, std::slice::from_ref(&second_pack))
        .unwrap();
    let second = commit_push(&b);
    let a = receive(&a);
    let delta =
        crate::task_sync::git::read_task_delta(&a.checkout, &project, &first, &second).unwrap();
    assert!(delta.upserts.is_empty() && delta.removed.is_empty());
    round_trip(&mut source, &a, &project, "incoming-b");
    for data in [&a, &b] {
        for pack in [&first_pack, &second_pack] {
            assert_eq!(
                fs::read(data.checkout.join(pack.relative_path())).unwrap(),
                pack.to_bytes().unwrap()
            );
        }
    }
    let after = [&a, &b].map(|data| {
        (
            run(&data.code_checkout, &["rev-parse", "HEAD"]),
            fs::read(data.code_checkout.join(".git/index")).unwrap(),
        )
    });
    assert_eq!(after, code_before);
    assert!(!a.code_checkout.join(".engram").exists() && !b.code_checkout.join(".engram").exists());
}

#[test]
fn admitted_folders_preserve_opaque_packs_and_refuse_bad_shapes_or_links() {
    for fault in ["valid", "origin", "prefix", "extra", "directory", "link"] {
        let f = Fixture::new();
        let data = DataWorktree::observe(&f.code, &f.data, "engram-data").unwrap();
        let project = ProjectId("data/fixture".into());
        let store = SqliteStore::open_in_memory().unwrap();
        let profile = ExportProfile {
            include_notes: false,
            origin_machine: None,
        };
        let exported = store.export_task_sync_folder(&project, &profile).unwrap();
        directory::write_data_folder(&data, &exported.tree, &profile).unwrap();
        let valid = super::evidence::stage_pack(&project).relative_path();
        let parts = valid.split('/').collect::<Vec<_>>();
        let path = match fault {
            "origin" => valid.replace(parts[2], "unknown"),
            "prefix" => valid.replace(&format!("/{}/", parts[3]), "/wrong/"),
            "extra" => format!("{valid}.extra"),
            _ => valid.clone(),
        };
        let file = data.checkout.join(&path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        if fault == "directory" {
            fs::create_dir(&file).unwrap();
        } else if fault == "link" {
            #[cfg(unix)]
            std::os::unix::fs::symlink(f.code.join("code.rs"), &file).unwrap();
            #[cfg(not(unix))]
            {
                continue;
            }
        } else {
            fs::write(&file, b"opaque, not JSON").unwrap();
        }
        let read = directory::read_resolved_data_folder(&data);
        if fault == "valid" {
            assert_eq!(read.unwrap().tree, exported.tree);
            directory::write_data_folder(&data, &exported.tree, &profile).unwrap();
            assert_eq!(fs::read(&file).unwrap(), b"opaque, not JSON");
            assert!(directory::read_folder(&data.checkout.join(".engram")).is_err());
        } else {
            assert!(read.is_err(), "{fault}");
        }
    }
}

#[cfg(unix)]
#[test]
fn admitted_folder_rejects_links_at_every_evidence_directory_depth() {
    for depth in 2..=4 {
        let f = Fixture::new();
        let data = DataWorktree::observe(&f.code, &f.data, "engram-data").unwrap();
        let project = ProjectId("data/fixture".into());
        let profile = ExportProfile {
            include_notes: false,
            origin_machine: None,
        };
        let store = SqliteStore::open_in_memory().unwrap();
        let exported = store.export_task_sync_folder(&project, &profile).unwrap();
        directory::write_data_folder(&data, &exported.tree, &profile).unwrap();
        let path = super::evidence::stage_pack(&project).relative_path();
        let parts = path.split('/').collect::<Vec<_>>();
        let link = data.checkout.join(parts[..depth].join("/"));
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        let target = f.code.join("target/untouched");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel"), b"do not read").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(directory::read_resolved_data_folder(&data).is_err());
        assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"do not read");
    }
}
