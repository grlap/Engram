//! Preparation never publishes progress state that its readers refuse.

use super::*;

const REFUSAL_START: &str = "task-sync preparation metadata is ";

/// The whole refusal an agent reads for state of `bytes` bytes.
fn refusal(bytes: usize) -> String {
    format!(
        "{REFUSAL_START}{bytes} bytes, over Engram's internal {}-byte bound; nothing was published, and the existing progress generations and output are unchanged. This is an Engram defect: report it with this byte count.",
        journal::STATE_LIMIT
    )
}

/// The CLI prints one `error:` line per cause in the chain, so the refusal
/// must be the only cause, worded exactly; returns its byte count.
fn refused_bytes(error: &anyhow::Error) -> usize {
    assert_eq!(error.chain().count(), 1, "{error:#}");
    let message = error.to_string();
    let bytes = message
        .strip_prefix(REFUSAL_START)
        .and_then(|rest| rest.split_once(" bytes,"))
        .and_then(|(bytes, _)| bytes.parse().ok())
        .unwrap_or_else(|| panic!("unexpected refusal: {message}"));
    assert_eq!(message, refusal(bytes));
    bytes
}

struct Seeded {
    home: crate::test_support::TempHome,
    store: PathBuf,
    progress: PathBuf,
    output: PathBuf,
    tree: TaskTree,
}

impl Seeded {
    /// Two completed generations with identical bytes, so that a further
    /// preparation would prune the older one before publishing its own.
    fn new() -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let store = home.path().join("store.db");
        fs::write(&store, b"route").unwrap();
        let progress = home.path().join("progress");
        let output = home.path().join("candidate");
        let seeded = Self {
            home,
            store,
            progress,
            output,
            tree: tree(2),
        };
        for _ in 0..2 {
            seeded.prepare(capture(seeded.tree.clone())).unwrap();
        }
        assert_eq!(fs::read_dir(&seeded.progress).unwrap().count(), 3);
        assert!(seeded.current().complete);
        seeded
    }

    fn prepare(&self, captured: ExportCapture) -> Result<PrepareReceipt> {
        prepare_with_producer(
            &self.output,
            &self.progress,
            &self.store,
            &self.tree.project.project_id,
            &profile(),
            false,
            &producer(),
            |_| Ok(captured),
        )
    }

    fn current(&self) -> journal::Generation {
        let loaded = journal::load(&self.progress).unwrap();
        assert_eq!(loaded.reason, None);
        loaded.current.unwrap()
    }

    fn with_reason(&self, length: usize) -> ExportCapture {
        let mut captured = capture(self.tree.clone());
        captured.full_reason = Some("r".repeat(length));
        captured
    }
}

/// Every entry under `root` with its bytes; directories carry none.
fn listing(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(root: &Path, directory: &Path, entries: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                entries.insert(relative, None);
                walk(root, &path, entries);
            } else {
                entries.insert(relative, Some(fs::read(&path).unwrap()));
            }
        }
    }
    let mut entries = BTreeMap::new();
    walk(root, root, &mut entries);
    entries
}

fn oversized(omissions: &mut NoteOmissions) {
    for index in 0..16_384 {
        omissions.counts.insert(format!("{index:064}"), 1);
    }
    assert!(serde_json::to_vec(omissions).unwrap().len() > journal::STATE_LIMIT);
}

type Enlarge = fn(&mut ExportCapture);

#[test]
fn task_prepare_refuses_oversized_progress_state_before_changing_progress_or_output() {
    let cases: [(&str, Enlarge); 3] = [
        ("capture reason", |captured: &mut ExportCapture| {
            captured.full_reason = Some("r".repeat(journal::STATE_LIMIT));
        }),
        ("note omissions", |captured: &mut ExportCapture| {
            oversized(&mut captured.omissions);
        }),
        ("activity omissions", |captured: &mut ExportCapture| {
            oversized(&mut captured.activity_omissions);
        }),
    ];
    for (field, enlarge) in cases {
        let seeded = Seeded::new();
        let retained = seeded.current().meta.operation;
        let before = listing(seeded.home.path());
        let mut captured = capture(seeded.tree.clone());
        enlarge(&mut captured);
        let error = seeded.prepare(captured).unwrap_err();
        assert!(refused_bytes(&error) > journal::STATE_LIMIT, "{field}");
        // The refusal says the existing generations and output are unchanged:
        // every entry and byte under the home, including both generations,
        // their state and the candidate output, is as it was, and no staging
        // directory was left behind.
        assert_eq!(
            before,
            listing(seeded.home.path()),
            "{field}: the refusal pruned, staged or wrote an entry"
        );
        let current = seeded.current();
        assert!(current.complete, "{field}");
        assert_eq!(current.meta.operation, retained, "{field}");
    }
}

#[test]
fn task_prepare_state_at_its_limit_stays_readable_and_recoverable_after_opt_out() {
    let seeded = Seeded::new();
    // The next generation differs from the current one only in fields of
    // fixed width and in its reason, so the room left for a reason is known.
    let mut probe = seeded.current().meta;
    probe.sequence += 1;
    probe.path = "full".into();
    probe.reason = String::new();
    probe.serialized_tasks = seeded.tree.tasks.len();
    let room = journal::STATE_LIMIT - serde_json::to_vec(&probe).unwrap().len();

    let before = listing(seeded.home.path());
    let error = seeded.prepare(seeded.with_reason(room + 1)).unwrap_err();
    assert_eq!(error.chain().count(), 1, "{error:#}");
    assert_eq!(error.to_string(), refusal(journal::STATE_LIMIT + 1));
    assert_eq!(before, listing(seeded.home.path()));

    let receipt = seeded.prepare(seeded.with_reason(room)).unwrap();
    assert!(!receipt.recovered);
    let published = seeded.current();
    assert!(published.complete);
    assert_eq!(published.meta.operation, receipt.operation);
    let state = fs::read(published.directory.join("state.json")).unwrap();
    assert_eq!(state.len(), journal::STATE_LIMIT);
    let documents = seeded.tree.documents().unwrap();
    let prepared = read_prepared(
        &seeded.progress,
        &PreparedSelection {
            operation: receipt.operation,
            destination: seeded.output.clone(),
            project: seeded.tree.project.project_id.clone(),
            profile: profile(),
        },
    )
    .unwrap();
    assert_eq!(prepared.provenance.operation, receipt.operation);
    assert_eq!(prepared.documents, documents);

    // Simulate an exit before completion, then recover with task sync off.
    fs::remove_file(published.directory.join("complete")).unwrap();
    assert!(
        fs::canonicalize(&seeded.output)
            .unwrap()
            .starts_with(fs::canonicalize(seeded.home.path()).unwrap())
    );
    fs::remove_dir_all(&seeded.output).unwrap();
    let recovered = resume_pending(
        &seeded.output,
        &seeded.progress,
        &seeded.store,
        &seeded.tree.project.project_id,
        &profile(),
        false,
    )
    .unwrap();
    assert!(recovered.recovered);
    assert_eq!(recovered.operation, receipt.operation);
    let finished = seeded.current();
    assert!(finished.complete);
    assert_eq!(finished.meta.operation, receipt.operation);
    assert_eq!(
        fs::read(finished.directory.join("state.json")).unwrap(),
        state
    );
    assert_eq!(
        directory::read_documents(&seeded.output).unwrap(),
        documents
    );
}
