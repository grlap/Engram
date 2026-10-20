use super::*;
use crate::task_sync::publication::with_unsupported_native_move_after;

struct Fixture {
    home: crate::test_support::TempHome,
    store: PathBuf,
    progress: PathBuf,
    output: PathBuf,
    tree: TaskTree,
}

impl Fixture {
    fn new() -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let store = home.path().join("store.db");
        fs::write(&store, b"route").unwrap();
        Self {
            store,
            progress: home.path().join("progress"),
            output: home.path().join("candidate"),
            tree: tree(2),
            home,
        }
    }

    fn prepare(
        &self,
        capture: impl FnOnce(Option<&ExportCut>) -> Result<ExportCapture>,
    ) -> Result<PrepareReceipt> {
        prepare_with_producer(
            &self.output,
            &self.progress,
            &self.store,
            &self.tree.project.project_id,
            &profile(),
            false,
            &producer(),
            capture,
        )
    }

    fn assert_unsupported(&self, error: &anyhow::Error) {
        let refusal = error.downcast_ref::<crate::storage::StoreError>().unwrap();
        assert_eq!(
            crate::store_error_value(refusal)["error"]["code"],
            "task_sync_publication_unsupported"
        );
        assert!(!self.output.exists());
        assert_eq!(fs::read_dir(self.home.path()).unwrap().count(), 2);
    }
}

#[test]
fn task_prepare_unsupported_journal_moves_remove_the_unpublished_stage() {
    // First the nested candidate move, then the assembled generation move.
    for successful_moves in [0, 1] {
        let fixture = Fixture::new();
        let error = with_unsupported_native_move_after(successful_moves, || {
            fixture.prepare(|_| Ok(capture(fixture.tree.clone())))
        })
        .unwrap_err();
        fixture.assert_unsupported(&error);
        // Inspect before load(), whose crash recovery also removes stages.
        assert_eq!(
            fs::read_dir(&fixture.progress)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("prepare.lock")]
        );
        assert!(journal::load(&fixture.progress).unwrap().current.is_none());
        let receipt = fixture
            .prepare(|_| Ok(capture(fixture.tree.clone())))
            .unwrap();
        assert!(!receipt.recovered);
        assert_eq!(
            directory::read_documents(&fixture.output).unwrap(),
            fixture.tree.documents().unwrap()
        );
    }
}

#[test]
fn task_prepare_unsupported_output_move_retains_pending_bytes_and_cut() {
    let fixture = Fixture::new();
    let error = with_unsupported_native_move_after(2, || {
        fixture.prepare(|_| Ok(capture(fixture.tree.clone())))
    })
    .unwrap_err();
    fixture.assert_unsupported(&error);
    // No abandoned outer or candidate stage: only lock and admitted generation.
    let before = listing(&fixture.progress);
    assert!(before.keys().all(|path| {
        path.components().all(|part| {
            let name = part.as_os_str().to_string_lossy();
            !name.starts_with(".staging-") && !name.starts_with(".engram-task-candidate-")
        })
    }));
    assert_eq!(fs::read_dir(&fixture.progress).unwrap().count(), 2);
    let pending = journal::load(&fixture.progress).unwrap().current.unwrap();
    assert!(!pending.complete);
    assert_eq!(pending.documents, fixture.tree.documents().unwrap());
    assert_eq!(pending.meta.cut, capture(fixture.tree.clone()).cut);
    let receipt = fixture
        .prepare(|_| panic!("pending retry must not capture a newer store cut"))
        .unwrap();
    assert!(receipt.recovered);
    assert_eq!(receipt.operation, pending.meta.operation);
    assert_eq!(receipt.project_position, pending.meta.cut.position);
    assert_eq!(
        directory::read_documents(&fixture.output).unwrap(),
        pending.documents
    );
    assert!(
        journal::load(&fixture.progress)
            .unwrap()
            .current
            .unwrap()
            .complete
    );
    let after = listing(&fixture.progress);
    for (path, bytes) in before {
        assert_eq!(after.get(&path), Some(&bytes));
    }
}
