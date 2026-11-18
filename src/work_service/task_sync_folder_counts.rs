//! The ordinary folder route touches only the change set: an export after
//! five edits and one addition, an import of those changes and one removal,
//! an unchanged repeat, and the reverse direction read, decode and write the
//! same documents and do the same store work at 2,000 tasks as at 200.

use std::{
    fs,
    path::{Path, PathBuf},
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

fn run(root: &Path, args: &[&str]) {
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
    .unwrap();
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
    pub(super) fn claim(&self, id: WorkId) {
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
        let exchange = Self {
            code,
            checkout,
            tree,
            a,
            b,
            home,
        };
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
