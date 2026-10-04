//! The tool-argument extractor of `engram mcp`: rmcp's `Parameters<P>`
//! contract, with a refusal that names the field it concerns.
//!
//! rmcp deserializes a tool's arguments before the tool runs and, on failure,
//! answers with an `isError` text result carrying serde's message, which names
//! an undeclared or missing field but, for a wrong type or an unknown value,
//! only the value or the expected type. This extractor
//! deserializes the same argument object through `serde_path_to_error`, so a
//! wrong type or an unknown action names its field (`peek`, `action`,
//! `verdicts[0].criterion`). Its input schema is `P`'s, exactly as rmcp's
//! wrapper gives it, and rmcp's `#[tool]` macro finds it by its name, so the
//! tools a client lists are unchanged.

use rmcp::{
    ErrorData,
    handler::server::{common::FromContextPart, tool::ToolCallContext},
    schemars::{JsonSchema, Schema, SchemaGenerator},
};
use serde::de::DeserializeOwned;

/// The prefix rmcp's router recognises: an invalid-params error that starts
/// with it becomes an `isError` tool result before any word runs.
const DESERIALIZATION_ERROR_PREFIX: &str = "failed to deserialize parameters:";

/// One tool's deserialized arguments.
///
/// The name is load-bearing: rmcp's `#[tool]` macro derives a tool's input
/// schema from the handler argument whose type's last path segment is
/// `Parameters` (rmcp-macros' `find_parameters_type_in_sig`). A future rmcp
/// that matched only its own wrapper would still call this extractor but list
/// an empty input schema. Two tests watch the listed schemas and then fail:
/// `every_listed_input_schema_is_the_one_rmcps_wrapper_gives`, and the stdio
/// dogfood refusal test, which reads `peek` and `action` from tools/list.
#[derive(Debug, Clone)]
pub(crate) struct Parameters<P>(pub P);

impl<P: JsonSchema> JsonSchema for Parameters<P> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        P::schema_name()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        P::json_schema(generator)
    }
}

impl<S, P> FromContextPart<ToolCallContext<'_, S>> for Parameters<P>
where
    P: DeserializeOwned,
{
    fn from_context_part(context: &mut ToolCallContext<S>) -> Result<Self, ErrorData> {
        let arguments = context.arguments.take().unwrap_or_default();
        parse(serde_json::Value::Object(arguments))
            .map(Parameters)
            .map_err(|message| ErrorData::invalid_params(message, None))
    }
}

/// The arguments as `P`, or the refusal text: the bad field's path, when it
/// has one, before serde's own message.
fn parse<P: DeserializeOwned>(arguments: serde_json::Value) -> Result<P, String> {
    serde_path_to_error::deserialize(arguments).map_err(|error| {
        let path = error.path();
        if path
            .iter()
            .all(|segment| matches!(segment, serde_path_to_error::Segment::Unknown))
        {
            format!("{DESERIALIZATION_ERROR_PREFIX} {}", error.inner())
        } else {
            format!(
                "{DESERIALIZATION_ERROR_PREFIX} field `{path}`: {}",
                error.inner()
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use super::parse;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code, reason = "the fields exist to be deserialized")]
    struct Args {
        peek: Option<bool>,
        action: Option<Action>,
        verdicts: Option<Vec<Verdict>>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Action {
        Release,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code, reason = "the field exists to be deserialized")]
    struct Verdict {
        criterion: u32,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code, reason = "the field exists to be deserialized")]
    struct Required {
        work_ref: String,
    }

    /// Every tool `engram mcp` lists keeps the input schema rmcp's own
    /// `Parameters` wrapper gives the same arguments type, so swapping in this
    /// extractor leaves tools/list byte for byte as it was. The reference is
    /// rmcp's derivation at run time, not a pinned copy.
    #[test]
    fn every_listed_input_schema_is_the_one_rmcps_wrapper_gives() {
        use rmcp::handler::server::{common::schema_for_input, wrapper::Parameters as Rmcp};
        let reference = |name: &str| {
            let schema = match name {
                "next" => schema_for_input::<Rmcp<super::super::NextArgs>>(),
                "ls" => schema_for_input::<Rmcp<super::super::LsArgs>>(),
                "search" => schema_for_input::<Rmcp<super::super::WorkSearchArgs>>(),
                "show" => schema_for_input::<Rmcp<super::super::ShowArgs>>(),
                "add" => schema_for_input::<Rmcp<super::super::AddArgs>>(),
                "claim" => schema_for_input::<Rmcp<super::super::WorkClaimArgs>>(),
                "update" => schema_for_input::<Rmcp<super::super::UpdateArgs>>(),
                "gate" => schema_for_input::<Rmcp<super::super::GateArgs>>(),
                "evaluate" => schema_for_input::<Rmcp<super::super::EvaluateArgs>>(),
                "note" => schema_for_input::<Rmcp<super::super::NoteArgs>>(),
                "done" => schema_for_input::<Rmcp<super::super::DoneArgs>>(),
                "handoff" => schema_for_input::<Rmcp<super::super::HandoffArgs>>(),
                "remember" => schema_for_input::<Rmcp<super::super::RememberArgs>>(),
                "memories" => schema_for_input::<Rmcp<super::super::MemoriesArgs>>(),
                "forget" => schema_for_input::<Rmcp<super::super::ForgetArgs>>(),
                other => panic!("a listed tool without a reference schema: {other}"),
            };
            schema.expect("reference schema")
        };
        let tools = super::super::McpServer::agent_tool_router().list_all();
        assert_eq!(tools.len(), 15);
        for tool in tools {
            let listed = serde_json::to_string(&tool.input_schema).unwrap();
            let expected = serde_json::to_string(&reference(&tool.name)).unwrap();
            assert_eq!(listed, expected, "{}", tool.name);
        }
    }

    fn refusal(arguments: serde_json::Value) -> String {
        parse::<Args>(arguments).expect_err("refused")
    }

    /// A wrong type, an unknown action and an undeclared field name their
    /// field, a nested one by its whole path; a missing field, which has no
    /// path, keeps serde's message, which names it already. Each keeps the
    /// prefix rmcp's router turns into an `isError` result.
    #[test]
    fn refusals_name_the_field_they_concern() {
        let wrong_type = refusal(json!({"peek": "yes"}));
        assert_eq!(
            wrong_type,
            "failed to deserialize parameters: field `peek`: invalid type: string \"yes\", expected a boolean"
        );
        let unknown_action = refusal(json!({"action": "frobnicate"}));
        assert!(
            unknown_action.starts_with(
                "failed to deserialize parameters: field `action`: unknown variant `frobnicate`"
            ),
            "{unknown_action}"
        );
        let nested = refusal(json!({"verdicts": [{"criterion": "one"}]}));
        assert!(
            nested.starts_with(
                "failed to deserialize parameters: field `verdicts[0].criterion`: invalid type"
            ),
            "{nested}"
        );
        let undeclared = refusal(json!({"accept": ["criterion"]}));
        assert!(
            undeclared.starts_with(
                "failed to deserialize parameters: field `accept`: unknown field `accept`, expected one of"
            ),
            "{undeclared}"
        );
        // A missing required field has no path; serde's message names it.
        assert_eq!(
            parse::<Required>(json!({})).expect_err("missing"),
            "failed to deserialize parameters: missing field `work_ref`"
        );
        assert!(parse::<Args>(json!({"peek": true, "action": "release"})).is_ok());
    }
}
