//! The ordinary folder route touches only the change set: an export after
//! five edits and one addition, an import of those changes and one removal,
//! an unchanged repeat, and the reverse direction read, decode and write the
//! same documents and do the same store work at 2,000 tasks as at 200.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    time::Duration,
};

use super::LocalWorkService;
use crate::{
    ActorContext, CreateWorkRequest, DevelopmentNoopRedactor, ProjectId, SessionId, SqliteStore,
    WorkId,
    domain::{AssuranceLevel, ChildRequirement, WorkItemKind, WorkLifecycle, WorkOrigin},
    task_sync::{
        self, FORMAT, ProjectDocument, TaskDocument, TaskTree,
        data_worktree::DataWorktree,
        directory::{self, DestinationBasis},
        git,
        progress::ExportProfile,
    },
};

const PROJECT: &str = "folder-counts";
const BRANCH: &str = "engram-data";

fn project() -> ProjectId {
    ProjectId(PROJECT.into())
}

pub(super) fn profile() -> ExportProfile {
    ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

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
        reason: "measure the ordinary folder route".into(),
    }
}

pub(super) fn run(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = git::command(root, args);
    command
        .env("GIT_AUTHOR_NAME", "Counts fixture")
        .env("GIT_AUTHOR_EMAIL", "counts@example.invalid")
        .env("GIT_COMMITTER_NAME", "Counts fixture")
        .env("GIT_COMMITTER_EMAIL", "counts@example.invalid")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-global-config"));
    git::run_query_command(
        command,
        args[0],
        false,
        None,
        Duration::from_secs(60),
        git::QueryStderrPolicy::ErrorOnly,
    )
    .unwrap()
}

pub(super) fn document(index: usize) -> TaskDocument {
    TaskDocument {
        id: WorkId(uuid::Uuid::new_v4()),
        external_ref: None,
        title: format!("task {index}"),
        outcome: format!("Deliver task {index}"),
        acceptance: vec!["delivered".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: std::collections::BTreeMap::default(),
    }
}

pub(super) struct Host {
    database: PathBuf,
    pub(super) service: LocalWorkService,
}

impl Host {
    fn new(root: &Path, name: &str) -> Self {
        let database = crate::project_database_path(&root.join(name), &project());
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        drop(SqliteStore::open(&database).unwrap());
        task_sync::settings::set(
            &database,
            &project(),
            task_sync::settings::Action::Enable,
            Some("fixture: explicit task sync opt-in for the folder counts"),
            "counts-fixture",
            "counts-fixture",
            chrono::Utc::now(),
        )
        .unwrap();
        let service = LocalWorkService::new(
            database.clone(),
            project(),
            "counts-fixture".into(),
            SessionId(format!("counts-{name}")),
            None,
        );
        Self { database, service }
    }

    /// This host's store file.
    pub(super) fn database(&self) -> &Path {
        &self.database
    }

    pub(super) fn rename(&self, ids: &[WorkId], suffix: &str) {
        let mut store = SqliteStore::open(&self.database).unwrap();
        for id in ids {
            let current = store.get_work_item(*id).unwrap();
            store
                .revise_work(
                    &crate::ReviseWorkRequest {
                        work_id: *id,
                        expected_revision: current.revision,
                        patch: crate::WorkRevisionPatch {
                            title: Some(format!("{} {suffix}", current.title)),
                            ..Default::default()
                        },
                        authority: crate::WorkPlanningAuthority::Project,
                        actor: actor(),
                        idempotency_key: format!("{suffix}-{}", id.0),
                        updated_at: chrono::Utc::now(),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
    }

    pub(super) fn lifecycle(&self, id: WorkId) -> crate::WorkLifecycle {
        SqliteStore::open(&self.database)
            .unwrap()
            .get_work_item(id)
            .unwrap()
            .lifecycle
    }

    pub(super) fn add(&self, title: &str) -> WorkId {
        let mut store = SqliteStore::open(&self.database).unwrap();
        store
            .create_work(
                &CreateWorkRequest {
                    acceptance_bindings: Vec::new(),
                    evaluation_mode: None,
                    external_ref: None,
                    notes: Vec::new(),
                    project_id: project(),
                    parent_id: None,
                    child_requirement: ChildRequirement::Required,
                    title: title.into(),
                    outcome: "Join the shared folder".into(),
                    acceptance: vec!["exported".into()],
                    kind: WorkItemKind::Task,
                    priority: 1,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    origin: WorkOrigin::Local,
                    source_snapshot_id: None,
                    actor: actor(),
                    idempotency_key: format!("add-{title}"),
                    created_at: chrono::Utc::now(),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap()
            .work_id
    }

    /// Whether this host's store holds a task with `id`.
    pub(super) fn holds(&self, id: WorkId) -> bool {
        SqliteStore::open(&self.database)
            .unwrap()
            .get_work_item(id)
            .is_ok()
    }

    /// A task of another project in this host's store.
    pub(super) fn add_to_project(&self, project_id: &str, title: &str) -> WorkId {
        let mut store = SqliteStore::open(&self.database).unwrap();
        store
            .create_work(
                &CreateWorkRequest {
                    acceptance_bindings: Vec::new(),
                    evaluation_mode: None,
                    external_ref: None,
                    notes: Vec::new(),
                    project_id: ProjectId(project_id.into()),
                    parent_id: None,
                    child_requirement: ChildRequirement::Required,
                    title: title.into(),
                    outcome: "Belong to another project".into(),
                    acceptance: vec!["kept apart".into()],
                    kind: WorkItemKind::Task,
                    priority: 1,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    origin: WorkOrigin::Local,
                    source_snapshot_id: None,
                    actor: actor(),
                    idempotency_key: format!("add-{project_id}-{title}"),
                    created_at: chrono::Utc::now(),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap()
            .work_id
    }

    /// Claims `id` for this host's planner session, engaging its run.
    pub(super) fn claim(&self, id: WorkId) -> crate::domain::WorkClaim {
        let mut store = SqliteStore::open(&self.database).unwrap();
        let item = store.get_work_item(id).unwrap();
        store
            .claim_work(
                &crate::ClaimWorkRequest {
                    work_id: id,
                    expected_work_revision: item.revision,
                    expected_run_id: item.active_run_id,
                    holder: SessionId("planner".into()),
                    ttl_seconds: 3600,
                    recovery_reason: None,
                    actor: actor(),
                    idempotency_key: format!("claim-{}", id.0),
                    claimed_at: chrono::Utc::now(),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap()
    }

    pub(super) fn release(&self, claim: &crate::domain::WorkClaim) {
        let mut store = SqliteStore::open(&self.database).unwrap();
        let item = store.get_work_item(claim.work_id).unwrap();
        store
            .release_work(
                &crate::domain::ReleaseWorkRequest {
                    work_id: claim.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: item.revision,
                    holder: claim.holder.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    reason: "finish the claim before retrying the import".into(),
                    waiver_reason: Some("the fixture records no task contribution".into()),
                    actor: actor(),
                    idempotency_key: format!("release-{}", claim.claim_id.0),
                    released_at: chrono::Utc::now(),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }

    pub(super) fn title(&self, id: WorkId) -> String {
        SqliteStore::open(&self.database)
            .unwrap()
            .get_work_item(id)
            .unwrap()
            .title
    }
}

/// What one phase read, decoded and wrote.
#[derive(Debug, Default)]
struct Phase {
    route: String,
    written: usize,
    removed: usize,
    file_reads: usize,
    blob_reads: usize,
    decodes: usize,
}

fn reset() {
    super::task_sync_folder_export::FILE_READS.with(|count| count.set(0));
    let _ = git::take_blob_reads();
    crate::canonical::reset_canonical_decode_count();
}

fn observed(route: &str, written: usize, removed: usize) -> Phase {
    Phase {
        route: route.into(),
        written,
        removed,
        file_reads: super::task_sync_folder_export::FILE_READS.with(std::cell::Cell::get),
        blob_reads: git::take_blob_reads()
            .iter()
            .filter(|path| path.starts_with(".engram/tasks/"))
            .count(),
        decodes: crate::canonical::canonical_decode_count(),
    }
}

/// One data worktree shared by two hosts, both enrolled from the same
/// complete folder of generated tasks.
pub(super) struct Exchange {
    code: PathBuf,
    pub(super) checkout: PathBuf,
    pub(super) tree: TaskTree,
    pub(super) a: Host,
    pub(super) b: Host,
    /// Last, because fields drop in declaration order: both hosts close their
    /// stores before the home is removed, and Windows cannot remove a file
    /// that is still open.
    home: crate::test_support::TempHome,
}

impl Exchange {
    pub(super) fn new(size: usize) -> Self {
        let exchange = Self::unborn(size);
        directory::write_data_folder(&exchange.data(), &exchange.tree, &profile()).unwrap();
        exchange.commit("Publish the shared planning");
        for host in [&exchange.a, &exchange.b] {
            let enrolled = host
                .service
                .import_task_sync_data_folder(
                    &exchange.data(),
                    Some(&DestinationBasis::Empty),
                    false,
                    false,
                    None,
                    chrono::Utc::now(),
                )
                .unwrap();
            assert_eq!(enrolled.route, "enrol");
        }
        exchange
    }

    pub(super) fn unborn(size: usize) -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let root = home.path();
        let code = root.join("code");
        fs::create_dir(&code).unwrap();
        run(&code, &["init", "--initial-branch=code"]);
        fs::write(code.join(".gitignore"), "/.engram-data/\n").unwrap();
        fs::write(code.join("code.rs"), "fn main() {}\n").unwrap();
        run(&code, &["add", ".gitignore", "code.rs"]);
        run(&code, &["commit", "-m", "Seed code"]);
        let checkout = code.join(".engram-data");
        run(
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
        run(
            &checkout,
            &["symbolic-ref", "HEAD", "refs/heads/engram-data"],
        );
        run(&checkout, &["read-tree", "--empty"]);
        let mut tasks = (0..size).map(document).collect::<Vec<_>>();
        tasks.sort_by_key(|task| task.id.0);
        let tree = TaskTree {
            project: ProjectDocument {
                format: FORMAT.into(),
                project_id: project(),
                notes_included: false,
            },
            tasks,
        };
        let a = Host::new(root, "home-a");
        let b = Host::new(root, "home-b");
        Self {
            code,
            checkout,
            tree,
            a,
            b,
            home,
        }
    }

    pub(super) fn data(&self) -> DataWorktree {
        DataWorktree::observe(&self.code, &self.checkout, BRANCH).unwrap()
    }

    pub(super) fn git(&self, args: &[&str]) {
        run(&self.checkout, args);
    }

    pub(super) fn commit(&self, subject: &str) {
        self.git(&["add", "--all", "--", ".engram"]);
        self.git(&["commit", "-m", subject]);
    }

    pub(super) fn ids(&self) -> Vec<WorkId> {
        self.tree.tasks.iter().map(|task| task.id).collect()
    }
}

pub(super) fn counted_queries<T>(run: impl FnOnce() -> T) -> (T, BTreeMap<String, usize>) {
    reset();
    let queries = Rc::new(RefCell::new(BTreeMap::new()));
    let observed_queries = Rc::clone(&queries);
    let previous = git::BEFORE_QUERY.with(|hook| {
        hook.replace(Some(Box::new(move |verb| {
            *observed_queries
                .borrow_mut()
                .entry(verb.to_owned())
                .or_insert(0) += 1;
        })))
    });
    let result = run();
    git::BEFORE_QUERY.with(|hook| {
        hook.replace(previous);
    });
    (result, Rc::try_unwrap(queries).unwrap().into_inner())
}

fn counted_export(exchange: &Exchange) -> (Phase, BTreeMap<String, usize>) {
    let (receipt, queries) = counted_queries(|| {
        exchange.a.service.export_task_sync_data_folder(
            &exchange.data(),
            &profile(),
            false,
            None,
            chrono::Utc::now(),
        )
    });
    let receipt = receipt.unwrap();
    let phase = observed(receipt.route, receipt.written, receipt.removed);
    (phase, queries)
}

#[test]
fn task_sync_status_queries_keep_their_verb_with_filter_overrides() {
    let exchange = Exchange::new(1);
    let (baseline, baseline_queries) = counted_export(&exchange);
    assert_eq!(baseline.route, "unchanged");

    exchange.git(&[
        "config",
        "--local",
        "filter.engram-count-fixture.clean",
        "false",
    ]);
    let (configured, configured_queries) = counted_export(&exchange);
    assert_eq!(configured.route, "unchanged");
    assert_eq!(configured_queries.get("status"), Some(&1));
    assert!(!configured_queries.contains_key("-c"));
    assert_eq!(configured_queries, baseline_queries);

    let error = git::query_listing(
        &exchange.checkout,
        &[
            "-c",
            "filter.engram-count-fixture.clean=",
            "status",
            "--engram-fixture-invalid",
        ],
        Duration::from_secs(60),
        git::MAX_LISTING_BYTES,
    )
    .unwrap_err();
    assert!(
        matches!(
            error.downcast_ref::<git::QueryFailure>(),
            Some(git::QueryFailure::Nonzero { command, .. }) if command == "status"
        ),
        "{error:#}"
    );
}

fn unenrolled_exchange(size: usize) -> Exchange {
    let exchange = Exchange::unborn(size);
    let host = &exchange.a;
    let now = chrono::Utc::now;
    let seed = directory::write_folder(
        &exchange.home.path().join("bootstrap-seed"),
        &exchange.tree,
        &profile(),
    )
    .unwrap();
    host.service
        .import_task_sync_folder(&seed, &DestinationBasis::Empty, false, now())
        .unwrap();
    assert!(
        host.service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .is_none()
    );
    exchange
}

fn count_bootstrap_confirmation(size: usize) -> BTreeMap<String, usize> {
    let exchange = unenrolled_exchange(size);
    let service = &exchange.a.service;
    let data = exchange.data();
    assert!(data.head_commit().is_none());
    let publication = service
        .export_task_sync_data_folder(&data, &profile(), true, None, chrono::Utc::now())
        .unwrap();
    assert_eq!(
        (publication.route, publication.written),
        ("bootstrap", size)
    );
    exchange.commit("Publish the first rebuilt folder");
    let data = exchange.data();
    let commit = data.head_commit().unwrap();
    let commit_and_parents = run(
        &exchange.checkout,
        &["rev-list", "--parents", "-n", "1", "HEAD"],
    );
    assert_eq!(
        std::str::from_utf8(&commit_and_parents).unwrap().trim(),
        commit
    );

    let (phase, queries) = counted_export(&exchange);
    eprintln!(
        "first export after committed rebuild bootstrap: tasks={size} Git starts={queries:?}"
    );
    assert_eq!(
        (phase.route.as_str(), phase.written, phase.removed),
        ("unchanged", 0, 0)
    );
    assert_eq!((phase.blob_reads, phase.file_reads), (0, 0));
    assert_eq!(queries.values().sum::<usize>(), 33);
    assert_eq!(queries.get("status"), Some(&1));
    assert_eq!(queries.get("diff-tree"), Some(&1));
    let (_, head) = service
        .task_sync_folder_head(&profile(), chrono::Utc::now())
        .unwrap()
        .unwrap();
    assert_eq!(head.import_basis().unwrap().commit, commit);
    queries
}

fn count_inexact_rebuild_import(size: usize) -> BTreeMap<String, usize> {
    let exchange = unenrolled_exchange(size);
    let host = &exchange.a;
    let now = chrono::Utc::now;
    host.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Publish the store's bootstrap");
    assert_eq!(counted_export(&exchange).0.route, "unchanged");
    let (_, head) = host
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert!(head.source.is_none());
    assert!(head.import_basis().is_some());

    host.rename(&exchange.ids()[..1], "edited before rebuilding");
    host.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), true, None, now())
        .unwrap();
    let evidence =
        task_sync::evidence::path::pack(&crate::ObjectId::mint(), &crate::ObjectId::mint());
    let path = exchange.checkout.join(&evidence);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"opaque evidence alongside the rebuild").unwrap();
    exchange.git(&["add", "--", &evidence]);
    exchange.commit("Publish the rebuild and evidence together");
    let (receipt, queries) = counted_queries(|| {
        host.service
            .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
    });
    let phase = observed(receipt.unwrap().route, 0, 0);
    assert_eq!(phase.route, "complete");
    assert_eq!(phase.blob_reads, 0);
    eprintln!("import after inexact rebuild: tasks={size} Git starts={queries:?}");
    queries
}

fn count_export_after_own_rebuild_basis(size: usize) -> BTreeMap<String, usize> {
    let exchange = Exchange::new(size);
    let host = &exchange.a;
    let id = exchange.ids()[0];
    let now = chrono::Utc::now;
    exchange.git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance the unimported tip",
    ]);
    host.rename(&[id], "at the rebuild");
    let rebuilt = host
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), true, None, now())
        .unwrap();
    assert_eq!(rebuilt.route, "rebuild");
    let basis = DestinationBasis::Recorded(Box::new(
        directory::read_folder_basis(&exchange.checkout.join(".engram/project.json")).unwrap(),
    ));
    exchange.commit("Publish the rebuild");
    host.rename(&[id], "after the rebuild");
    let imported = host
        .service
        .import_task_sync_data_folder(&exchange.data(), Some(&basis), false, false, None, now())
        .unwrap();
    assert_eq!(imported.route, "complete");

    let (phase, queries) = counted_export(&exchange);
    assert_eq!((phase.route.as_str(), phase.written), ("incremental", 1));
    assert_eq!((phase.blob_reads, phase.file_reads), (0, 1));
    assert_eq!(queries.get("status"), Some(&1));
    assert!(!queries.contains_key("diff-tree"));
    eprintln!("first export after own rebuild basis: tasks={size} Git starts={queries:?}");
    queries
}

#[test]
fn ordinary_export_after_own_rebuild_basis_has_constant_git_starts() {
    assert_eq!(
        count_export_after_own_rebuild_basis(200),
        count_export_after_own_rebuild_basis(2_000)
    );
}

#[test]
fn ordinary_export_git_starts_are_constant_across_folder_and_change_counts() {
    let mut first_after_rebuild = Vec::new();
    let mut write_queries = Vec::new();
    let mut confirm_queries = Vec::new();
    for size in [200, 2_000] {
        let exchange = Exchange::new(size);
        let ids = exchange.ids();
        let host = &exchange.a;
        let now = chrono::Utc::now;
        host.service
            .export_task_sync_data_folder(&exchange.data(), &profile(), true, None, now())
            .unwrap();
        exchange.commit("Rebuild the shared folder");
        let (phase, queries) = counted_export(&exchange);
        eprintln!("first export after committed rebuild: tasks={size} Git starts={queries:?}");
        assert_eq!(
            (phase.route.as_str(), phase.written, phase.removed),
            ("unchanged", 0, 0)
        );
        assert_eq!((phase.blob_reads, phase.file_reads), (0, 0));
        assert_eq!(queries.get("diff-tree"), Some(&1));
        assert_eq!(queries.get("status"), Some(&1));
        first_after_rebuild.push(queries);

        let change_counts: &[usize] = if size == 200 { &[1] } else { &[1, 100, 2_000] };
        for &changed in change_counts {
            host.rename(&ids[..changed], &format!("local edit of {changed}"));
            let (phase, queries) = counted_export(&exchange);
            eprintln!("ordinary export: tasks={size} changed={changed} Git starts={queries:?}");
            assert_eq!(
                (phase.route.as_str(), phase.written, phase.removed),
                ("incremental", changed, 0)
            );
            assert_eq!((phase.blob_reads, phase.file_reads), (0, changed));
            assert!(!queries.contains_key("diff-tree"));
            assert_eq!(queries.get("status"), Some(&1));
            assert_eq!(queries.get("for-each-ref"), Some(&1));
            write_queries.push(queries);
            exchange.commit("Publish this host's edits");

            let (phase, queries) = counted_export(&exchange);
            eprintln!("confirm own export: tasks={size} changed={changed} Git starts={queries:?}");
            assert_eq!((phase.route.as_str(), phase.written), ("unchanged", 0));
            assert_eq!((phase.blob_reads, phase.file_reads), (0, 0));
            assert_eq!(queries.get("diff-tree"), Some(&1));
            assert_eq!(queries.get("status"), Some(&1));
            confirm_queries.push(queries);
        }
    }
    assert_eq!(first_after_rebuild[0], first_after_rebuild[1]);
    assert!(write_queries.windows(2).all(|pair| pair[0] == pair[1]));
    assert!(confirm_queries.windows(2).all(|pair| pair[0] == pair[1]));
    assert_eq!(
        count_bootstrap_confirmation(200),
        count_bootstrap_confirmation(2_000)
    );
    assert_eq!(
        count_inexact_rebuild_import(2),
        count_inexact_rebuild_import(11)
    );
}

/// The phases of one exchange cycle over a shared folder of `size` tasks.
fn measure(size: usize) -> [Phase; 5] {
    let exchange = Exchange::new(size);
    let (a, b, tree, checkout) = (&exchange.a, &exchange.b, &exchange.tree, &exchange.checkout);
    let data = || exchange.data();
    let commit = |subject: &str| exchange.commit(subject);
    let now = chrono::Utc::now;
    let ids = exchange.ids();

    // A edits five tasks and adds one; its export writes those six files.
    a.rename(&ids[..5], "edited on A");
    let added = a.add("added on A");
    reset();
    let receipt = a
        .service
        .export_task_sync_data_folder(&data(), &profile(), false, None, now())
        .unwrap();
    let export = observed(receipt.route, receipt.written, receipt.removed);
    // The merge resolution also removes one task.
    let removed = ids[5];
    fs::remove_file(
        checkout
            .join(".engram/tasks")
            .join(format!("{}.json", removed.0)),
    )
    .unwrap();
    commit("Edit five tasks, add one and remove one");

    // B reads only the seven changed paths, then nothing on a repeat.
    reset();
    let receipt = b
        .service
        .import_task_sync_data_folder(&data(), None, false, false, None, now())
        .unwrap();
    let import = observed(receipt.route, 0, 0);
    assert_eq!(
        b.title(ids[0]),
        format!("{} edited on A", tree.tasks[0].title)
    );
    assert_eq!(b.title(added), "added on A");
    assert_eq!(
        SqliteStore::open(&b.database)
            .unwrap()
            .get_work_item(removed)
            .unwrap()
            .lifecycle,
        WorkLifecycle::Cancelled
    );
    reset();
    let receipt = b
        .service
        .import_task_sync_data_folder(&data(), None, false, false, None, now())
        .unwrap();
    let repeat = observed(receipt.route, 0, 0);
    a.service
        .import_task_sync_data_folder(&data(), None, false, false, None, now())
        .unwrap();

    // The reverse direction: B edits five other tasks and A takes them.
    b.rename(&ids[10..15], "edited on B");
    reset();
    let receipt = b
        .service
        .export_task_sync_data_folder(&data(), &profile(), false, None, now())
        .unwrap();
    let reverse_export = observed(receipt.route, receipt.written, receipt.removed);
    commit("Edit five tasks on B");
    reset();
    let receipt = a
        .service
        .import_task_sync_data_folder(&data(), None, false, false, None, now())
        .unwrap();
    let reverse_import = observed(receipt.route, 0, 0);
    assert_eq!(
        a.title(ids[10]),
        format!("{} edited on B", tree.tasks[10].title)
    );
    assert!(
        SqliteStore::open(&a.database)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    [export, import, repeat, reverse_export, reverse_import]
}

#[test]
fn the_ordinary_folder_route_reads_decodes_and_writes_only_the_change_set() {
    let small = measure(200);
    let large = measure(2_000);
    eprintln!("folder route counts at 200 tasks: {small:#?}");
    eprintln!("folder route counts at 2000 tasks: {large:#?}");
    for phases in [&small, &large] {
        let [export, import, repeat, reverse_export, reverse_import] = phases;
        // Five edits and one addition: six files written, the five edited
        // tasks' old files read for the root arithmetic.
        assert_eq!(
            (export.route.as_str(), export.written, export.removed),
            ("incremental", 6, 0)
        );
        assert_eq!(export.file_reads, 5);
        // Old and new blobs of five edits, the removal's old blob, the
        // addition's new one.
        assert_eq!(
            (import.route.as_str(), import.blob_reads),
            ("incremental", 12)
        );
        assert_eq!(
            (repeat.route.as_str(), repeat.blob_reads, repeat.file_reads),
            ("unchanged", 0, 0)
        );
        assert_eq!(
            (reverse_export.route.as_str(), reverse_export.written),
            ("incremental", 5)
        );
        assert_eq!(reverse_export.file_reads, 5);
        assert_eq!(
            (reverse_import.route.as_str(), reverse_import.blob_reads),
            ("incremental", 10)
        );
    }
    // Store work does not grow with the folder: ten times the tasks, the
    // same decodes within a small constant.
    for (small, large) in small.iter().zip(&large) {
        assert!(
            large.decodes <= small.decodes + 16,
            "{} decodes grew from {} at 200 tasks to {} at 2000",
            large.route,
            small.decodes,
            large.decodes
        );
    }
}

#[test]
fn a_dropped_exchange_removes_its_home_after_both_hosts_have_used_their_stores() {
    let exchange = Exchange::new(2);
    for host in [&exchange.a, &exchange.b] {
        let receipt = host
            .service
            .export_task_sync_data_folder(
                &exchange.data(),
                &profile(),
                false,
                None,
                chrono::Utc::now(),
            )
            .unwrap();
        assert_eq!(receipt.route, "unchanged");
    }
    let home = exchange.home.path().to_path_buf();
    assert!(home.exists());
    drop(exchange);
    assert!(!home.exists(), "{} was left behind", home.display());
}
