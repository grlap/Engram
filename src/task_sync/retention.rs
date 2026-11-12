//! Exchange-owned artifacts outlive preparation's two-generation journal.
//! A partial or divergent operation is preserved for explicit reconciliation.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{
    TaskTree,
    ledger::{Binding, Event, ImportPlan, Intent, RetainedCandidate, RetainedCommit},
    progress::{ExportCut, ExportProfile, PreparedCandidate, documents_fingerprint},
    setup::PrivateDirectory,
};
use crate::{ObjectId, ProjectId};

pub mod audit;
pub mod delta;
pub mod evidence;
pub mod successor;

const ROOT: &str = "engram-task-sync-exchange";
const COMMITS: &str = "engram-task-sync-commits";
const IMPORTS: &str = "engram-task-sync-import-plans";
const PUSHES: &str = "engram-task-sync-publication-attempts";
pub(crate) const MAX_IMPORT_PLAN: usize = 2 * 1024 * 1024;
const MAX_TASKS: usize = 100_000;
const MAX_FILE: usize = 1024 * 1024;
const MAX_TREE: usize = 128 * 1024 * 1024;

/// External operation identities remain visible after a SQLite rewind. This is
/// an inventory, not evidence that import, ref publication or a push succeeded.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RetainedInventory {
    pub candidates: BTreeSet<uuid::Uuid>,
    pub delta_candidates: BTreeMap<uuid::Uuid, delta::RetainedRequest>,
    pub evidence_imports: BTreeMap<uuid::Uuid, crate::task_sync::ledger::EvidenceImportRecipe>,
    pub planning_successors: BTreeMap<uuid::Uuid, successor::RetainedSuccessor>,
    pub commits: BTreeSet<uuid::Uuid>,
    pub imports: BTreeSet<uuid::Uuid>,
    pub pushes: BTreeSet<uuid::Uuid>,
}

/// Enumerate private operation artifacts without repairing any unknown state.
///
/// # Errors
/// Refuses unknown names, partial content, unsafe entries and bounds. A caller
/// must preserve those artifacts and reconcile them rather than declaring them
/// orphaned merely because its current SQLite ledger has no matching operation.
pub fn inventory(git_directory: &Path) -> Result<RetainedInventory> {
    ensure!(
        git_directory.is_absolute(),
        "Git admin path must be absolute"
    );
    let git = PrivateDirectory::open(git_directory)?;
    let mut result = RetainedInventory {
        delta_candidates: delta::inventory(&git, git_directory)?,
        evidence_imports: evidence::inventory(&git)?,
        planning_successors: successor::inventory(&git)?,
        ..RetainedInventory::default()
    };
    for (name, commits) in [(ROOT, false), (COMMITS, true)] {
        let root = match git.child(Path::new(name)) {
            Ok(root) => root,
            Err(error) if missing(&error) => continue,
            Err(error) => return Err(error),
        };
        for name in root.names(MAX_TASKS)? {
            let text = name.to_str().context("non-UTF8 exchange artifact name")?;
            let operation = uuid::Uuid::parse_str(if commits {
                text.strip_suffix(".json")
                    .context("unknown commit selection name")?
            } else {
                text
            })?;
            ensure!(
                !operation.is_nil()
                    && text
                        == if commits {
                            format!("{operation}.json")
                        } else {
                            operation.to_string()
                        },
                "noncanonical exchange artifact name"
            );
            if commits {
                let bytes = root
                    .read_bounded(Path::new(&name), MAX_FILE)?
                    .context("commit selection disappeared")?;
                let retained: RetainedCommit = serde_json::from_slice(&bytes)?;
                retained.validate()?;
                ensure!(
                    retained.retention_ref
                        == format!("refs/engram/task-sync/exchange/{operation}/commit"),
                    "commit selection names another operation"
                );
                result.commits.insert(operation);
            } else {
                let (manifest, _) = read_operation(&root.child(Path::new(&name))?)?;
                ensure!(
                    manifest.exchange == operation,
                    "candidate names another operation"
                );
                result.candidates.insert(operation);
            }
        }
    }
    match git.child(Path::new(IMPORTS)) {
        Ok(root) => {
            for name in root.names(MAX_TASKS)? {
                let text = name.to_str().context("non-UTF8 import plan operation")?;
                let operation = uuid::Uuid::parse_str(text)?;
                ensure!(
                    !operation.is_nil() && text == operation.to_string(),
                    "noncanonical import plan operation"
                );
                let plan = read_plan_directory(&root.child(Path::new(&name))?)?;
                ensure!(
                    plan.operation == operation,
                    "import plan names another operation"
                );
                result.imports.insert(operation);
            }
        }
        Err(error) if missing(&error) => (),
        Err(error) => return Err(error),
    }
    match git.child(Path::new(PUSHES)) {
        Ok(root) => {
            for name in root.names(MAX_TASKS)? {
                let text = name.to_str().context("non-UTF8 publication witness name")?;
                let operation = uuid::Uuid::parse_str(
                    text.strip_suffix(".json")
                        .context("unknown publication witness name")?,
                )?;
                ensure!(
                    !operation.is_nil() && text == format!("{operation}.json"),
                    "noncanonical publication witness name"
                );
                read_publication_witness(git_directory, operation)?
                    .context("publication witness disappeared")?;
                result.pushes.insert(operation);
            }
        }
        Err(error) if missing(&error) => (),
        Err(error) => return Err(error),
    }
    ensure!(
        result
            .candidates
            .iter()
            .all(|operation| !result.delta_candidates.contains_key(operation)),
        "operation has both legacy and delta retained candidates; preserve uncertainty"
    );
    ensure!(
        result.planning_successors.keys().all(|operation| {
            !result.candidates.contains(operation)
                && !result.delta_candidates.contains_key(operation)
                && !result.evidence_imports.contains_key(operation)
                && !result.commits.contains(operation)
                && !result.imports.contains(operation)
                && !result.pushes.contains(operation)
        }),
        "planning successor aliases another retained family; preserve uncertainty"
    );
    Ok(result)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublicationWitness {
    pub operation: uuid::Uuid,
    pub event_id: ObjectId,
    pub event: Event,
}

pub(crate) fn read_publication_witness(
    git_directory: &Path,
    operation: uuid::Uuid,
) -> Result<Option<PublicationWitness>> {
    let git = PrivateDirectory::open(git_directory)?;
    let root = match git.child(Path::new(PUSHES)) {
        Ok(root) => root,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    root.read_bounded(Path::new(&format!("{operation}.json")), MAX_FILE)?
        .map(|bytes| {
            let witness: PublicationWitness = serde_json::from_slice(&bytes)?;
            ensure!(
                witness.operation == operation
                    && witness.event.schema_version == 1
                    && witness.event.sequence > 1
                    && witness.event.predecessor.is_some()
                    && witness.event.transition.publication_request().is_some()
                    && crate::canonical::canonical_bytes(&witness)? == bytes,
                "invalid or partial publication witness; preserve uncertainty"
            );
            Ok(witness)
        })
        .transpose()
}

// Flushed before the owning SQL transaction commits and before any caller may
// dispatch transport. A failed SQL commit leaves a detectable external witness.
pub(crate) fn retain_publication_witness(
    git_directory: &Path,
    witness: &PublicationWitness,
) -> Result<()> {
    let bytes = crate::canonical::canonical_bytes(witness)?;
    ensure!(
        bytes.len() <= MAX_FILE,
        "publication witness exceeds byte bound"
    );
    let _reservation = audit::AuditReservation::acquire(git_directory)?;
    let git = PrivateDirectory::open(git_directory)?;
    let root = container(&git, Path::new(PUSHES))?;
    write_new(
        &root,
        Path::new(&format!("{}.json", witness.operation)),
        &bytes,
    )?;
    ensure!(
        read_publication_witness(git_directory, witness.operation)?.as_ref() == Some(witness),
        "publication witness changed during write"
    );
    Ok(())
}

pub(crate) fn read_import_plan(
    git_directory: &Path,
    operation: uuid::Uuid,
) -> Result<Option<ImportPlan>> {
    let git = PrivateDirectory::open(git_directory)?;
    let root = match git.child(Path::new(IMPORTS)) {
        Ok(root) => root,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    match root.child(Path::new(&operation.to_string())) {
        Ok(directory) => {
            let plan = read_plan_directory(&directory)?;
            ensure!(
                plan.operation == operation,
                "import plan names another operation"
            );
            Ok(Some(plan))
        }
        Err(error) if missing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

// The operation directory is the first-attempt reservation. Its existence with
// no complete plan is uncertainty, never permission to mint replacements.
pub(crate) fn publish_import_plan(git_directory: &Path, plan: &ImportPlan) -> Result<ImportPlan> {
    ensure!(!plan.operation.is_nil(), "missing import plan operation");
    let bytes = crate::canonical::canonical_bytes(plan)?;
    ensure!(
        bytes.len() <= MAX_IMPORT_PLAN,
        "import plan exceeds supported byte bound"
    );
    let _reservation = audit::AuditReservation::acquire(git_directory)?;
    let git = PrivateDirectory::open(git_directory)?;
    let root = container(&git, Path::new(IMPORTS))?;
    let name = plan.operation.to_string();
    let operation = match root.create_child(Path::new(&name)) {
        Ok(operation) => operation,
        Err(error) => {
            // A competitor may have reserved/published this operation. Reuse
            // only its complete plan; a pending writer is a bounded refusal.
            let existing = root
                .child(Path::new(&name))
                .with_context(|| format!("import plan reservation failed: {error}"))?;
            return read_plan_directory(&existing);
        }
    };
    write_new(&operation, Path::new("plan.pending"), &bytes)?;
    // Both names are resolved through the retained operation directory handle,
    // never a rebuilt path. The hard link atomically creates the publication
    // name without replacement; keeping the original name preserves a
    // pre-publication crash for recovery.
    operation
        .dir()
        .hard_link("plan.pending", operation.dir(), "plan.json")?;
    read_plan_directory(&operation)
}

fn read_plan_directory(operation: &PrivateDirectory) -> Result<ImportPlan> {
    ensure!(
        operation.names(2)? == ["plan.json", "plan.pending"].map(std::ffi::OsString::from),
        "missing, partial or unknown import plan; preserve uncertainty"
    );
    let bytes = operation
        .read_bounded(Path::new("plan.json"), MAX_IMPORT_PLAN)?
        .context("import plan is missing")?;
    ensure!(
        operation
            .read_bounded(Path::new("plan.pending"), MAX_IMPORT_PLAN)?
            .as_ref()
            == Some(&bytes),
        "import plan publication differs from reservation"
    );
    let plan: ImportPlan = serde_json::from_slice(&bytes)?;
    ensure!(
        plan.schema_version == 1
            && !plan.operation.is_nil()
            && crate::canonical::canonical_bytes(&plan)? == bytes,
        "unsupported or noncanonical import plan"
    );
    Ok(plan)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CandidateManifest {
    schema_version: u32,
    exchange: uuid::Uuid,
    preparation: uuid::Uuid,
    store: PathBuf,
    project: ProjectId,
    profile: ExportProfile,
    producer: ObjectId,
    cut: ExportCut,
    documents_fingerprint: ObjectId,
    planning_fingerprint: ObjectId,
}

/// Copy the selected raw bytes before admitting an exchange intent. This creates
/// only private artifacts under the explicitly supplied Git admin directory.
/// It neither edits the worktree/index nor records a successful exchange.
///
/// # Errors
/// Refuses unsafe paths, changed requests, incomplete prior attempts and bounds.
/// Leftovers are retained; retry never repairs, prunes or replaces their bytes.
pub fn retain_prepared(
    git_directory: &Path,
    exchange: uuid::Uuid,
    prepared: &PreparedCandidate,
) -> Result<RetainedCandidate> {
    ensure!(!exchange.is_nil(), "missing exchange operation");
    ensure!(
        git_directory.is_absolute(),
        "Git admin path must be absolute"
    );
    super::directory::admit_documents(&prepared.documents)?;
    let tree = TaskTree::from_stored_documents(&prepared.documents)?;
    let provenance = &prepared.provenance;
    provenance.profile.validate()?;
    ensure!(
        tree == prepared.tree
            && tree.project.project_id == provenance.project
            && tree.project.notes_included == provenance.profile.include_notes
            && !provenance.operation.is_nil()
            && provenance.store.is_absolute()
            && provenance.cut.position >= 0
            && (provenance.cut.position == 0) == provenance.cut.anchor.is_none(),
        "prepared candidate provenance differs"
    );
    let manifest = CandidateManifest {
        schema_version: 1,
        exchange,
        preparation: provenance.operation,
        store: provenance.store.clone(),
        project: provenance.project.clone(),
        profile: provenance.profile.clone(),
        producer: provenance.producer.clone(),
        cut: provenance.cut.clone(),
        documents_fingerprint: fingerprint(&prepared.documents)?,
        planning_fingerprint: fingerprint(&tree.documents()?)?,
    };
    let bytes = crate::canonical::canonical_bytes(&manifest)?;
    ensure!(bytes.len() <= MAX_FILE, "candidate metadata exceeds bound");
    let _reservation = audit::AuditReservation::acquire(git_directory)?;
    let git = PrivateDirectory::open(git_directory)?;
    let root = container(&git, Path::new(ROOT))?;
    let name = exchange.to_string();
    let existing = root
        .names(MAX_TASKS)?
        .iter()
        .any(|entry| entry.as_os_str() == std::ffi::OsStr::new(&name));
    let operation = if existing {
        let operation = root.child(Path::new(&name))?;
        let (stored, documents) = read_operation(&operation)?;
        ensure!(
            stored == manifest && documents == prepared.documents,
            "retained exchange request differs; preserve both inputs"
        );
        operation
    } else {
        // create_new semantics for the directory/files prevent another attempt
        // from being overwritten. Any failure leaves an explicitly partial input.
        let operation = root.create_child(Path::new(&name))?;
        let candidate = operation.create_child(Path::new("candidate"))?;
        let tasks = candidate.create_child(Path::new("tasks"))?;
        for (path, body) in &prepared.documents {
            let (directory, name) = if path == "project.json" {
                (&candidate, "project.json")
            } else {
                (
                    &tasks,
                    path.strip_prefix("tasks/")
                        .context("invalid candidate path")?,
                )
            };
            write_new(directory, Path::new(name), body)?;
        }
        // Written last, only after all raw files were flushed. A partial marker
        // or directory remains a refusal, not permission to recapture.
        write_new(&operation, Path::new("candidate.json"), &bytes)?;
        let (stored, documents) = read_operation(&operation)?;
        ensure!(
            stored == manifest && documents == prepared.documents,
            "retained candidate changed during admission"
        );
        operation
    };
    let _retained_handle = operation;
    Ok(descriptor(git_directory, &manifest))
}

/// Reopen exactly the recorded operation, independent of preparation progress.
/// No artifact is created or removed, including on missing-file refusal.
///
/// # Errors
/// Refuses missing, partial, changed, linked or unexpected artifact entries.
pub fn read_retained(
    git_directory: &Path,
    exchange: uuid::Uuid,
    expected: &RetainedCandidate,
) -> Result<TaskTree> {
    Ok(TaskTree::from_stored_documents(&read_retained_documents(
        git_directory,
        exchange,
        expected,
    )?)?)
}

/// Read the admitted raw map for exact Git tree construction, with no semantic
/// reserialization or replacement by the latest preparation output.
///
/// # Errors
/// Refuses the same incomplete, unsafe or divergent inputs as `read_retained`.
pub fn read_retained_documents(
    git_directory: &Path,
    exchange: uuid::Uuid,
    expected: &RetainedCandidate,
) -> Result<BTreeMap<String, Vec<u8>>> {
    ensure!(
        git_directory.is_absolute() && !exchange.is_nil(),
        "invalid retention selection"
    );
    let git = PrivateDirectory::open(git_directory)?;
    let operation = git
        .child(Path::new(ROOT))?
        .child(Path::new(&exchange.to_string()))?;
    let (manifest, documents) = read_operation(&operation)?;
    ensure!(
        descriptor(git_directory, &manifest) == *expected && manifest.exchange == exchange,
        "retained selection differs; preserve all artifacts"
    );
    Ok(documents)
}

pub(crate) fn read_for_intent(
    binding: &Binding,
    intent: &Intent,
) -> Result<BTreeMap<String, Vec<u8>>> {
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let operation = git
        .child(Path::new(ROOT))?
        .child(Path::new(&intent.operation.to_string()))?;
    let (manifest, documents) = read_operation(&operation)?;
    ensure!(
        manifest.exchange == intent.operation
            && manifest.project == binding.project
            && manifest.profile == binding.profile
            && manifest.producer == intent.producer
            && manifest.store == binding.store.database
            && descriptor(&binding.repository.git_directory, &manifest) == intent.candidate,
        "retained input binding, profile, store or producer differs"
    );
    Ok(documents)
}

fn descriptor(git_directory: &Path, manifest: &CandidateManifest) -> RetainedCandidate {
    RetainedCandidate {
        operation: manifest.preparation,
        directory: git_directory
            .join(ROOT)
            .join(manifest.exchange.to_string())
            .join("candidate"),
        documents_fingerprint: manifest.documents_fingerprint.clone(),
        planning_fingerprint: manifest.planning_fingerprint.clone(),
        source_cut: manifest.cut.clone(),
    }
}

fn fingerprint(documents: &BTreeMap<String, Vec<u8>>) -> Result<ObjectId> {
    documents_fingerprint(documents)
        .parse()
        .map_err(anyhow::Error::msg)
}

// A write-ahead selection survives a missing ref and database rewind. Its
// presence never asserts that update-ref completed. No recovery path overwrites
// it or recreates its ref merely because the current ref is absent.
pub(crate) fn commit_selection(
    git_directory: &Path,
    operation: uuid::Uuid,
) -> Result<Option<RetainedCommit>> {
    let git = PrivateDirectory::open(git_directory)?;
    let root = match git.child(Path::new(COMMITS)) {
        Ok(root) => root,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let name = format!("{operation}.json");
    root.read_bounded(Path::new(&name), MAX_FILE)?
        .map(|bytes| {
            let retained: RetainedCommit = serde_json::from_slice(&bytes)?;
            retained.validate()?;
            Ok(retained)
        })
        .transpose()
}

pub(in crate::task_sync) fn reserve_commit(
    git_directory: &Path,
    operation: uuid::Uuid,
    retained: &RetainedCommit,
    reservation: &audit::AuditReservation,
) -> Result<()> {
    retained.validate()?;
    reservation.require_root(git_directory)?;
    let git = PrivateDirectory::open(git_directory)?;
    let root = container(&git, Path::new(COMMITS))?;
    let bytes = crate::canonical::canonical_bytes(retained)?;
    ensure!(
        bytes.len() <= MAX_FILE,
        "retained commit selection exceeds bound"
    );
    write_new(&root, Path::new(&format!("{operation}.json")), &bytes)
}

fn missing(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

pub(in crate::task_sync) fn container(
    parent: &PrivateDirectory,
    name: &Path,
) -> Result<PrivateDirectory> {
    match parent.child(name) {
        Ok(directory) => Ok(directory),
        Err(error) if missing(&error) => parent.create_child(name),
        Err(error) => Err(error),
    }
}

pub(in crate::task_sync) fn write_new(
    directory: &PrivateDirectory,
    name: &Path,
    bytes: &[u8],
) -> Result<()> {
    let mut file = directory.open_file(name, true, true)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn read_operation(
    operation: &PrivateDirectory,
) -> Result<(CandidateManifest, BTreeMap<String, Vec<u8>>)> {
    let names = operation.names(2)?;
    ensure!(
        names == ["candidate", "candidate.json"].map(std::ffi::OsString::from),
        "incomplete or unknown exchange artifact; preserve it"
    );
    let bytes = operation
        .read_bounded(Path::new("candidate.json"), MAX_FILE)?
        .context("candidate metadata is missing")?;
    let manifest: CandidateManifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.schema_version == 1
            && !manifest.exchange.is_nil()
            && !manifest.preparation.is_nil(),
        "invalid retained metadata"
    );
    manifest.profile.validate()?;
    let candidate = operation.child(Path::new("candidate"))?;
    ensure!(
        candidate.names(2)? == ["project.json", "tasks"].map(std::ffi::OsString::from),
        "unknown candidate entry"
    );
    let tasks = candidate.child(Path::new("tasks"))?;
    let task_names = tasks.names(MAX_TASKS)?;
    let mut documents = BTreeMap::new();
    let project = candidate
        .read_bounded(Path::new("project.json"), MAX_FILE)?
        .context("project document is missing")?;
    let mut total = project.len();
    documents.insert("project.json".into(), project);
    for name in &task_names {
        let name_text = name.to_str().context("non-UTF8 retained task name")?;
        let id = uuid::Uuid::parse_str(
            name_text
                .strip_suffix(".json")
                .context("invalid retained task name")?,
        )?;
        ensure!(
            name_text == format!("{id}.json"),
            "noncanonical retained task name"
        );
        let body = tasks
            .read_bounded(Path::new(name), MAX_FILE)?
            .context("retained task is missing")?;
        total = total
            .checked_add(body.len())
            .context("retained byte bound overflow")?;
        ensure!(total <= MAX_TREE, "retained candidate exceeds byte bound");
        documents.insert(format!("tasks/{name_text}"), body);
    }
    let tree = TaskTree::from_stored_documents(&documents)?;
    ensure!(
        tree.project.project_id == manifest.project
            && tree.project.notes_included == manifest.profile.include_notes
            && fingerprint(&documents)? == manifest.documents_fingerprint
            && fingerprint(&tree.documents()?)? == manifest.planning_fingerprint,
        "retained candidate differs from admitted bytes"
    );
    ensure!(
        operation.names(2)? == names
            && tasks.names(MAX_TASKS)? == task_names
            && operation
                .read_bounded(Path::new("candidate.json"), MAX_FILE)?
                .as_ref()
                == Some(&bytes),
        "retained candidate changed during read"
    );
    Ok((manifest, documents))
}
