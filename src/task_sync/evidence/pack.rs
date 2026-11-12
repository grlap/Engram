use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

use crate::{ObjectId, ProjectId};

use super::{EvidenceError, RecordEnvelope, limits, record::WireRecord};

const FORMAT: &str = "engram-evidence-v1";

/// An immutable transport container. Its prepared id is retained on every retry.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct EvidencePack {
    // Sorted keys make serde's streaming encoding canonical without buffering
    // a potentially oversized whole pack inside a sorting serializer.
    format: String,
    pack_id: ObjectId,
    project_id: ProjectId,
    publishing_origin: ObjectId,
    records: Vec<RecordEnvelope>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePack {
    format: String,
    pack_id: ObjectId,
    project_id: ProjectId,
    publishing_origin: ObjectId,
    records: Vec<WireRecord>,
}

impl EvidencePack {
    /// Freeze a deterministic member order under a previously prepared UUID.
    ///
    /// # Errors
    /// Refuses duplicate identities, conflicting bodies and profile size bounds.
    pub fn prepare(
        pack_id: ObjectId,
        project_id: ProjectId,
        publishing_origin: ObjectId,
        mut records: Vec<RecordEnvelope>,
    ) -> Result<Self, EvidenceError> {
        if pack_id.as_str().len() != 32 {
            return Err(EvidenceError::InvalidPack("pack id must be a minted UUID"));
        }
        if publishing_origin.as_str().len() != 32 {
            return Err(EvidenceError::InvalidPack(
                "publishing origin must be a minted UUID",
            ));
        }
        if project_id.0.trim().is_empty() {
            return Err(EvidenceError::InvalidPack("empty project id"));
        }
        limits::enforce(
            limits::LimitKind::PackTooLarge,
            Some(&pack_id),
            limits::Dimension::Objects,
            records.len(),
            limits::MAX_PACK_RECORDS,
        )?;
        records.sort_by(|left, right| left.object_id().cmp(right.object_id()));
        for pair in records.windows(2) {
            if pair[0].object_id() == pair[1].object_id() {
                if !pair[0].same_record(&pair[1]) {
                    return Err(EvidenceError::RecordConflict(pair[0].object_id().clone()));
                }
                return Err(EvidenceError::InvalidPack("duplicate record id"));
            }
        }
        let pack = Self {
            format: FORMAT.into(),
            pack_id,
            project_id,
            publishing_origin,
            records,
        };
        // The counting writer enforces the exact encoded size before retaining it.
        pack.encode(&mut std::io::sink())?;
        Ok(pack)
    }

    pub fn pack_id(&self) -> &ObjectId {
        &self.pack_id
    }

    pub fn project_id(&self) -> &ProjectId {
        &self.project_id
    }

    pub fn publishing_origin(&self) -> &ObjectId {
        &self.publishing_origin
    }

    pub fn records(&self) -> &[RecordEnvelope] {
        &self.records
    }

    /// The stable path is relative to the separately configured data worktree.
    pub fn relative_path(&self) -> String {
        super::path::pack(&self.publishing_origin, &self.pack_id)
    }

    /// Encode the immutable pack with deterministic LF termination.
    ///
    /// # Errors
    /// Refuses an oversized encoding or an output failure.
    pub fn to_bytes(&self) -> Result<Vec<u8>, EvidenceError> {
        let mut bytes = Vec::new();
        self.encode(&mut bytes)?;
        Ok(bytes)
    }

    fn encode(&self, output: &mut impl Write) -> Result<(), EvidenceError> {
        let mut bounded = BoundedWriter {
            output,
            written: 0,
            overflow: None,
        };
        let encoded = serde_json::to_writer(&mut bounded, self);
        if let Some(actual) = bounded.overflow {
            return Err(limits::Oversize {
                kind: limits::LimitKind::PackTooLarge,
                subject: Some(self.pack_id.clone()),
                dimension: limits::Dimension::Bytes,
                actual,
                limit: limits::MAX_PACK_BYTES,
            }
            .into());
        }
        encoded?;
        bounded.write_all(b"\n").map_err(|error| {
            bounded.overflow.map_or_else(
                || EvidenceError::Io(error),
                |actual| {
                    EvidenceError::Oversize(limits::Oversize {
                        kind: limits::LimitKind::PackTooLarge,
                        subject: Some(self.pack_id.clone()),
                        dimension: limits::Dimension::Bytes,
                        actual,
                        limit: limits::MAX_PACK_BYTES,
                    })
                },
            )
        })?;
        Ok(())
    }

    /// Read one bounded pack. A failed read publishes no partially decoded records.
    ///
    /// # Errors
    /// Refuses malformed or oversized inputs, noncanonical encodings and conflicts.
    pub fn read(reader: impl Read) -> Result<Self, EvidenceError> {
        let mut bytes = Vec::new();
        reader
            .take((limits::MAX_PACK_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        limits::enforce(
            limits::LimitKind::PackTooLarge,
            None,
            limits::Dimension::Bytes,
            bytes.len(),
            limits::MAX_PACK_BYTES,
        )?;
        let wire: WirePack = serde_json::from_slice(&bytes)?;
        if wire.format != FORMAT {
            return Err(EvidenceError::InvalidPack("unsupported format"));
        }
        limits::enforce(
            limits::LimitKind::PackTooLarge,
            Some(&wire.pack_id),
            limits::Dimension::Objects,
            wire.records.len(),
            limits::MAX_PACK_RECORDS,
        )?;
        let records = wire
            .records
            .into_iter()
            .map(RecordEnvelope::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let pack = Self::prepare(
            wire.pack_id,
            wire.project_id,
            wire.publishing_origin,
            records,
        )?;
        if pack.to_bytes()? != bytes {
            return Err(EvidenceError::InvalidPack(
                "noncanonical encoding or record order",
            ));
        }
        Ok(pack)
    }
}

struct BoundedWriter<'a, W> {
    output: &'a mut W,
    written: usize,
    overflow: Option<usize>,
}

impl<W: Write> Write for BoundedWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.written.saturating_add(bytes.len());
        if next > limits::MAX_PACK_BYTES {
            self.overflow = Some(next);
            return Err(std::io::Error::other("evidence pack size limit"));
        }
        let count = self.output.write(bytes)?;
        self.written += count;
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
