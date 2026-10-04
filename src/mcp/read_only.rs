//! The MCP read-only mode: `engram mcp --read-only` admits only the read
//! words, in their reading forms, so a host can hand it to a child it must
//! not let write. The server enforces this itself; an annotation would only
//! advise.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::{LsArgs, MemoriesArgs, NextArgs, ShowArgs, WorkSearchArgs};

/// The tools the read-only mode lists and admits.
pub(super) const READ_TOOLS: [&str; 5] = ["next", "ls", "search", "show", "memories"];

/// What `initialize` tells a read-only connection.
pub(super) const INSTRUCTIONS: &str = "Read-only mode: five read words. next only with peek: true; ls; search; show; memories without context_generation. Every other tool, and every writing form of a read word, is refused as a tool error with code mcp_read_only_refused. Answers may still suggest commands that write; here those are refused.";

/// The stable code of every read-only refusal.
pub(super) const READ_ONLY_REFUSED: &str = "mcp_read_only_refused";

/// Why the read-only mode refused a call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Restriction {
    /// The tool is not one of the read words.
    ToolNotAdmitted,
    /// The arguments are not the read word's declared arguments.
    ArgumentNotAdmitted,
    /// `next` was called without `peek: true`, which would stage delivery.
    NextWithoutPeek,
    /// `memories` was given `context_generation`, which records a listing.
    MemoriesWithContextGeneration,
}

impl Restriction {
    fn word(self) -> &'static str {
        match self {
            Self::ToolNotAdmitted => "tool_not_admitted",
            Self::ArgumentNotAdmitted => "argument_not_admitted",
            Self::NextWithoutPeek => "next_without_peek",
            Self::MemoriesWithContextGeneration => "memories_with_context_generation",
        }
    }

    /// The read the caller can make instead, as every Engram error's `next`
    /// gives one; arguments a read word does not declare have no fixed one.
    fn next(self) -> &'static [&'static str] {
        match self {
            Self::ToolNotAdmitted | Self::NextWithoutPeek => &["engram work next --peek"],
            Self::MemoriesWithContextGeneration => &["engram work memories"],
            Self::ArgumentNotAdmitted => &[],
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::ToolNotAdmitted => {
                "this tool is not a read word; the read-only mode admits next, ls, search, show and memories"
            }
            Self::ArgumentNotAdmitted => {
                "these arguments are not valid declared arguments of the read word"
            }
            Self::NextWithoutPeek => {
                "next is admitted only with peek: true, which reads without staging delivery"
            }
            Self::MemoriesWithContextGeneration => {
                "memories is admitted only without context_generation, which records a listing"
            }
        }
    }
}

/// Whether the read-only mode admits this call, decided on the raw call
/// before any tool runs: `Err` carries the refusal.
pub(super) fn admit(name: &str, arguments: Option<&Map<String, Value>>) -> Result<(), Restriction> {
    let empty = Map::new();
    let arguments = arguments.unwrap_or(&empty);
    let declared = match name {
        "next" => declares::<NextArgs>(arguments),
        "ls" => declares::<LsArgs>(arguments),
        "search" => declares::<WorkSearchArgs>(arguments),
        "show" => declares::<ShowArgs>(arguments),
        "memories" => declares::<MemoriesArgs>(arguments),
        _ => return Err(Restriction::ToolNotAdmitted),
    };
    if !declared {
        return Err(Restriction::ArgumentNotAdmitted);
    }
    if name == "next" && arguments.get("peek") != Some(&Value::Bool(true)) {
        return Err(Restriction::NextWithoutPeek);
    }
    // Present at all, even as null, is refused: only an omitted generation
    // reads without recording.
    if name == "memories" && arguments.contains_key("context_generation") {
        return Err(Restriction::MemoriesWithContextGeneration);
    }
    Ok(())
}

/// Whether the arguments are exactly the tool's own: its argument type
/// refuses an undeclared field, as the tool itself would.
fn declares<T: DeserializeOwned>(arguments: &Map<String, Value>) -> bool {
    serde_json::from_value::<T>(Value::Object(arguments.clone())).is_ok()
}

/// The refusal a host reads: an MCP tool error whose JSON names the mode and,
/// like every other tool error Engram itself returns, carries `reminders` and
/// `next`.
pub(super) fn refusal(name: &str, restriction: Restriction) -> Value {
    json!({
        "error": {
            "code": READ_ONLY_REFUSED,
            "message": format!("MCP read-only mode refused {name}: {}", restriction.reason()),
            "details": {
                "mode": "read_only",
                "tool": name,
                "restriction": restriction.word(),
            },
            "reminders": [format!("this connection is read-only: {}", restriction.reason())],
            "next": restriction.next(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::super::McpServer;
    use super::super::parameters::Parameters;
    use super::{READ_ONLY_REFUSED, READ_TOOLS, Restriction, admit, refusal};
    use crate::{ProjectId, SessionId};
    use rmcp::model::CallToolResult;
    use serde_json::{Map, Value, json};

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    fn admitted(name: &str, arguments: Value) -> Result<(), Restriction> {
        admit(name, Some(&object(arguments)))
    }

    /// The ten words that write, and a name no tool has.
    const NOT_READ: [&str; 11] = [
        "add",
        "claim",
        "update",
        "gate",
        "evaluate",
        "remember",
        "forget",
        "note",
        "done",
        "handoff",
        "drop_everything",
    ];

    #[test]
    fn next_is_admitted_only_with_peek_exactly_true() {
        assert_eq!(admitted("next", json!({ "peek": true })), Ok(()));
        assert_eq!(
            admitted("next", json!({ "peek": true, "limit": 3, "verbose": true })),
            Ok(())
        );
        assert_eq!(
            admitted("next", json!({ "peek": true, "context_generation": "g" })),
            Ok(()),
            "a peek with a generation records nothing"
        );
        assert_eq!(admit("next", None), Err(Restriction::NextWithoutPeek));
        for refused in [
            json!({}),
            json!({ "peek": null }),
            json!({ "peek": false }),
            json!({ "limit": 3 }),
        ] {
            assert_eq!(
                admitted("next", refused.clone()),
                Err(Restriction::NextWithoutPeek),
                "{refused}"
            );
        }
        // A peek of another type is not a declared argument at all.
        for refused in [
            json!({ "peek": "true" }),
            json!({ "peek": 1 }),
            json!({ "peek": { "value": true } }),
        ] {
            assert_eq!(
                admitted("next", refused.clone()),
                Err(Restriction::ArgumentNotAdmitted),
                "{refused}"
            );
        }
    }

    #[test]
    fn memories_is_refused_whenever_a_generation_is_present() {
        for admitted_call in [
            json!({}),
            json!({ "query": "rule" }),
            json!({ "after": "key" }),
            json!({ "query": "key", "full": true, "revision": 2 }),
        ] {
            assert_eq!(
                admitted("memories", admitted_call.clone()),
                Ok(()),
                "{admitted_call}"
            );
        }
        for refused in [
            json!({ "context_generation": "g" }),
            json!({ "context_generation": null }),
            json!({ "context_generation": "" }),
            json!({ "query": "rule", "context_generation": null }),
            json!({ "full": true, "query": "key", "context_generation": "g" }),
            json!({ "after": "key", "context_generation": "g" }),
        ] {
            assert_eq!(
                admitted("memories", refused.clone()),
                Err(Restriction::MemoriesWithContextGeneration),
                "{refused}"
            );
        }
        assert_eq!(
            admitted("memories", json!({ "context_generation": 7 })),
            Err(Restriction::ArgumentNotAdmitted),
            "a generation of another type is not a declared argument"
        );
    }

    #[test]
    fn every_read_word_refuses_an_undeclared_argument() {
        for (name, valid) in [
            ("next", json!({ "peek": true })),
            ("ls", json!({})),
            ("search", json!({ "query": "x" })),
            ("show", json!({ "work_ref": "w-000000000001" })),
            ("memories", json!({})),
        ] {
            assert_eq!(admitted(name, valid.clone()), Ok(()), "{name}");
            let mut extra = object(valid);
            extra.insert("write".into(), json!(true));
            assert_eq!(
                admit(name, Some(&extra)),
                Err(Restriction::ArgumentNotAdmitted),
                "{name}"
            );
        }
        assert_eq!(
            admitted("show", json!({})),
            Err(Restriction::ArgumentNotAdmitted),
            "a missing required argument is not the declared arguments either"
        );
    }

    #[test]
    fn every_other_tool_is_refused_whatever_its_arguments() {
        for name in NOT_READ {
            assert_eq!(
                admit(name, None),
                Err(Restriction::ToolNotAdmitted),
                "{name}"
            );
            assert_eq!(
                admitted(name, json!({ "title": "x", "peek": true })),
                Err(Restriction::ToolNotAdmitted),
                "{name}"
            );
        }
    }

    #[test]
    fn a_refusal_names_the_read_only_mode_with_a_stable_code() {
        let value = refusal("note", Restriction::ToolNotAdmitted);
        assert_eq!(value["error"]["code"], READ_ONLY_REFUSED);
        assert_eq!(value["error"]["details"]["mode"], "read_only");
        assert_eq!(value["error"]["details"]["tool"], "note");
        assert_eq!(
            value["error"]["details"]["restriction"],
            "tool_not_admitted"
        );
        let message = value["error"]["message"].as_str().expect("message");
        assert!(
            message.starts_with("MCP read-only mode refused note:"),
            "{message}"
        );
        let result = CallToolResult::structured_error(value.clone());
        assert_eq!(result.is_error, Some(true));
        assert_eq!(result.structured_content, Some(value));
        // Like every tool error Engram itself returns, each refusal carries
        // its reminders and the read the caller can make instead.
        for (restriction, next) in [
            (
                Restriction::ToolNotAdmitted,
                json!(["engram work next --peek"]),
            ),
            (
                Restriction::NextWithoutPeek,
                json!(["engram work next --peek"]),
            ),
            (
                Restriction::MemoriesWithContextGeneration,
                json!(["engram work memories"]),
            ),
            (Restriction::ArgumentNotAdmitted, json!([])),
        ] {
            let error = &refusal("anything", restriction)["error"];
            assert_eq!(
                error["reminders"],
                json!([format!(
                    "this connection is read-only: {}",
                    restriction.reason()
                )]),
                "{restriction:?}"
            );
            assert_eq!(error["next"], next, "{restriction:?}");
        }
    }

    fn servers(name: &str) -> (crate::test_support::TempHome, McpServer, McpServer, String) {
        let directory = crate::test_support::temp_home().expect("temporary home");
        let database = directory.path().join(format!("{name}.sqlite3"));
        crate::storage::SqliteStore::open(&database).expect("store");
        let project = ProjectId(format!("project-{name}"));
        let writer = McpServer::new_with_actor_context(
            database.clone(),
            project.clone(),
            "writer".into(),
            SessionId("writer".into()),
            None,
            None,
        );
        let added = writer.add(Parameters(
            serde_json::from_value(json!({ "title": "Read me" })).expect("add args"),
        ));
        let work_ref = added.structured_content.expect("add receipt")["work"]["short_ref"]
            .as_str()
            .expect("short ref")
            .to_owned();
        for call in [
            writer.claim(Parameters(
                serde_json::from_value(json!({ "work_ref": work_ref })).expect("claim args"),
            )),
            writer.note(Parameters(
                serde_json::from_value(json!({ "text": "a note to read" })).expect("note args"),
            )),
            writer.remember(Parameters(
                serde_json::from_value(json!({ "text": "a rule to read", "key": "read-rule" }))
                    .expect("remember args"),
            )),
            writer.next(Parameters(
                serde_json::from_value(json!({})).expect("next args"),
            )),
        ] {
            assert_ne!(call.is_error, Some(true), "{call:?}");
        }
        let reader = McpServer::new_read_only_with_actor_context(
            database,
            project,
            "reader".into(),
            SessionId("reader".into()),
            None,
            None,
        );
        (directory, writer, reader, work_ref)
    }

    #[test]
    fn the_read_only_server_lists_only_the_read_words() {
        let (_directory, writer, reader, _) = servers("read-only-list");
        let mut read: Vec<_> = reader
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        read.sort();
        let mut expected: Vec<_> = READ_TOOLS.iter().map(ToString::to_string).collect();
        expected.sort();
        assert_eq!(read, expected);
        for name in NOT_READ {
            assert!(reader.tool_router.get(name).is_none(), "{name}");
        }
        // The ordinary server keeps every tool, and marks none read-only:
        // next and memories have writing forms there.
        let ordinary = writer.tool_router.list_all();
        assert_eq!(ordinary.len(), 15);
        for tool in ordinary {
            assert!(
                tool.annotations
                    .as_ref()
                    .and_then(|annotations| annotations.read_only_hint)
                    .is_none(),
                "{}",
                tool.name
            );
        }
    }

    #[test]
    fn each_read_word_answers_and_leaves_the_store_unchanged() {
        let (directory, _writer, reader, work_ref) = servers("read-only-reads");
        let database = directory.path().join("read-only-reads.sqlite3");
        let shape = || {
            let connection = rusqlite::Connection::open(&database).expect("inspect");
            crate::storage::test_database_shape_snapshot(&connection).expect("snapshot")
        };
        let before = shape();
        for result in [
            reader.next(Parameters(
                serde_json::from_value(json!({ "peek": true })).expect("next"),
            )),
            reader.ls(Parameters(serde_json::from_value(json!({})).expect("ls"))),
            reader.search(Parameters(
                serde_json::from_value(json!({ "query": "Read" })).expect("search"),
            )),
            reader.show(Parameters(
                serde_json::from_value(json!({ "work_ref": work_ref, "notes": true }))
                    .expect("show"),
            )),
            reader.memories(Parameters(
                serde_json::from_value(json!({})).expect("memories"),
            )),
            reader.memories(Parameters(
                serde_json::from_value(json!({ "query": "read-rule", "full": true }))
                    .expect("memories full"),
            )),
        ] {
            assert_ne!(result.is_error, Some(true), "{result:?}");
        }
        // Past the gate, the read-only service itself never takes the
        // writable connection: a writing word called directly is refused.
        let note = reader.note(Parameters(
            serde_json::from_value(json!({ "text": "must not land", "work_ref": work_ref }))
                .expect("note"),
        ));
        let next = reader.next(Parameters(serde_json::from_value(json!({})).expect("next")));
        for refused in [note, next] {
            assert_eq!(refused.is_error, Some(true), "{refused:?}");
            let message = refused.structured_content.expect("refusal")["error"]["message"]
                .as_str()
                .expect("message")
                .to_owned();
            assert!(
                message.contains("read-only and never opens the store for writing"),
                "{message}"
            );
        }
        assert_eq!(shape(), before, "the read-only server wrote nothing");
    }

    // The database file and its WAL keep their bytes through a session of
    // reads and refusals, with the writer's connection still open so no
    // checkpoint runs in between.
    #[test]
    fn reads_leave_the_database_and_wal_bytes_unchanged() {
        let (directory, writer, reader, work_ref) = servers("read-only-bytes");
        let database = directory.path().join("read-only-bytes.sqlite3");
        let mut wal = database.clone().into_os_string();
        wal.push("-wal");
        let wal = std::path::PathBuf::from(wal);
        let bytes = || {
            (
                std::fs::read(&database).expect("database bytes"),
                std::fs::read(&wal).unwrap_or_default(),
            )
        };
        let before = bytes();
        // The reads answer, so an unchanged store is not merely a refused one.
        for result in [
            reader.next(Parameters(
                serde_json::from_value(json!({ "peek": true })).expect("next"),
            )),
            reader.ls(Parameters(
                serde_json::from_value(json!({ "all": true })).expect("ls"),
            )),
            reader.show(Parameters(
                serde_json::from_value(json!({ "work_ref": work_ref, "history": true }))
                    .expect("show"),
            )),
            reader.memories(Parameters(
                serde_json::from_value(json!({})).expect("memories"),
            )),
        ] {
            assert_ne!(result.is_error, Some(true), "{result:?}");
        }
        let note = reader.note(Parameters(
            serde_json::from_value(json!({ "text": "must not land" })).expect("note"),
        ));
        assert_eq!(note.is_error, Some(true), "{note:?}");
        assert_eq!(bytes(), before, "the database and WAL bytes are unchanged");
        drop(writer);
    }

    #[test]
    fn initialize_describes_the_mode_it_serves() {
        use rmcp::ServerHandler;
        let (_directory, writer, reader, _) = servers("read-only-info");
        let ordinary = writer.get_info();
        assert_eq!(
            ordinary.instructions.as_deref(),
            Some(super::super::INSTRUCTIONS)
        );
        let read_only = reader.get_info();
        assert_eq!(read_only.instructions.as_deref(), Some(super::INSTRUCTIONS));
        assert_eq!(read_only.server_info, ordinary.server_info);
        assert_eq!(read_only.capabilities, ordinary.capabilities);
    }
}
