use super::NamedRootState;
use crate::domain::{
    ControlTurnBeginDecision, ObservationRootBasis, SessionId, SessionPhase, TaskId,
    TurnBeginReceipt,
};
use chrono::{DateTime, Utc};

/// One state's wire entries, in the order the derived serializer writes them,
/// each with a different value of the same type.
struct Shape {
    state: NamedRootState,
    entries: Vec<(&'static str, &'static str, &'static str)>,
}

fn named_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-29T01:00:00Z")
        .expect("time")
        .with_timezone(&Utc)
}

fn shapes() -> Vec<Shape> {
    vec![
        Shape {
            state: NamedRootState::NoRoot,
            entries: vec![("state", r#""none""#, r#""bound""#)],
        },
        Shape {
            state: NamedRootState::Bound {
                workspace_id: "workspace-B".into(),
                generation: 9,
                named_at: named_at(),
            },
            entries: vec![
                ("state", r#""bound""#, r#""none""#),
                ("workspace_id", r#""workspace-B""#, r#""workspace-C""#),
                ("generation", "9", "10"),
                (
                    "named_at",
                    r#""2026-09-29T01:00:00Z""#,
                    r#""2026-09-29T02:00:00Z""#,
                ),
            ],
        },
        Shape {
            state: NamedRootState::UnboundByRelease {
                last_generation: 9,
                released_at_position: 42,
            },
            entries: vec![
                ("state", r#""unbound_by_release""#, r#""none""#),
                ("last_generation", "9", "10"),
                ("released_at_position", "42", "43"),
            ],
        },
    ]
}

fn object(entries: &[(&str, &str)]) -> String {
    let body: Vec<String> = entries
        .iter()
        .map(|(key, value)| format!("\"{key}\":{value}"))
        .collect();
    format!("{{{}}}", body.join(","))
}

impl Shape {
    fn text(&self) -> String {
        object(
            &self
                .entries
                .iter()
                .map(|(key, value, _)| (*key, *value))
                .collect::<Vec<_>>(),
        )
    }

    /// The text with `extra` inserted after the entry at `index`.
    fn with(&self, index: usize, extra: (&str, &str)) -> String {
        let mut entries: Vec<(&str, &str)> = self
            .entries
            .iter()
            .map(|(key, value, _)| (*key, *value))
            .collect();
        entries.insert(index + 1, extra);
        object(&entries)
    }
}

fn refused(text: &str) -> String {
    match serde_json::from_str::<NamedRootState>(text) {
        Ok(state) => panic!("{text} must be refused, decoded as {state:?}"),
        Err(error) => error.to_string(),
    }
}

// Each state, the field-less `none` among them, refuses a field no state
// names, wherever it stands.
#[test]
fn each_state_refuses_a_field_no_state_names() {
    for shape in shapes() {
        for index in [0, shape.entries.len() - 1] {
            let error = refused(&shape.with(index, ("root_hint", r#""x""#)));
            assert!(error.contains("unknown field `root_hint`"), "{error}");
        }
    }
}

// A field another state names is refused too, never dropped.
#[test]
fn each_state_refuses_the_fields_of_another_state() {
    let [none, bound, unbound] = <[Shape; 3]>::try_from(shapes()).ok().expect("three states");
    for (shape, field, value) in [
        (&none, "workspace_id", r#""workspace-B""#),
        (&none, "generation", "9"),
        (&none, "named_at", r#""2026-09-29T01:00:00Z""#),
        (&none, "last_generation", "9"),
        (&none, "released_at_position", "42"),
        (&bound, "last_generation", "9"),
        (&bound, "released_at_position", "42"),
        (&unbound, "workspace_id", r#""workspace-B""#),
        (&unbound, "generation", "9"),
        (&unbound, "named_at", r#""2026-09-29T01:00:00Z""#),
    ] {
        let error = refused(&shape.with(0, (field, value)));
        assert!(
            error.contains(&format!("unknown field `{field}`")),
            "{error}"
        );
    }
}

// A repeated key is refused before either value is used, whether the two
// values agree or not, the tag included.
#[test]
fn a_repeated_key_is_refused_whether_or_not_its_values_agree() {
    for shape in shapes() {
        for (index, (key, value, other)) in shape.entries.iter().enumerate() {
            for repeated in [value, other] {
                let error = refused(&shape.with(index, (key, repeated)));
                assert!(
                    error.contains(&format!("duplicate field `{key}`")),
                    "{error}"
                );
            }
        }
    }
}

#[test]
fn a_missing_tag_or_field_is_refused_by_name() {
    for shape in shapes() {
        for skipped in 0..shape.entries.len() {
            let entries: Vec<(&str, &str)> = shape
                .entries
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != skipped)
                .map(|(_, (key, value, _))| (*key, *value))
                .collect();
            let error = refused(&object(&entries));
            let key = shape.entries[skipped].0;
            assert!(error.contains(&format!("missing field `{key}`")), "{error}");
        }
    }
}

#[test]
fn malformed_states_are_refused() {
    for (text, expected) in [
        (
            r#"{"state":"bound_later"}"#,
            "unknown variant `bound_later`",
        ),
        (r#"{"state":1}"#, "invalid type"),
        (r#"{"state":null}"#, "invalid type"),
        (r#"["none"]"#, "invalid type"),
        (r#""none""#, "invalid type"),
        ("null", "invalid type"),
        ("{}", "missing field `state`"),
        (
            r#"{"state":"bound","workspace_id":"w","generation":"9","named_at":"2026-09-29T01:00:00Z"}"#,
            "invalid type",
        ),
        (
            r#"{"state":"bound","workspace_id":"w","generation":9,"named_at":"yesterday"}"#,
            "input contains invalid characters",
        ),
        (
            r#"{"state":"unbound_by_release","last_generation":9.5,"released_at_position":42}"#,
            "invalid type",
        ),
    ] {
        let error = refused(text);
        assert!(error.contains(expected), "{text}: {error}");
    }
}

// The tag may stand anywhere; the written shape and its canonical bytes read
// back as the same state.
#[test]
fn the_wire_and_canonical_shapes_read_back_unchanged() {
    for shape in shapes() {
        let mut reversed = shape
            .entries
            .iter()
            .rev()
            .map(|(key, value, _)| (*key, *value));
        let reversed = object(&reversed.by_ref().collect::<Vec<_>>());
        for text in [
            shape.text(),
            reversed,
            serde_json::to_string(&shape.state).expect("wire"),
            String::from_utf8(crate::canonical::canonical_bytes(&shape.state).expect("canonical"))
                .expect("utf-8"),
        ] {
            assert_eq!(
                serde_json::from_str::<NamedRootState>(&text).expect("valid state"),
                shape.state,
                "{text}"
            );
        }
        assert_eq!(
            serde_json::to_string(&shape.state).expect("wire"),
            shape.text()
        );
    }
}

/// Every way `shape` can carry what its state does not name.
fn tampered(shape: &Shape) -> Vec<(String, &'static str)> {
    let foreign = match shape.state {
        NamedRootState::Bound { .. } => "released_at_position",
        _ => "generation",
    };
    let (tag, value, _) = shape.entries[0];
    vec![
        (shape.with(0, ("root_hint", r#""x""#)), "unknown field"),
        (shape.with(0, (foreign, "1")), "unknown field"),
        (shape.with(0, (tag, value)), "duplicate field"),
    ]
}

// The request a host sends and the observation stored from it decode the root
// state through the same strict decoder.
#[test]
fn an_observation_root_basis_refuses_what_its_state_does_not_name() {
    for shape in shapes() {
        let basis =
            |state: &str| format!(r#"{{"capture_run_cut":3,"latest_event":null,"state":{state}}}"#);
        let decoded: ObservationRootBasis =
            serde_json::from_str(&basis(&shape.text())).expect("valid basis");
        assert_eq!(decoded.state, shape.state);
        for (text, expected) in tampered(&shape) {
            let error = serde_json::from_str::<ObservationRootBasis>(&basis(&text))
                .expect_err("tampered basis");
            assert!(error.to_string().contains(expected), "{text}: {error}");
        }
    }
}

// A begin receipt sits inside an internally tagged decision, which buffers
// its content before decoding it; a stored receipt replays through it.
#[test]
fn a_begin_decision_refuses_what_its_receipt_state_does_not_name() {
    let receipt = TurnBeginReceipt {
        grant_id: "grant-1".into(),
        session_id: SessionId("session-1".into()),
        task_id: TaskId(uuid::Uuid::nil()),
        phase: SessionPhase::TurnOpen,
        tentative_cursor: None,
        session_revision: 3,
        begun_at: named_at(),
        named_root: None,
    };
    let without = serde_json::to_string(&ControlTurnBeginDecision::Begin {
        receipt: receipt.clone(),
    })
    .expect("decision");
    let carrying = |state: &str| {
        let open = without.strip_suffix("}}").expect("nested object");
        format!(r#"{open},"named_root":{state}}}}}"#)
    };
    for shape in shapes() {
        let decoded: ControlTurnBeginDecision =
            serde_json::from_str(&carrying(&shape.text())).expect("valid decision");
        assert_eq!(
            decoded,
            ControlTurnBeginDecision::Begin {
                receipt: TurnBeginReceipt {
                    named_root: Some(shape.state.clone()),
                    ..receipt.clone()
                }
            }
        );
        for (text, expected) in tampered(&shape) {
            let error = serde_json::from_str::<ControlTurnBeginDecision>(&carrying(&text))
                .expect_err("tampered decision");
            assert!(error.to_string().contains(expected), "{text}: {error}");
        }
    }
}
