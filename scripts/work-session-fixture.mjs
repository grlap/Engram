// Test fixtures send facts through the ordinary host evidence protocol.
// They assert a registered producer; they do not attest that a command ran.
import { createHash } from "node:crypto";
import { readdirSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";

export function gitRevision(label) {
  return `git-commit:${createHash("sha256").update(label).digest("hex").slice(0, 40)}`;
}

function measuredSource(source) {
  if (source.source_id) return source;
  const { source_revision: revision, git_tree: tree, ...scope } = source;
  const digest = createHash("sha256").update(revision).digest("hex").slice(0, 40);
  return { ...scope, source_id: {
    commit: revision.startsWith("git-commit:") ? revision.slice("git-commit:".length) : digest,
    tree: tree ?? (revision.startsWith("git-tree:") ? revision.slice("git-tree:".length) : digest),
    submodules: [], changes: [],
  } };
}

export async function observeSource({ request, policy, binding, key, source, changed = false }) {
  source = measuredSource(source);
  const root = await request({ operation: "named_root_read", run_id: binding.run_id, claim_id: binding.claim_id });
  const time = new Date().toISOString();
  const occurrence = changed ? {
    kind: "inter_turn_change",
    source_change: { detection: "assumed_missing_baseline", workspace_id: source.workspace_id,
      sighting: { source_basis: source, observed_at: time } },
  } : { kind: "source_sighting", source_basis: source, observed_at: time };
  const currentPolicy = policy();
  return request({
    operation: "execution_observe", idempotency_key: key, binding,
    root_basis: { capture_run_cut: root.read_cut.position, latest_event: root.latest_event?.event ?? null,
      state: root.named_root },
    observed_interval: { from: time, through: time }, occurrence,
    causality: { kind: "unknown" },
    policy_basis: { mode: "account_if_eligible", project_policy_epoch: currentPolicy.epoch,
      policy: currentPolicy.policy, obligation_rule_set: currentPolicy.obligation_rules },
  });
}

// Acceptance's legacy string readers are a separate transition. Seed their
// retained pre-cutoff observations explicitly, rather than letting those tests
// keep a legacy producer alive on the current host protocol. Only throwaway
// fixture stores are passed here; minted ids and links stay unchanged.
export async function observeHistoricalSource({ home, ...args }) {
  const receipt = await observeSource(args);
  const projects = readdirSync(join(home, "projects"), { withFileTypes: true }).filter((entry) => entry.isDirectory());
  if (projects.length !== 1) throw new Error("historical fixture requires exactly one project");
  const database = join(home, "projects", projects[0].name, "engram.db");
  const seed = String.raw`
    import { DatabaseSync } from "node:sqlite";
    import { createHash } from "node:crypto";
    const [file, id, encodedSource] = process.argv.slice(-3);
    const source = JSON.parse(encodedSource);
    const canonical = (value) => {
      if (Array.isArray(value)) return "[" + value.map(canonical).join(",") + "]";
      if (value && typeof value === "object") return "{" + Object.keys(value).sort().map((key) => JSON.stringify(key) + ":" + canonical(value[key])).join(",") + "}";
      return JSON.stringify(value);
    };
    const replaceBasis = (occurrence) => {
      if (occurrence.kind === "source_sighting") occurrence.source_basis = source;
      else occurrence.source_change.sighting.source_basis = source;
    };
    const db = new DatabaseSync(file);
    try {
      db.exec("BEGIN IMMEDIATE");
      const row = db.prepare("SELECT canonical_json FROM objects WHERE object_id = ? AND object_kind = 'unadmitted_execution_observation'").get(id);
      if (!row) throw new Error("missing historical fixture observation");
      const record = JSON.parse(Buffer.from(row.canonical_json));
      replaceBasis(record.occurrence);
      db.prepare("UPDATE objects SET canonical_json = ? WHERE object_id = ?").run(Buffer.from(canonical(record)), id);
      const receipts = db.prepare("SELECT idempotency_key, result_json FROM work_operation_results WHERE operation = 'host_execution_observe'").all();
      const operation = receipts.find((row) => JSON.parse(Buffer.from(row.result_json)).result.observation === id);
      if (!operation) throw new Error("missing historical fixture receipt");
      const envelope = JSON.parse(Buffer.from(operation.result_json));
      replaceBasis(envelope.intent.occurrence);
      const hash = createHash("sha256").update(canonical(envelope.intent)).digest("hex");
      db.prepare("UPDATE work_operation_results SET request_hash = ?, result_json = ? WHERE operation = 'host_execution_observe' AND idempotency_key = ?")
        .run(hash, Buffer.from(canonical(envelope)), operation.idempotency_key);
      db.exec("COMMIT");
    } finally { db.close(); }
  `;
  const result = spawnSync(process.execPath, ["--no-warnings", "--input-type=module", "--eval", seed,
    database, receipt.observation, JSON.stringify(args.source)], { encoding: "utf8" });
  if (result.status !== 0) throw new Error(result.stderr || "historical observation seeding failed");
  return receipt;
}

export async function ensureRoot({ request, binding, key, source }) {
  let root = await request({ operation: "named_root_read", run_id: binding.run_id, claim_id: binding.claim_id });
  if (root.named_root.state === "none") {
    await request({ operation: "named_root_bind", claim_id: binding.claim_id, claim_fence: binding.claim_fence,
      workspace_id: source.workspace_id, generation: 1, named_at: new Date().toISOString(),
      kind: "bound", idempotency_key: `${key}-root` });
    root = await request({ operation: "named_root_read", run_id: binding.run_id, claim_id: binding.claim_id });
  }
  return root;
}

export async function recordCheck({ request, binding, session, key, source, checkFingerprint, input }) {
  const root = await ensureRoot({ request, binding, key, source });
  const time = new Date().toISOString();
  const command = "fixture registered check";
  const hostRun = `test-${key}`;
  return request({
    operation: "verification_record", operation_key: `termal-run:${hostRun}`,
    registration: { host_run_id: hostRun, request_identity: key, registered_at: time,
      initiating_session_id: session, owning_session_id: session, binding,
      root: { workspace_id: source.workspace_id, generation: root.named_root.generation, state: "named" } },
    input: input ?? { kind: "commit", commit: source.source_revision.slice("git-commit:".length) },
    run: { mode: "focused", command, started_at: time, finished_at: time, outcome: "passed" },
    checks: [{ stage: "focused", check_kind: "test", command,
      check_fingerprint: checkFingerprint ?? createHash("sha256").update(command).digest("hex"),
      started_at: time, completed_at: time, outcome: "passed", summary: "fixture host check", refs: [] }],
    post_run: { outcome: "clean", clean_before: true, clean_after: true, dirty_paths: [] },
  });
}
