use super::*;
use std::{cell::Cell, rc::Rc};

fn observed_with_query_count(fixture: &Fixture) -> (DataWorktree, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            git::BEFORE_QUERY.with(|hook| {
                hook.borrow_mut().take();
            });
        }
    }

    let count = Rc::new(Cell::new(0));
    let observed = Rc::clone(&count);
    git::BEFORE_QUERY.with(|hook| {
        assert!(hook.borrow().is_none());
        *hook.borrow_mut() = Some(Box::new(move |_| observed.set(observed.get() + 1)));
    });
    let reset = Reset;
    let result = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data");
    drop(reset);
    (result.unwrap(), count.get())
}

#[test]
fn admission_git_spawns_stay_bounded_with_unborn_and_populated_data() {
    let fixture = Fixture::new();
    let (unborn, unborn_count) = observed_with_query_count(&fixture);
    assert!(unborn.head.is_none());

    fs::create_dir_all(fixture.data.join(".engram/tasks")).unwrap();
    fs::create_dir(fixture.code.join("unrelated")).unwrap();
    for index in 0..100 {
        fs::write(
            fixture
                .data
                .join(format!(".engram/tasks/task-{index}.json")),
            b"{}\n",
        )
        .unwrap();
        fs::write(
            fixture.code.join(format!("unrelated/source-{index}.rs")),
            b"// unrelated code\n",
        )
        .unwrap();
    }
    run(&fixture.data, &["add", ".engram"]);
    run(&fixture.data, &["commit", "-m", "Populate orphan data"]);
    run(&fixture.code, &["add", "unrelated"]);
    run(
        &fixture.code,
        &["commit", "-m", "Advance unrelated code history"],
    );
    let expected_head = oid_line(&run(&fixture.data, &["rev-parse", "HEAD"])).unwrap();
    let before = fixture.code_snapshot();
    let (populated, populated_count) = observed_with_query_count(&fixture);
    assert_eq!(populated.head.as_deref(), Some(expected_head.as_str()));
    assert_eq!(populated.git_directory, unborn.git_directory);
    assert_eq!(fixture.code_snapshot(), before);

    eprintln!("data admission Git starts: unborn={unborn_count}, populated={populated_count}");
    // The bound includes current code/data roots, administration, branch and
    // HEAD checks, orphan ancestry, reserved paths and the complete inventory.
    // It must remain independent of the number of unrelated files/documents.
    // Before batching, these real-Git cases started 18 and 21 queries.
    assert!(unborn_count <= 11 && populated_count <= 11);
}
