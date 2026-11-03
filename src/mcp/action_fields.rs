//! Refuse unused action fields before typed extraction loses their presence.

use rmcp::model::CallToolResult;
use serde_json::{Map, Value};

use super::arguments::{HandoffActionArg, HandoffArgs, UpdateActionArg, UpdateArgs};

pub(super) fn refusal(
    tool: &str,
    arguments: Option<&Map<String, Value>>,
) -> Option<CallToolResult> {
    let arguments = arguments?;
    // Leave unknown fields, actions and wrong types to the existing extractor,
    // which retains its field-path text refusal and the published input schema.
    let (action, allowed): (&str, &[&str]) = match tool {
        "update" => {
            let args: UpdateArgs = serde_json::from_value(Value::Object(arguments.clone())).ok()?;
            match args.action {
                UpdateActionArg::Release => ("release", &["reason"]),
                UpdateActionArg::Cancel => ("cancel", &["reason"]),
                UpdateActionArg::Reject => ("reject", &["reason"]),
                UpdateActionArg::Detach => ("detach", &["reason"]),
                UpdateActionArg::Blocked => ("blocked", &["text"]),
                UpdateActionArg::Unblock => ("unblock", &["blocker"]),
                UpdateActionArg::Revise => (
                    "revise",
                    &[
                        "external",
                        "clear_external",
                        "title",
                        "outcome",
                        "acceptance",
                        "bindings",
                        "assignee",
                        "priority",
                        "defer",
                        "kind",
                        "labels",
                        "unlabels",
                    ],
                ),
                UpdateActionArg::EvaluationMode => ("evaluation_mode", &["evaluation_mode"]),
                UpdateActionArg::After => ("after", &["prerequisite"]),
                UpdateActionArg::DropAfter => ("drop_after", &["prerequisite"]),
                UpdateActionArg::Waive => ("waive", &["child", "reason"]),
                UpdateActionArg::Supersede => ("supersede", &["replacement", "reason"]),
            }
        }
        "handoff" => {
            let args: HandoffArgs =
                serde_json::from_value(Value::Object(arguments.clone())).ok()?;
            match args.action {
                HandoffActionArg::Offer => ("offer", &["to", "summary", "ttl_seconds"]),
                HandoffActionArg::Accept => ("accept", &[]),
                HandoffActionArg::Cancel => ("cancel", &["reason"]),
            }
        }
        _ => return None,
    };
    let field = arguments.keys().find(|field| {
        !matches!(field.as_str(), "action" | "work_ref") && !allowed.contains(&field.as_str())
    })?;
    Some(super::tools::invalid_argument(
        field,
        &format!("{tool} action `{action}` does not use field `{field}`"),
    ))
}

#[cfg(test)]
mod tests {
    use super::{HandoffArgs, UpdateArgs, refusal};
    use serde_json::{Value, json};

    #[test]
    fn supplied_unused_fields_are_refused_even_when_null_false_or_empty() {
        for (tool, action, field, value) in [
            ("update", "release", "title", json!("ignored")),
            ("update", "blocked", "reason", json!("ignored")),
            ("update", "unblock", "text", json!("ignored")),
            ("update", "revise", "reason", json!("ignored")),
            ("update", "evaluation_mode", "labels", json!([])),
            ("update", "cancel", "clear_external", json!(false)),
            ("update", "reject", "title", Value::Null),
            ("update", "after", "child", json!("ignored")),
            ("update", "drop_after", "replacement", json!("ignored")),
            ("update", "waive", "prerequisite", json!("ignored")),
            ("update", "supersede", "child", json!("ignored")),
            ("update", "detach", "priority", json!(0)),
            ("handoff", "offer", "reason", json!("ignored")),
            ("handoff", "accept", "to", Value::Null),
            ("handoff", "accept", "summary", json!("ignored")),
            ("handoff", "accept", "reason", json!("ignored")),
            ("handoff", "accept", "ttl_seconds", json!(0)),
            ("handoff", "cancel", "summary", json!("ignored")),
        ] {
            for action_value in [json!(action), json!({(action): null})] {
                let mut args = json!({"action": action_value, "work_ref": "w-000000000001"});
                args[field] = value.clone();
                let result = refusal(tool, args.as_object()).expect("unused field refused");
                assert_eq!(result.is_error, Some(true));
                let body = result.structured_content.unwrap();
                assert_eq!(body["error"]["code"], "invalid_argument");
                assert_eq!(body["error"]["details"]["field"], field);
                let message = body["error"]["message"].as_str().unwrap();
                assert!(message.contains(action), "{message}");
                assert!(message.contains(field), "{message}");
            }
        }
    }

    #[test]
    fn every_consumed_field_continues_to_the_existing_handlers() {
        for (tool, args) in [
            ("update", json!({"action":"release","reason":null})),
            ("update", json!({"action":"blocked","text":"blocked"})),
            ("update", json!({"action":"unblock","blocker":"one"})),
            (
                "update",
                json!({"action":"revise","external":"source","clear_external":false,
                    "title":"new","outcome":"delivered","acceptance":["one"],
                    "bindings":[],"assignee":"actor","priority":1,"defer":"2026-12-01",
                    "kind":"task","labels":["new"],"unlabels":["old"]}),
            ),
            (
                "update",
                json!({"action":"evaluation_mode","evaluation_mode":null}),
            ),
            ("update", json!({"action":"cancel","reason":"cancel"})),
            ("update", json!({"action":"reject","reason":"reject"})),
            ("update", json!({"action":"after","prerequisite":"one"})),
            (
                "update",
                json!({"action":"drop_after","prerequisite":"one"}),
            ),
            (
                "update",
                json!({"action":"waive","child":"one","reason":"waive"}),
            ),
            (
                "update",
                json!({"action":"supersede","replacement":"one","reason":"replace"}),
            ),
            ("update", json!({"action":"detach","reason":"detach"})),
            (
                "handoff",
                json!({"action":"offer","to":"session-2","summary":null,"ttl_seconds":60}),
            ),
            ("handoff", json!({"action":"accept"})),
            ("handoff", json!({"action":"cancel","reason":"cancel"})),
        ] {
            // A schema refusal also returns None, so validate these fixtures
            // separately to prove the allowlist admits each supplied field.
            match tool {
                "update" => {
                    serde_json::from_value::<UpdateArgs>(args.clone()).unwrap();
                }
                "handoff" => {
                    serde_json::from_value::<HandoffArgs>(args.clone()).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(refusal(tool, args.as_object()).is_none(), "{tool}: {args}");
        }
    }

    #[test]
    fn schema_refusals_continue_to_the_existing_extractor() {
        for (tool, args) in [
            ("update", json!({"action":"unknown","title":"x"})),
            ("update", json!({"action":"cancel","title":false})),
            ("handoff", json!({"action":"accept","undeclared":true})),
        ] {
            assert!(refusal(tool, args.as_object()).is_none(), "{tool}: {args}");
        }
    }
}
