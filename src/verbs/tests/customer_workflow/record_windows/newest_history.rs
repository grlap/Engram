use super::*;

// Native history is held oldest first. Under byte pressure, terse show and the
// focus section of next shed the oldest native rows first, keeping the newest
// events, and count every row they shed.
#[test]
fn byte_pressure_sheds_the_oldest_native_history_first() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Newest history kept", None, false, 0);
    for index in 0..4 {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work.clone()),
                    action: UpdateAction::Revise {
                        bindings: None,
                        clear_external: false,
                        external: None,
                        title: Some(format!("Newest history kept {index}")),
                        outcome: None,
                        acceptance: None,
                        assignee: None,
                        priority: None,
                        defer: None,
                        kind: None,
                        labels: Vec::new(),
                        unlabels: Vec::new(),
                    },
                },
                at(1 + index),
            )
            .unwrap();
    }
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(600),
                recover: None,
            },
            at(10),
        )
        .unwrap();
    let positions = |history: &crate::work_service::WorkHistoryView| {
        history
            .items
            .iter()
            .map(|change| change.entry.position.position)
            .collect::<Vec<_>>()
    };

    // Terse show.
    let mut view = verbs.service.work_focus_for_agent(&work, at(11)).unwrap();
    let before = positions(&view.history);
    assert!(before.len() >= 4, "{before:?}");
    assert!(
        before.windows(2).all(|pair| pair[0] < pair[1]),
        "{before:?}"
    );
    let omitted = view.history.omitted;
    let mut shed = 0;
    while !view.history.items.is_empty() {
        assert!(crate::verbs::show::shed_show_context_once(&mut view));
        shed += 1;
        assert_eq!(positions(&view.history), before[shed..], "show shed {shed}");
        assert_eq!(view.history.omitted, omitted + shed);
    }

    // The focus section of next.
    let mut next = verbs
        .service
        .work_next_for_agent(20, 20, false, WorkNextQuery::default(), at(12))
        .unwrap();
    let focus = next.focus.as_ref().expect("the claimed item is the focus");
    let before = positions(&focus.history);
    assert!(before.len() >= 4, "{before:?}");
    let omitted = focus.history.omitted;
    for shed in 1..before.len() {
        assert!(crate::work_service::shed_work_next_focus(&mut next));
        let focus = next.focus.as_ref().unwrap();
        assert_eq!(
            positions(&focus.history),
            before[shed..],
            "next shed {shed}"
        );
        assert_eq!(focus.history.omitted, omitted + shed);
    }
}
