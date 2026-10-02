//! The `backup status --json` receipt as docs/features/cli-and-mcp.md
//! documents it, field by field, against the receipt the command emits: the
//! same field paths, and no emitted value of a listed field the table does
//! not list. Fixtures reach every field that can be null, so each documented
//! path is emitted somewhere.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use engram::{
    ObjectId, ProjectId,
    backup::{
        CopyKind,
        restore::{RestoreRecord, RestoreState, restore_record_path, write_restore_record},
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement},
    },
    project_digest,
};
use serde_json::Value;

const PROJECT: &str = "status-receipt-fixture";
const BEGIN: &str = "<!-- backup-status-receipt:begin -->";
const END: &str = "<!-- backup-status-receipt:end -->";

/// One home under the test's root, with the project file beside it.
struct Home {
    root: PathBuf,
    name: &'static str,
}

impl Home {
    fn new(root: &Path, name: &'static str) -> Self {
        fs::write(root.join(".engram-project"), format!("{PROJECT}\n")).unwrap();
        Self {
            root: root.to_path_buf(),
            name,
        }
    }

    fn home(&self) -> PathBuf {
        self.root.join(self.name)
    }

    fn copies(&self) -> PathBuf {
        self.root.join(format!("{}-copies", self.name))
    }

    fn engram(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_engram"))
            .current_dir(&self.root)
            .env_remove("ENGRAM_HOME")
            .arg("--home")
            .arg(self.home())
            .arg("--project-file")
            .arg(self.root.join(".engram-project"))
            .args(args)
            .output()
            .expect("run engram")
    }

    fn succeeded(&self, args: &[&str]) -> Output {
        let output = self.engram(args);
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            text(&output.stdout),
            text(&output.stderr)
        );
        output
    }

    fn status(&self, check_target: bool) -> Value {
        let mut args = vec!["backup", "status", "--json"];
        if check_target {
            args.push("--check-target");
        }
        let value: Value = serde_json::from_slice(&self.succeeded(&args).stdout).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value.get("checks").is_some(), check_target, "{value}");
        value
    }

    /// An initialized store, a directory target and one pushed copy.
    fn pushed(&self) {
        self.succeeded(&["init"]);
        fs::create_dir_all(self.copies()).unwrap();
        self.succeeded(&[
            "backup",
            "target",
            "set",
            "--kind",
            "store",
            "--adapter",
            "directory",
            "--dir",
            self.copies().to_str().unwrap(),
            "--disclosure-authorized-by",
            "greg",
            "--off-host-asserted-by",
            "greg",
        ]);
        self.succeeded(&["backup", "push"]);
    }

    fn paths(&self) -> RecordPaths {
        RecordPaths::new(&self.home(), &ProjectId(PROJECT.into()), CopyKind::Store)
    }

    fn state(&self) -> Value {
        serde_json::from_slice(&fs::read(self.paths().state).unwrap()).unwrap()
    }

    fn write_state(&self, state: &Value) {
        fs::write(
            self.paths().state,
            serde_json::to_vec_pretty(state).unwrap(),
        )
        .unwrap();
    }

    fn write_restore(&self, state: RestoreState) {
        let project = ProjectId(PROJECT.into());
        let lock = PushLock::try_acquire(&self.paths()).unwrap();
        let now = chrono::Utc::now();
        write_restore_record(
            &self.home(),
            &project,
            &lock,
            &RestoreRecord {
                format_version: RECORD_FORMAT_VERSION,
                project: PROJECT.into(),
                copy: "20261002T000000Z-receipt".into(),
                sha256: "ab".repeat(32),
                origin_host: Some("old-host".into()),
                origin_retired: Statement {
                    by: "greg".into(),
                    at: now,
                },
                staging: "staging".into(),
                state,
                pending_at: now,
                completed_at: (state == RestoreState::Completed).then_some(now),
            },
        )
        .unwrap();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The documented receipt.
struct Documented {
    /// Every field path, in table order.
    paths: Vec<String>,
    /// For a field that lists its values, those values.
    values: BTreeMap<String, BTreeSet<String>>,
    /// The JSON types each field may take: its base type, and `null` when it
    /// can be null.
    types: BTreeMap<String, BTreeSet<&'static str>>,
}

/// The JSON type of a value, as the table names it.
fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn documented() -> Documented {
    let doc = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/features/cli-and-mcp.md"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    let start = doc.find(BEGIN).expect("the receipt table's begin marker") + BEGIN.len();
    let end = doc[start..]
        .find(END)
        .expect("the receipt table's end marker")
        + start;
    let mut paths = Vec::new();
    let mut values = BTreeMap::new();
    let mut types = BTreeMap::new();
    for row in doc[start..end]
        .lines()
        .filter(|line| line.starts_with("| `"))
    {
        let cells: Vec<&str> = row.split(" | ").collect();
        let path = cells[0]
            .trim_start_matches("| `")
            .trim_end_matches('`')
            .to_owned();
        let kind = cells[1];
        let base = ["string", "integer", "boolean", "object", "array"]
            .into_iter()
            .find(|base| kind.starts_with(base))
            .unwrap_or_else(|| panic!("{row}: no base type"));
        let mut allowed = BTreeSet::from([base]);
        if kind.contains("or null") {
            allowed.insert("null");
        }
        types.insert(path.clone(), allowed);
        if let Some((_, listed)) = kind.split_once("one of: ") {
            let listed: BTreeSet<String> = listed
                .split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect();
            assert!(!listed.is_empty(), "{row}");
            values.insert(path.clone(), listed);
        }
        paths.push(path);
    }
    assert!(!paths.is_empty(), "the receipt table has rows");
    Documented {
        paths,
        values,
        types,
    }
}

/// What the fixtures emitted: every field path, with array elements as `[]`,
/// the string values seen at each path, and the JSON types seen there.
#[derive(Default)]
struct Emitted {
    paths: BTreeSet<String>,
    values: BTreeMap<String, BTreeSet<String>>,
    types: BTreeMap<String, BTreeSet<&'static str>>,
}

fn emitted(value: &Value, prefix: &str, seen: &mut Emitted) {
    match value {
        Value::Object(fields) => {
            for (key, field) in fields {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                seen.paths.insert(path.clone());
                seen.types
                    .entry(path.clone())
                    .or_default()
                    .insert(json_type(field));
                if let Value::String(text) = field {
                    seen.values
                        .entry(path.clone())
                        .or_default()
                        .insert(text.clone());
                }
                emitted(field, &path, seen);
            }
        }
        Value::Array(items) => {
            let path = format!("{prefix}[]");
            for item in items {
                if let Value::String(text) = item {
                    seen.values
                        .entry(path.clone())
                        .or_default()
                        .insert(text.clone());
                }
                emitted(item, &path, seen);
            }
        }
        _ => {}
    }
}

/// The receipts of fixtures that between them reach every field.
fn receipts(root: &Path) -> Vec<Value> {
    let mut receipts = Vec::new();

    // No store at all: the store's cut is unavailable, no target.
    let bare = Home::new(root, "bare");
    receipts.push(bare.status(false));
    assert_eq!(
        receipts[0]["store_cut_unavailable"]["code"], "store_not_initialized",
        "{}",
        receipts[0]
    );

    // A pushed copy; then a failed push leaves the copy qualifying, and the
    // failed last attempt is shown beside `local_backed_up`.
    let failing = Home::new(root, "failing");
    failing.pushed();
    let pushed = failing.status(false);
    // An absent value is present as null, never left out.
    let target = &pushed["kinds"][0]["target"];
    for key in ["pending", "copy", "last_attempt"] {
        assert!(target.get(key).is_some(), "{key}: {target}");
    }
    assert_eq!(target["pending"], Value::Null);
    assert_eq!(pushed["restore"], Value::Null);
    receipts.push(pushed);
    let aside = root.join("failing-copies-gone");
    fs::rename(failing.copies(), &aside).unwrap();
    let push = failing.engram(&["backup", "push"]);
    assert!(!push.status.success(), "the push must fail");
    let failed = failing.status(false);
    assert_eq!(failed["durability"]["mode"], "local_backed_up", "{failed}");
    let attempt = &failed["kinds"][0]["target"]["last_attempt"];
    assert_eq!(attempt["outcome"], "failed", "{attempt}");
    assert!(
        attempt["code"].is_string() && attempt["message"].is_string(),
        "{attempt}"
    );
    receipts.push(failed);

    // A pending attempt and a copy checked by another build, then a copy
    // the target no longer holds, found by a check.
    let edited = Home::new(root, "edited");
    edited.pushed();
    let mut state = edited.state();
    let other_build = ObjectId::from_canonical_bytes(b"another build")
        .as_str()
        .to_owned();
    state["newest_receipt"]["manifest"]["capture"]["build_fingerprint"] =
        other_build.clone().into();
    for receipt in state["receipts"].as_array_mut().unwrap() {
        receipt["manifest"]["capture"]["build_fingerprint"] = other_build.clone().into();
    }
    let manifest = state["newest_receipt"]["manifest"].clone();
    state["pending"] = serde_json::json!({
        "id": "01a0fe00-0000-7000-8000-000000000000",
        "manifest": manifest,
        "data_file": "pending.db.gz",
        "temporary_data_file": "pending.db.gz.tmp",
    });
    edited.write_state(&state);
    let checked_by_another = edited.status(false);
    let copy = &checked_by_another["kinds"][0]["target"]["copy"];
    assert_eq!(copy["checking_build"], other_build.as_str(), "{copy}");
    assert!(checked_by_another["kinds"][0]["target"]["pending"].is_object());
    receipts.push(checked_by_another);
    let data = edited
        .copies()
        .join(project_digest(&ProjectId(PROJECT.into())))
        .join(format!("{}.db.gz", manifest["copy"].as_str().unwrap()));
    fs::remove_file(data).unwrap();
    let checked = edited.status(true);
    assert_eq!(checked["checks"][0]["outcome"], "missing", "{checked}");
    assert!(checked["kinds"][0]["target"]["copy"]["missing"].is_object());
    receipts.push(checked);

    // A recorded restore: pending, restored, and a record that cannot be
    // used. The outer `restored` is the record's `completed`.
    edited.write_restore(RestoreState::Pending);
    let pending = edited.status(false);
    assert_eq!(pending["restore"]["state"], "pending");
    assert_eq!(pending["restore"]["record"]["state"], "pending");
    receipts.push(pending);
    edited.write_restore(RestoreState::Completed);
    let restored = edited.status(false);
    assert_eq!(restored["restore"]["state"], "restored");
    assert_eq!(restored["restore"]["record"]["state"], "completed");
    receipts.push(restored);
    fs::write(
        restore_record_path(&edited.home(), &ProjectId(PROJECT.into())),
        b"not a record",
    )
    .unwrap();
    let unreadable = edited.status(false);
    assert_eq!(unreadable["restore"]["state"], "unreadable");
    assert_eq!(unreadable["restore"]["record"], Value::Null);
    assert!(unreadable["restore"]["unreadable"].is_string());
    receipts.push(unreadable);

    // A state file this build cannot use.
    let broken = Home::new(root, "broken");
    broken.pushed();
    fs::write(broken.paths().state, b"{ not json").unwrap();
    let broken_status = broken.status(false);
    assert!(broken_status["kinds"][0]["unreadable"].is_string());
    receipts.push(broken_status);
    receipts
}

#[test]
fn the_documented_status_receipt_is_the_one_the_command_emits() {
    let documented = documented();
    let unique: BTreeSet<String> = documented.paths.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        documented.paths.len(),
        "a path is documented twice"
    );

    let root = test_support::temp_home().unwrap();
    let mut seen = Emitted::default();
    for receipt in receipts(root.path()) {
        emitted(&receipt, "", &mut seen);
    }

    let undocumented: Vec<_> = seen.paths.difference(&unique).collect();
    let never_emitted: Vec<_> = unique.difference(&seen.paths).collect();
    assert!(
        undocumented.is_empty() && never_emitted.is_empty(),
        "emitted but not documented: {undocumented:?}; documented but never emitted: {never_emitted:?}"
    );
    for (path, listed) in &documented.values {
        let values = seen.values.get(path).cloned().unwrap_or_default();
        // Each listed field is checked against something the fixtures emit.
        assert!(!values.is_empty(), "{path}: no fixture emits a value");
        let unlisted: Vec<_> = values.difference(listed).collect();
        assert!(
            unlisted.is_empty(),
            "{path} emitted {unlisted:?}, which its documented values {listed:?} do not list"
        );
    }
    // Each field takes only its documented type, and is null only where the
    // table says it can be.
    for (path, allowed) in &documented.types {
        let types = &seen.types[path];
        let undocumented: Vec<_> = types.difference(allowed).collect();
        assert!(
            undocumented.is_empty(),
            "{path} emitted {undocumented:?}, which its documented type {allowed:?} does not allow"
        );
    }
}
