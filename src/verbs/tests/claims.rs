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
