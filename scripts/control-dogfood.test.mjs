#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const binary = join(root, "target", "debug", "engram");
const projectId = readFileSync(join(root, ".engram-project"), "utf8").trim();
const sourceTree = {
  kind: "path",
  project_id: projectId,
  segments: ["src"],
  coverage: "tree",
};
const libraryFile = {
  kind: "path",
  project_id: projectId,
  segments: ["src", "lib.rs"],
  coverage: "exact",
};
const manifestFile = {
  kind: "path",
  project_id: projectId,
  segments: ["Cargo.toml"],
  coverage: "exact",
};

function fingerprint(value) {
  return createHash("sha256").update(value).digest("hex");
}

async function holdSqliteWriter(database) {
  const helper = String.raw`
    import { DatabaseSync } from "node:sqlite";
    const database = new DatabaseSync(process.argv.at(-1));
    database.exec("BEGIN IMMEDIATE");
    process.stdout.write("ready\n");
    process.stdin.resume();
    process.stdin.on("end", () => {
      database.exec("ROLLBACK");
      database.close();
    });
  `;
  const child = spawn(
    process.execPath,
    ["--no-warnings", "--input-type=module", "--eval", helper, database],
    { cwd: root, stdio: ["pipe", "pipe", "pipe"] },
  );
  let stderr = "";
  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString("utf8");
  });
  await new Promise((resolvePromise, reject) => {
    let output = "";
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`SQLite writer helper timed out: ${stderr}`));
    }, 5000);
    child.stdout.on("data", (chunk) => {
      output += chunk.toString("utf8");
      if (!output.includes("ready\n")) return;
      clearTimeout(timer);
      resolvePromise();
    });
    child.once("exit", (code, signal) => {
      clearTimeout(timer);
      reject(
        new Error(
          `SQLite writer helper exited code=${code} signal=${signal}: ${stderr}`,
        ),
      );
    });
  });
  return {
    close() {
      if (child.exitCode !== null) return Promise.resolve();
      return new Promise((resolvePromise) => {
        child.once("exit", resolvePromise);
        child.stdin.end();
      });
    },
  };
}

function canonicalJson(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) {
    return `[${value.map((item) => canonicalJson(item)).join(",")}]`;
  }
  return `{${Object.keys(value)
    .sort()
    .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
    .join(",")}}`;
}

function canonicalFingerprint(value) {
  return fingerprint(canonicalJson(value));
}

class ControlClient {
  constructor(engramHome, sessionId) {
    this.pending = [];
    this.buffer = "";
    this.stderr = "";
    this.child = spawn(
      binary,
      [
        "--home",
        engramHome,
        "control",
        "--actor-id",
        sessionId,
        "--session-id",
        sessionId,
        "--source-skill",
        "engram-control-dogfood",
      ],
      { cwd: root, stdio: ["pipe", "pipe", "pipe"] },
    );
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    this.child.on("exit", (code, signal) => {
      const error = new Error(
        `control server exited code=${code} signal=${signal}: ${this.stderr}`,
      );
      for (const pending of this.pending) pending.reject(error);
      this.pending = [];
    });
  }

  #receive(chunk) {
    this.buffer += chunk.toString("utf8");
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      if (line === "") continue;
      const pending = this.pending.shift();
      assert.ok(pending, `unexpected control response: ${line}`);
      pending.resolve(JSON.parse(line));
    }
  }

  request(request) {
    return new Promise((resolvePromise, reject) => {
      const timer = setTimeout(() => {
        reject(
          new Error(
            `control request timed out: ${request.operation}; stderr=${this.stderr}`,
          ),
        );
      }, 15000);
      this.pending.push({
        resolve: (response) => {
          clearTimeout(timer);
          resolvePromise(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.child.stdin.write(`${JSON.stringify(request)}\n`);
    });
  }

  close() {
    if (this.child.exitCode !== null) return Promise.resolve();
    return new Promise((resolvePromise) => {
      this.child.once("exit", resolvePromise);
      this.child.stdin.end();
    });
  }
}

function ok(response) {
  assert.equal(response.status, "ok", JSON.stringify(response));
  return response.result;
}

function installWorkGrant(engramHome, actorId) {
  const granted = spawnSync(
    binary,
    [
      "--home",
      engramHome,
      "authority",
      "grant",
      "--subject-actor-id",
      actorId,
      "--issued-by",
      "control-dogfood-host",
      "--reason",
      "work-bound host-control dogfood",
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(granted.status, 0, granted.stderr);
  return JSON.parse(granted.stdout).grant;
}

function installObligationWaiverGrant(engramHome, waivedBy) {
  const granted = spawnSync(
    binary,
    [
      "--home",
      engramHome,
      "authority",
      "grant",
      "--subject-actor-id",
      waivedBy,
      "--issued-by",
      "control-dogfood-host",
      "--allow-obligation-waiver",
      "--reason",
      "authorize one reviewed host-private obligation waiver",
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(granted.status, 0, granted.stderr);
  return JSON.parse(granted.stdout).grant;
}

function cliWork(engramHome, actorId, grant, operation, input) {
  const args = [
    "--home",
    engramHome,
    "work",
    "--actor-id",
    actorId,
    "--session-id",
    actorId,
    "--authority-grant",
    grant,
    operation,
  ];
  if (input !== undefined) args.push("--input", JSON.stringify(input));
  const executed = spawnSync(binary, args, {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(executed.status, 0, executed.stderr);
  return JSON.parse(executed.stdout);
}

function cliWorkFocus(engramHome, actorId, grant, workRef) {
  const focused = spawnSync(
    binary,
    [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actorId,
      "--session-id",
      actorId,
      "--authority-grant",
      grant,
      "focus",
      workRef,
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(focused.status, 0, focused.stderr);
  return JSON.parse(focused.stdout);
}

function cliWorkAcknowledge(engramHome, actorId, grant, page) {
  if (page.delivery_token === undefined) return;
  const acknowledged = spawnSync(
    binary,
    [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actorId,
      "--session-id",
      actorId,
      "--authority-grant",
      grant,
      "next",
      "--acknowledge-through",
      String(page.delivered_through),
      "--acknowledge-token",
      page.delivery_token,
      "--sections",
      "focus",
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(acknowledged.status, 0, acknowledged.stderr);
}

test("host control survives restart and gates turn dispatch", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-control-dogfood-"));
  const actionGatedHome = mkdtempSync(
    join(tmpdir(), "engram-control-action-gated-"),
  );
  let client;
  let peer;
  let advisory;
  let sqliteWriter;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
    const unattributedBootstrap = spawnSync(
      binary,
      [
        "--home",
        actionGatedHome,
        "init",
        "--required-assurance",
        "advisory",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(unattributedBootstrap.status, 0);
    assert.match(unattributedBootstrap.stderr, /--authorized-by/);
    const actionGatedInit = spawnSync(
      binary,
      [
        "--home",
        actionGatedHome,
        "init",
        "--required-assurance",
        "action_gated",
        "--authorized-by",
        "dogfood-bootstrap-operator",
        "--reason",
        "exercise an attributed fail-closed bootstrap",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(actionGatedInit.status, 0, actionGatedInit.stderr);
    assert.match(actionGatedInit.stdout, /epoch 1, required action_gated/);
    assert.match(
      actionGatedInit.stderr,
      /no current V1 host can bind at action_gated/,
    );
    const actionGatedSetter = spawnSync(
      binary,
      [
        "--home",
        actionGatedHome,
        "control-policy",
        "set-required-assurance",
        "action_gated",
        "--authorized-by",
        "dogfood-operator",
        "--reason",
        "exercise the fail-closed warning",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(actionGatedSetter.status, 0, actionGatedSetter.stderr);
    assert.match(
      actionGatedSetter.stderr,
      /no current V1 host can bind at action_gated/,
    );
    const actionGatedRecovery = spawnSync(
      binary,
      [
        "--home",
        actionGatedHome,
        "control-policy",
        "set-required-assurance",
        "turn_gated",
        "--authorized-by",
        "dogfood-operator",
        "--reason",
        "restore a bindable V1 requirement",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(actionGatedRecovery.status, 0, actionGatedRecovery.stderr);
    assert.equal(
      JSON.parse(actionGatedRecovery.stdout).required_assurance,
      "turn_gated",
    );
    assert.equal(
      JSON.parse(actionGatedRecovery.stdout).previous_required_assurance,
      "action_gated",
    );
    assert.match(
      actionGatedRecovery.stderr,
      /required assurance was lowered from action_gated to turn_gated/,
    );
    const initialized = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "init",
        "--required-assurance",
        "advisory",
        "--authorized-by",
        "dogfood-bootstrap-operator",
        "--reason",
        "exercise an attributed advisory bootstrap",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(initialized.status, 0, initialized.stderr);
    assert.match(initialized.stdout, /epoch 1, required advisory/);
    const advisoryDoctor = spawnSync(
      binary,
      ["--home", engramHome, "doctor"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(advisoryDoctor.status, 0, advisoryDoctor.stderr);
    const initialPolicy = advisoryDoctor.stdout.match(
      /Control policy schema=1 id=([0-9a-f]{64}) epoch=1 required=advisory obligation_rules=([0-9a-f]{64})/,
    );
    assert.ok(initialPolicy, advisoryDoctor.stdout);
    const plainReinit = spawnSync(binary, ["--home", engramHome, "init"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(plainReinit.status, 0, plainReinit.stderr);
    assert.match(plainReinit.stdout, /epoch 1, required advisory/);

    advisory = new ControlClient(engramHome, "host-advisory");
    const advisoryBinding = ok(
      await advisory.request({
        operation: "session_bind",
        external_ref: "dummy:HOST-ADVISORY",
        title: "Honest advisory host",
        assurance: "advisory",
        mediated_effects: ["observe", "communicate", "mutate_local"],
        capability_map_revision: 1,
        idempotency_key: "bind-host-advisory",
      }),
    );
    assert.deepEqual(advisoryBinding.effective_mediated_effects, [
      "observe",
      "communicate",
    ]);
    const advisorySync = ok(
      await advisory.request({
        operation: "turn_evaluate",
        routing_token: advisoryBinding.routing_token,
        idempotency_key: "turn-advisory-sync",
        intent_fingerprint: fingerprint("turn-advisory-sync"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(advisorySync.decision, "grant");
    const advisorySyncTokens = advisorySync.grant.delivery
      ? [advisorySync.grant.delivery.page.delivery_token]
      : [];
    assert.equal(
      ok(
        await advisory.request({
          operation: "turn_begin",
          routing_token: advisoryBinding.routing_token,
          grant_id: advisorySync.grant.grant_id,
          delivery_tokens: advisorySyncTokens,
          idempotency_key: "begin-advisory-sync",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await advisory.request({
          operation: "turn_checkpoint",
          routing_token: advisoryBinding.routing_token,
          grant_id: advisorySync.grant.grant_id,
          next_intent: "continue",
          idempotency_key: "checkpoint-advisory-sync",
        }),
      ).decision,
      "checkpointed",
    );
    const advisoryLease = ok(
      await advisory.request({
        operation: "lease_acquire",
        routing_token: advisoryBinding.routing_token,
        kind: "execution",
        mode: "exclusive",
        subject: sourceTree,
        ttl_seconds: 60,
        idempotency_key: "lease-advisory-src",
      }),
    );
    assert.equal(advisoryLease.decision, "refuse");
    assert.equal(
      advisoryLease.directive.code,
      "control_assurance_insufficient",
    );
    assert.equal(advisoryLease.directive.effect, "mutate_local");
    assert.equal(advisoryLease.directive.required_assurance, "turn_gated");
    assert.deepEqual(advisoryLease.directive.declared_mediated_effects, [
      "observe",
      "communicate",
      "mutate_local",
    ]);
    assert.deepEqual(advisoryLease.directive.effective_mediated_effects, [
      "observe",
      "communicate",
    ]);
    assert.deepEqual(
      ok(
        await advisory.request({
          operation: "lease_acquire",
          routing_token: advisoryBinding.routing_token,
          kind: "execution",
          mode: "exclusive",
          subject: sourceTree,
          ttl_seconds: 60,
          idempotency_key: "lease-advisory-src",
        }),
      ),
      advisoryLease,
    );
    const advisoryMutation = ok(
      await advisory.request({
        operation: "turn_evaluate",
        routing_token: advisoryBinding.routing_token,
        idempotency_key: "turn-advisory-mutation",
        intent_fingerprint: fingerprint("turn-advisory-mutation"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
        resource_intents: [sourceTree],
      }),
    );
    assert.equal(advisoryMutation.decision, "refuse");
    assert.equal(
      advisoryMutation.directive.code,
      "control_assurance_insufficient",
    );
    assert.equal(advisoryMutation.directive.effect, "mutate_local");
    assert.equal(
      advisoryMutation.directive.required_assurance,
      "turn_gated",
    );
    assert.deepEqual(advisoryMutation.directive.declared_mediated_effects, [
      "observe",
      "communicate",
      "mutate_local",
    ]);
    assert.deepEqual(advisoryMutation.directive.effective_mediated_effects, [
      "observe",
      "communicate",
    ]);
    const advisoryIssued = ok(
      await advisory.request({
        operation: "turn_evaluate",
        routing_token: advisoryBinding.routing_token,
        idempotency_key: "turn-advisory-before-policy-change",
        intent_fingerprint: fingerprint("turn-advisory-before-policy-change"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(advisoryIssued.decision, "grant");

    for (const [authorizedBy, reason] of [
      ["", "missing administrator"],
      ["dogfood-operator", ""],
    ]) {
      const invalidAttribution = spawnSync(
        binary,
        [
          "--home",
          engramHome,
          "control-policy",
          "set-required-assurance",
          "advisory",
          "--authorized-by",
          authorizedBy,
          "--reason",
          reason,
        ],
        { cwd: root, encoding: "utf8" },
      );
      assert.notEqual(invalidAttribution.status, 0);
      assert.match(invalidAttribution.stderr, /must contain from 1 through 4096 bytes/);
    }
    const badExpectedPolicy = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "control-policy",
        "set-required-assurance",
        "turn_gated",
        "--authorized-by",
        "dogfood-operator",
        "--reason",
        "reject a stale operator",
        "--expected-policy-hash",
        "0".repeat(64),
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(badExpectedPolicy.status, 0);
    assert.match(badExpectedPolicy.stderr, /active control policy changed/);

    const configured = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "control-policy",
        "set-required-assurance",
        "turn_gated",
        "--authorized-by",
        "dogfood-operator",
        "--reason",
        "exercise attributed policy activation",
        "--expected-policy-hash",
        initialPolicy[1],
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(configured.status, 0, configured.stderr);
    const configuredPolicy = JSON.parse(configured.stdout);
    assert.equal(configuredPolicy.policy_epoch, 2);
    assert.equal(configuredPolicy.required_assurance, "turn_gated");
    assert.equal(configuredPolicy.previous_required_assurance, "advisory");
    assert.equal(configuredPolicy.changed, true);
    assert.equal(configuredPolicy.previous_policy, initialPolicy[1]);
    assert.match(configured.stderr, /asserted host context, not an authenticated identity/);
    const configuredDoctor = spawnSync(
      binary,
      ["--home", engramHome, "doctor"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(configuredDoctor.status, 0, configuredDoctor.stderr);
    const configuredPolicyLine = configuredDoctor.stdout.match(
      /Control policy schema=1 id=[0-9a-f]{64} epoch=2 required=turn_gated obligation_rules=([0-9a-f]{64})/,
    );
    assert.ok(configuredPolicyLine, configuredDoctor.stdout);
    assert.equal(configuredPolicyLine[1], initialPolicy[2]);
    const advisoryTokens = advisoryIssued.grant.delivery
      ? [advisoryIssued.grant.delivery.page.delivery_token]
      : [];
    const advisoryBeginAfterPolicyChange = ok(
      await advisory.request({
        operation: "turn_begin",
        routing_token: advisoryBinding.routing_token,
        grant_id: advisoryIssued.grant.grant_id,
        delivery_tokens: advisoryTokens,
        idempotency_key: "begin-advisory-after-policy-change",
      }),
    );
    assert.equal(advisoryBeginAfterPolicyChange.decision, "refuse");
    assert.equal(advisoryBeginAfterPolicyChange.code, "policy_epoch_changed");
    const advisoryStatus = ok(
      await advisory.request({
        operation: "session_status",
        routing_token: advisoryBinding.routing_token,
      }),
    );
    assert.equal(advisoryStatus.epochs.project_policy, 2);
    const advisoryAfterPolicyChange = ok(
      await advisory.request({
        operation: "turn_evaluate",
        routing_token: advisoryBinding.routing_token,
        idempotency_key: "turn-advisory-after-policy-change",
        intent_fingerprint: fingerprint("turn-advisory-after-policy-change"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(advisoryAfterPolicyChange.decision, "refuse");
    assert.equal(
      advisoryAfterPolicyChange.directive.code,
      "control_assurance_insufficient",
    );
    await advisory.close();
    advisory = undefined;

    client = new ControlClient(engramHome, "host-a");
    const binding = ok(
      await client.request({
        operation: "session_bind",
        external_ref: "dummy:HOST-1",
        title: "Host control dogfood",
        assurance: "turn_gated",
        mediated_effects: ["observe", "communicate", "mutate_local"],
        capability_map_revision: 1,
        idempotency_key: "bind-host-a",
      }),
    );
    assert.equal(binding.status.phase, "sync_required");
    assert.ok(binding.routing_token);

    const successor = new ControlClient(engramHome, "host-a");
    const successorStatus = ok(
      await successor.request({
        operation: "session_status",
        routing_token: binding.routing_token,
      }),
    );
    assert.equal(successorStatus.phase, "sync_required");
    const superseded = await client.request({
      operation: "session_status",
      routing_token: binding.routing_token,
    });
    assert.equal(superseded.status, "error");
    assert.equal(superseded.error.code, "control_connection_superseded");
    await client.close();
    client = successor;

    const firstDecision = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: binding.routing_token,
        idempotency_key: "turn-host-a",
        intent_fingerprint: fingerprint("turn-host-a"),
        purpose: "ordinary",
        requested_effects: ["observe", "communicate"],
      }),
    );
    assert.equal(firstDecision.decision, "grant");
    assert.ok(firstDecision.grant.delivery.context.header.packet_hash);
    assert.equal(firstDecision.grant.delivery.delta.after, 0);
    assert.equal(
      firstDecision.grant.delivery.delta.cursor,
      firstDecision.grant.delivery.page.to_cursor,
    );
    assert.ok(
      firstDecision.grant.delivery.delta.changes.some(
        (change) => change.object_kind === "task_started_event",
      ),
    );
    firstDecision.grant.delivery.delta.changes.forEach((change, index) => {
      assert.equal(change.cursor, index + 1);
    });
    const grant = firstDecision.grant;
    await client.close();

    client = new ControlClient(engramHome, "host-a");
    const expiredBegin = ok(
      await client.request({
        operation: "turn_begin",
        routing_token: binding.routing_token,
        grant_id: grant.grant_id,
        delivery_tokens: [grant.delivery.page.delivery_token],
        idempotency_key: "begin-host-a",
      }),
    );
    assert.equal(expiredBegin.decision, "refuse");
    assert.equal(expiredBegin.code, "grant_scope_mismatch");

    const resumedDecision = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: binding.routing_token,
        idempotency_key: "turn-host-after-restart",
        intent_fingerprint: fingerprint("turn-host-after-restart"),
        purpose: "ordinary",
        requested_effects: ["observe", "communicate"],
      }),
    );
    assert.equal(resumedDecision.decision, "grant");
    const resumedGrant = resumedDecision.grant;
    const begun = ok(
      await client.request({
        operation: "turn_begin",
        routing_token: binding.routing_token,
        grant_id: resumedGrant.grant_id,
        delivery_tokens: [resumedGrant.delivery.page.delivery_token],
        idempotency_key: "begin-host-after-restart",
      }),
    );
    assert.equal(begun.decision, "begin");
    await client.close();
    client = new ControlClient(engramHome, "host-a");
    const begunRestartStatus = ok(
      await client.request({
        operation: "session_status",
        routing_token: binding.routing_token,
      }),
    );
    assert.equal(begunRestartStatus.phase, "turn_open");
    assert.equal(begunRestartStatus.open_grant_id, resumedGrant.grant_id);
    assert.equal(begunRestartStatus.recoverable_grant, null);
    const checkpointed = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: binding.routing_token,
        grant_id: resumedGrant.grant_id,
        next_intent: "continue",
        idempotency_key: "checkpoint-host-a",
      }),
    );
    assert.equal(checkpointed.decision, "checkpointed");

    const mutation = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: binding.routing_token,
        idempotency_key: "turn-host-mutation",
        intent_fingerprint: fingerprint("turn-host-mutation"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
      }),
    );
    assert.equal(mutation.decision, "refuse");
    assert.equal(mutation.directive.code, "lease_required");

    const acquired = ok(
      await client.request({
        operation: "lease_acquire",
        routing_token: binding.routing_token,
        kind: "execution",
        mode: "exclusive",
        subject: sourceTree,
        ttl_seconds: 60,
        idempotency_key: "lease-host-a-src",
      }),
    );
    assert.equal(acquired.decision, "granted");
    assert.equal(acquired.lease.fence, 1);
    assert.deepEqual(
      ok(
        await client.request({
          operation: "lease_acquire",
          routing_token: binding.routing_token,
          kind: "execution",
          mode: "exclusive",
          subject: sourceTree,
          ttl_seconds: 60,
          idempotency_key: "lease-host-a-src",
        }),
      ),
      acquired,
    );
    const conflictingAcquire = await client.request({
      operation: "lease_acquire",
      routing_token: binding.routing_token,
      kind: "execution",
      mode: "exclusive",
      subject: libraryFile,
      ttl_seconds: 60,
      idempotency_key: "lease-host-a-src",
    });
    assert.equal(conflictingAcquire.status, "error");
    assert.equal(
      conflictingAcquire.error.code,
      "control_operation_idempotency_conflict",
    );

    peer = new ControlClient(engramHome, "host-b");
    const peerBinding = ok(
      await peer.request({
        operation: "session_bind",
        external_ref: "dummy:HOST-1",
        title: "Host control dogfood",
        assurance: "turn_gated",
        mediated_effects: ["observe", "communicate", "mutate_local"],
        capability_map_revision: 1,
        idempotency_key: "bind-host-b",
      }),
    );
    const peerDecision = ok(
      await peer.request({
        operation: "turn_evaluate",
        routing_token: peerBinding.routing_token,
        idempotency_key: "turn-host-b",
        intent_fingerprint: fingerprint("turn-host-b"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(peerDecision.decision, "grant");
    const peerGrant = peerDecision.grant;
    assert.equal(
      ok(
        await peer.request({
          operation: "turn_begin",
          routing_token: peerBinding.routing_token,
          grant_id: peerGrant.grant_id,
          delivery_tokens: [peerGrant.delivery.page.delivery_token],
          idempotency_key: "begin-host-b",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await peer.request({
          operation: "turn_checkpoint",
          routing_token: peerBinding.routing_token,
          grant_id: peerGrant.grant_id,
          next_intent: "continue",
          idempotency_key: "checkpoint-host-b",
        }),
      ).decision,
      "checkpointed",
    );
    const peerLease = ok(
      await peer.request({
        operation: "lease_acquire",
        routing_token: peerBinding.routing_token,
        kind: "execution",
        mode: "exclusive",
        subject: manifestFile,
        ttl_seconds: 60,
        idempotency_key: "lease-host-b-manifest",
      }),
    );
    assert.equal(peerLease.decision, "granted");

    const mutationWithLease = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: binding.routing_token,
        idempotency_key: "turn-host-mutation-with-lease",
        intent_fingerprint: fingerprint("turn-host-mutation-with-lease"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
        resource_intents: [libraryFile],
      }),
    );
    assert.equal(mutationWithLease.decision, "grant");
    assert.ok(
      mutationWithLease.grant.delivery.delta.changes.some(
        (change) => change.object_kind === "work_lease_event",
      ),
    );
    assert.equal(mutationWithLease.grant.basis.leases.length, 1);
    assert.equal(
      mutationWithLease.grant.basis.leases[0].lease_id,
      acquired.lease.lease_id,
    );
    const mutationBegun = ok(
      await client.request({
        operation: "turn_begin",
        routing_token: binding.routing_token,
        grant_id: mutationWithLease.grant.grant_id,
        delivery_tokens: [
          mutationWithLease.grant.delivery.page.delivery_token,
        ],
        idempotency_key: "begin-host-mutation-with-lease",
      }),
    );
    assert.equal(mutationBegun.decision, "begin");
    const mutationCheckpointed = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: binding.routing_token,
        grant_id: mutationWithLease.grant.grant_id,
        next_intent: "continue",
        idempotency_key: "checkpoint-host-mutation-with-lease",
      }),
    );
    assert.equal(mutationCheckpointed.decision, "checkpointed");

    const released = ok(
      await client.request({
        operation: "lease_release",
        routing_token: binding.routing_token,
        lease_id: acquired.lease.lease_id,
        idempotency_key: "release-host-a-src",
      }),
    );
    assert.equal(released.lease_id, acquired.lease.lease_id);

    const status = ok(
      await client.request({
        operation: "session_status",
        routing_token: binding.routing_token,
      }),
    );
    assert.equal(status.phase, "ready");

    const wrongToken = await client.request({
      operation: "session_status",
      routing_token: "wrong-token",
    });
    assert.equal(wrongToken.status, "error");
    assert.equal(wrongToken.error.code, "control_session_token_mismatch");

    const database = join(
      engramHome,
      "projects",
      fingerprint(projectId),
      "engram.db",
    );
    sqliteWriter = await holdSqliteWriter(database);
    const doctor = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(doctor.status, 0, doctor.stderr);
    await sqliteWriter.close();
    sqliteWriter = undefined;
  } finally {
    await sqliteWriter?.close();
    await advisory?.close();
    await peer?.close();
    await client?.close();
    rmSync(engramHome, { recursive: true, force: true });
    rmSync(actionGatedHome, { recursive: true, force: true });
  }
});

test("work-bound control records observations and rebinds after a stale fence", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-control-work-bound-"));
  const actor = "bound-runner";
  let client;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
    const initialized = spawnSync(binary, ["--home", engramHome, "init"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(initialized.status, 0, initialized.stderr);
    const authorityGrant = installWorkGrant(engramHome, actor);
    const proposed = cliWork(engramHome, actor, authorityGrant, "propose", {
      kind: "root",
      title: "Exercise work-bound control",
      outcome: "Host observations follow the exact live run claim",
      acceptance: ["The control binding survives only its exact fence"],
      work_kind: "chore",
      idempotency_key: "bound-root",
    });
    const claimed = cliWork(engramHome, actor, authorityGrant, "update", {
      kind: "claim",
      ttl_seconds: 300,
      idempotency_key: "bound-claim-1",
    });
    const originalBinding = claimed.receipt.control_binding;
    assert.ok(originalBinding, JSON.stringify(claimed));
    assert.equal(originalBinding.work_id, proposed.work.work_id);
    assert.equal(originalBinding.work_revision, claimed.receipt.revision);
    const focused = cliWorkFocus(
      engramHome,
      actor,
      authorityGrant,
      proposed.work.short_ref,
    );
    assert.deepEqual(focused.control_binding, originalBinding);
    assert.equal(focused.run.root_execution_id, originalBinding.root_execution_id);
    assert.equal(focused.run.work_id, originalBinding.work_id);
    assert.equal(focused.claim.claim_id, originalBinding.claim_id);

    client = new ControlClient(engramHome, actor);
    const bound = ok(
      await client.request({
        operation: "session_bind",
        external_ref: "local-work:bound-control-dogfood",
        title: "Work-bound host control",
        assurance: "turn_gated",
        mediated_effects: ["observe", "communicate", "mutate_local"],
        work_binding: originalBinding,
        capability_map_revision: 1,
        idempotency_key: "bind-bound-run-1",
      }),
    );
    assert.deepEqual(bound.status.work_binding, originalBinding);

    const sync = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: bound.routing_token,
        idempotency_key: "bound-sync",
        intent_fingerprint: fingerprint("bound-sync"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(sync.decision, "grant");
    assert.deepEqual(sync.grant.basis.work_binding, originalBinding);
    const syncTokens = sync.grant.delivery
      ? [sync.grant.delivery.page.delivery_token]
      : [];
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: bound.routing_token,
          grant_id: sync.grant.grant_id,
          delivery_tokens: syncTokens,
          idempotency_key: "begin-bound-sync",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await client.request({
          operation: "turn_checkpoint",
          routing_token: bound.routing_token,
          grant_id: sync.grant.grant_id,
          next_intent: "continue",
          idempotency_key: "checkpoint-bound-sync",
        }),
      ).decision,
      "checkpointed",
    );

    const boundLease = ok(
      await client.request({
        operation: "lease_acquire",
        routing_token: bound.routing_token,
        kind: "execution",
        mode: "exclusive",
        subject: sourceTree,
        ttl_seconds: 60,
        idempotency_key: "lease-bound-run-src",
      }),
    );
    assert.equal(boundLease.decision, "granted");

    const observedTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: bound.routing_token,
        idempotency_key: "bound-observed-turn",
        intent_fingerprint: fingerprint("bound-observed-turn"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
        resource_intents: [libraryFile],
      }),
    );
    assert.equal(observedTurn.decision, "grant");
    const observedTokens = observedTurn.grant.delivery
      ? [observedTurn.grant.delivery.page.delivery_token]
      : [];
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: bound.routing_token,
          grant_id: observedTurn.grant.grant_id,
          delivery_tokens: observedTokens,
          idempotency_key: "begin-bound-observed-turn",
        }),
      ).decision,
      "begin",
    );
    const observations = [
      {
        observation_id: "bound-observation-1",
        action_fingerprint: fingerprint("bound-observation-1"),
        effect: "mutate_local",
        outcome: "succeeded",
        source_changed: true,
        source_basis: {
          workspace_id: "control-dogfood-workspace",
          source_revision: "revision-1",
        },
        observed_at: "2026-08-28T20:00:00Z",
      },
      {
        observation_id: "bound-observation-2",
        action_fingerprint: fingerprint("bound-observation-2"),
        effect: "mutate_local",
        outcome: "failed",
        source_changed: false,
      },
    ];
    const outOfScopeCheckpoint = await client.request({
      operation: "turn_checkpoint",
      routing_token: bound.routing_token,
      grant_id: observedTurn.grant.grant_id,
      next_intent: "continue",
      observations: [
        {
          observation_id: "bound-out-of-scope-observation",
          action_fingerprint: fingerprint("bound-out-of-scope-observation"),
          effect: "observe",
          outcome: "succeeded",
          source_changed: false,
        },
      ],
      idempotency_key: "checkpoint-bound-out-of-scope",
    });
    assert.equal(outOfScopeCheckpoint.status, "error");
    assert.equal(
      outOfScopeCheckpoint.error.code,
      "observation_scope_mismatch",
    );
    const boundEnvironmentComponents = {
      toolchain: "rustc-control-dogfood",
      sandbox: "control-dogfood-sandbox-v1",
      workspace_id: "control-dogfood-workspace",
      capability_map_revision: 1,
    };
    const checkpointRequest = {
      operation: "turn_checkpoint",
      routing_token: bound.routing_token,
      grant_id: observedTurn.grant.grant_id,
      next_intent: "continue",
      observations,
      verification_evidence: [
        {
          producer_observation: {
            kind: "observation_id",
            observation_id: "bound-observation-1",
          },
          check_kind: "test",
          environment: { kind: "index", index: 0 },
          summary: "host observed the bound verification check",
          refs: ["command:control-dogfood-bound-check"],
        },
      ],
      environment_evidence: [
        {
          source_basis: {
            workspace_id: "control-dogfood-workspace",
            source_revision: "revision-1",
          },
          environment_fingerprint: canonicalFingerprint(
            boundEnvironmentComponents,
          ),
          components: boundEnvironmentComponents,
          observed_at: "2026-08-28T20:00:00Z",
        },
      ],
      idempotency_key: "checkpoint-bound-observations",
    };
    const mismatchedEnvironmentCheckpoint = await client.request({
      ...checkpointRequest,
      environment_evidence: checkpointRequest.environment_evidence.map(
        (environment) => ({
          ...environment,
          environment_fingerprint: fingerprint(
            "mismatched-control-dogfood-environment",
          ),
        }),
      ),
      idempotency_key: "checkpoint-bound-mismatched-environment",
    });
    assert.equal(mismatchedEnvironmentCheckpoint.status, "error");
    assert.equal(
      mismatchedEnvironmentCheckpoint.error.code,
      "environment_fingerprint_mismatch",
    );
    const checkpointed = ok(await client.request(checkpointRequest));
    assert.equal(checkpointed.decision, "checkpointed");
    assert.equal(checkpointed.receipt.execution_observations.length, 2);
    assert.equal(checkpointed.receipt.verification_evidence.length, 1);
    assert.equal(checkpointed.receipt.environment_evidence.length, 1);
    assert.deepEqual(ok(await client.request(checkpointRequest)), checkpointed);
    const obligationFocus = cliWorkFocus(
      engramHome,
      actor,
      authorityGrant,
      proposed.work.short_ref,
    );
    assert.equal(obligationFocus.obligation_page.items.length, 1);
    assert.equal(obligationFocus.obligation_page.items[0].state, "satisfied");
    assert.equal(
      obligationFocus.obligation_page.items[0].evidence,
      checkpointed.receipt.verification_evidence[0],
    );
    const environmentSummary = obligationFocus.evidence_items.find(
      (item) => item.evidence === checkpointed.receipt.environment_evidence[0],
    );
    assert.deepEqual(
      environmentSummary.environment_components,
      boundEnvironmentComponents,
    );
    const verificationSummary = obligationFocus.evidence_items.find(
      (item) => item.evidence === checkpointed.receipt.verification_evidence[0],
    );
    assert.equal(
      verificationSummary.environment,
      checkpointed.receipt.environment_evidence[0],
    );
    const conflictingCheckpoint = await client.request({
      ...checkpointRequest,
      observations: observations.slice(0, 1),
    });
    assert.equal(conflictingCheckpoint.status, "error");
    assert.equal(
      conflictingCheckpoint.error.code,
      "control_operation_idempotency_conflict",
    );

    const verificationEvidence = checkpointed.receipt.verification_evidence[0];
    const attachedEvidence = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "update",
      {
        kind: "evidence",
        attach: { evidence: verificationEvidence },
        idempotency_key: "bound-attach-verification-evidence",
      },
    ).receipt.result;
    assert.equal(attachedEvidence.attached, true);
    assert.equal(attachedEvidence.evidence, verificationEvidence);
    assert.equal(attachedEvidence.evidence_kind, "verification");
    cliWork(engramHome, actor, authorityGrant, "update", {
      kind: "checkpoint",
      summary: "record a contribution before releasing the claim",
      evidence: [verificationEvidence],
      idempotency_key: "bound-contribution-checkpoint",
    });
    const releasedBoundLease = ok(
      await client.request({
        operation: "lease_release",
        routing_token: bound.routing_token,
        lease_id: boundLease.lease.lease_id,
        idempotency_key: "release-bound-run-src",
      }),
    );
    assert.equal(releasedBoundLease.lease_id, boundLease.lease.lease_id);
    cliWork(engramHome, actor, authorityGrant, "update", {
      kind: "release",
      reason: "exercise stale control binding",
      idempotency_key: "bound-release-1",
    });
    const stale = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: bound.routing_token,
        idempotency_key: "bound-stale-turn",
        intent_fingerprint: fingerprint("bound-stale-turn"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(stale.decision, "refuse");
    assert.equal(stale.directive.code, "stale_fence");

    const reclaimed = cliWork(engramHome, actor, authorityGrant, "update", {
      kind: "claim",
      ttl_seconds: 300,
      idempotency_key: "bound-claim-2",
    });
    const replacementBinding = reclaimed.receipt.control_binding;
    assert.ok(replacementBinding, JSON.stringify(reclaimed));
    assert.ok(replacementBinding.claim_fence > originalBinding.claim_fence);
    const staleBind = await client.request({
      operation: "session_bind",
      external_ref: "local-work:bound-control-dogfood",
      title: "Work-bound host control",
      assurance: "turn_gated",
      mediated_effects: ["observe"],
      work_binding: originalBinding,
      capability_map_revision: 1,
      idempotency_key: "bind-stale-run",
    });
    assert.equal(staleBind.status, "error");
    assert.equal(staleBind.error.code, "stale_fence");
    const rebound = ok(
      await client.request({
        operation: "session_bind",
        external_ref: "local-work:bound-control-dogfood",
        title: "Work-bound host control",
        assurance: "turn_gated",
        mediated_effects: ["observe", "mutate_local"],
        work_binding: replacementBinding,
        capability_map_revision: 1,
        idempotency_key: "bind-bound-run-2",
      }),
    );
    assert.deepEqual(rebound.status.work_binding, replacementBinding);
    const reboundTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: rebound.routing_token,
        idempotency_key: "bound-rebound-turn",
        intent_fingerprint: fingerprint("bound-rebound-turn"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(reboundTurn.decision, "grant");
    const reboundTokens = reboundTurn.grant.delivery
      ? [reboundTurn.grant.delivery.page.delivery_token]
      : [];
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: rebound.routing_token,
          grant_id: reboundTurn.grant.grant_id,
          delivery_tokens: reboundTokens,
          idempotency_key: "begin-bound-rebound-turn",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await client.request({
          operation: "turn_checkpoint",
          routing_token: rebound.routing_token,
          grant_id: reboundTurn.grant.grant_id,
          next_intent: "continue",
          idempotency_key: "checkpoint-bound-rebound-turn",
        }),
      ).decision,
      "checkpointed",
    );

    const completionLease = ok(
      await client.request({
        operation: "lease_acquire",
        routing_token: rebound.routing_token,
        kind: "execution",
        mode: "exclusive",
        subject: sourceTree,
        ttl_seconds: 60,
        idempotency_key: "lease-bound-completion-src",
      }),
    );
    assert.equal(completionLease.decision, "granted");
    const finalMutationTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: rebound.routing_token,
        idempotency_key: "bound-final-mutation-turn",
        intent_fingerprint: fingerprint("bound-final-mutation-turn"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
        resource_intents: [libraryFile],
      }),
    );
    assert.equal(finalMutationTurn.decision, "grant");
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: rebound.routing_token,
          grant_id: finalMutationTurn.grant.grant_id,
          delivery_tokens: finalMutationTurn.grant.delivery
            ? [finalMutationTurn.grant.delivery.page.delivery_token]
            : [],
          idempotency_key: "begin-bound-final-mutation-turn",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await client.request({
          operation: "turn_checkpoint",
          routing_token: rebound.routing_token,
          grant_id: finalMutationTurn.grant.grant_id,
          next_intent: "continue",
          observations: [
            {
              observation_id: "bound-final-source-mutation",
              action_fingerprint: fingerprint("bound-final-source-mutation"),
              effect: "mutate_local",
              outcome: "succeeded",
              source_changed: true,
              source_basis: {
                workspace_id: "control-dogfood-workspace",
                source_revision: "revision-2",
              },
              observed_at: "2026-08-28T20:01:00Z",
            },
          ],
          idempotency_key: "checkpoint-bound-final-mutation-turn",
        }),
      ).decision,
      "checkpointed",
    );

    const openNext = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "next",
    );
    const openNextItem = openNext.focus.obligation_page.items.find(
      (item) => item.state === "open",
    );
    assert.ok(openNextItem, JSON.stringify(openNext.focus.obligation_page));
    assert.equal(
      openNextItem.guidance.action,
      "record_verification_then_checkpoint",
    );
    cliWorkAcknowledge(engramHome, actor, authorityGrant, openNext);
    const openFocus = cliWorkFocus(
      engramHome,
      actor,
      authorityGrant,
      proposed.work.short_ref,
    );
    assert.deepEqual(openFocus.obligation_page, openNext.focus.obligation_page);
    const openUpdate = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "update",
      {
        kind: "checkpoint",
        summary: "checkpoint typed open-obligation guidance",
        idempotency_key: "bound-open-obligation-guidance-checkpoint",
      },
    );
    assert.ok(
      openUpdate.obligation_page.items.some((item) => item.state === "open"),
    );
    assert.ok(Array.isArray(openUpdate.obligations));

    const refusedCompletionInput = {
      capture: {
        summary: "capture the attempted completion cut",
        refs: ["test:control-dogfood-open-obligation"],
      },
      acceptance: [
        { satisfied: true, note: "the control binding behavior is verified" },
      ],
      idempotency_key: "bound-open-obligation-completion",
    };
    const refusedCompletion = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "complete",
      refusedCompletionInput,
    );
    assert.equal(refusedCompletion.code, "open_work_obligations");
    assert.equal(refusedCompletion.work_id, proposed.work.work_id);
    assert.equal(refusedCompletion.obligation_page.items.length, 1);
    assert.equal(
      refusedCompletion.obligation_page.items[0].requirement.check_kind,
      "test",
    );
    assert.equal(refusedCompletion.obligation_page.omitted_count, 0);
    assert.match(refusedCompletion.remedy, /checkpoint_work acknowledging it/);
    assert.deepEqual(
      cliWork(
        engramHome,
        actor,
        authorityGrant,
        "complete",
        refusedCompletionInput,
      ),
      refusedCompletion,
    );

    const staleVerificationTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: rebound.routing_token,
        idempotency_key: "bound-stale-verification-turn",
        intent_fingerprint: fingerprint("bound-stale-verification-turn"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(staleVerificationTurn.decision, "grant");
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: rebound.routing_token,
          grant_id: staleVerificationTurn.grant.grant_id,
          delivery_tokens: staleVerificationTurn.grant.delivery
            ? [staleVerificationTurn.grant.delivery.page.delivery_token]
            : [],
          idempotency_key: "begin-bound-stale-verification-turn",
        }),
      ).decision,
      "begin",
    );
    const staleVerificationCheckpoint = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: rebound.routing_token,
        grant_id: staleVerificationTurn.grant.grant_id,
        next_intent: "continue",
        observations: [
          {
            observation_id: "bound-stale-verification",
            action_fingerprint: fingerprint("bound-stale-verification"),
            effect: "observe",
            outcome: "succeeded",
            source_changed: false,
            source_basis: {
              workspace_id: "control-dogfood-workspace",
              source_revision: "revision-1",
            },
            observed_at: "2026-08-28T20:01:30Z",
          },
        ],
        verification_evidence: [
          {
            producer_observation: {
              kind: "observation_id",
              observation_id: "bound-stale-verification",
            },
            check_kind: "test",
            summary: "stale source verification must not satisfy revision-2",
            refs: ["command:control-dogfood-stale-check"],
          },
        ],
        idempotency_key: "checkpoint-bound-stale-verification-turn",
      }),
    );
    const staleVerification =
      staleVerificationCheckpoint.receipt.verification_evidence[0];
    const staleRefusal = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "complete",
      {
        capture: {
          summary: "checkpoint stale verification without laundering it",
          refs: ["test:control-dogfood-stale-verification"],
        },
        evidence: [staleVerification],
        acceptance: [
          { satisfied: true, note: "stale evidence remains completion-ineligible" },
        ],
        idempotency_key: "bound-stale-obligation-completion",
      },
    );
    assert.equal(staleRefusal.code, "open_work_obligations");
    assert.equal(staleRefusal.obligation_page.items[0].state, "open");

    const verificationTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: rebound.routing_token,
        idempotency_key: "bound-final-verification-turn",
        intent_fingerprint: fingerprint("bound-final-verification-turn"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(verificationTurn.decision, "grant");
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: rebound.routing_token,
          grant_id: verificationTurn.grant.grant_id,
          delivery_tokens: verificationTurn.grant.delivery
            ? [verificationTurn.grant.delivery.page.delivery_token]
            : [],
          idempotency_key: "begin-bound-final-verification-turn",
        }),
      ).decision,
      "begin",
    );
    const finalEnvironmentComponents = {
      ...boundEnvironmentComponents,
      sandbox: "control-dogfood-sandbox-v2",
    };
    const finalVerificationCheckpoint = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: rebound.routing_token,
        grant_id: verificationTurn.grant.grant_id,
        next_intent: "continue",
        observations: [
          {
            observation_id: "bound-final-verification",
            action_fingerprint: fingerprint("bound-final-verification"),
            effect: "observe",
            outcome: "succeeded",
            source_changed: false,
            source_basis: {
              workspace_id: "control-dogfood-workspace",
              source_revision: "revision-2",
            },
            observed_at: "2026-08-28T20:02:00Z",
          },
        ],
        verification_evidence: [
          {
            producer_observation: {
              kind: "observation_id",
              observation_id: "bound-final-verification",
            },
            check_kind: "test",
            environment: { kind: "index", index: 0 },
            summary: "host observed the final source verification",
            refs: ["command:control-dogfood-final-check"],
          },
        ],
        environment_evidence: [
          {
            source_basis: {
              workspace_id: "control-dogfood-workspace",
              source_revision: "revision-2",
            },
            environment_fingerprint: canonicalFingerprint(
              finalEnvironmentComponents,
            ),
            components: finalEnvironmentComponents,
            observed_at: "2026-08-28T20:02:00Z",
          },
        ],
        idempotency_key: "checkpoint-bound-final-verification-turn",
      }),
    );
    const finalVerification =
      finalVerificationCheckpoint.receipt.verification_evidence[0];
    const finalEnvironment =
      finalVerificationCheckpoint.receipt.environment_evidence[0];
    cliWork(engramHome, actor, authorityGrant, "update", {
      kind: "checkpoint",
      summary: "acknowledge the final typed verification",
      evidence: [finalVerification],
      idempotency_key: "bound-final-work-checkpoint",
    });
    const completed = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "complete",
      {
        evidence: [finalVerification],
        acceptance: [
          { satisfied: true, note: "the final typed verification passed" },
        ],
        idempotency_key: "bound-obligation-completion-sealed",
      },
    );
    assert.equal(completed.work_id, proposed.work.work_id);
    assert.ok(completed.seal);
    assert.equal(
      completed.obligation_page.items.filter(
        (item) => item.state === "satisfied",
      ).length,
      2,
    );

    assert.equal(
      ok(
        await client.request({
          operation: "lease_release",
          routing_token: rebound.routing_token,
          lease_id: completionLease.lease.lease_id,
          idempotency_key: "release-bound-completion-src",
        }),
      ).lease_id,
      completionLease.lease.lease_id,
    );

    const waiverProposed = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "propose",
      {
        kind: "root",
        title: "Exercise host-private obligation waiver",
        outcome: "A human-attributed host waiver resolves one exact obligation",
        acceptance: ["The typed waiver is replayable and agent-inaccessible"],
        work_kind: "chore",
        idempotency_key: "waiver-root",
      },
    );
    const waiverClaimed = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "update",
      {
        kind: "claim",
        ttl_seconds: 300,
        idempotency_key: "waiver-claim",
      },
    );
    const waiverBinding = waiverClaimed.receipt.control_binding;
    assert.ok(waiverBinding, JSON.stringify(waiverClaimed));
    const waiverBound = ok(
      await client.request({
        operation: "session_bind",
        external_ref: "local-work:host-waiver-dogfood",
        title: "Host-private obligation waiver",
        assurance: "turn_gated",
        mediated_effects: ["observe", "mutate_local"],
        work_binding: waiverBinding,
        capability_map_revision: 1,
        idempotency_key: "bind-host-waiver-run",
      }),
    );
    const waiverSync = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: waiverBound.routing_token,
        idempotency_key: "host-waiver-sync",
        intent_fingerprint: fingerprint("host-waiver-sync"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(waiverSync.decision, "grant");
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: waiverBound.routing_token,
          grant_id: waiverSync.grant.grant_id,
          delivery_tokens: waiverSync.grant.delivery
            ? [waiverSync.grant.delivery.page.delivery_token]
            : [],
          idempotency_key: "begin-host-waiver-sync",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await client.request({
          operation: "turn_checkpoint",
          routing_token: waiverBound.routing_token,
          grant_id: waiverSync.grant.grant_id,
          next_intent: "continue",
          idempotency_key: "checkpoint-host-waiver-sync",
        }),
      ).decision,
      "checkpointed",
    );
    const waiverLease = ok(
      await client.request({
        operation: "lease_acquire",
        routing_token: waiverBound.routing_token,
        kind: "execution",
        mode: "exclusive",
        subject: sourceTree,
        ttl_seconds: 60,
        idempotency_key: "lease-host-waiver-src",
      }),
    );
    assert.equal(waiverLease.decision, "granted");
    const waiverMutationTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: waiverBound.routing_token,
        idempotency_key: "host-waiver-mutation-turn",
        intent_fingerprint: fingerprint("host-waiver-mutation-turn"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
        resource_intents: [libraryFile],
      }),
    );
    assert.equal(waiverMutationTurn.decision, "grant");
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: waiverBound.routing_token,
          grant_id: waiverMutationTurn.grant.grant_id,
          delivery_tokens: waiverMutationTurn.grant.delivery
            ? [waiverMutationTurn.grant.delivery.page.delivery_token]
            : [],
          idempotency_key: "begin-host-waiver-mutation-turn",
        }),
      ).decision,
      "begin",
    );
    assert.equal(
      ok(
        await client.request({
          operation: "turn_checkpoint",
          routing_token: waiverBound.routing_token,
          grant_id: waiverMutationTurn.grant.grant_id,
          next_intent: "continue",
          observations: [
            {
              observation_id: "host-waiver-source-mutation",
              action_fingerprint: fingerprint("host-waiver-source-mutation"),
              effect: "mutate_local",
              outcome: "succeeded",
              source_changed: true,
              source_basis: {
                workspace_id: "control-dogfood-waiver-workspace",
                source_revision: "waiver-revision-1",
              },
              observed_at: "2026-08-28T20:03:00Z",
            },
          ],
          idempotency_key: "checkpoint-host-waiver-mutation-turn",
        }),
      ).decision,
      "checkpointed",
    );
    const waiverOpenFocus = cliWorkFocus(
      engramHome,
      actor,
      authorityGrant,
      waiverProposed.work.short_ref,
    );
    const waiverOpen = waiverOpenFocus.obligation_page.items.find(
      (item) => item.state === "open",
    );
    assert.ok(waiverOpen, JSON.stringify(waiverOpenFocus.obligation_page));

    const forbiddenAgentWaiver = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "--authority-grant",
        authorityGrant,
        "update",
        "--input",
        JSON.stringify({
          kind: "waive_obligation",
          obligation_id: waiverOpen.obligation_id,
          idempotency_key: "agent-must-not-waive-obligation",
        }),
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(forbiddenAgentWaiver.status, 0);
    assert.match(forbiddenAgentWaiver.stderr, /unknown variant|waive_obligation/);

    const humanOperator = "dogfood-human-operator";
    const waiverAuthority = installObligationWaiverGrant(
      engramHome,
      humanOperator,
    );
    const notAdmittedWaiver = ok(
      await client.request({
        operation: "obligation_waive",
        routing_token: waiverBound.routing_token,
        obligation_id: waiverOpen.obligation_id,
        expected_definition: waiverOpen.definition,
        authority_grant: authorityGrant,
        waived_by: humanOperator,
        reason: "human reviewed the exact final mutation",
        idempotency_key: "host-waiver-not-admitted",
      }),
    );
    assert.equal(notAdmittedWaiver.decision, "refused");
    assert.equal(notAdmittedWaiver.code, "waiver_not_admitted");
    const wrongDefinitionWaiver = ok(
      await client.request({
        operation: "obligation_waive",
        routing_token: waiverBound.routing_token,
        obligation_id: waiverOpen.obligation_id,
        expected_definition: fingerprint("wrong-obligation-definition"),
        authority_grant: waiverAuthority,
        waived_by: humanOperator,
        reason: "human reviewed the exact final mutation",
        idempotency_key: "host-waiver-wrong-definition",
      }),
    );
    assert.equal(wrongDefinitionWaiver.decision, "refused");
    assert.equal(wrongDefinitionWaiver.code, "definition_changed");
    assert.equal(
      wrongDefinitionWaiver.current_definition,
      waiverOpen.definition,
    );
    const waiverRequest = {
      operation: "obligation_waive",
      routing_token: waiverBound.routing_token,
      obligation_id: waiverOpen.obligation_id,
      expected_definition: waiverOpen.definition,
      authority_grant: waiverAuthority,
      waived_by: humanOperator,
      reason: "human reviewed the exact final mutation",
      idempotency_key: "host-waiver-success",
    };
    const waived = ok(await client.request(waiverRequest));
    assert.equal(waived.decision, "waived");
    assert.equal(waived.receipt.waived_by, humanOperator);
    assert.equal(waived.receipt.state, "waived");
    assert.equal(JSON.stringify(waived).includes(waiverAuthority), false);
    assert.equal(
      JSON.stringify(waived).includes(waiverRequest.reason),
      false,
    );
    assert.deepEqual(ok(await client.request(waiverRequest)), waived);
    const terminalWaiver = ok(
      await client.request({
        ...waiverRequest,
        idempotency_key: "host-waiver-already-terminal",
      }),
    );
    assert.equal(terminalWaiver.decision, "refused");
    assert.equal(terminalWaiver.code, "obligation_not_open");

    const waivedFocus = cliWorkFocus(
      engramHome,
      actor,
      authorityGrant,
      waiverProposed.work.short_ref,
    );
    assert.equal(waivedFocus.obligation_page.items[0].state, "waived");
    assert.equal(
      waivedFocus.obligation_page.items[0].waived_by,
      humanOperator,
    );
    assert.equal(
      JSON.stringify(waivedFocus.obligation_page).includes(waiverAuthority),
      false,
    );
    const waiverCompleted = cliWork(
      engramHome,
      actor,
      authorityGrant,
      "complete",
      {
        capture: {
          summary: "capture completion after the authorized waiver",
          refs: ["test:control-dogfood-host-waiver"],
        },
        acceptance: [
          { satisfied: true, note: "the host-private waiver path is verified" },
        ],
        idempotency_key: "host-waiver-completion",
      },
    );
    assert.ok(waiverCompleted.seal);
    assert.equal(waiverCompleted.obligation_page.items[0].state, "waived");
    assert.equal(
      waiverCompleted.obligation_page.items[0].waived_by,
      humanOperator,
    );
    assert.equal(
      ok(
        await client.request({
          operation: "lease_release",
          routing_token: waiverBound.routing_token,
          lease_id: waiverLease.lease.lease_id,
          idempotency_key: "release-host-waiver-src",
        }),
      ).lease_id,
      waiverLease.lease.lease_id,
    );

    const freshSatisfied = cliWorkFocus(
      engramHome,
      "fresh-obligation-explainer",
      authorityGrant,
      proposed.work.short_ref,
    );
    assert.equal(
      freshSatisfied.obligation_page.items.filter(
        (item) => item.state === "satisfied",
      ).length,
      2,
    );
    const freshEnvironmentItems = freshSatisfied.evidence_items.filter(
      (item) => item.evidence_kind === "environment",
    );
    assert.equal(freshEnvironmentItems.length, 2);
    assert.deepEqual(
      freshEnvironmentItems.find(
        (item) => item.evidence === checkpointed.receipt.environment_evidence[0],
      ).environment_components,
      boundEnvironmentComponents,
    );
    assert.deepEqual(
      freshEnvironmentItems.find((item) => item.evidence === finalEnvironment)
        .environment_components,
      finalEnvironmentComponents,
    );
    assert.equal(
      freshSatisfied.evidence_items.find(
        (item) => item.evidence === finalVerification,
      ).environment,
      finalEnvironment,
    );
    const freshWaived = cliWorkFocus(
      engramHome,
      "fresh-obligation-explainer",
      authorityGrant,
      waiverProposed.work.short_ref,
    );
    assert.equal(freshWaived.obligation_page.items[0].state, "waived");
    assert.equal(
      freshWaived.obligation_page.items[0].waived_by,
      humanOperator,
    );

    const doctor = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(doctor.status, 0, doctor.stderr);
  } finally {
    await client?.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});
