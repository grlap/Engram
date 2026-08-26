# Security and Trust Review

Focus on secrets, asserted identity, local data exposure, and irreversible
publication.

## Check

- Actor ids and authority instructions supplied by tools/skills are labeled
  asserted unless a real authenticator or signature verifies them.
- A no-op redactor is visible in diagnostics and never described as compliance
  protection.
- Secrets and credentials are rejected or replaced with vault references
  before persistence, export, logs, retrieval packets, or reports.
- Report publication is an explicit authorized action; local distillation does
  not imply external mutation.
- File/database paths are normalized and constrained as documented; no shell
  interpolation of user-controlled values.
- Error messages and telemetry avoid report bodies, memory bodies, credentials,
  and sensitive ticket content.
- Logical tombstones are not presented as guaranteed physical erasure across
  backups, exports, tracker history, or future shared stores.
- SQLite permissions and backup/export defaults minimize accidental disclosure.

Do not claim local-only deployment eliminates malicious local inputs or unsafe
exports.
