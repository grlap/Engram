use super::*;

fn session(directory: &std::path::Path, session: &str) -> AgentVerbs {
    AgentVerbs::new(
        directory.join("claims.db"),
        ProjectId("next-ready".into()),
        "agent".into(),
        SessionId(session.into()),
        None,
    )
}

fn add(verbs: &AgentVerbs, title: &str, under: Option<&str>, priority: i32, second: i64) -> String {
    verbs
        .add(
            AddInput {
                title: title.into(),
                under: under.map(str::to_owned),
                priority: Some(priority),
                acceptance: vec![format!("{title} accepted")],
                ..AddInput::default()
            },
            at(second),
        )
        .expect("add")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned()
}

fn claim_ref(verbs: &AgentVerbs, work: &str, second: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: None,
                recover: None,
            },
            at(second),
        )
        .expect("claim");
}

fn release(
    verbs: &AgentVerbs,
    work: &str,
    reason: Option<&str>,
    second: i64,
) -> Result<Receipt, VerbError> {
    verbs.update(
        UpdateInput {
            work_ref: Some(work.into()),
            action: UpdateAction::Release {
                reason: reason.map(str::to_owned),
            },
        },
        at(second),
    )
}

#[test]
fn a_holder_that_did_no_work_releases_with_a_reason_recorded_as_its_waiver() {
    let directory = crate::test_support::temp_home().expect("temp");
    let one = session(directory.path(), "one");
    let work = add(&one, "Redirected", None, 1, 0);
    claim_ref(&one, &work, 1);

    // No reason, or only spaces, is refused with the input that is missing
    // and the command that supplies it.
    for reason in [None, Some("   ")] {
        let error = release(&one, &work, reason, 2).expect_err("a release without a reason");
        assert!(
            matches!(error.error, StoreError::WorkReleaseWaiverRequired { .. }),
            "{:?}",
            error.error
        );
        let guidance = one.error_guidance(&error);
        assert!(
            guidance.reminders[0].contains("needs a reason")
                && guidance.reminders[0].contains("attributed waiver"),
            "{:?}",
            guidance.reminders
        );
        assert_eq!(
            guidance.next,
            vec![format!(
                "engram work update {work} --release --reason \"…\""
            )]
        );
    }

    // The claim survived both refusals: the same holder can still release it.
    let released = release(&one, &work, Some("  redirected before any work  "), 3)
        .expect("release with a reason");
    assert!(
        released.text().starts_with(&format!(
            "released {work} \"Redirected\"; your reason is recorded as the waiver of this session's missing contribution"
        )),
        "{}",
        released.text()
    );

    // A keyless retry after success no longer holds a claim; it is refused
    // for that, not for a missing waiver.
    let retry = release(&one, &work, Some("redirected before any work"), 4)
        .expect_err("a retry after success");
    assert!(
        matches!(retry.error, StoreError::WorkClaimMismatch { .. }),
        "{:?}",
        retry.error
    );
}

#[test]
fn a_holder_that_contributed_releases_without_recording_a_waiver() {
    let directory = crate::test_support::temp_home().expect("temp");
    let one = session(directory.path(), "one");
    let work = add(&one, "Worked on", None, 1, 0);
    claim_ref(&one, &work, 1);
    one.note(
        &NoteInput {
            status: false,
            work_ref: Some(work.clone()),
            text: "recorded some progress".into(),
            refs: Vec::new(),
        },
        at(2),
    )
    .expect("contribution");
    let released = release(&one, &work, Some("pausing"), 3).expect("release");
    assert!(
        released
            .text()
            .starts_with(&format!("released {work} \"Worked on\"")),
        "{}",
        released.text()
    );
    assert!(!released.text().contains("waiver"), "{}", released.text());
}

fn under(parent: &str, ttl_seconds: Option<i64>) -> ClaimUnderInput {
    ClaimUnderInput {
        under: parent.to_owned(),
        ttl_seconds,
        recover: None,
    }
}

#[test]
fn claim_under_holds_the_next_ready_child_and_names_its_place() {
    let directory = crate::test_support::temp_home().expect("temp");
    let one = session(directory.path(), "one");
    let root = add(&one, "Root", None, 1, 0);
    let first = add(&one, "First", Some(&root), 1, 1);
    let second = add(&one, "Second", Some(&root), 2, 2);
    let third = add(&one, "Third", Some(&root), 3, 3);

    let claimed = one
        .claim_under(under(&root, None), at(4))
        .expect("claim under");
    assert!(
        claimed.text().starts_with(&format!(
            "claimed {first} \"First\", ready child 1 of 3 under {root} (held by you until"
        )),
        "{}",
        claimed.text()
    );
    assert_eq!(claimed.value["work"]["short_ref"], first);
    assert_eq!(claimed.value["claim"]["holder"], "you");
    assert_eq!(
        claimed.value["under"],
        serde_json::json!({"parent_ref": root, "position": 1, "ready_count": 3, "renewed": false})
    );

    // A second session is handed the next child, never the held one.
    let two = session(directory.path(), "two");
    let handed = two
        .claim_under(under(&root, None), at(5))
        .expect("second session");
    assert_eq!(handed.value["work"]["short_ref"], second);
    assert!(
        handed
            .text()
            .contains(&format!("ready child 1 of 2 under {root}")),
        "{}",
        handed.text()
    );

    // The first session's repeat renews what it already holds.
    let renewed = one
        .claim_under(under(&root, Some(7200)), at(6))
        .expect("renewal");
    assert_eq!(renewed.value["work"]["short_ref"], first);
    assert!(
        renewed.text().starts_with(&format!(
            "renewed {first} \"First\", already held under {root} (held by you until"
        )),
        "{}",
        renewed.text()
    );
    assert_eq!(renewed.value["under"]["renewed"], true);
    assert_eq!(renewed.value["under"]["ready_count"], 1);

    // With nothing ready the call refuses at the parent and holds nothing.
    let three = session(directory.path(), "three");
    let last = three
        .claim_under(under(&root, None), at(7))
        .expect("third child");
    assert_eq!(last.value["work"]["short_ref"], third);
    let four = session(directory.path(), "four");
    let refused = four
        .claim_under(under(&root, None), at(8))
        .expect_err("nothing ready");
    assert_eq!(refused.work_ref.as_deref(), Some(root.as_str()));
    assert!(
        refused
            .to_string()
            .contains("no ready child to claim: 3 claimed"),
        "{refused}"
    );
    let shown = four.show(&third, at(9)).expect("show");
    assert!(!shown.text().contains("held by you"), "{}", shown.text());
}
