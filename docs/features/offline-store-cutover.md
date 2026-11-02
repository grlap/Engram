# Offline store cutover

The Windows cutover command prepares every store before replacing any active
store file or the installed binary. It supports the explicit conversion in
[full store migration](full-store-migration.md). It does not stop consumers,
restart TermAl, approve a migration, or enable execution policy.

The operator runs it only after migration approval, with TermAl closed and all
Engram consumers stopped. The coordinator must confirm that other hosts do not
write these stores. Keep consumers stopped until the command completes or
recovery finishes. A process census cannot prevent a new process from starting.

## Paired TermAl build

The paired TermAl build comes first. Before this build lands, is installed or
is cut over, the TermAl build that will drive it must already:

- start the host channel only as `engram control --work-session`. The flag
  is mandatory, and there is no fallback: no other `engram control` form is
  served, and a host must not retry without the flag;
- never send `routing_token`: every work-session request is strict, so a
  frame that carries it is refused as `invalid_request`;
- not require any field that `readiness`, `doctor`, `control-policy` or
  restore output no longer carries, such as the removed turn, grant and
  assurance fields.

An older TermAl build fails against this build. This page does not record
whether such a TermAl build exists or is installed: the coordinators confirm
that from TermAl's own evidence before landing.

The integration owner supplies a private JSON plan and two immutable release
artifacts: the existing build for export and the final gated build for import.
The plan records the executable SHA-256, build fingerprint, source revision and
schema reference of each artifact. Those values must come from the actual
artifacts and readiness output. The release profile must be supported by the
build evidence; a plan field alone does not prove it.

## Rehearse on copies

Rehearsal is the default. Its source home must be beneath this repository's
`target/` and contain coherent copies of all stores, including existing WAL,
SHM and journal files. The command clones them into a fresh rehearsal home and
installs the old binary there before exercising the same activation and
rollback machinery. Operational files are never rehearsal destinations.

```powershell
pwsh -NoProfile -File scripts/eo-cutover.ps1 -Plan target/cutover/rehearsal.json
```

The plan has this shape; bracketed values are placeholders:

```json
{
  "format": "engram-cutover-plan-1",
  "home": "C:/github/Personal/Engram/target/cutover/copied-home",
  "install_path": "C:/Users/grzeg/.cargo/bin/engram.exe",
  "installed_sha256": "<installed executable SHA-256>",
  "old": {
    "binary": "C:/github/Personal/Engram/target/cutover/old/engram.exe",
    "profile": "release",
    "sha256": "<executable SHA-256>",
    "build_fingerprint": "<build fingerprint>",
    "source_revision": "<full clean commit>",
    "schema_reference": "<schema reference>"
  },
  "final": {
    "binary": "C:/github/Personal/Engram/target/cutover/final/engram.exe",
    "profile": "release",
    "sha256": "<executable SHA-256>",
    "build_fingerprint": "<build fingerprint>",
    "source_revision": "<full clean commit>",
    "schema_reference": "<schema reference>"
  },
  "projects": [
    {"name": "Engram", "id": "github.com/grlap/Engram", "host_path_policy": "case_fold"}
  ]
}
```

Include every store in the selected home, with a distinct public alias. The
example's single project is illustrative; the actual manifest must pass an
exact store census. `host_path_policy` must match each store. Project ids select
the normal home-relative store directory; they never identify records.

The default short working directory is `target/e-<random>`. An explicit
`-WorkDirectory` must be fresh and beneath this repository's `target/`. Long
Windows paths are refused before import because import creates a temporary
store beside its destination.

## Approved live command

The live plan names the current user's `.engram` home and
`.cargo/bin/engram.exe`. The `-Apply` switch activates that plan:

```powershell
pwsh -NoProfile -File scripts/eo-cutover.ps1 -Plan target/cutover/live.json -Apply
```

The script refuses any running Engram executable before capture and checks
again before activation, between moves and before completion. A named Windows
mutex serializes supported cutover and recovery commands. The Node engine and
host helper are internal; use the PowerShell entry point with Node 24 or newer.
The census also refuses another cutover Node engine, including an engine whose
PowerShell wrapper exited unexpectedly.

Preparation makes private copies of each complete SQLite file packet and
exports those copies with the old artifact. It counts former complete-tree
`task_sync_import_planning` objects before any import. A nonzero count refuses
activation and reports counts using known public project aliases; unknown
project ids are labelled `unlisted`. It does not guess a conversion.

Independent store imports run in parallel. Every child finishes before any
failure returns or activation begins. Import itself performs its full
integrity, control, work and pending-delivery checks; the offline path does not
run a duplicate doctor. The script exports each new file and compares every
surviving column, row, value, storage class, blob representation, row order and
sequence high-water mark. It checks named retirement counts and reasons, the
new format marker, and readiness with the exact final artifact. Allowed new
task-sync tables must be empty when absent from the old export. The source
packets and installed binary must still match their captured fingerprints.

Console output contains fixed stages, public aliases, counts, artifact
identities and timings. Child stdout/stderr, SQL, row bodies and arbitrary
error messages are never printed or logged. Exports, prepared files and the
journal are private to the current Windows user and SYSTEM. They contain
sensitive data and must be retained through recovery, then removed according
to repository evidence-retention rules.

## Activation and recovery

All stores must pass before the first rename. Each original main file and
existing sidecar moves to `.engram/backups/eo-<random>/stores/<alias>/`; all old
sidecars leave before the new main file is installed. The old executable moves
to the same backup's `binary/` directory. Moves require the same volume and
never fall back to copy-and-delete.

The new main file and the new executable are prepared inside the private
work directory, whose protected access list grants only the current Windows
user and SYSTEM. A same-volume rename keeps a file's own access list, so the
activated main file and the installed executable keep that private,
protected access list too: they do not take on the permissions of the store
home or the install directory. This is deliberate: activation grants no
other account access to them.

Several files cannot be replaced by one filesystem transaction. Activation is
a journaled sequence while consumers remain stopped. A handled failure restores
every original packet and the old binary. A process interruption leaves a
flushed journal and a printed recovery command. This is process-interruption
recovery, not a guarantee against power loss or filesystem failure. If recovery
is refused or cannot finish, keep consumers stopped and preserve every file;
the command reports `manual_recovery_required` and the failing stage.

Completion prints the ready/total count, whole-command wall time and a one-line
rollback command. To roll back before any consumer has resumed:

```powershell
pwsh -NoProfile -File scripts/eo-cutover.ps1 -RollbackJournal '<printed journal path>'
```

To recover an interrupted activation, use `-RecoverJournal` with that same
path. Recovery restores the old files unless activation was already recorded
complete. It validates the whole reverse move sequence before restoring the
first file and can be retried after interruption. Empty new WAL/journal and
new SHM files are kept separately in the backup, never mixed with old packets.
Changed managed files, changed backups, nonempty new WAL/journal files, linked
paths or running consumers refuse automatic rollback. The command never
discards work produced after a restart. After consumers resume, use an
explicitly planned migration instead of this rollback.

After successful live activation, Greg restarts TermAl at the coordinated
moment. Run any separately requested post-resume doctor/canary through the
integration owner. Nothing in this command touches `.termal`.
