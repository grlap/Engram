//! The `directory` adapter: copies stored gzip-compressed under an absolute
//! path on this host, such as a share, a removable disk or a folder that a
//! sync client replicates. It uses plain file operations and accepts network
//! paths, links and cloud folders on purpose.
//!
//! Every confirmation decompresses the stored file and compares its length and
//! SHA-256 with the manifest; names and sizes may show a copy missing but
//! never confirm one. The deadline is checked between chunks: a read that
//! stalls inside one chunk, as on a hung network share, is not interrupted.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use engram::{ObjectId, ProjectId, backup::CaptureManifest};
use flate2::{Compression, bufread::GzDecoder, write::GzEncoder};
use sha2::{Digest, Sha256};

use super::adapter::{
    Acknowledgement, AdapterError, Attempt, BackupAdapter, BackupReceipt, Confirmation, Encoding,
    ManifestPage, OffHost, Reconciled, STORED_FORMAT_VERSION, StoredManifest,
};

/// How many manifests one `list` page carries.
const PAGE: usize = 100;

/// The most bytes a stored manifest may have.
const MANIFEST_LIMIT: u64 = 1 << 20;

/// Bytes read or written per chunk; the deadline is checked between chunks.
const CHUNK: usize = 1 << 16;

/// How often a removal or rename that another program holds open is retried.
const SHARING_RETRIES: u32 = 20;
const SHARING_PAUSE: Duration = Duration::from_millis(100);

/// The directory adapter for one configured target.
pub(crate) struct DirectoryAdapter<'a> {
    root: PathBuf,
    target_identity: ObjectId,
    deadline: Duration,
    free_space: &'a dyn Fn(&Path) -> io::Result<u64>,
    page_size: usize,
    /// Makes a write to the target fail as a full disk once this many bytes
    /// were written, so the cleanup after a failed write can be tested.
    #[cfg(test)]
    pub(crate) fail_after: Option<u64>,
}

impl<'a> DirectoryAdapter<'a> {
    /// An adapter for the directory `root`, the target with `target_identity`.
    /// `deadline` bounds each read of a stored file, checked between chunks.
    pub(crate) fn new(
        root: PathBuf,
        target_identity: ObjectId,
        deadline: Duration,
        free_space: &'a dyn Fn(&Path) -> io::Result<u64>,
    ) -> Self {
        Self {
            root,
            target_identity,
            deadline,
            free_space,
            page_size: PAGE,
            #[cfg(test)]
            fail_after: None,
        }
    }

    /// The same adapter with smaller `list` pages.
    #[cfg(test)]
    pub(crate) const fn with_page_size(mut self, page_size: usize) -> Self {
        self.page_size = page_size;
        self
    }

    /// [`Self::prepare_until`] with time enough for any test artifact.
    #[cfg(test)]
    pub(crate) fn prepare(
        &self,
        artifact: &Path,
        capture: &CaptureManifest,
        attempt_id: uuid::Uuid,
    ) -> Result<(Attempt, PathBuf), AdapterError> {
        self.prepare_until(
            artifact,
            capture,
            attempt_id,
            Instant::now() + Duration::from_secs(3600),
        )
    }

    /// Compresses a captured `artifact` into the local stage beside it and
    /// returns the attempt the caller records before `put`, with its complete
    /// manifest, and the stored file `put` takes. The compression stops at
    /// `until`, checked between reads, and then removes its stored file.
    pub(crate) fn prepare_until(
        &self,
        artifact: &Path,
        capture: &CaptureManifest,
        attempt_id: uuid::Uuid,
        until: Instant,
    ) -> Result<(Attempt, PathBuf), AdapterError> {
        let copy = copy_name(capture.capture_started_at, attempt_id);
        let stored = artifact.with_file_name(data_name(&copy));
        let io_at = |path: &Path| {
            let path = path.to_path_buf();
            move |source| AdapterError::Io { path, source }
        };
        let mut input = UntilReader {
            inner: BufReader::new(File::open(artifact).map_err(io_at(artifact))?),
            until,
        };
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stored)
            .map_err(io_at(&stored))?;
        let written = (|| {
            let mut encoder = GzEncoder::new(BufWriter::new(output), Compression::fast());
            io::copy(&mut input, &mut encoder)?;
            let file = encoder
                .finish()?
                .into_inner()
                .map_err(io::IntoInnerError::into_error)?;
            file.sync_all()
        })();
        let written = written.and_then(|()| {
            if Instant::now() >= until {
                Err(io::Error::new(io::ErrorKind::TimedOut, "past the deadline"))
            } else {
                Ok(())
            }
        });
        if let Err(source) = written {
            let error = if source.kind() == io::ErrorKind::TimedOut {
                AdapterError::PrepareDeadline {
                    path: stored.clone(),
                }
            } else {
                AdapterError::Io {
                    path: stored.clone(),
                    source,
                }
            };
            return Err(with_cleanup(error, &stored));
        }
        let stored_bytes = fs::metadata(&stored).map_err(io_at(&stored))?.len();
        let manifest = StoredManifest {
            format_version: STORED_FORMAT_VERSION,
            copy,
            target_identity: self.target_identity.clone(),
            encoding: Encoding::Gzip,
            stored_bytes,
            capture: capture.clone(),
        };
        let data_file = data_name(&manifest.copy);
        Ok((
            Attempt {
                id: attempt_id,
                temporary_data_file: temporary_name(&data_file),
                data_file,
                manifest,
            },
            stored,
        ))
    }

    /// Checks that `attempt` records exactly the file names its copy has, so
    /// no recorded name can lead anywhere else.
    fn check_attempt(&self, project: &ProjectId, attempt: &Attempt) -> Result<(), AdapterError> {
        self.check_manifest(project, &attempt.manifest)?;
        let data_file = data_name(&attempt.manifest.copy);
        if attempt.data_file != data_file
            || attempt.temporary_data_file != temporary_name(&data_file)
        {
            return Err(AdapterError::CopyInvalid {
                path: self.root.clone(),
                reason: "the recorded attempt names other files than its copy's".into(),
            });
        }
        Ok(())
    }

    fn project_dir(&self, project: &ProjectId) -> PathBuf {
        self.root.join(engram::project_digest(project))
    }

    /// The files one copy has, or had, at the target.
    fn files(&self, project: &ProjectId, copy: &str) -> CopyFiles {
        let dir = self.project_dir(project);
        let data = data_name(copy);
        let manifest = manifest_name(copy);
        CopyFiles {
            temporary_data: dir.join(temporary_name(&data)),
            temporary_manifest: dir.join(temporary_name(&manifest)),
            data: dir.join(data),
            manifest: dir.join(manifest),
        }
    }

    /// Whether the configured directory itself can be reached. A copy is
    /// only ever called missing from a reachable directory.
    fn reachable(&self) -> Result<(), String> {
        match fs::metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(format!("{} is not a directory", self.root.display())),
            Err(error) => Err(format!(
                "{} cannot be reached: {error}",
                self.root.display()
            )),
        }
    }

    /// Checks that `manifest` describes a copy of `project` this build reads.
    fn check_copy(
        &self,
        project: &ProjectId,
        manifest: &StoredManifest,
    ) -> Result<(), AdapterError> {
        let invalid = |reason: &str| AdapterError::CopyInvalid {
            path: self.root.clone(),
            reason: reason.into(),
        };
        if manifest.format_version != STORED_FORMAT_VERSION {
            return Err(invalid("the manifest's format version is not this build's"));
        }
        if !valid_copy_name(&manifest.copy) {
            return Err(invalid("the manifest names no valid copy"));
        }
        if manifest.capture.project_digest != engram::project_digest(project) {
            return Err(invalid("the manifest describes another project's copy"));
        }
        Ok(())
    }

    /// Checks as [`Self::check_copy`] does, and that the copy was made for
    /// this target's current identity, as every push-side request needs.
    fn check_manifest(
        &self,
        project: &ProjectId,
        manifest: &StoredManifest,
    ) -> Result<(), AdapterError> {
        self.check_copy(project, manifest)?;
        if manifest.target_identity != self.target_identity {
            return Err(AdapterError::CopyInvalid {
                path: self.root.clone(),
                reason: "the manifest was made for another target".into(),
            });
        }
        Ok(())
    }

    /// Reads the stored file in full and compares what it decodes to with the
    /// manifest.
    fn verify(&self, path: &Path, manifest: &StoredManifest) -> Verified {
        let started = Instant::now();
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Verified::Missing,
            Err(error) => return Verified::Unreadable(error.to_string()),
        };
        match file.metadata() {
            Ok(metadata) if metadata.len() != manifest.stored_bytes => {
                return Verified::Other(format!(
                    "the stored file has {} bytes, not the manifest's {}",
                    metadata.len(),
                    manifest.stored_bytes
                ));
            }
            Ok(_) => {}
            Err(error) => return Verified::Unreadable(error.to_string()),
        }
        match decode_bounded(
            BufReader::new(file),
            &mut io::sink(),
            manifest.capture.bytes,
            started,
            self.deadline,
        ) {
            Ok(sha256) if sha256 == manifest.capture.sha256 => Verified::Equal,
            Ok(_) => Verified::Other("it decodes to other bytes than the manifest's".into()),
            Err(Decode::Invalid(reason)) => Verified::Other(reason),
            Err(Decode::Read(error) | Decode::Write(error)) => {
                Verified::Unreadable(error.to_string())
            }
            Err(Decode::Deadline) => Verified::Deadline,
        }
    }

    /// Finishes or removes an attempt this home recorded and that a push was
    /// cut off in. Only the attempt's own files are touched. Only a push calls
    /// this; `confirm` never writes.
    pub(crate) fn reconcile(&self, project: &ProjectId, attempt: &Attempt) -> Reconciled {
        if let Err(reason) = self.reachable() {
            return Reconciled::Unknown { reason };
        }
        if self.check_attempt(project, attempt).is_err() {
            return Reconciled::Unknown {
                reason: "the recorded attempt does not belong to this target".into(),
            };
        }
        let files = self.files(project, &attempt.manifest.copy);
        // The attempt's own temporary files go first: one a cut-off put left
        // behind must not stand in the way of finishing the copy.
        for temporary in [&files.temporary_data, &files.temporary_manifest] {
            if let Err(error) = remove_with_retry(temporary) {
                return Reconciled::Unknown {
                    reason: error.to_string(),
                };
            }
        }
        let exists = |path: &Path| path.try_exists();
        let (data, manifest) = match (exists(&files.data), exists(&files.manifest)) {
            (Ok(data), Ok(manifest)) => (data, manifest),
            (Err(error), _) | (_, Err(error)) => {
                return Reconciled::Unknown {
                    reason: error.to_string(),
                };
            }
        };
        match (data, manifest) {
            (true, true) => match read_manifest(&files.manifest) {
                Ok(Some(stored)) if stored == attempt.manifest => Reconciled::Complete,
                Ok(_) | Err(ManifestRead::Unusable(_)) => Reconciled::Unknown {
                    reason: "another manifest stands under the attempt's name".into(),
                },
                Err(ManifestRead::Unreadable(reason)) => Reconciled::Unknown { reason },
            },
            (false, false) => Reconciled::Absent,
            (false, true) => match read_manifest(&files.manifest) {
                Ok(Some(stored)) if stored == attempt.manifest => {
                    if let Err(error) = remove_with_retry(&files.manifest) {
                        return Reconciled::Unknown {
                            reason: error.to_string(),
                        };
                    }
                    Reconciled::Removed
                }
                Ok(_) | Err(ManifestRead::Unusable(_)) => Reconciled::Unknown {
                    reason: "another manifest stands under the attempt's name".into(),
                },
                Err(ManifestRead::Unreadable(reason)) => Reconciled::Unknown { reason },
            },
            (true, false) => match self.verify(&files.data, &attempt.manifest) {
                Verified::Equal => {
                    match publish_bytes(
                        &files.temporary_manifest,
                        &files.manifest,
                        &manifest_bytes(&attempt.manifest),
                        None,
                    ) {
                        Ok(()) => Reconciled::Completed,
                        Err(error) => Reconciled::Unknown {
                            reason: error.to_string(),
                        },
                    }
                }
                Verified::Missing | Verified::Other(_) => {
                    if let Err(error) = remove_with_retry(&files.data) {
                        return Reconciled::Unknown {
                            reason: error.to_string(),
                        };
                    }
                    Reconciled::Removed
                }
                Verified::Unreadable(reason) => Reconciled::Unknown { reason },
                Verified::Deadline => Reconciled::Unknown {
                    reason: "the read passed its deadline".into(),
                },
            },
        }
    }

    /// Removes every file of an attempt this home recorded and then found
    /// abandoned, and nothing else.
    ///
    /// # Errors
    ///
    /// Returns the first removal that failed.
    pub(crate) fn remove_attempt(
        &self,
        project: &ProjectId,
        attempt: &Attempt,
    ) -> Result<(), AdapterError> {
        self.check_attempt(project, attempt)?;
        self.reachable()
            .map_err(|reason| AdapterError::Unreachable {
                path: self.root.clone(),
                reason,
            })?;
        let files = self.files(project, &attempt.manifest.copy);
        // The manifest goes before its data, so no manifest is ever left
        // standing without the copy it describes.
        for path in [
            &files.temporary_data,
            &files.temporary_manifest,
            &files.manifest,
            &files.data,
        ] {
            remove_with_retry(path).map_err(|source| AdapterError::Io {
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }
}

impl BackupAdapter for DirectoryAdapter<'_> {
    fn put(
        &self,
        project: &ProjectId,
        attempt: &Attempt,
        artifact: &Path,
    ) -> Result<BackupReceipt, AdapterError> {
        let manifest = &attempt.manifest;
        self.check_attempt(project, attempt)?;
        // A configured directory that is gone is never recreated: the copy
        // would land wherever the path now leads, likely on this machine.
        self.reachable()
            .map_err(|reason| AdapterError::Unreachable {
                path: self.root.clone(),
                reason,
            })?;
        let files = self.files(project, &manifest.copy);
        let manifest_text = manifest_bytes(manifest);
        let required = manifest
            .stored_bytes
            .saturating_add(manifest_text.len() as u64);
        let probe = nearest_existing_ancestor(&files.data);
        let available =
            (self.free_space)(&probe).map_err(|source| AdapterError::TargetSpaceUnknown {
                path: probe.clone(),
                source,
            })?;
        if available < required {
            return Err(AdapterError::TargetNoSpace {
                path: probe,
                required,
                available,
            });
        }
        let directory = self.project_dir(project);
        match fs::create_dir(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(AdapterError::Io {
                    path: directory,
                    source,
                });
            }
        }

        #[cfg(test)]
        let fail_after = self.fail_after;
        #[cfg(not(test))]
        let fail_after = None;
        let source = File::open(artifact).map_err(|source| AdapterError::Io {
            path: artifact.to_path_buf(),
            source,
        })?;
        publish(&files.temporary_data, &files.data, source, fail_after)
            .map_err(Publish::into_adapter_error)?;

        // Read back what landed under the final name before anything says it
        // is there.
        match self.verify(&files.data, manifest) {
            Verified::Equal => {}
            Verified::Deadline => {
                let error = AdapterError::Deadline {
                    path: files.data.clone(),
                    deadline: self.deadline,
                };
                return Err(with_cleanup(error, &files.data));
            }
            failed => {
                let error = AdapterError::CopyInvalid {
                    path: files.data.clone(),
                    reason: format!("the read-back did not match: {}", failed.reason()),
                };
                return Err(with_cleanup(error, &files.data));
            }
        }
        publish_bytes(
            &files.temporary_manifest,
            &files.manifest,
            &manifest_text,
            fail_after,
        )
        .map_err(Publish::into_adapter_error)?;
        Ok(BackupReceipt {
            sha256: manifest.capture.sha256.clone(),
            target_identity: self.target_identity.clone(),
            at: Utc::now(),
            acknowledgement: Acknowledgement::ReadBack,
            off_host: OffHost::Asserted,
            manifest: manifest.clone(),
        })
    }

    fn confirm(&self, project: &ProjectId, manifest: &StoredManifest) -> Confirmation {
        if let Err(reason) = self.reachable() {
            return Confirmation::Unknown { reason };
        }
        if let Err(error) = self.check_manifest(project, manifest) {
            return Confirmation::Missing {
                reason: error.to_string(),
            };
        }
        let files = self.files(project, &manifest.copy);
        match read_manifest(&files.manifest) {
            Ok(Some(stored)) if &stored == manifest => {}
            Ok(Some(_)) => {
                return Confirmation::Missing {
                    reason: "another manifest stands under the copy's name".into(),
                };
            }
            Ok(None) => {
                return Confirmation::Missing {
                    reason: "the copy has no manifest at the target".into(),
                };
            }
            Err(ManifestRead::Unusable(reason)) => return Confirmation::Missing { reason },
            Err(ManifestRead::Unreadable(reason)) => return Confirmation::Unknown { reason },
        }
        match self.verify(&files.data, manifest) {
            Verified::Equal => Confirmation::Confirmed,
            Verified::Missing => Confirmation::Missing {
                reason: "the stored file is not at the target".into(),
            },
            Verified::Other(reason) => Confirmation::Missing {
                reason: format!("the stored file holds other bytes: {reason}"),
            },
            Verified::Unreadable(reason) => Confirmation::Unknown { reason },
            Verified::Deadline => Confirmation::Unknown {
                reason: format!("the read passed its deadline of {:?}", self.deadline),
            },
        }
    }

    fn list(
        &self,
        project: &ProjectId,
        cursor: Option<&str>,
    ) -> Result<ManifestPage, AdapterError> {
        let directory = self.project_dir(project);
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return match self.reachable() {
                    Ok(()) => Ok(ManifestPage::default()),
                    Err(reason) => Err(AdapterError::Unreachable {
                        path: self.root.clone(),
                        reason,
                    }),
                };
            }
            Err(source) => {
                return Err(AdapterError::Io {
                    path: directory,
                    source,
                });
            }
        };
        let mut copies = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| AdapterError::Io {
                path: directory.clone(),
                source,
            })?;
            let name = entry.file_name();
            let Some(copy) = name
                .to_str()
                .and_then(|name| name.strip_suffix(MANIFEST_SUFFIX))
            else {
                continue;
            };
            if valid_copy_name(copy) && cursor.is_none_or(|cursor| copy > cursor) {
                copies.push(copy.to_owned());
            }
        }
        copies.sort();
        let more = copies.len() > self.page_size;
        copies.truncate(self.page_size);
        let mut page = ManifestPage {
            next: more.then(|| copies.last().cloned()).flatten(),
            ..ManifestPage::default()
        };
        for copy in copies {
            match read_manifest(&directory.join(manifest_name(&copy))) {
                Ok(Some(manifest))
                    if manifest.copy == copy
                        && manifest.capture.project_digest == engram::project_digest(project) =>
                {
                    page.manifests.push(manifest);
                }
                _ => page.unreadable.push(manifest_name(&copy)),
            }
        }
        Ok(page)
    }

    fn get(
        &self,
        project: &ProjectId,
        manifest: &StoredManifest,
        destination: &Path,
    ) -> Result<(), AdapterError> {
        // A copy made for an earlier identity of the target is still this
        // project's copy: a restore on a new machine configures the target
        // again, which gives it a new identity.
        self.check_copy(project, manifest)?;
        let started = Instant::now();
        let probe = nearest_existing_ancestor(destination);
        let available =
            (self.free_space)(&probe).map_err(|source| AdapterError::LocalSpaceUnknown {
                path: probe.clone(),
                source,
            })?;
        if available < manifest.capture.bytes {
            return Err(AdapterError::LocalNoSpace {
                path: probe,
                required: manifest.capture.bytes,
                available,
            });
        }
        self.reachable()
            .map_err(|reason| AdapterError::Unreachable {
                path: self.root.clone(),
                reason,
            })?;
        let files = self.files(project, &manifest.copy);
        let stored = File::open(&files.data).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                AdapterError::CopyMissing {
                    path: files.data.clone(),
                }
            } else {
                AdapterError::Io {
                    path: files.data.clone(),
                    source,
                }
            }
        })?;
        let output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(destination)
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AlreadyExists {
                    AdapterError::Exists {
                        path: destination.to_path_buf(),
                    }
                } else {
                    AdapterError::Io {
                        path: destination.to_path_buf(),
                        source,
                    }
                }
            })?;
        // From here the destination is this call's own. Its writer is closed
        // before it is removed on any failure.
        let written = (|| {
            let mut writer = BufWriter::new(output);
            let sha256 = decode_bounded(
                BufReader::new(stored),
                &mut writer,
                manifest.capture.bytes,
                started,
                self.deadline,
            )
            .map_err(|failure| match failure {
                Decode::Invalid(reason) => AdapterError::CopyInvalid {
                    path: files.data.clone(),
                    reason,
                },
                Decode::Read(source) => AdapterError::Io {
                    path: files.data.clone(),
                    source,
                },
                Decode::Write(source) => AdapterError::Io {
                    path: destination.to_path_buf(),
                    source,
                },
                Decode::Deadline => AdapterError::Deadline {
                    path: files.data.clone(),
                    deadline: self.deadline,
                },
            })?;
            if sha256 != manifest.capture.sha256 {
                return Err(AdapterError::CopyInvalid {
                    path: files.data.clone(),
                    reason: "it decodes to other bytes than the manifest's".into(),
                });
            }
            writer
                .into_inner()
                .map_err(io::IntoInnerError::into_error)
                .and_then(|file| file.sync_all())
                .map_err(|source| AdapterError::Io {
                    path: destination.to_path_buf(),
                    source,
                })
        })();
        written.map_err(|error| with_cleanup(error, destination))
    }
}

/// A reader that fails as timed out once `until` has passed, checked before
/// each read.
struct UntilReader<R> {
    inner: R,
    until: Instant,
}

impl<R: Read> Read for UntilReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.until {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "past the deadline"));
        }
        self.inner.read(buffer)
    }
}

/// The four names one copy can have at the target.
struct CopyFiles {
    temporary_data: PathBuf,
    temporary_manifest: PathBuf,
    data: PathBuf,
    manifest: PathBuf,
}

/// What reading a stored file in full found.
enum Verified {
    Equal,
    Missing,
    Other(String),
    Unreadable(String),
    Deadline,
}

impl Verified {
    fn reason(&self) -> String {
        match self {
            Self::Equal => "it matched".into(),
            Self::Missing => "the file is gone".into(),
            Self::Other(reason) | Self::Unreadable(reason) => reason.clone(),
            Self::Deadline => "the read passed its deadline".into(),
        }
    }
}

/// Why a bounded decode stopped.
#[derive(Debug)]
pub(crate) enum Decode {
    /// The stream is not a gzip of exactly the declared length.
    Invalid(String),
    Read(io::Error),
    Write(io::Error),
    Deadline,
}

/// Decodes the gzip stream `stored` into `output`, writing no more than
/// `declared` bytes and reading at most one byte beyond them, in memory, to
/// detect excess. Returns the SHA-256 of the written bytes.
pub(crate) fn decode_bounded(
    stored: impl BufRead,
    output: &mut dyn Write,
    declared: u64,
    started: Instant,
    deadline: Duration,
) -> Result<String, Decode> {
    let mut decoder = GzDecoder::new(stored);
    let mut digest = Sha256::new();
    let mut written = 0_u64;
    let mut buffer = vec![0_u8; CHUNK];
    loop {
        if started.elapsed() >= deadline {
            return Err(Decode::Deadline);
        }
        let remaining = declared - written;
        if remaining == 0 {
            // The end of the stream is where the gzip trailer is checked.
            let mut probe = [0_u8; 1];
            match read_some(&mut decoder, &mut probe) {
                Ok(0) => {}
                Ok(_) => {
                    return Err(Decode::Invalid(format!(
                        "it decodes to more than the manifest's {declared} bytes"
                    )));
                }
                Err(error) => return Err(classify(error)),
            }
            // The decoder stops after one gzip member: anything after it,
            // another member or stray bytes, is not the copy.
            let mut rest = decoder.into_inner();
            match rest.fill_buf() {
                Ok([]) => {}
                Ok(_) => {
                    return Err(Decode::Invalid(
                        "the stored file has more after the gzip stream".into(),
                    ));
                }
                Err(error) => return Err(Decode::Read(error)),
            }
            // A read that ended after the deadline does not count, however
            // it ended: this is checked after the last read of all.
            if started.elapsed() >= deadline {
                return Err(Decode::Deadline);
            }
            return Ok(format!("{:x}", digest.finalize()));
        }
        let want = usize::try_from(remaining).map_or(CHUNK, |remaining| remaining.min(CHUNK));
        match read_some(&mut decoder, &mut buffer[..want]) {
            Ok(0) => {
                return Err(Decode::Invalid(format!(
                    "it decodes to {written} bytes, not the manifest's {declared}"
                )));
            }
            Ok(read) => {
                output.write_all(&buffer[..read]).map_err(Decode::Write)?;
                digest.update(&buffer[..read]);
                written += read as u64;
            }
            Err(error) => return Err(classify(error)),
        }
    }
}

fn read_some(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        match reader.read(buffer) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            result => return result,
        }
    }
}

/// A corrupt stream is a copy holding other bytes; anything else is a read
/// that could not finish.
fn classify(error: io::Error) -> Decode {
    match error.kind() {
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => {
            Decode::Invalid(format!("it is not a gzip of the copy: {error}"))
        }
        _ => Decode::Read(error),
    }
}

/// Why a stored manifest could not be used.
enum ManifestRead {
    /// It is there but is not a manifest this build reads.
    Unusable(String),
    /// It could not be read at all.
    Unreadable(String),
}

fn read_manifest(path: &Path) -> Result<Option<StoredManifest>, ManifestRead> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ManifestRead::Unreadable(error.to_string())),
    };
    // A manifest is small; anything larger is not one, and is not loaded.
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes) {
        return Err(ManifestRead::Unreadable(error.to_string()));
    }
    if bytes.len() as u64 > MANIFEST_LIMIT {
        return Err(ManifestRead::Unusable(
            "the manifest is larger than any manifest is".into(),
        ));
    }
    let manifest: StoredManifest = serde_json::from_slice(&bytes)
        .map_err(|error| ManifestRead::Unusable(format!("the manifest cannot be used: {error}")))?;
    if manifest.format_version != STORED_FORMAT_VERSION {
        return Err(ManifestRead::Unusable(
            "the manifest's format version is not this build's".into(),
        ));
    }
    Ok(Some(manifest))
}

fn manifest_bytes(manifest: &StoredManifest) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(manifest).expect("a stored manifest serializes");
    bytes.push(b'\n');
    bytes
}

/// Why publishing a file failed.
enum Publish {
    /// Something already stands under the final name; it is left as it is.
    Exists(PathBuf),
    /// The target filled up while the file was written.
    Full(PathBuf, io::Error),
    Io(PathBuf, io::Error),
    /// The failure, and the temporary file that could not be removed after it.
    Leftover(Box<Publish>, PathBuf, io::Error),
}

impl std::fmt::Display for Publish {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(path) => write!(formatter, "{} already exists", path.display()),
            Self::Full(path, error) | Self::Io(path, error) => {
                write!(formatter, "{}: {error}", path.display())
            }
            Self::Leftover(error, path, source) => write!(
                formatter,
                "{error}; {} could not be removed: {source}",
                path.display()
            ),
        }
    }
}

impl Publish {
    fn into_adapter_error(self) -> AdapterError {
        match self {
            Self::Exists(path) => AdapterError::Exists { path },
            Self::Full(path, source) => AdapterError::TargetFull { path, source },
            Self::Io(path, source) => AdapterError::Io { path, source },
            Self::Leftover(error, path, source) => AdapterError::Leftover {
                error: Box::new(error.into_adapter_error()),
                path,
                source,
            },
        }
    }
}

/// Writes `source` to a new `temporary` file, syncs and closes it, then moves
/// it to `final_path` without replacing anything there. On any failure the
/// temporary file, which this call created, is removed; nothing else is.
fn publish(
    temporary: &Path,
    final_path: &Path,
    mut source: impl Read,
    fail_after: Option<u64>,
) -> Result<(), Publish> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .map_err(|error| Publish::Io(temporary.to_path_buf(), error))?;
    let cleanup = |error: Publish| match remove_with_retry(temporary) {
        Ok(()) => Err(error),
        Err(source) => Err(Publish::Leftover(
            Box::new(error),
            temporary.to_path_buf(),
            source,
        )),
    };
    let mut writer = LimitedWriter {
        inner: BufWriter::new(file),
        written: 0,
        fail_after,
    };
    if let Err(error) = io::copy(&mut source, &mut writer) {
        return cleanup(classify_write(temporary, error));
    }
    let file = match writer.inner.into_inner() {
        Ok(file) => file,
        Err(error) => return cleanup(classify_write(temporary, error.into_error())),
    };
    if let Err(error) = file.sync_all() {
        return cleanup(classify_write(temporary, error));
    }
    drop(file);
    // The move takes the long-path forms, which a deep target needs: the
    // system call behind it, unlike std's file operations, is not given them
    // otherwise.
    let (moved, destination) = match (verbatim(temporary), verbatim(final_path)) {
        (Ok(moved), Ok(destination)) => (moved, destination),
        (Err(error), _) | (_, Err(error)) => {
            return cleanup(Publish::Io(temporary.to_path_buf(), error));
        }
    };
    let mut pending = match tempfile::TempPath::try_from_path(moved) {
        Ok(pending) => pending,
        Err(error) => return cleanup(Publish::Io(temporary.to_path_buf(), error)),
    };
    for attempt in 0..=SHARING_RETRIES {
        match pending.persist_noclobber(&destination) {
            Ok(()) => return Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                // Dropping the returned path removes only the temporary file.
                drop(error.path);
                return Err(Publish::Exists(final_path.to_path_buf()));
            }
            Err(error) if is_sharing_violation(&error.error) && attempt < SHARING_RETRIES => {
                pending = error.path;
                std::thread::sleep(SHARING_PAUSE);
            }
            Err(error) => {
                drop(error.path);
                return Err(Publish::Io(final_path.to_path_buf(), error.error));
            }
        }
    }
    unreachable!("the last attempt returns")
}

/// `path` in the Windows long-path form (`\\?\C:\…`, `\\?\UNC\server\…`),
/// absolute and normalized; elsewhere the absolute path.
pub(super) fn verbatim(path: &Path) -> io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if !cfg!(windows) {
        return Ok(absolute);
    }
    let text = absolute.to_string_lossy();
    if text.starts_with(r"\\?\") {
        return Ok(absolute);
    }
    if let Some(device) = text.strip_prefix(r"\\.\") {
        return Ok(PathBuf::from(format!(r"\\?\{device}")));
    }
    Ok(PathBuf::from(match text.strip_prefix(r"\\") {
        Some(share) => format!(r"\\?\UNC\{share}"),
        None => format!(r"\\?\{text}"),
    }))
}

fn publish_bytes(
    temporary: &Path,
    final_path: &Path,
    bytes: &[u8],
    fail_after: Option<u64>,
) -> Result<(), Publish> {
    publish(temporary, final_path, bytes, fail_after)
}

fn classify_write(path: &Path, error: io::Error) -> Publish {
    if error.kind() == io::ErrorKind::StorageFull {
        Publish::Full(path.to_path_buf(), error)
    } else {
        Publish::Io(path.to_path_buf(), error)
    }
}

/// A writer that reports a full disk once `fail_after` bytes were written.
struct LimitedWriter<W> {
    inner: W,
    written: u64,
    fail_after: Option<u64>,
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(limit) = self.fail_after
            && self.written + buffer.len() as u64 > limit
        {
            return Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "the target is full",
            ));
        }
        let written = self.inner.write(buffer)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// Removes `path`, a file this call created, after `error`; a removal that
/// fails is reported beside the error rather than lost.
pub(super) fn with_cleanup(error: AdapterError, path: &Path) -> AdapterError {
    match remove_with_retry(path) {
        Ok(()) => error,
        Err(source) => AdapterError::Leftover {
            error: Box::new(error),
            path: path.to_path_buf(),
            source,
        },
    }
}

/// Removes `path`, waiting out another program that holds it open, such as
/// a sync client or a scanner. A missing file is fine.
fn remove_with_retry(path: &Path) -> io::Result<()> {
    let mut attempt = 0;
    loop {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) if is_sharing_violation(&error) && attempt < SHARING_RETRIES => {
                attempt += 1;
                std::thread::sleep(SHARING_PAUSE);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Windows reports a file another process holds open as a sharing or lock
/// violation.
fn is_sharing_violation(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(32 | 33)) && cfg!(windows)
}

const DATA_SUFFIX: &str = ".db.gz";
const MANIFEST_SUFFIX: &str = ".manifest.json";

/// The name a copy carries at the target: its capture time, readable, and
/// the full attempt id, which no other attempt shares.
pub(crate) fn copy_name(captured: DateTime<Utc>, attempt: uuid::Uuid) -> String {
    format!("{}-{attempt}", captured.format("%Y%m%dT%H%M%SZ"))
}

fn data_name(copy: &str) -> String {
    format!("{copy}{DATA_SUFFIX}")
}

fn manifest_name(copy: &str) -> String {
    format!("{copy}{MANIFEST_SUFFIX}")
}

fn temporary_name(name: &str) -> String {
    format!(".{name}.tmp")
}

/// A copy name holds only ASCII letters, digits and hyphens, so it can never
/// leave its directory.
fn valid_copy_name(copy: &str) -> bool {
    !copy.is_empty()
        && copy.len() <= 96
        && copy
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    path.ancestors()
        .skip(1)
        .find(|ancestor| ancestor.exists())
        .unwrap_or(path)
        .to_path_buf()
}
