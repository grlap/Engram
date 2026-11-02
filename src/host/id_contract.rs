//! The identifier contract of the host channel, as tests hold real receipts
//! to it. Every identifier or fingerprint that crosses the channel has one
//! class, named by its field; a value in that field must have its class's
//! form. The table in the host protocol documentation lists the same fields.
//!
//! A field's name decides its class, never the value's shape: a legacy
//! 64-digit record id and a SHA-256 fingerprint are spelled alike, and only
//! the field tells them apart. The shape is used once, as a coverage guard: a
//! value shaped like an identifier in a field this contract does not list
//! fails, so a new field cannot cross the channel unclassified.

use serde_json::Value;

use crate::ObjectId;
use crate::domain::GitTreeId;

/// The class of an identifier or fingerprint on the host channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdClass {
    /// An Engram record id: 32 lowercase hex digits for a record stored now,
    /// 64 for one stored before ids were minted. Opaque, compared exactly.
    RecordId,
    /// A typed identity such as a work, run or claim id: a lowercase,
    /// hyphenated UUID.
    Uuid,
    /// A content fingerprint Engram computes: the 64-digit lowercase SHA-256
    /// of canonical bytes. It compares content and is never an id.
    Fingerprint,
    /// A content fingerprint the host sends, returned unchanged: a host sends
    /// the 64-digit SHA-256, and Engram today also admits 32 digits.
    HostFingerprint,
    /// An obligation rule's id, as an obligation rule set admits it: a
    /// built-in rule's, or one an operator's rule set names.
    RuleId,
    /// A Git tree the host names: 40 or 64 lowercase hex digits, as Git
    /// spells it.
    GitTree,
    /// A Git commit the host names: 40 lowercase hex digits, the only form
    /// registered verification admits.
    GitCommit,
    /// A historical execution observation's grant id, returned unchanged as
    /// recorded data and never used for live authority.
    Token,
    /// An identifier the host mints and Engram keeps unchanged, such as a
    /// session id, a host run id or an idempotency key: non-empty text,
    /// compared exactly. Its producer's admission bounds its length and may
    /// fix a prefix; inner whitespace is allowed.
    HostId,
    /// Host text that names what the host measured or asserted, such as a
    /// workspace, a source revision or a project: non-empty, compared
    /// exactly, never parsed.
    HostText,
    /// A work item's reference as Engram shows it: `w-` and 12 lowercase hex
    /// digits, or the work id itself when no item of the project has it.
    WorkRef,
}

impl IdClass {
    /// Whether `value` has this class's form, checked by the producer's own
    /// parser where one exists.
    pub(crate) fn admits(self, value: &str) -> bool {
        match self {
            Self::RecordId | Self::HostFingerprint => value.parse::<ObjectId>().is_ok(),
            Self::RuleId => crate::domain::BuiltinObligationRuleRef::admits_rule_id(value),
            Self::Uuid => uuid::Uuid::parse_str(value).is_ok_and(|uuid| {
                let mut buffer = uuid::Uuid::encode_buffer();
                &*uuid.hyphenated().encode_lower(&mut buffer) == value
            }),
            Self::Fingerprint => value.len() == 64 && value.parse::<ObjectId>().is_ok(),
            Self::GitTree => GitTreeId::parse(value.to_owned()).is_ok(),
            Self::GitCommit => value.len() == 40 && is_lower_hex(value),
            Self::Token | Self::HostId | Self::HostText => !value.is_empty(),
            Self::WorkRef => {
                value
                    .strip_prefix("w-")
                    .is_some_and(|rest| rest.len() == 12 && is_lower_hex(rest))
                    || Self::Uuid.admits(value)
            }
        }
    }
}

fn is_lower_hex(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// The class of a field that carries an identifier or fingerprint across the
/// host channel, by the field's name; `None` for any other field.
pub(crate) fn class_of(field: &str) -> Option<IdClass> {
    Some(match field {
        // Records Engram stores and returns by id.
        "checkpoint"
        | "last_checkpoint"
        | "execution_observations"
        | "verification_evidence"
        | "environment_evidence"
        | "event"
        | "latest_event"
        | "binding_event"
        | "current_binding"
        | "claim_epoch_event"
        | "observation"
        | "opened_obligations"
        | "original"
        | "verification"
        | "sighting"
        | "record"
        | "records"
        | "definition"
        | "triggering_observation"
        | "producer_observation"
        | "resolution"
        | "satisfied_by"
        | "source_change"
        | "environment"
        | "evidence"
        | "evidence_basis"
        | "evaluation"
        | "supersedes"
        | "named_root_binding"
        | "root_binding"
        | "completion_seal"
        | "required_child_seals"
        | "policy"
        | "previous_policy"
        | "obligation_rule_set"
        | "rule_set"
        | "authority"
        | "replacement_decision"
        | "producer"
        | "record_id"
        | "object_id" => IdClass::RecordId,
        // Typed identities.
        "task_id" | "root_execution_id" | "work_id" | "run_id" | "run" | "claim_id"
        | "obligation_id" | "root_id" | "memory_id" => IdClass::Uuid,
        // Content fingerprints the host sends, and those Engram computes.
        "intent_fingerprint"
        | "action_fingerprint"
        | "check_fingerprint"
        | "environment_fingerprint" => IdClass::HostFingerprint,
        "content_digest" | "work_revision_hash" | "accepted_work_revision_hash" | "packet_hash" => {
            IdClass::Fingerprint
        }
        // Built-in obligation rules.
        "rule_id" | "obligations_satisfied" => IdClass::RuleId,
        // Git objects the host names.
        "git_tree" | "tree" => IdClass::GitTree,
        "commit" | "base_commit" => IdClass::GitCommit,
        // Identifiers and keys the host mints, and Engram's directive id,
        // which is built from the host's request key.
        "session_id"
        | "initiating_session_id"
        | "owning_session_id"
        | "observing_session"
        | "holder"
        | "host_run_id"
        | "request_identity"
        | "operation_key"
        | "idempotency_key"
        | "request_key"
        | "directive_id"
        | "observation_id"
        | "host_check_id"
        | "host_turn_ref"
        | "host_evidence_ref"
        | "external_ref" => IdClass::HostId,
        // Host text naming what the host measured or asserted.
        "workspace_id" | "source_revision" | "revision" | "project_id" | "actor_id"
        | "agent_id" | "namespace" | "stage" => IdClass::HostText,
        "work_ref" => IdClass::WorkRef,
        // Historical ExecutionObservation records may expose their original
        // grant id. No current host operation mints or admits a grant.
        "grant_id" => IdClass::Token,
        _ => return None,
    })
}

/// The class of a refused target's `expected` and `actual` values, which
/// depends on the refusal's `reason`; `None` when the reason names a counter,
/// host text or a time instead.
pub(crate) fn refusal_value_class(reason: &str) -> Option<IdClass> {
    match reason {
        "run_not_active" | "root_execution_moved" => Some(IdClass::Uuid),
        "sighting_not_newest" => Some(IdClass::RecordId),
        "workspace_differs" | "sighting_revision_differs" => Some(IdClass::HostText),
        _ => None,
    }
}

/// The class of a feed's `id`, which depends on the feed's `kind`: a root's
/// or a run's feed is named by a typed identity, the project's feed by the
/// project's asserted text.
pub(crate) fn feed_id_class(kind: &str) -> Option<IdClass> {
    match kind {
        "root_work" | "run_execution" => Some(IdClass::Uuid),
        "project" => Some(IdClass::HostText),
        _ => None,
    }
}

/// The class of `key` in `map`: its own class, or for a value whose class
/// depends on a sibling, the class that sibling names.
fn member_class(key: &str, map: &serde_json::Map<String, Value>) -> Option<IdClass> {
    let sibling = |name: &str| map.get(name).and_then(Value::as_str);
    match (key, sibling("reason"), sibling("kind")) {
        ("expected" | "actual", Some(reason), _) => refusal_value_class(reason),
        ("id", _, Some(kind)) => feed_id_class(kind),
        // A registered verification receipt names its operation record by
        // id; elsewhere `operation` is a protocol label.
        ("operation", _, _) if map.contains_key("verifications") => Some(IdClass::RecordId),
        // A refused bind names why its original was refused, beside the
        // refused `targets` and with no `measurement`, which only a bind's
        // request carries; elsewhere `original` names the original record.
        ("original", _, _) if map.contains_key("targets") && !map.contains_key("measurement") => {
            None
        }
        // An actor context's run id is asserted text; a provenance link's
        // source and reference are host text.
        ("run_id", _, _) if map.contains_key("actor_kind") => Some(IdClass::HostText),
        ("source" | "reference", _, _) if map.contains_key("relation") => Some(IdClass::HostText),
        // A historical delivery packet's memory names its version by record
        // id and its authority by a label.
        ("version", _, _) if map.contains_key("memory_id") => Some(IdClass::RecordId),
        ("authority", _, _) if map.contains_key("memory_id") => None,
        (key, _, _) => class_of(key),
    }
}

/// Whether `value` is shaped like an identifier: lowercase hex of a record
/// id's, Git tree's or fingerprint's length, or a UUID. Used only to find
/// fields this contract does not list.
fn looks_like_an_identifier(value: &str) -> bool {
    (matches!(value.len(), 32 | 40 | 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
        || uuid::Uuid::parse_str(value).is_ok_and(|_| value.len() == 36)
}

/// Every place in `value` where an identifier breaks the contract: a value
/// that does not have its field's class, or one shaped like an identifier in
/// a field the contract does not list. Each entry names the JSON path.
pub(crate) fn contract_breaks(value: &Value) -> Vec<String> {
    let mut breaks = Vec::new();
    walk(value, "", None, &mut breaks);
    breaks
}

/// Walks `value`, whose field gives it `class` (`None` for a field the
/// contract does not list).
fn walk(value: &Value, path: &str, class: Option<IdClass>, breaks: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, member) in map {
                let class = member_class(key, map);
                walk(member, &format!("{path}/{key}"), class, breaks);
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                walk(item, &format!("{path}/{index}"), class, breaks);
            }
        }
        Value::String(text) => match class {
            Some(class) if !class.admits(text) => {
                breaks.push(format!("{path}: {text:?} is not a {class:?}"));
            }
            None if looks_like_an_identifier(text) => breaks.push(format!(
                "{path}: {text:?} looks like an identifier in a field the contract does not list"
            )),
            _ => {}
        },
        _ => {}
    }
}

/// Asserts that `receipt`, serialized as the host channel sends it, holds to
/// the identifier contract.
///
/// # Panics
///
/// When it does not, naming every break.
#[track_caller]
pub(crate) fn assert_host_ids<T: serde::Serialize + ?Sized>(receipt: &T) {
    let value = serde_json::to_value(receipt).expect("a host receipt serializes");
    let breaks = contract_breaks(&value);
    assert!(
        breaks.is_empty(),
        "the host identifier contract is broken:\n{}\nin {value}",
        breaks.join("\n")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minted() -> String {
        ObjectId::mint().as_str().to_owned()
    }

    fn fingerprint() -> String {
        ObjectId::from_canonical_bytes(b"content")
            .as_str()
            .to_owned()
    }

    #[test]
    fn each_class_admits_its_producers_form_and_nothing_else() {
        let legacy = fingerprint();
        let uuid = crate::WorkId::new().0.to_string();
        let tree40 = "a".repeat(40);
        // A record id is 32 digits now and 64 for a record stored before.
        for (class, admitted, refused) in [
            (
                IdClass::RecordId,
                vec![minted(), legacy.clone()],
                vec![
                    tree40.clone(),
                    minted().to_uppercase(),
                    uuid.clone(),
                    String::new(),
                ],
            ),
            (
                IdClass::Fingerprint,
                vec![fingerprint()],
                vec![minted(), tree40.clone(), fingerprint().to_uppercase()],
            ),
            (
                IdClass::HostFingerprint,
                vec![fingerprint(), minted()],
                vec![tree40.clone(), fingerprint().to_uppercase(), String::new()],
            ),
            (
                IdClass::RuleId,
                vec![
                    crate::control::SOURCE_CHANGE_RULE_ID.into(),
                    format!("{}2", crate::control::ACCEPTANCE_BINDING_RULE_PREFIX),
                    "operator requires a test".into(),
                ],
                vec![" padded".into(), "x".repeat(129), String::new()],
            ),
            (
                IdClass::Uuid,
                vec![uuid.clone()],
                vec![uuid.replace('-', ""), uuid.to_uppercase(), minted()],
            ),
            (
                IdClass::GitTree,
                vec![tree40.clone(), "b".repeat(64)],
                vec![minted(), "A".repeat(40), "a".repeat(39)],
            ),
            (
                IdClass::GitCommit,
                vec![tree40.clone()],
                vec![
                    minted(),
                    "A".repeat(40),
                    "a".repeat(39),
                    "b".repeat(64),
                    uuid.clone(),
                ],
            ),
            (
                IdClass::Token,
                vec![uuid.clone(), "a historical observation's grant id".into()],
                vec![String::new()],
            ),
            (
                IdClass::HostId,
                vec![
                    "session-a".into(),
                    "runner one".into(),
                    "termal-run:test-1".into(),
                    uuid.clone(),
                ],
                vec![String::new()],
            ),
            (
                IdClass::HostText,
                vec![
                    "content-v1:x".into(),
                    uuid.clone(),
                    fingerprint(),
                    "a project".into(),
                ],
                vec![String::new()],
            ),
            (
                IdClass::WorkRef,
                vec!["w-0123456789ab".into(), uuid.clone()],
                vec![
                    "w-0123".into(),
                    "W-0123456789ab".into(),
                    "w-0123456789AB".into(),
                    minted(),
                    String::new(),
                ],
            ),
        ] {
            for value in admitted {
                assert!(class.admits(&value), "{class:?} refused {value:?}");
            }
            for value in refused {
                assert!(!class.admits(&value), "{class:?} admitted {value:?}");
            }
        }
    }

    #[test]
    fn a_field_decides_the_class_and_an_unlisted_identifier_is_a_break() {
        let uuid = crate::WorkId::new().0.to_string();
        // The same 64 digits are a legacy record id in one field and a
        // fingerprint in another: both hold.
        let digits = fingerprint();
        let held = serde_json::json!({
            "checkpoint": minted(),
            "verification_evidence": [minted(), digits.clone()],
            "check_fingerprint": digits,
            // A host-sent fingerprint comes back as sent, 32 digits included;
            // rule ids name built-in obligation rules.
            "intent_fingerprint": minted(),
            "obligations_satisfied": [crate::control::SOURCE_CHANGE_RULE_ID,
                format!("{}1", crate::control::ACCEPTANCE_BINDING_RULE_PREFIX)],
            "rule": {"rule_id": crate::control::SOURCE_CHANGE_RULE_ID, "rule_version": 1},
            "work_id": crate::WorkId::new().0.to_string(),
            "source_basis": {"git_tree": "c".repeat(40), "source_revision": "content-v1:x"},
            "historical_observation": {"grant_id": "recorded-grant"},
            "accounting": {"kind": "repeat", "source_change": minted()},
            "occurrence": {"source_change": {"detection": "watcher_only"}},
            // A refused target's values take the class its reason names.
            "targets": [
                {"reason": "sighting_not_newest", "expected": minted(), "actual": minted()},
                {"reason": "root_execution_moved", "expected": crate::WorkId::new().0.to_string()},
                {"reason": "claim_fence_moved", "expected": "3", "actual": "4"},
            ],
            // A refused bind's original is a reason; a bind's request and
            // receipt name the original record.
            "refusal": {"original": "claim_not_held", "request": "more_targets_than_held_claims", "targets": []},
            "bind": {"original": minted(), "replayed": false, "bound": []},
            "bind_request": {"original": minted(), "measurement": {}, "targets": []},
            // A feed's id takes the class its kind names.
            "read_cut": {"feed": {"kind": "run_execution", "id": crate::WorkId::new().0.to_string()}, "position": 7},
            "project_cut": {"feed": {"kind": "project", "id": "project-a"}, "position": 3},
            // A registered verification names its records; its request names
            // host-minted ids and Git objects.
            "receipt": {"operation": minted(), "producer": minted(), "verifications": [{"stage": "full", "record_id": minted()}]},
            "request": {"operation": "verification_record", "operation_key": "termal-run:test-1",
                "registration": {"host_run_id": "test-1", "request_identity": "request-a", "initiating_session_id": "session-1"},
                "input": {"kind": "staged_tree", "tree": "d".repeat(40), "base_commit": "e".repeat(40)}},
            // Host-minted keys, refs and text hold whatever form the host
            // gives them, including one shaped like an identifier.
            "idempotency_key": uuid.clone(),
            "observed_checks": [{"host_check_id": uuid.clone()}],
            "session_id": "runner one",
            "named_root": {"workspace_id": uuid.clone(), "source_revision": "f".repeat(40)},
            "named_targets": [
                {"reason": "workspace_differs", "expected": uuid.clone(), "actual": "f".repeat(64)},
            ],
            "project_feed": {"feed": {"kind": "project", "id": uuid.clone()}, "position": 1},
            "checkpoint_reference": {"kind": "object_id", "object_id": minted()},
            // A work ref is the short ref, or the work id when no item has it.
            "bound": [{"work_ref": "w-0123456789ab"}, {"work_ref": uuid.clone()}],
            // An actor context's run id and provenance are asserted text.
            "binder": {"actor_id": "agent", "actor_kind": "agent", "run_id": "f".repeat(32),
                "provenance_chain": [{"relation": "asserted_by", "source": "agent", "reference": uuid.clone()}]},
            // A historical packet's memory names its version by record id.
            "pinned": [{"memory_id": uuid.clone(), "version": minted(), "authority": "firm"}],
        });
        assert_eq!(contract_breaks(&held), Vec::<String>::new());

        let broken = serde_json::json!({
            "content_digest": minted(),
            "obligations_satisfied": [" padded"],
            "verification_evidence": ["not-an-id"],
            "work_id": minted(),
            "unlisted": minted(),
            "nested": [{"also_unlisted": crate::WorkId::new().0.to_string()}],
            "targets": [{"reason": "root_execution_moved", "expected": minted()}],
            "read_cut": {"feed": {"kind": "root_work", "id": minted()}, "position": 7},
            // A receipt's operation is a record id; a commit is a Git object.
            "receipt": {"operation": "verification_record", "verifications": []},
            "input": {"commit": minted()},
            // Outside a refused bind, `original` is a record id.
            "bind": {"original": "claim_not_held", "bound": []},
            "bind_request": {"original": "claim_not_held", "measurement": {}, "targets": []},
            // A work ref has its form; a registered commit is 40 digits; a
            // host key is never empty.
            "work_ref": "not-a-ref",
            "registered": {"base_commit": "b".repeat(64)},
            "idempotency_key": "",
        });
        let breaks = contract_breaks(&broken);
        for path in [
            "/content_digest:",
            "/obligations_satisfied/0:",
            "/verification_evidence/0:",
            "/work_id:",
            "/unlisted:",
            "/nested/0/also_unlisted:",
            "/targets/0/expected:",
            "/read_cut/feed/id:",
            "/receipt/operation:",
            "/input/commit:",
            "/bind/original:",
            "/bind_request/original:",
            "/work_ref:",
            "/registered/base_commit:",
            "/idempotency_key:",
        ] {
            assert!(
                breaks.iter().any(|entry| entry.starts_with(path)),
                "{path} not reported in {breaks:?}"
            );
        }
        assert_eq!(breaks.len(), 15, "{breaks:?}");
    }
}
