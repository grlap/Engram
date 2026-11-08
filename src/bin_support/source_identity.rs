//! Decode the saved attempt object from one bounded read of one open handle.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use engram::source::{MAX_JSON_BYTES, SourceId};

pub(crate) fn read_optional(path: Option<&Path>) -> Result<Option<SourceId>> {
    path.map(read).transpose()
}

fn read(path: &Path) -> Result<SourceId> {
    decode(open(path)?)
}

fn open(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ: keep replacement, deletion and writes excluded while
        // the saved object is consumed. The producer owns pathname immutability.
        options.share_mode(1);
    }
    options.open(path).context("cannot open source_id file")
}

fn decode(reader: impl Read) -> Result<SourceId> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_JSON_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("cannot read source_id file")?;
    if bytes.len() > MAX_JSON_BYTES {
        bail!("source_id file exceeds the owner byte limit; recapture the complete identity");
    }
    let source: SourceId = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "source_id file is invalid: {:?} at line {} column {}",
            error.classify(),
            error.line(),
            error.column()
        )
    })?;
    source
        .validate()
        .map_err(|reason| anyhow::anyhow!("source_id file is invalid: {reason}"))?;
    if serde_json::to_vec(&source)?.len() > MAX_JSON_BYTES {
        bail!("source_id object exceeds the owner byte limit; recapture the complete identity");
    }
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_decode_preserves_identity_and_never_echoes_invalid_values() {
        let source = SourceId {
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            changes: vec![],
            submodules: vec![],
        };
        let bytes = serde_json::to_vec(&source).unwrap();
        assert_eq!(decode(bytes.as_slice()).unwrap(), source);
        for bytes in [
            b"{\"commit\":\"private-source-value\"".to_vec(),
            b"\"private-source-value\"".to_vec(),
            vec![b' '; MAX_JSON_BYTES + 1],
        ] {
            let error = decode(bytes.as_slice()).unwrap_err().to_string();
            assert!(!error.contains("private-source-value"));
        }
    }

    #[test]
    fn a_real_file_is_read_and_missing_files_report_no_payload_or_path() {
        let directory = crate::test_support::temp_home().unwrap();
        let path = directory.path().join("private-source-path.json");
        let source = SourceId {
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            changes: vec![],
            submodules: vec![],
        };
        std::fs::write(&path, serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(read_optional(Some(&path)).unwrap(), Some(source));
        assert_eq!(read_optional(None).unwrap(), None);
        std::fs::remove_file(&path).unwrap();
        let error = read(&path).unwrap_err().to_string();
        assert_eq!(error, "cannot open source_id file");
    }

    #[cfg(windows)]
    #[test]
    fn an_open_source_file_allows_readers_and_excludes_writes_and_deletion() {
        let directory = crate::test_support::temp_home().unwrap();
        let path = directory.path().join("saved-source.json");
        std::fs::write(&path, b"{}").unwrap();
        let held = open(&path).unwrap();
        let concurrent_reader = open(&path).unwrap();
        assert!(OpenOptions::new().write(true).open(&path).is_err());
        assert!(std::fs::remove_file(&path).is_err());
        drop(concurrent_reader);
        drop(held);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_bounded_read_does_not_consume_beyond_the_sentinel() {
        let bytes = vec![b' '; MAX_JSON_BYTES + 100];
        let mut reader = std::io::Cursor::new(bytes);
        assert!(decode(&mut reader).is_err());
        assert_eq!(reader.position(), (MAX_JSON_BYTES + 1) as u64);
    }
}
