import assert from "node:assert/strict";

const HASH = /\b[0-9a-f]{64}\b/u;
export const UUID = /\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/iu;
const INTERNAL_FIELDS = new Set([
  "accepted_work_revision",
  "active_run_id",
  "blocker_id",
  "claim",
  "claim_id",
  "completion_seal",
  "control_binding",
  "entry",
  "evidence",
  "evidence_items",
  "latest_evidence_item",
  "fence",
  "last_checkpoint",
  "memories",
  "object_hash",
  "obligation_page",
  "offer_id",
  "parent_id",
  "revision",
  "root_id",
  "run",
  "run_id",
  "session",
  "waivable_required_children",
  "work_id",
]);

// The Rust allowlist projection is the primary boundary. This recursive
// denylist is defense in depth against accidentally reintroducing known
// authority, identity, or integrity fields on either agent transport.
export function assertTerseShow(value) {
  const encoded = JSON.stringify(value);
  assert.doesNotMatch(encoded, HASH, encoded);
  assert.doesNotMatch(encoded, UUID, encoded);
  const visit = (node) => {
    if (Array.isArray(node)) {
      for (const item of node) visit(item);
      return;
    }
    if (node === null || typeof node !== "object") return;
    for (const [key, child] of Object.entries(node)) {
      assert.equal(INTERNAL_FIELDS.has(key), false, `${key} leaked into show`);
      visit(child);
    }
  };
  visit(value);
}
