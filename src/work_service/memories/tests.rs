use super::super::test_support::*;
use super::super::*;
use tempfile::tempdir;

#[test]
fn project_memory_advisory_is_constant_decode_at_scale() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("project-memory-advisory-scale".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("memory-scale-session".into()),
        Some("project-memory-scale-test".into()),
    );
    for index in 0..256 {
        service
            .remember_project_memory(
                format!("retained project observation {index}"),
                Some(format!("memory-{index:03}")),
                at(index),
            )
            .expect("remember scale fixture");
    }
    crate::canonical::reset_canonical_decode_count();
    let next = service
        .work_next_for_agent(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Memories],
                ..WorkNextQuery::default()
            },
            at(300),
        )
        .expect("read O(1) advisory state");
    assert_eq!(next.memories.as_ref().map(|signal| signal.count), Some(256));
    assert_eq!(
        crate::canonical::canonical_decode_count(),
        0,
        "the advisory hot path must not walk canonical memory history"
    );
}
