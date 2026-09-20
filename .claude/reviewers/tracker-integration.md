# Tracker Integration Review

Focus on capability boundaries, report freezing, idempotency, and receipts.

## Check

- Core ports use backend-neutral DTOs; proprietary ticket fields live in an
  extension map or adapter layer.
- External tickets are referenced or read on demand, never mirrored as local
  task truth.
- `report_ready` freezes report bytes and fingerprint. A separately requested
  publication creates an immutable intent binding those bytes to its target
  and durable idempotency key before publication begins.
- Retry sends identical bytes under the same key. Same key with a different
  payload is a conflict, not an update.
- Revising a failed report creates a superseding report and new publication
  intent/key.
- No receipt means no `published` transition. Adapter errors retain the frozen
  report and attempt metadata.
- Capabilities gate every external mutation.
- Publication is deferred; no dummy adapter or publication pipeline ships.
  Apply the publication checks above when a change implements that capability,
  not as a demand to retain disconnected scaffolding.

Do not require the proprietary adapter in V1.
