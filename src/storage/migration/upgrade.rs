//! Coordinated offline upgrade: prepare, activate, rollback, finalize, recover.
//!
//! Operator `--offline-confirmed` attests coordinated downtime. It is not a lock,
//! and this module does not change ordinary open or execute an archived binary.
//!
//! File and journal preflight must complete before the first move, unlink,
//! publication, or journal append. Journal records intent; observed files are
//! the state. Instrumented `UpgradeFault` hooks inventory process-exit
//! boundaries; they are not a power-loss durability claim. Contract:
//! `docs/features/coordinated-upgrade-state-machine.md`.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    MigrationError, compare_export_to_source, export_store, import_archive, refused, verify_export,
};

pub const OPERATIONAL_PRECONDITION: &str =
    "operator attested coordinated downtime; not a proven lock";
pub const PROCESS_INTERRUPTION_RECOVERY: &str = "published files after create_new and sync_all; recovery inspects files plus intact journal records; a torn last record refuses mutation, preserves files, and requires explicit diagnosis";
pub const POWER_LOSS_DURABILITY: &str = "unavailable";

const BACKUP_NAME: &str = "backup.db";
const ARCHIVE_NAME: &str = "archive.db";
const CANDIDATE_NAME: &str = "candidate.db";
const EXECUTABLE_NAME: &str = "old-executable";
const RETAINED_NAME: &str = "retained-original.db";
const PUBLISH_STAGING_NAME: &str = "publish-staging";
const PUBLISH_STAGING_PARTIAL_NAME: &str = "publish-staging.partial";
const JOURNAL_PUBLISH_TEMP_NAME: &str = "journal-publish.tmp";
const JOURNAL_DIR: &str = "journal";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradePhase {
    Prepared,
    Activating,
    Activated,
    RollingBack,
    RolledBack,
    Finalized,
}

#[derive(Clone, Debug)]
pub struct UpgradePrepareRequest {
    pub database: PathBuf,
    pub operation: PathBuf,
    pub old_executable: PathBuf,
    pub offline_confirmed: bool,
}

#[derive(Clone, Debug)]
pub struct UpgradeOperationRequest {
    pub operation: PathBuf,
    pub offline_confirmed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeReport {
    pub phase: UpgradePhase,
    pub operation: PathBuf,
    pub database: PathBuf,
    pub offline_confirmed: bool,
    pub operational_precondition: String,
    pub rollback_closed: bool,
    pub backup_sha256: String,
    pub archive_sha256: String,
    pub candidate_sha256: String,
    pub selected_executable: PathBuf,
    pub selected_executable_sha256: String,
    pub current_executable: PathBuf,
    pub current_executable_sha256: String,
    pub retained_original: Option<PathBuf>,
    pub process_interruption_recovery: String,
    pub power_loss_durability: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpgradeFault {
    BackupPublished,
    ArchivePublished,
    CandidatePublished,
    ExecutableCopied,
    PreparedRecorded,
    ActivatingRecorded,
    OriginalRetained,
    RetainedWal,
    RetainedShm,
    RetainedJournal,
    RetainedMain,
    LiveStagingCopy,
    LiveStagingLinked,
    LiveStaged,
    LiveLinked,
    LivePublished,
    JournalRecordTempCreated,
    JournalRecordLinked,
    ActivatedRecorded,
    RollbackMoved,
    RestoredWal,
    RestoredShm,
    RestoredJournal,
    RestoredMain,
    FinalizedRecorded,
}

#[cfg(test)]
std::thread_local! {
    static FAULT: std::cell::Cell<Option<UpgradeFault>> = const { std::cell::Cell::new(None) };
    static EXIT: std::cell::Cell<Option<UpgradeFault>> = const { std::cell::Cell::new(None) };
    static MUTATE_SOURCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub fn inject_upgrade_fault(fault: Option<UpgradeFault>) {
    FAULT.with(|cell| cell.set(fault));
}

#[cfg(test)]
pub fn inject_upgrade_exit(fault: Option<UpgradeFault>) {
    EXIT.with(|cell| cell.set(fault));
}

#[cfg(test)]
pub fn inject_prepare_source_mutation(mutate: bool) {
    MUTATE_SOURCE.with(|cell| cell.set(mutate));
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct JournalRecord {
    sequence: u32,
    kind: JournalKind,
    database: PathBuf,
    database_normalized: String,
    selected_executable: PathBuf,
    backup_sha256: String,
    archive_sha256: String,
    candidate_sha256: String,
    selected_executable_sha256: String,
    current_executable: PathBuf,
    current_executable_sha256: String,
    live_main_sha256: String,
    live_wal_sha256: Option<String>,
    live_journal_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalKind {
    Prepared,
    Activating,
    Activated,
    RollingBack,
    RolledBack,
    Finalized,
}

#[allow(clippy::unnecessary_wraps)]
fn trip(fault: UpgradeFault) -> Result<(), MigrationError> {
    #[cfg(test)]
    {
        if EXIT.with(std::cell::Cell::get) == Some(fault) {
            std::process::exit(73);
        }
        if FAULT.with(std::cell::Cell::get) == Some(fault) {
            FAULT.with(|cell| cell.set(None));
            return Err(refused(format!("injected fault {fault:?}")));
        }
    }
    let _ = fault;
    Ok(())
}

#[cfg(test)]
fn maybe_mutate_source_for_test(database: &Path) -> Result<(), MigrationError> {
    if MUTATE_SOURCE.with(std::cell::Cell::get) {
        MUTATE_SOURCE.with(|cell| cell.set(false));
        let mut bytes = fs::read(database)?;
        bytes.push(0x5a);
        fs::write(database, bytes)?;
    }
    Ok(())
}

fn require_confirmed(confirmed: bool) -> Result<(), MigrationError> {
    if confirmed {
        Ok(())
    } else {
        Err(refused(
            "pass --offline-confirmed after coordinating downtime; this is not a lock",
        ))
    }
}

fn is_unsupported_alias(path: &Path) -> Result<bool, MigrationError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & REPARSE_POINT != 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn path_occupied(path: &Path) -> Result<bool, MigrationError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn refuse_leaf_alias(path: &Path) -> Result<(), MigrationError> {
    if is_unsupported_alias(path)? {
        return Err(refused("refusing symlink or reparse alias"));
    }
    Ok(())
}

fn require_absent_or_regular(path: &Path) -> Result<bool, MigrationError> {
    if !path_occupied(path)? {
        return Ok(false);
    }
    refuse_leaf_alias(path)?;
    if !is_regular_file(path)? {
        return Err(refused(format!("unexpected file {}", path.display())));
    }
    Ok(true)
}

fn require_absent_or_directory(path: &Path) -> Result<bool, MigrationError> {
    if !path_occupied(path)? {
        return Ok(false);
    }
    refuse_leaf_alias(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(refused(format!("unexpected file {}", path.display())));
    }
    Ok(true)
}

fn refuse_if_present(path: &Path, retained: bool) -> Result<(), MigrationError> {
    if !require_absent_or_regular(path)? {
        return Ok(());
    }
    if retained {
        Err(refused(format!(
            "unexpected retained file {}",
            path.display()
        )))
    } else {
        Err(refused(format!("unexpected file {}", path.display())))
    }
}

fn refuse_unsupported_ancestors(path: &Path) -> Result<(), MigrationError> {
    #[cfg(windows)]
    {
        let mut current = path.parent();
        while let Some(path) = current {
            if path.as_os_str().is_empty() {
                break;
            }
            if is_unsupported_alias(path)? {
                return Err(refused("refusing symlink or reparse alias"));
            }
            current = path.parent();
            if current.is_some_and(|parent| parent == path) {
                break;
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
    Ok(())
}

fn bound_path(path: &Path) -> Result<PathBuf, MigrationError> {
    refuse_leaf_alias(path)?;
    refuse_unsupported_ancestors(path)?;
    let absolute = std::path::absolute(path)?;
    if absolute.to_str().is_none() {
        return Err(refused("path contains non-unicode bytes"));
    }
    if absolute.try_exists()? {
        return Ok(fs::canonicalize(&absolute).unwrap_or(absolute));
    }
    if absolute
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err(refused("unresolved parent directory component"));
    }
    let mut current = absolute;
    let mut missing = Vec::new();
    while !current.as_os_str().is_empty() && !current.try_exists()? {
        match current.file_name() {
            Some(name) => missing.push(name.to_os_string()),
            None => {
                return Err(refused("unresolved parent directory component"));
            }
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    refuse_unsupported_ancestors(&current)?;
    let mut bound = if current.try_exists()? {
        fs::canonicalize(&current).unwrap_or(current)
    } else {
        current
    };
    for name in missing.into_iter().rev() {
        bound.push(name);
    }
    Ok(bound)
}

fn os_eq_ignore_fs_case(left: &std::ffi::OsStr, right: &std::ffi::OsStr) -> bool {
    if left == right {
        return true;
    }
    let Some(left) = left.to_str() else {
        return true;
    };
    let Some(right) = right.to_str() else {
        return true;
    };
    if left.is_ascii() && right.is_ascii() {
        left.eq_ignore_ascii_case(right)
    } else {
        true
    }
}

fn path_prefix_overlap(left: &Path, right: &Path) -> bool {
    let left: Vec<_> = left.components().collect();
    let right: Vec<_> = right.components().collect();
    let shared = left.len().min(right.len());
    if shared == 0 {
        return false;
    }
    (0..shared).all(|index| os_eq_ignore_fs_case(left[index].as_os_str(), right[index].as_os_str()))
}

fn same_existing_file(left: &Path, right: &Path) -> Result<bool, MigrationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left_meta = match fs::symlink_metadata(left) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let right_meta = match fs::symlink_metadata(right) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        return Ok(left_meta.dev() == right_meta.dev() && left_meta.ino() == right_meta.ino());
    }
    #[cfg(not(unix))]
    {
        if !path_occupied(left)? || !path_occupied(right)? {
            return Ok(false);
        }
        if !is_regular_file(left)? || !is_regular_file(right)? {
            return Ok(false);
        }
        Ok(sha256_file(left)? == sha256_file(right)?)
    }
}

fn paths_overlap(left: &Path, right: &Path) -> Result<bool, MigrationError> {
    let left_n = PathBuf::from(normalize_path(left)?);
    let right_n = PathBuf::from(normalize_path(right)?);
    if path_prefix_overlap(&left_n, &right_n) {
        return Ok(true);
    }
    if left.try_exists()? && right.try_exists()? {
        let left_c = fs::canonicalize(left).unwrap_or(left_n);
        let right_c = fs::canonicalize(right).unwrap_or(right_n);
        return Ok(path_prefix_overlap(&left_c, &right_c));
    }
    Ok(false)
}

fn refuse_overlap(database: &Path, operation: &Path) -> Result<(), MigrationError> {
    let mut reserved = vec![database.to_path_buf()];
    for suffix in ["-wal", "-shm", "-journal"] {
        reserved.push(sidecar(database, suffix));
    }
    for reserved in reserved {
        if paths_overlap(operation, &reserved)? {
            return Err(refused(
                "operation directory overlaps the live database path",
            ));
        }
    }
    Ok(())
}

fn require_same_upgrader(prepared: &JournalRecord) -> Result<(), MigrationError> {
    let current = std::env::current_exe().map_err(|error| {
        refused(format!(
            "cannot identify the current upgrader executable: {error}"
        ))
    })?;
    if sha256_file(&current)? != prepared.current_executable_sha256 {
        return Err(refused(
            "running upgrader does not match the prepared executable identity",
        ));
    }
    Ok(())
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn store_sidecars(path: &Path) -> [PathBuf; 3] {
    [
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
        sidecar(path, "-journal"),
    ]
}

fn sha256_file(path: &Path) -> Result<String, MigrationError> {
    if !require_absent_or_regular(path)? {
        return Err(refused(format!("missing {}", path.display())));
    }
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn optional_sha256(path: &Path) -> Result<Option<String>, MigrationError> {
    if !require_absent_or_regular(path)? {
        return Ok(None);
    }
    Ok(Some(sha256_file(path)?))
}

fn normalize_path(path: &Path) -> Result<String, MigrationError> {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let resolved = fs::canonicalize(&absolute).unwrap_or(absolute);
    let Some(mut text) = resolved.to_str().map(str::to_owned) else {
        return Err(refused("path contains non-unicode bytes"));
    };
    if cfg!(windows) {
        text = text.replace('/', "\\");
        text = text.trim_start_matches(r"\\?\").to_string();
    }
    Ok(text)
}

fn journal_record_path(operation: &Path, sequence: u32) -> PathBuf {
    operation
        .join(JOURNAL_DIR)
        .join(format!("{sequence:08}.json"))
}

fn journal_publish_temp(operation: &Path) -> PathBuf {
    operation.join(JOURNAL_PUBLISH_TEMP_NAME)
}

fn is_regular_file(path: &Path) -> Result<bool, MigrationError> {
    let metadata = fs::symlink_metadata(path)?;
    Ok(metadata.file_type().is_file() && !metadata.file_type().is_symlink())
}

fn is_prefix_of_bytes(path: &Path, expected: &[u8]) -> Result<bool, MigrationError> {
    if !is_regular_file(path)? {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    if len > expected.len() as u64 {
        return Ok(false);
    }
    let mut actual =
        vec![0_u8; usize::try_from(len).map_err(|_| refused("journal publish temp is too large"))?];
    file.read_exact(&mut actual)?;
    Ok(expected.starts_with(&actual))
}

fn is_prefix_of_file(partial: &Path, source: &Path) -> Result<bool, MigrationError> {
    if !is_regular_file(partial)? || !is_regular_file(source)? {
        return Ok(false);
    }
    let source_len = fs::symlink_metadata(source)?.len();
    let partial_len = fs::symlink_metadata(partial)?.len();
    if partial_len > source_len {
        return Ok(false);
    }
    let mut expected = File::open(source)?;
    let mut actual = File::open(partial)?;
    let mut expected_buf = [0_u8; 8192];
    let mut actual_buf = [0_u8; 8192];
    let mut remaining = partial_len;
    while remaining > 0 {
        let chunk = usize::try_from(remaining.min(8192))
            .map_err(|_| refused("publish staging partial is too large"))?;
        actual.read_exact(&mut actual_buf[..chunk])?;
        expected.read_exact(&mut expected_buf[..chunk])?;
        if actual_buf[..chunk] != expected_buf[..chunk] {
            return Ok(false);
        }
        remaining -= chunk as u64;
    }
    Ok(true)
}

fn legal_next_kinds(previous: Option<JournalKind>) -> &'static [JournalKind] {
    match previous {
        None => &[JournalKind::Prepared],
        Some(JournalKind::Prepared) => &[JournalKind::Activating],
        Some(JournalKind::Activating) => &[JournalKind::Activated, JournalKind::RollingBack],
        Some(JournalKind::Activated) => &[JournalKind::Finalized, JournalKind::RollingBack],
        Some(JournalKind::RollingBack) => &[JournalKind::RolledBack],
        Some(JournalKind::RolledBack | JournalKind::Finalized) => &[],
    }
}

fn journal_temp_should_unlink(
    operation: &Path,
    records: &[JournalRecord],
) -> Result<bool, MigrationError> {
    let temp = journal_publish_temp(operation);
    if !path_occupied(&temp)? {
        return Ok(false);
    }
    refuse_leaf_alias(&temp)?;
    for record in records {
        let dest = journal_record_path(operation, record.sequence);
        if path_occupied(&dest)? && same_existing_file(&temp, &dest)? {
            return Ok(true);
        }
    }
    let dest = journal_record_path(operation, next_sequence(records));
    if path_occupied(&dest)? {
        return Err(refused(
            "journal publish temp contradicts a published record",
        ));
    }
    if records.is_empty() {
        return Err(refused(
            "incomplete prepare; files were kept and must be inspected",
        ));
    }
    let prepared = &records[0];
    let sequence = next_sequence(records);
    for kind in legal_next_kinds(records.last().map(|record| record.kind)) {
        let mut next = prepared.clone();
        next.sequence = sequence;
        next.kind = *kind;
        let bytes = serde_json::to_vec_pretty(&next)?;
        if is_prefix_of_bytes(&temp, &bytes)? {
            return Ok(true);
        }
    }
    Err(refused(
        "journal publish temp is not a prefix of a legal next record",
    ))
}

fn publish_record(operation: &Path, record: &JournalRecord) -> Result<(), MigrationError> {
    let directory = operation.join(JOURNAL_DIR);
    fs::create_dir_all(&directory)?;
    let dest = journal_record_path(operation, record.sequence);
    let temp = journal_publish_temp(operation);
    if path_occupied(&dest)? || path_occupied(&temp)? {
        return Err(refused(format!("refusing to replace {}", dest.display())));
    }
    let bytes = serde_json::to_vec_pretty(record)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let split = bytes.len().saturating_add(1) / 2;
    let split = split.clamp(1, bytes.len());
    let result: Result<(), MigrationError> = (|| {
        file.write_all(&bytes[..split])?;
        trip(UpgradeFault::JournalRecordTempCreated)?;
        file.write_all(&bytes[split..])?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    drop(file);
    protect_private(&temp)?;
    publish_hard_link(&temp, &dest, UpgradeFault::JournalRecordLinked)?;
    Ok(())
}

struct JournalState {
    records: Vec<JournalRecord>,
    torn_last: bool,
}

fn read_journal(operation: &Path) -> Result<JournalState, MigrationError> {
    let directory = operation.join(JOURNAL_DIR);
    if !require_absent_or_directory(&directory)? {
        return Ok(JournalState {
            records: Vec::new(),
            torn_last: false,
        });
    }
    let mut names = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(text) = name.to_str() else {
            return Err(refused("journal contains a non-utf8 name"));
        };
        if text.len() != 13
            || !text.as_bytes().iter().take(8).all(u8::is_ascii_digit)
            || !Path::new(text)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            return Err(refused(format!("journal contains unexpected name {text}")));
        }
        names.push(entry.path());
    }
    names.sort();
    let mut records = Vec::new();
    let mut torn_last = false;
    for (index, path) in names.iter().enumerate() {
        let expected = format!("{:08}.json", index + 1);
        if path.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
            return Err(refused(format!(
                "journal sequence gap at {}",
                path.display()
            )));
        }
        if !require_absent_or_regular(path)? {
            return Err(refused(format!(
                "journal contains unexpected name {}",
                path.display()
            )));
        }
        match serde_json::from_slice::<JournalRecord>(&fs::read(path)?) {
            Ok(record) if usize::try_from(record.sequence).ok() == Some(index + 1) => {
                records.push(record);
            }
            Ok(_) => {
                return Err(refused(format!(
                    "journal sequence field mismatches {}",
                    path.display()
                )));
            }
            Err(_) if index + 1 == names.len() => torn_last = true,
            Err(_) => {
                return Err(refused(format!("torn journal record {}", path.display())));
            }
        }
    }
    validate_journal_records(&records)?;
    Ok(JournalState { records, torn_last })
}

fn legal_transition(previous: Option<JournalKind>, next: JournalKind) -> bool {
    matches!(
        (previous, next),
        (None, JournalKind::Prepared)
            | (Some(JournalKind::Prepared), JournalKind::Activating)
            | (
                Some(JournalKind::Activating),
                JournalKind::Activated | JournalKind::RollingBack
            )
            | (
                Some(JournalKind::Activated),
                JournalKind::Finalized | JournalKind::RollingBack
            )
            | (Some(JournalKind::RollingBack), JournalKind::RolledBack)
    )
}

fn validate_journal_records(records: &[JournalRecord]) -> Result<(), MigrationError> {
    let mut previous = None;
    let prepared = records.first();
    for record in records {
        if !legal_transition(previous, record.kind) {
            return Err(refused("illegal journal phase transition"));
        }
        if let Some(prepared) = prepared {
            let mut expected = prepared.clone();
            expected.sequence = record.sequence;
            expected.kind = record.kind;
            if record != &expected {
                return Err(refused("journal identities contradict the prepared record"));
            }
        }
        previous = Some(record.kind);
    }
    Ok(())
}

fn sync_path(path: &Path) -> Result<(), MigrationError> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    file.sync_all()?;
    Ok(())
}

fn copy_create_new(source: &Path, destination: &Path) -> Result<(), MigrationError> {
    copy_create_new_with_mid(source, destination, None)
}

fn copy_create_new_with_mid(
    source: &Path,
    destination: &Path,
    mid: Option<UpgradeFault>,
) -> Result<(), MigrationError> {
    if path_occupied(destination)? {
        return Err(refused(format!(
            "refusing to replace {}",
            destination.display()
        )));
    }
    let mut input = File::open(source)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(destination)?;
    let result: Result<(), MigrationError> = (|| {
        let mut buffer = [0_u8; 8192];
        let mut copied = false;
        loop {
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            if !copied {
                copied = true;
                if let Some(fault) = mid {
                    trip(fault)?;
                }
            }
        }
        output.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        drop(output);
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    drop(output);
    protect_private(destination)?;
    Ok(())
}

fn publish_hard_link(
    source: &Path,
    destination: &Path,
    linked_fault: UpgradeFault,
) -> Result<(), MigrationError> {
    if path_occupied(destination)? {
        return Err(refused(format!(
            "refusing to replace {}",
            destination.display()
        )));
    }
    if let Err(error) = fs::hard_link(source, destination) {
        return Err(refused(format!(
            "cannot publish without a hard link: {error}"
        )));
    }
    if !same_existing_file(source, destination)? {
        return Err(refused("publish link did not alias the source"));
    }
    trip(linked_fault)?;
    fs::remove_file(source)?;
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), MigrationError> {
    let created = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            builder.create(path)
        }
        #[cfg(not(unix))]
        {
            fs::DirBuilder::new().create(path)
        }
    };
    match created {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(refused(format!("refusing to replace {}", path.display())))
        }
        Err(error) => Err(error.into()),
    }
}

#[allow(clippy::unnecessary_wraps)]
fn protect_private(path: &Path) -> Result<(), MigrationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

fn publish_staging_path(operation: &Path) -> PathBuf {
    operation.join(PUBLISH_STAGING_NAME)
}

fn publish_staging_partial_path(operation: &Path) -> PathBuf {
    operation.join(PUBLISH_STAGING_PARTIAL_NAME)
}

fn refuse_uncreated_publish_staging(operation: &Path) -> Result<(), MigrationError> {
    for path in [
        publish_staging_path(operation),
        publish_staging_partial_path(operation),
    ] {
        if path_occupied(&path)? {
            return Err(refused(
                "publish staging path already exists; refuse before retain",
            ));
        }
    }
    Ok(())
}

fn complete_owned_copy(path: &Path, record: &JournalRecord) -> Result<bool, MigrationError> {
    if !path_occupied(path)? {
        return Ok(false);
    }
    refuse_leaf_alias(path)?;
    if !is_regular_file(path)? {
        return Ok(false);
    }
    Ok(sha256_file(path)? == record.candidate_sha256)
}

fn live_absent_sidecars_clear(record: &JournalRecord) -> Result<(), MigrationError> {
    if path_occupied(&record.database)? {
        return Ok(());
    }
    for path in store_sidecars(&record.database) {
        if path_occupied(&path)? {
            return Err(refused(format!("unexpected file {}", path.display())));
        }
    }
    Ok(())
}

fn owned_publish_staging(operation: &Path, record: &JournalRecord) -> Result<bool, MigrationError> {
    Ok(retained_matches(operation, record)?
        && complete_owned_copy(&publish_staging_path(operation), record)?)
}

fn accounted_publish_cleanup(
    operation: &Path,
    record: &JournalRecord,
    phase: UpgradePhase,
) -> Result<bool, MigrationError> {
    if !matches!(phase, UpgradePhase::Activating | UpgradePhase::RollingBack) {
        return Ok(false);
    }
    if !retained_matches(operation, record)? {
        return Ok(false);
    }
    let live_missing = !path_occupied(&record.database)?;
    if live_missing {
        for path in store_sidecars(&record.database) {
            if path_occupied(&path)? {
                return Ok(false);
            }
        }
    }
    Ok(live_missing || identities_match(record, &record.database)? || candidate_is_live(record)?)
}

fn retained_identity_ready(operation: &Path, record: &JournalRecord) -> Result<(), MigrationError> {
    if retained_matches(operation, record)? {
        Ok(())
    } else {
        Err(refused(
            "retained original does not match prepare identities",
        ))
    }
}

fn retain_component_ready(
    live: &Path,
    retained: &Path,
    expect: &ComponentExpect,
) -> Result<(), MigrationError> {
    match expect {
        ComponentExpect::Absent => {
            if require_absent_or_regular(live)? || require_absent_or_regular(retained)? {
                return Err(refused(format!("unexpected file {}", live.display())));
            }
            Ok(())
        }
        ComponentExpect::Untracked => {
            let retained_present = require_absent_or_regular(retained)?;
            let live_present = require_absent_or_regular(live)?;
            if retained_present && live_present {
                return Err(refused("untracked sidecar present at live and retained"));
            }
            Ok(())
        }
        ComponentExpect::Hash(hash) => {
            if require_absent_or_regular(retained)? {
                if !hash_eq(retained, hash)? {
                    return Err(refused(format!(
                        "cannot reconcile {} with the prepared identity",
                        retained.display()
                    )));
                }
                if require_absent_or_regular(live)? {
                    return Err(refused("component present at live and retained"));
                }
                return Ok(());
            }
            if hash_eq(live, hash)? {
                return Ok(());
            }
            Err(refused(format!(
                "cannot reconcile {} with the prepared identity",
                live.display()
            )))
        }
    }
}

fn retain_plan_ready(record: &JournalRecord, operation: &Path) -> Result<(), MigrationError> {
    let retained = operation.join(RETAINED_NAME);
    retain_component_ready(
        &sidecar(&record.database, "-wal"),
        &sidecar(&retained, "-wal"),
        &record
            .live_wal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
    )?;
    retain_component_ready(
        &sidecar(&record.database, "-journal"),
        &sidecar(&retained, "-journal"),
        &record
            .live_journal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
    )?;
    retain_component_ready(
        &sidecar(&record.database, "-shm"),
        &sidecar(&retained, "-shm"),
        &ComponentExpect::Untracked,
    )?;
    retain_component_ready(
        &record.database,
        &retained,
        &ComponentExpect::Hash(record.live_main_sha256.clone()),
    )?;
    Ok(())
}

fn nonterminal_file_preflight(
    operation: &Path,
    record: &JournalRecord,
    phase: UpgradePhase,
) -> Result<(), MigrationError> {
    match phase {
        UpgradePhase::RollingBack => restore_plan_ready(record, operation),
        UpgradePhase::Activating => {
            let publication_ready = candidate_is_live(record)?
                && hash_eq(&operation.join(RETAINED_NAME), &record.live_main_sha256)?;
            if publication_ready {
                retained_identity_ready(operation, record)
            } else {
                if !path_occupied(&record.database)? {
                    live_absent_sidecars_clear(record)?;
                }
                retain_plan_ready(record, operation)
            }
        }
        _ => Ok(()),
    }
}

fn publish_partial_should_unlink(
    operation: &Path,
    record: &JournalRecord,
    phase: UpgradePhase,
) -> Result<bool, MigrationError> {
    let partial = publish_staging_partial_path(operation);
    if !path_occupied(&partial)? {
        return Ok(false);
    }
    refuse_leaf_alias(&partial)?;
    let staged = publish_staging_path(operation);
    if path_occupied(&staged)? && same_existing_file(&partial, &staged)? {
        if owned_publish_staging(operation, record)? {
            return Ok(true);
        }
        return Err(refused(
            "foreign or changed publish staging file; refuse deletion",
        ));
    }
    if complete_owned_copy(&partial, record)? && retained_matches(operation, record)? {
        return Ok(matches!(phase, UpgradePhase::RollingBack));
    }
    if accounted_publish_cleanup(operation, record, phase)? {
        let candidate = operation.join(CANDIDATE_NAME);
        if is_prefix_of_file(&partial, &candidate)? {
            return Ok(true);
        }
        return Err(refused(
            "publish staging partial is not a prefix of the candidate",
        ));
    }
    Err(refused(
        "publish staging path already exists; refuse before retain",
    ))
}

fn reconcile_publish_partial(
    operation: &Path,
    record: &JournalRecord,
    phase: UpgradePhase,
) -> Result<(), MigrationError> {
    if publish_partial_should_unlink(operation, record, phase)? {
        fs::remove_file(publish_staging_partial_path(operation))?;
    }
    Ok(())
}

fn reserved_temps_ready(
    operation: &Path,
    records: &[JournalRecord],
    prepared: &JournalRecord,
    phase: UpgradePhase,
) -> Result<(bool, bool), MigrationError> {
    let unlink_journal = journal_temp_should_unlink(operation, records)?;
    let partial_phase = if phase == UpgradePhase::RollingBack {
        UpgradePhase::RollingBack
    } else {
        UpgradePhase::Activating
    };
    let unlink_partial = path_occupied(&publish_staging_partial_path(operation))?
        && publish_partial_should_unlink(operation, prepared, partial_phase)?;
    let staged = publish_staging_path(operation);
    if path_occupied(&staged)? && !owned_publish_staging(operation, prepared)? {
        return Err(refused(
            "foreign or changed publish staging file; refuse deletion",
        ));
    }
    Ok((unlink_journal, unlink_partial))
}

fn reconcile_reserved_temps(
    operation: &Path,
    records: &[JournalRecord],
    prepared: &JournalRecord,
    phase: UpgradePhase,
) -> Result<(), MigrationError> {
    let (unlink_journal, unlink_partial) =
        reserved_temps_ready(operation, records, prepared, phase)?;
    if unlink_journal {
        fs::remove_file(journal_publish_temp(operation))?;
    }
    if unlink_partial {
        fs::remove_file(publish_staging_partial_path(operation))?;
    }
    Ok(())
}

fn cleanup_owned_publish_staging(
    operation: &Path,
    record: &JournalRecord,
) -> Result<(), MigrationError> {
    let staged = publish_staging_path(operation);
    if !path_occupied(&staged)? {
        return Ok(());
    }
    refuse_leaf_alias(&staged)?;
    if !owned_publish_staging(operation, record)? {
        return Err(refused(
            "foreign or changed publish staging file; refuse deletion",
        ));
    }
    remove_if_exists(&staged)?;
    Ok(())
}

fn stage_candidate(
    operation: &Path,
    candidate: &Path,
    record: &JournalRecord,
    phase: UpgradePhase,
) -> Result<(), MigrationError> {
    reconcile_publish_partial(operation, record, phase)?;
    let staged = publish_staging_path(operation);
    let partial = publish_staging_partial_path(operation);
    if owned_publish_staging(operation, record)? {
        return Ok(());
    }
    if complete_owned_copy(&partial, record)? && retained_matches(operation, record)? {
        publish_hard_link(&partial, &staged, UpgradeFault::LiveStagingLinked)?;
        trip(UpgradeFault::LiveStaged)?;
        return Ok(());
    }
    if path_occupied(&staged)? {
        return Err(refused(
            "publish staging path already exists; refuse before retain",
        ));
    }
    copy_create_new_with_mid(candidate, &partial, Some(UpgradeFault::LiveStagingCopy))?;
    publish_hard_link(&partial, &staged, UpgradeFault::LiveStagingLinked)?;
    trip(UpgradeFault::LiveStaged)?;
    Ok(())
}

fn publish_candidate_to_live(
    operation: &Path,
    candidate: &Path,
    live: &Path,
    record: &JournalRecord,
    phase: UpgradePhase,
) -> Result<(), MigrationError> {
    if path_occupied(live)? {
        return Err(refused(format!("refusing to replace {}", live.display())));
    }
    stage_candidate(operation, candidate, record, phase)?;
    let staged = publish_staging_path(operation);
    if let Err(error) = fs::hard_link(&staged, live) {
        return Err(refused(format!(
            "cannot publish candidate without a hard link: {error}"
        )));
    }
    trip(UpgradeFault::LiveLinked)?;
    cleanup_owned_publish_staging(operation, record)?;
    Ok(())
}

fn move_file(source: &Path, destination: &Path) -> Result<(), MigrationError> {
    if path_occupied(destination)? {
        return Err(refused(format!(
            "refusing to replace {}",
            destination.display()
        )));
    }
    if !require_absent_or_regular(source)? {
        return Err(refused(format!("missing {}", source.display())));
    }
    fs::rename(source, destination)?;
    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), MigrationError> {
    if path.try_exists()? {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn coherent_backup(source: &Path, backup: &Path) -> Result<(), MigrationError> {
    if path_occupied(backup)? {
        return Err(refused("backup.db already exists"));
    }
    copy_create_new(source, backup)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let side = sidecar(source, suffix);
        if require_absent_or_regular(&side)? {
            copy_create_new(&side, &sidecar(backup, suffix))?;
        }
    }
    let copy = Connection::open(backup)?;
    copy.busy_timeout(std::time::Duration::from_secs(5))?;
    copy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    drop(copy);
    for path in store_sidecars(backup) {
        remove_if_exists(&path)?;
    }
    sync_path(backup)?;
    Ok(())
}

fn live_identities(
    database: &Path,
) -> Result<(String, Option<String>, Option<String>), MigrationError> {
    if !require_absent_or_regular(database)? {
        return Err(refused(format!(
            "database {} is missing",
            database.display()
        )));
    }
    require_absent_or_regular(&sidecar(database, "-shm"))?;
    Ok((
        sha256_file(database)?,
        optional_sha256(&sidecar(database, "-wal"))?,
        optional_sha256(&sidecar(database, "-journal"))?,
    ))
}

fn identities_match(record: &JournalRecord, database: &Path) -> Result<bool, MigrationError> {
    if !require_absent_or_regular(database)? {
        return Ok(false);
    }
    if normalize_path(database)? != record.database_normalized {
        return Ok(false);
    }
    let (main, wal, journal) = live_identities(database)?;
    Ok(main == record.live_main_sha256
        && wal == record.live_wal_sha256
        && journal == record.live_journal_sha256)
}

fn artifact_hashes_match(operation: &Path, record: &JournalRecord) -> Result<bool, MigrationError> {
    Ok(
        sha256_file(&operation.join(BACKUP_NAME))? == record.backup_sha256
            && sha256_file(&operation.join(ARCHIVE_NAME))? == record.archive_sha256
            && sha256_file(&operation.join(CANDIDATE_NAME))? == record.candidate_sha256
            && sha256_file(&operation.join(EXECUTABLE_NAME))? == record.selected_executable_sha256,
    )
}

fn retained_matches(operation: &Path, record: &JournalRecord) -> Result<bool, MigrationError> {
    let retained = operation.join(RETAINED_NAME);
    let wal = sidecar(&retained, "-wal");
    let journal = sidecar(&retained, "-journal");
    require_absent_or_regular(&sidecar(&retained, "-shm"))?;
    let wal_hash = optional_sha256(&wal)?;
    let journal_hash = optional_sha256(&journal)?;
    if !require_absent_or_regular(&retained)? {
        return Ok(false);
    }
    Ok(sha256_file(&retained)? == record.live_main_sha256
        && wal_hash == record.live_wal_sha256
        && journal_hash == record.live_journal_sha256)
}

fn candidate_is_live(record: &JournalRecord) -> Result<bool, MigrationError> {
    if !require_absent_or_regular(&record.database)? {
        return Ok(false);
    }
    Ok(sha256_file(&record.database)? == record.candidate_sha256
        && optional_sha256(&sidecar(&record.database, "-wal"))?.is_none()
        && optional_sha256(&sidecar(&record.database, "-journal"))?.is_none())
}

fn report_from(
    record: &JournalRecord,
    operation: &Path,
    phase: UpgradePhase,
    offline_confirmed: bool,
) -> UpgradeReport {
    let retained = operation.join(RETAINED_NAME);
    UpgradeReport {
        phase,
        operation: operation.to_path_buf(),
        database: record.database.clone(),
        offline_confirmed,
        operational_precondition: OPERATIONAL_PRECONDITION.to_owned(),
        rollback_closed: phase == UpgradePhase::Finalized,
        backup_sha256: record.backup_sha256.clone(),
        archive_sha256: record.archive_sha256.clone(),
        candidate_sha256: record.candidate_sha256.clone(),
        selected_executable: record.selected_executable.clone(),
        selected_executable_sha256: record.selected_executable_sha256.clone(),
        current_executable: record.current_executable.clone(),
        current_executable_sha256: record.current_executable_sha256.clone(),
        retained_original: path_occupied(&retained)
            .ok()
            .unwrap_or(false)
            .then_some(retained),
        process_interruption_recovery: PROCESS_INTERRUPTION_RECOVERY.to_owned(),
        power_loss_durability: POWER_LOSS_DURABILITY.to_owned(),
    }
}

fn next_sequence(records: &[JournalRecord]) -> u32 {
    records.last().map_or(1, |record| record.sequence + 1)
}

fn append(
    operation: &Path,
    records: &[JournalRecord],
    mut record: JournalRecord,
    kind: JournalKind,
) -> Result<JournalRecord, MigrationError> {
    if !legal_transition(records.last().map(|record| record.kind), kind) {
        return Err(refused("illegal journal phase transition"));
    }
    record.sequence = next_sequence(records);
    record.kind = kind;
    let phase = if matches!(kind, JournalKind::RollingBack | JournalKind::RolledBack) {
        UpgradePhase::RollingBack
    } else {
        UpgradePhase::Activating
    };
    reconcile_reserved_temps(operation, records, &record, phase)?;
    publish_record(operation, &record)?;
    Ok(record)
}

fn inspect_phase(
    operation: &Path,
    state: &JournalState,
) -> Result<(UpgradePhase, JournalRecord), MigrationError> {
    let prepared = state
        .records
        .iter()
        .rev()
        .find(|record| record.kind == JournalKind::Prepared)
        .cloned()
        .ok_or_else(|| refused("upgrade operation has no prepared record"))?;
    if !artifact_hashes_match(operation, &prepared)? {
        return Err(refused(
            "operation artifacts do not match the prepared identities",
        ));
    }
    if state.torn_last {
        return Err(refused("torn journal record; refuse mutation"));
    }
    let last_kind = state.records.last().map(|record| record.kind);
    let finalized = last_kind == Some(JournalKind::Finalized)
        || state
            .records
            .iter()
            .any(|record| record.kind == JournalKind::Finalized);
    if finalized {
        if retained_matches(operation, &prepared)? {
            return Ok((UpgradePhase::Finalized, prepared));
        }
        return Err(refused(
            "finalized journal missing retained original; rollback stays closed",
        ));
    }
    if last_kind == Some(JournalKind::RolledBack) {
        return Ok((UpgradePhase::RolledBack, prepared));
    }
    if last_kind == Some(JournalKind::RollingBack) {
        return Ok((UpgradePhase::RollingBack, prepared));
    }
    let live_original = identities_match(&prepared, &prepared.database)?;
    let live_candidate = candidate_is_live(&prepared)?;
    let retained = retained_matches(operation, &prepared)?;
    let retain_in_progress = retain_started(operation, &prepared)?;
    let live_missing = !require_absent_or_regular(&prepared.database)?;
    if last_kind == Some(JournalKind::Activated) {
        if live_missing || !retained {
            return Err(refused(
                "upgrade files contradict the journal; consumers stay excluded",
            ));
        }
        if live_candidate {
            return Ok((UpgradePhase::Activated, prepared));
        }
        if live_original {
            return Err(refused(
                "upgrade files contradict the journal; consumers stay excluded",
            ));
        }
        return Err(refused("activated store received unknown writes"));
    }
    if last_kind == Some(JournalKind::Prepared) {
        if retained || retain_in_progress {
            return Err(refused(
                "upgrade files contradict the journal; consumers stay excluded",
            ));
        }
        let retained_path = operation.join(RETAINED_NAME);
        if path_occupied(&retained_path)?
            || path_occupied(&sidecar(&retained_path, "-wal"))?
            || path_occupied(&sidecar(&retained_path, "-journal"))?
            || path_occupied(&sidecar(&retained_path, "-shm"))?
        {
            return Err(refused(
                "retained original does not match prepare identities",
            ));
        }
        if live_original {
            return Ok((UpgradePhase::Prepared, prepared));
        }
        return Err(refused(
            "live database changed after prepare; refuse activation",
        ));
    }
    if live_original && !retain_in_progress && !retained {
        let retained_path = operation.join(RETAINED_NAME);
        if path_occupied(&retained_path)? {
            return Err(refused(
                "retained original does not match prepare identities",
            ));
        }
    }
    let live_main_original = hash_eq(&prepared.database, &prepared.live_main_sha256)?;
    let retained_main = hash_eq(&operation.join(RETAINED_NAME), &prepared.live_main_sha256)?;
    if !live_main_original && !retained_main {
        return Err(refused(
            "upgrade files contradict the journal; consumers stay excluded",
        ));
    }
    if live_candidate && retained {
        return Ok((UpgradePhase::Activated, prepared));
    }
    if last_kind == Some(JournalKind::Activating) && (retain_in_progress || live_missing) {
        return Ok((UpgradePhase::Activating, prepared));
    }
    if live_original && !retained && !retain_in_progress {
        return Ok((UpgradePhase::Prepared, prepared));
    }
    if path_occupied(&prepared.database)? && !live_original && !live_candidate && !retained {
        return Err(refused(
            "live database changed after prepare; refuse activation",
        ));
    }
    if retained && path_occupied(&prepared.database)? && !live_original && !live_candidate {
        return Err(refused("activated store received unknown writes"));
    }
    Err(refused(
        "upgrade files contradict the journal; consumers stay excluded",
    ))
}

fn require_operation(operation: &Path) -> Result<(), MigrationError> {
    if operation.is_dir() {
        refuse_leaf_alias(operation)?;
        refuse_unsupported_ancestors(operation)?;
        Ok(())
    } else {
        Err(refused(format!(
            "operation directory {} is missing",
            operation.display()
        )))
    }
}

/// Backup, export, import, and validate without moving the live store.
///
/// # Errors
/// Refuses when downtime is not attested, the operation directory exists, a
/// source or artifact cannot be published, or validation fails.
pub fn prepare_upgrade(request: &UpgradePrepareRequest) -> Result<UpgradeReport, MigrationError> {
    require_confirmed(request.offline_confirmed)?;
    if !request.old_executable.is_file() {
        return Err(refused("old executable must be an existing regular file"));
    }
    if !require_absent_or_regular(&request.database)? {
        return Err(refused(format!(
            "database {} is missing",
            request.database.display()
        )));
    }
    refuse_leaf_alias(&request.database)?;
    refuse_leaf_alias(&request.old_executable)?;
    let database = bound_path(&request.database)?;
    let operation = bound_path(&request.operation)?;
    let old_executable = bound_path(&request.old_executable)?;
    let current_executable = bound_path(&std::env::current_exe().map_err(|error| {
        refused(format!(
            "cannot identify the current upgrader executable: {error}"
        ))
    })?)?;
    normalize_path(&database)?;
    normalize_path(&operation)?;
    normalize_path(&old_executable)?;
    normalize_path(&current_executable)?;
    refuse_overlap(&database, &operation)?;
    if let Some(parent) = operation.parent()
        && !parent.as_os_str().is_empty()
    {
        let _ = bound_path(parent)?;
    }
    let current_executable_sha256 = sha256_file(&current_executable)?;
    let initial_identities = live_identities(&database)?;
    if let Some(parent) = operation.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    create_private_dir(&operation)?;
    create_private_dir(&operation.join(JOURNAL_DIR))?;
    let backup = operation.join(BACKUP_NAME);
    let archive = operation.join(ARCHIVE_NAME);
    let candidate = operation.join(CANDIDATE_NAME);
    let executable = operation.join(EXECUTABLE_NAME);
    coherent_backup(&database, &backup)?;
    if live_identities(&database)? != initial_identities {
        return Err(refused("source changed during backup; refuse prepare"));
    }
    trip(UpgradeFault::BackupPublished)?;
    export_store(&backup, &archive)?;
    verify_export(&archive)?;
    compare_export_to_source(&backup, &archive)?;
    trip(UpgradeFault::ArchivePublished)?;
    import_archive(&archive, &candidate)?;
    trip(UpgradeFault::CandidatePublished)?;
    copy_create_new(&old_executable, &executable)?;
    trip(UpgradeFault::ExecutableCopied)?;
    #[cfg(test)]
    maybe_mutate_source_for_test(&database)?;
    if live_identities(&database)? != initial_identities {
        return Err(refused("source changed during prepare; refuse prepare"));
    }
    let backup_sha256 = sha256_file(&backup)?;
    let archive_sha256 = sha256_file(&archive)?;
    let candidate_sha256 = sha256_file(&candidate)?;
    let selected_executable_sha256 = sha256_file(&executable)?;
    if live_identities(&database)? != initial_identities {
        return Err(refused("source changed during prepare; refuse prepare"));
    }
    let (live_main_sha256, live_wal_sha256, live_journal_sha256) = initial_identities;
    let record = JournalRecord {
        sequence: 1,
        kind: JournalKind::Prepared,
        database: database.clone(),
        database_normalized: normalize_path(&database)?,
        selected_executable: old_executable,
        backup_sha256,
        archive_sha256,
        candidate_sha256,
        selected_executable_sha256,
        current_executable,
        current_executable_sha256,
        live_main_sha256,
        live_wal_sha256,
        live_journal_sha256,
    };
    publish_record(&operation, &record)?;
    trip(UpgradeFault::PreparedRecorded)?;
    Ok(report_from(
        &record,
        &operation,
        UpgradePhase::Prepared,
        request.offline_confirmed,
    ))
}

/// # Errors
/// Refuses a missing directory or files that contradict the journal.
pub fn upgrade_status(operation: &Path) -> Result<UpgradeReport, MigrationError> {
    require_operation(operation)?;
    let state = read_journal(operation)?;
    let (phase, record) = inspect_phase(operation, &state)?;
    match phase {
        UpgradePhase::Prepared => {
            retain_plan_ready(&record, operation)?;
            reserved_temps_ready(operation, &state.records, &record, phase)?;
        }
        UpgradePhase::Activating | UpgradePhase::RollingBack => {
            nonterminal_file_preflight(operation, &record, phase)?;
            reserved_temps_ready(operation, &state.records, &record, phase)?;
        }
        UpgradePhase::Activated => {
            reserved_temps_ready(operation, &state.records, &record, phase)?;
        }
        UpgradePhase::RolledBack | UpgradePhase::Finalized => {}
    }
    Ok(report_from(&record, operation, phase, false))
}

enum ComponentExpect {
    Absent,
    Hash(String),
    Untracked,
}

fn hash_eq(path: &Path, expected: &str) -> Result<bool, MigrationError> {
    if !require_absent_or_regular(path)? {
        return Ok(false);
    }
    Ok(sha256_file(path)? == expected)
}

fn retain_component(
    live: &Path,
    retained: &Path,
    expect: ComponentExpect,
    fault: UpgradeFault,
) -> Result<(), MigrationError> {
    match expect {
        ComponentExpect::Absent => {
            refuse_if_present(live, false)?;
            refuse_if_present(retained, false)?;
            Ok(())
        }
        ComponentExpect::Untracked => {
            let retained_present = require_absent_or_regular(retained)?;
            let live_present = require_absent_or_regular(live)?;
            if retained_present {
                if live_present {
                    return Err(refused("untracked sidecar present at live and retained"));
                }
                return Ok(());
            }
            if live_present {
                move_file(live, retained)?;
                trip(fault)?;
            }
            Ok(())
        }
        ComponentExpect::Hash(hash) => {
            let at_retained = hash_eq(retained, &hash)?;
            if at_retained {
                if require_absent_or_regular(live)? {
                    return Err(refused("component present at live and retained"));
                }
                return Ok(());
            }
            if hash_eq(live, &hash)? {
                move_file(live, retained)?;
                trip(fault)?;
                return Ok(());
            }
            Err(refused(format!(
                "cannot reconcile {} with the prepared identity",
                live.display()
            )))
        }
    }
}

fn restore_component(
    live: &Path,
    retained: &Path,
    expect: ComponentExpect,
    fault: UpgradeFault,
) -> Result<(), MigrationError> {
    match expect {
        ComponentExpect::Absent => {
            refuse_if_present(retained, true)?;
            refuse_if_present(live, false)?;
            Ok(())
        }
        ComponentExpect::Untracked => {
            let live_present = require_absent_or_regular(live)?;
            let retained_present = require_absent_or_regular(retained)?;
            if live_present {
                return Ok(());
            }
            if retained_present {
                move_file(retained, live)?;
                trip(fault)?;
            }
            Ok(())
        }
        ComponentExpect::Hash(hash) => {
            if hash_eq(live, &hash)? {
                if hash_eq(retained, &hash)? {
                    return Err(refused("component present at live and retained"));
                }
                return Ok(());
            }
            if require_absent_or_regular(live)? {
                return Err(refused("unknown writes during rollback; refuse deletion"));
            }
            if hash_eq(retained, &hash)? {
                move_file(retained, live)?;
                trip(fault)?;
                return Ok(());
            }
            Err(refused(format!(
                "cannot reconcile {} with the prepared identity",
                live.display()
            )))
        }
    }
}

fn retain_original(record: &JournalRecord, operation: &Path) -> Result<(), MigrationError> {
    let retained = operation.join(RETAINED_NAME);
    retain_component(
        &sidecar(&record.database, "-wal"),
        &sidecar(&retained, "-wal"),
        record
            .live_wal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
        UpgradeFault::RetainedWal,
    )?;
    retain_component(
        &sidecar(&record.database, "-journal"),
        &sidecar(&retained, "-journal"),
        record
            .live_journal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
        UpgradeFault::RetainedJournal,
    )?;
    retain_component(
        &sidecar(&record.database, "-shm"),
        &sidecar(&retained, "-shm"),
        ComponentExpect::Untracked,
        UpgradeFault::RetainedShm,
    )?;
    retain_component(
        &record.database,
        &retained,
        ComponentExpect::Hash(record.live_main_sha256.clone()),
        UpgradeFault::RetainedMain,
    )?;
    trip(UpgradeFault::OriginalRetained)?;
    Ok(())
}

fn occupying_candidate_main(
    record: &JournalRecord,
    retained: &Path,
) -> Result<bool, MigrationError> {
    if !require_absent_or_regular(&record.database)? {
        return Ok(false);
    }
    Ok(sha256_file(&record.database)? == record.candidate_sha256
        && hash_eq(retained, &record.live_main_sha256)?)
}

fn restore_component_ready(
    live: &Path,
    retained: &Path,
    expect: &ComponentExpect,
    occupying_candidate: bool,
) -> Result<(), MigrationError> {
    match expect {
        ComponentExpect::Absent => {
            refuse_if_present(retained, true)?;
            refuse_if_present(live, false)?;
            Ok(())
        }
        ComponentExpect::Untracked => {
            require_absent_or_regular(live)?;
            require_absent_or_regular(retained)?;
            Ok(())
        }
        ComponentExpect::Hash(hash) => {
            let at_live = hash_eq(live, hash)?;
            let at_retained = hash_eq(retained, hash)?;
            if occupying_candidate && at_retained {
                return Ok(());
            }
            if at_live && at_retained {
                return Err(refused("component present at live and retained"));
            }
            if require_absent_or_regular(retained)? && !at_retained {
                return Err(refused(format!(
                    "cannot reconcile {} with the prepared identity",
                    retained.display()
                )));
            }
            if require_absent_or_regular(live)? && !at_live {
                return Err(refused("unknown writes during rollback; refuse deletion"));
            }
            if !at_live && !at_retained {
                return Err(refused(format!(
                    "cannot reconcile {} with the prepared identity",
                    live.display()
                )));
            }
            Ok(())
        }
    }
}

fn restore_plan_ready(record: &JournalRecord, operation: &Path) -> Result<(), MigrationError> {
    let retained = operation.join(RETAINED_NAME);
    let occupying = occupying_candidate_main(record, &retained)?;
    restore_component_ready(
        &sidecar(&record.database, "-wal"),
        &sidecar(&retained, "-wal"),
        &record
            .live_wal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
        false,
    )?;
    restore_component_ready(
        &sidecar(&record.database, "-journal"),
        &sidecar(&retained, "-journal"),
        &record
            .live_journal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
        false,
    )?;
    restore_component_ready(
        &sidecar(&record.database, "-shm"),
        &sidecar(&retained, "-shm"),
        &ComponentExpect::Untracked,
        false,
    )?;
    restore_component_ready(
        &record.database,
        &retained,
        &ComponentExpect::Hash(record.live_main_sha256.clone()),
        occupying,
    )?;
    Ok(())
}

fn restore_original(record: &JournalRecord, operation: &Path) -> Result<(), MigrationError> {
    restore_plan_ready(record, operation)?;
    reconcile_publish_partial(operation, record, UpgradePhase::RollingBack)?;
    if path_occupied(&publish_staging_path(operation))? {
        if !owned_publish_staging(operation, record)? {
            return Err(refused(
                "publish staging path already exists; refuse before retain",
            ));
        }
        cleanup_owned_publish_staging(operation, record)?;
    }
    let retained = operation.join(RETAINED_NAME);
    if occupying_candidate_main(record, &retained)? {
        remove_if_exists(&record.database)?;
    }
    restore_component(
        &sidecar(&record.database, "-wal"),
        &sidecar(&retained, "-wal"),
        record
            .live_wal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
        UpgradeFault::RestoredWal,
    )?;
    restore_component(
        &sidecar(&record.database, "-journal"),
        &sidecar(&retained, "-journal"),
        record
            .live_journal_sha256
            .clone()
            .map_or(ComponentExpect::Absent, ComponentExpect::Hash),
        UpgradeFault::RestoredJournal,
    )?;
    restore_component(
        &sidecar(&record.database, "-shm"),
        &sidecar(&retained, "-shm"),
        ComponentExpect::Untracked,
        UpgradeFault::RestoredShm,
    )?;
    restore_component(
        &record.database,
        &retained,
        ComponentExpect::Hash(record.live_main_sha256.clone()),
        UpgradeFault::RestoredMain,
    )?;
    trip(UpgradeFault::RollbackMoved)?;
    Ok(())
}

fn retain_started(operation: &Path, record: &JournalRecord) -> Result<bool, MigrationError> {
    let retained = operation.join(RETAINED_NAME);
    if hash_eq(&retained, &record.live_main_sha256)? {
        return Ok(true);
    }
    if let Some(hash) = &record.live_wal_sha256
        && hash_eq(&sidecar(&retained, "-wal"), hash)?
    {
        return Ok(true);
    }
    if let Some(hash) = &record.live_journal_sha256
        && hash_eq(&sidecar(&retained, "-journal"), hash)?
    {
        return Ok(true);
    }
    require_absent_or_regular(&sidecar(&retained, "-shm"))
}

fn finish_activation(
    operation: &Path,
    records: &[JournalRecord],
    prepared: &JournalRecord,
    offline_confirmed: bool,
) -> Result<UpgradeReport, MigrationError> {
    if !legal_transition(
        records.last().map(|record| record.kind),
        JournalKind::Activated,
    ) {
        return Err(refused("illegal journal phase transition"));
    }
    reconcile_reserved_temps(operation, records, prepared, UpgradePhase::Activating)?;
    if !retained_matches(operation, prepared)? {
        return Err(refused(
            "retained original does not match prepare identities",
        ));
    }
    let candidate = operation.join(CANDIDATE_NAME);
    live_absent_sidecars_clear(prepared)?;
    if !require_absent_or_regular(&prepared.database)? {
        publish_candidate_to_live(
            operation,
            &candidate,
            &prepared.database,
            prepared,
            UpgradePhase::Activating,
        )?;
        trip(UpgradeFault::LivePublished)?;
    }
    cleanup_owned_publish_staging(operation, prepared)?;
    if !candidate_is_live(prepared)? {
        return Err(refused("live database is not the prepared candidate"));
    }
    if !retained_matches(operation, prepared)? {
        return Err(refused(
            "retained original does not match prepare identities",
        ));
    }
    let record = append(operation, records, prepared.clone(), JournalKind::Activated)?;
    trip(UpgradeFault::ActivatedRecorded)?;
    Ok(report_from(
        &record,
        operation,
        UpgradePhase::Activated,
        offline_confirmed,
    ))
}

/// # Errors
/// Refuses when downtime is not attested, the live store changed after
/// prepare, or journal and files contradict.
pub fn activate_upgrade(
    request: &UpgradeOperationRequest,
) -> Result<UpgradeReport, MigrationError> {
    require_confirmed(request.offline_confirmed)?;
    require_operation(&request.operation)?;
    let state = read_journal(&request.operation)?;
    if state.torn_last {
        return Err(refused("torn journal record; refuse activation"));
    }
    let (phase, prepared) = inspect_phase(&request.operation, &state)?;
    require_same_upgrader(&prepared)?;
    match phase {
        UpgradePhase::Activated => {
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            if state
                .records
                .iter()
                .any(|record| record.kind == JournalKind::Activated)
            {
                Ok(report_from(
                    &prepared,
                    &request.operation,
                    phase,
                    request.offline_confirmed,
                ))
            } else {
                finish_activation(
                    &request.operation,
                    &state.records,
                    &prepared,
                    request.offline_confirmed,
                )
            }
        }
        UpgradePhase::Finalized => Err(refused("upgrade is finalized; rollback is closed")),
        UpgradePhase::RolledBack => Err(refused("rolled-back operation is not activateable")),
        UpgradePhase::RollingBack => Err(refused(
            "rollback is in progress; refuse activation before effects",
        )),
        UpgradePhase::Activating => {
            nonterminal_file_preflight(&request.operation, &prepared, UpgradePhase::Activating)?;
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            if !owned_publish_staging(&request.operation, &prepared)?
                && !complete_owned_copy(
                    &publish_staging_partial_path(&request.operation),
                    &prepared,
                )?
            {
                refuse_uncreated_publish_staging(&request.operation)?;
            }
            retain_original(&prepared, &request.operation)?;
            finish_activation(
                &request.operation,
                &state.records,
                &prepared,
                request.offline_confirmed,
            )
        }
        UpgradePhase::Prepared => {
            retain_plan_ready(&prepared, &request.operation)?;
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            refuse_uncreated_publish_staging(&request.operation)?;
            if !identities_match(&prepared, &prepared.database)? {
                return Err(refused(
                    "live database changed after prepare; refuse activation",
                ));
            }
            let state = if state.records.last().map(|record| record.kind)
                == Some(JournalKind::Activating)
            {
                state
            } else {
                append(
                    &request.operation,
                    &state.records,
                    prepared.clone(),
                    JournalKind::Activating,
                )?;
                trip(UpgradeFault::ActivatingRecorded)?;
                read_journal(&request.operation)?
            };
            retain_original(&prepared, &request.operation)?;
            finish_activation(
                &request.operation,
                &state.records,
                &prepared,
                request.offline_confirmed,
            )
        }
    }
}

/// # Errors
/// Refuses when downtime is not attested, finalize has closed rollback, or
/// the retained original cannot be restored.
pub fn rollback_upgrade(
    request: &UpgradeOperationRequest,
) -> Result<UpgradeReport, MigrationError> {
    require_confirmed(request.offline_confirmed)?;
    require_operation(&request.operation)?;
    let state = read_journal(&request.operation)?;
    if state.torn_last {
        return Err(refused("torn journal record; rollback stays closed"));
    }
    let (phase, prepared) = inspect_phase(&request.operation, &state)?;
    require_same_upgrader(&prepared)?;
    match phase {
        UpgradePhase::Finalized => Err(refused(
            "upgrade is finalized; automatic rollback is closed forever",
        )),
        UpgradePhase::Prepared => {
            retain_plan_ready(&prepared, &request.operation)?;
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            if state.records.last().map(|record| record.kind) == Some(JournalKind::Activating) {
                append(
                    &request.operation,
                    &state.records,
                    prepared.clone(),
                    JournalKind::RollingBack,
                )?;
                let state = read_journal(&request.operation)?;
                let record = append(
                    &request.operation,
                    &state.records,
                    prepared.clone(),
                    JournalKind::RolledBack,
                )?;
                return Ok(report_from(
                    &record,
                    &request.operation,
                    UpgradePhase::RolledBack,
                    request.offline_confirmed,
                ));
            }
            Ok(report_from(
                &prepared,
                &request.operation,
                phase,
                request.offline_confirmed,
            ))
        }
        UpgradePhase::RolledBack => Ok(report_from(
            &prepared,
            &request.operation,
            phase,
            request.offline_confirmed,
        )),
        UpgradePhase::RollingBack | UpgradePhase::Activating | UpgradePhase::Activated => {
            if phase == UpgradePhase::Activated && !candidate_is_live(&prepared)? {
                return Err(refused("activated store received unknown writes"));
            }
            if phase == UpgradePhase::Activating {
                nonterminal_file_preflight(
                    &request.operation,
                    &prepared,
                    UpgradePhase::Activating,
                )?;
            }
            restore_plan_ready(&prepared, &request.operation)?;
            let cleanup_phase = if phase == UpgradePhase::Activated {
                UpgradePhase::RollingBack
            } else {
                phase
            };
            reconcile_reserved_temps(&request.operation, &state.records, &prepared, cleanup_phase)?;
            let state = if phase == UpgradePhase::RollingBack {
                state
            } else {
                append(
                    &request.operation,
                    &state.records,
                    prepared.clone(),
                    JournalKind::RollingBack,
                )?;
                read_journal(&request.operation)?
            };
            restore_original(&prepared, &request.operation)?;
            if !identities_match(&prepared, &prepared.database)? {
                return Err(refused("rollback did not restore the prepared original"));
            }
            let record = append(
                &request.operation,
                &state.records,
                prepared.clone(),
                JournalKind::RolledBack,
            )?;
            Ok(report_from(
                &record,
                &request.operation,
                UpgradePhase::RolledBack,
                request.offline_confirmed,
            ))
        }
    }
}

/// # Errors
/// Refuses when downtime is not attested or the upgrade is not activated.
pub fn finalize_upgrade(
    request: &UpgradeOperationRequest,
) -> Result<UpgradeReport, MigrationError> {
    require_confirmed(request.offline_confirmed)?;
    require_operation(&request.operation)?;
    let state = read_journal(&request.operation)?;
    if state.torn_last {
        return Err(refused("torn journal record; refuse finalize"));
    }
    let (phase, prepared) = inspect_phase(&request.operation, &state)?;
    require_same_upgrader(&prepared)?;
    match phase {
        UpgradePhase::Finalized => Ok(report_from(
            &prepared,
            &request.operation,
            phase,
            request.offline_confirmed,
        )),
        UpgradePhase::Activated => {
            if !candidate_is_live(&prepared)? || !retained_matches(&request.operation, &prepared)? {
                return Err(refused(
                    "finalize requires the prepared candidate live and the retained original",
                ));
            }
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            cleanup_owned_publish_staging(&request.operation, &prepared)?;
            let state = if state
                .records
                .iter()
                .any(|record| record.kind == JournalKind::Activated)
            {
                state
            } else {
                append(
                    &request.operation,
                    &state.records,
                    prepared.clone(),
                    JournalKind::Activated,
                )?;
                read_journal(&request.operation)?
            };
            let record = append(
                &request.operation,
                &state.records,
                prepared.clone(),
                JournalKind::Finalized,
            )?;
            trip(UpgradeFault::FinalizedRecorded)?;
            Ok(report_from(
                &record,
                &request.operation,
                UpgradePhase::Finalized,
                request.offline_confirmed,
            ))
        }
        UpgradePhase::RollingBack => Err(refused(
            "rollback is in progress; refuse finalize before effects",
        )),
        _ => Err(refused(
            "finalize requires an activated upgrade that has not been rolled back",
        )),
    }
}

/// Reconcile interrupted filesystem effects from actual files, not phase alone.
///
/// # Errors
/// Refuses when downtime is not attested, prepare is incomplete, artifacts
/// were tampered, or files contradict the journal.
pub fn recover_upgrade(request: &UpgradeOperationRequest) -> Result<UpgradeReport, MigrationError> {
    require_confirmed(request.offline_confirmed)?;
    require_operation(&request.operation)?;
    let state = read_journal(&request.operation)?;
    let Some(prepared) = state
        .records
        .iter()
        .find(|record| record.kind == JournalKind::Prepared)
        .cloned()
    else {
        return Err(refused(
            "incomplete prepare; files were kept and must be inspected",
        ));
    };
    require_same_upgrader(&prepared)?;
    if !artifact_hashes_match(&request.operation, &prepared)? {
        return Err(refused("tampered or incomplete operation artifacts"));
    }
    if state.torn_last {
        return Err(refused("torn journal record; refuse mutation"));
    }
    let (phase, _) = inspect_phase(&request.operation, &state)?;
    match phase {
        UpgradePhase::RollingBack => {
            restore_plan_ready(&prepared, &request.operation)?;
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::RollingBack,
            )?;
            restore_original(&prepared, &request.operation)?;
            if !identities_match(&prepared, &prepared.database)? {
                return Err(refused("rollback did not restore the prepared original"));
            }
            let record = append(
                &request.operation,
                &state.records,
                prepared.clone(),
                JournalKind::RolledBack,
            )?;
            Ok(report_from(
                &record,
                &request.operation,
                UpgradePhase::RolledBack,
                request.offline_confirmed,
            ))
        }
        UpgradePhase::Activating => {
            nonterminal_file_preflight(&request.operation, &prepared, UpgradePhase::Activating)?;
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            retain_original(&prepared, &request.operation)?;
            finish_activation(
                &request.operation,
                &state.records,
                &prepared,
                request.offline_confirmed,
            )
        }
        UpgradePhase::Activated => {
            if !state
                .records
                .iter()
                .any(|record| record.kind == JournalKind::Activated)
            {
                reconcile_reserved_temps(
                    &request.operation,
                    &state.records,
                    &prepared,
                    UpgradePhase::Activating,
                )?;
                return finish_activation(
                    &request.operation,
                    &state.records,
                    &prepared,
                    request.offline_confirmed,
                );
            }
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            Ok(report_from(
                &prepared,
                &request.operation,
                phase,
                request.offline_confirmed,
            ))
        }
        UpgradePhase::Finalized | UpgradePhase::Prepared | UpgradePhase::RolledBack => {
            if phase == UpgradePhase::Prepared {
                retain_plan_ready(&prepared, &request.operation)?;
            }
            reconcile_reserved_temps(
                &request.operation,
                &state.records,
                &prepared,
                UpgradePhase::Activating,
            )?;
            Ok(report_from(
                &prepared,
                &request.operation,
                phase,
                request.offline_confirmed,
            ))
        }
    }
}

#[cfg(test)]
mod tests;
