use super::*;
use crate::{
    ObjectId, WorkBlockerKind,
    task_sync::{
        SharedActor, SharedBlocker,
        activity::{ActivityNotice, ActivityState},
        notes::{SharedNote, SharedNoteKind},
    },
};

fn actor() -> SharedActor {
    SharedActor {
        actor_id: "planner".into(),
        actor_kind: "test_agent".into(),
    }
}

pub(super) fn note() -> SharedNote {
    SharedNote {
        id: ObjectId::mint(),
        kind: SharedNoteKind::Generic,
        body: "Planning is ready".into(),
        author: actor(),
        recorded_at: chrono::Utc::now(),
        origin_machine: "fixture".into(),
    }
}

fn activity(task: &mut TaskDocument) {
    let id = ObjectId::mint();
    let notice = ActivityNotice {
        activity_id: id.clone(),
        task_id: task.id,
        origin_machine: "fixture".into(),
        actor: actor(),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: id.clone(),
    };
    task.activity = BTreeMap::from([("fixture".into(), BTreeMap::from([(id, notice)]))]);
}

fn refused(fixture: &Fixture, prior: &str, document: &TaskDocument, reason: &str) {
    fixture.write(document);
    let incoming = fixture.commit();
    let error = fixture.read(prior, &incoming).unwrap_err();
    assert_eq!(code(&error), "task_sync_invalid_graph");
    assert!(error.to_string().contains(reason), "{error:#}");
}

#[test]
fn document_fields_and_blocker_shapes_are_checked_at_commit_intake() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let original = &fixture.tree.tasks[0];
    let mut notes_off = original.clone();
    notes_off.notes.push(note());
    refused(&fixture, &prior, &notes_off, "notes_included");
    let mut unsorted = original.clone();
    unsorted.prerequisites = vec![WorkId::new(), WorkId::new()];
    unsorted
        .prerequisites
        .sort_by_key(|id| std::cmp::Reverse(*id));
    refused(&fixture, &prior, &unsorted, "prerequisites must be sorted");
    let mut completion = original.clone();
    completion.lifecycle = WorkLifecycle::Completed;
    refused(
        &fixture,
        &prior,
        &completion,
        "completion and planning lifecycle disagree",
    );
    let mut blocker = original.clone();
    blocker.blockers.push(SharedBlocker {
        blocker_id: uuid::Uuid::new_v4().to_string(),
        kind: WorkBlockerKind::Manual,
        detail: " unnormalized ".into(),
        created_by: actor(),
        created_at: chrono::Utc::now(),
    });
    refused(&fixture, &prior, &blocker, "blocker detail");
}

#[test]
fn notes_and_activity_disclosure_are_checked_under_a_stable_valid_profile() {
    let fixture = Fixture::new(1);
    fixture.profile(true, Some("fixture"));
    let prior = fixture.commit();
    let mut eligible = fixture.tree.tasks[0].clone();
    eligible.notes.push(note());
    activity(&mut eligible);
    fixture.write(&eligible);
    let incoming = fixture.commit();
    assert_eq!(
        fixture.read(&prior, &incoming).unwrap().upserts,
        BTreeMap::from([(eligible.id, eligible.clone())])
    );
    let mut restricted = eligible.clone();
    restricted.labels = vec!["restricted".into()];
    refused(&fixture, &prior, &restricted, "notes on restricted");
    let mut prose = eligible.clone();
    prose.notes[0].body = "See ./private/evidence".into();
    refused(&fixture, &prior, &prose, "shared note refused");
    let mut structure = eligible.clone();
    structure
        .activity
        .values_mut()
        .next()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .task_id = WorkId::new();
    refused(
        &fixture,
        &prior,
        &structure,
        "activity map key or task binding",
    );
    let mut state = eligible.clone();
    state
        .activity
        .values_mut()
        .next()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
        .state_revision = 2;
    refused(
        &fixture,
        &prior,
        &state,
        "activity state, revision and event",
    );
    let mut excluded_activity = restricted;
    excluded_activity.notes.clear();
    refused(
        &fixture,
        &prior,
        &excluded_activity,
        "shared activity refused: restricted-task",
    );
}
