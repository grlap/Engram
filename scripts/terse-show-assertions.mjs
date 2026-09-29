import assert from "node:assert/strict";

// A full record id: minted ids are 32 hex digits, earlier ids are 64.
const HASH = /\b(?:[0-9a-f]{32}|[0-9a-f]{64})\b/u;
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
  "object_id",
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

// A completed item's landing names a commit and a build fingerprint on
// purpose: it is asserted provenance, shown as recorded, not an identity or
// integrity field. It may hold only its own fields, the words that say no
// landing was recorded, or why it is unavailable; everything else keeps the
// hash ban.
const LANDING_FIELDS = new Set(["commit", "remote", "branch", "pushed_at", "installed_build"]);
const LANDING_WORDS = /^(?:no landing recorded|unavailable \([^()]*\))$/u;

// The Rust allowlist projection is the primary boundary. This recursive
// denylist is defense in depth against accidentally reintroducing known
// authority, identity, or integrity fields on either agent transport.
export function assertTerseShow(shown) {
  const { landing, ...value } = shown;
  if (typeof landing === "string") {
    assert.match(landing, LANDING_WORDS);
  } else if (landing !== undefined) {
    for (const key of Object.keys(landing)) {
      assert.equal(LANDING_FIELDS.has(key), true, `${key} leaked into show's landing`);
    }
  }
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
