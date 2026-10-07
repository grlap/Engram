#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  readFileSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { fixtureHome, removeFixtureHomes, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { join, resolve } from "node:path";
import test, { after } from "node:test";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");
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

function fingerprint(value) {
  return createHash("sha256").update(value).digest("hex");
}

function executeSql(database, sql) {
  const helper = String.raw`
    import { DatabaseSync } from "node:sqlite";
    const database = new DatabaseSync(process.argv.at(-2));
    database.exec(process.argv.at(-1));
    database.close();
  `;
  const result = spawnSync(
    process.execPath,
    [
      "--no-warnings",
      "--input-type=module",
      "--eval",
      helper,
      database,
      sql,
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(result.status, 0, result.stderr);
}

function closeChild(child, label, stderr) {
  if (child.exitCode !== null) return Promise.resolve();
  return new Promise((resolvePromise, reject) => {
    const onExit = () => {
      clearTimeout(timer);
      resolvePromise();
    };
    const timer = setTimeout(() => {
      child.removeListener("exit", onExit);
      child.kill();
      reject(new Error(`${label} shutdown timed out: ${stderr()}`));
    }, 5000);
    child.once("exit", onExit);
    child.stdin.end();
  });
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
      return closeChild(child, "SQLite writer helper", () => stderr);
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

// The opt-in phase trace: bounded JSON lines on the control process's
// stderr, keyed by this marker with the process id and frame number.
const PHASE_TRACE_ENV = "ENGRAM_MCP_PHASE_TRACE";
const CONTROL_TRACE_KEY = "engram_control_phase_trace";

function controlTraceLines(stderr) {
  return stderr
    .split("\n")
    .filter((line) => line.includes(`"${CONTROL_TRACE_KEY}"`))
    .map((line) => JSON.parse(line));
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.floor(sorted.length / 2)];
}

class ControlClient {
  constructor(engramHome, sessionId, { phaseTrace = false } = {}) {
    this.pending = [];
    this.buffer = "";
    this.stderr = "";
    const environment = { ...process.env };
    if (phaseTrace) environment[PHASE_TRACE_ENV] = "1";
    else delete environment[PHASE_TRACE_ENV];
    this.child = spawn(
      binary,
      [
        "--home",
        engramHome,
        "control",
        "--actor-id",
        sessionId,
        // The live host passes its execution context to every channel of a
        // session, in this position; a control command that refuses it dies
        // at argument parsing and the host degrades to no mediation at all.
        "--actor-context",
        "agent=dogfood;model=control-dogfood;reasoning=high",
        "--session-id",
        sessionId,
        "--source-skill",
        "engram-control-dogfood",
      ],
      { cwd: root, env: environment, stdio: ["pipe", "pipe", "pipe"] },
    );
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    // Resolves once the child's stdio has closed, after its last stderr
    // bytes arrived; "exit" can come before them.
    this.closed = new Promise((resolvePromise) => {
      this.child.once("close", () => resolvePromise());
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
    return closeChild(this.child, "control server", () => this.stderr);
  }
}

function ok(response) {
  assert.equal(response.status, "ok", JSON.stringify(response));
  return response.result;
}

function setObligationRuleSet(
  engramHome,
  input,
  idempotencyKey,
  expectedPolicy,
) {
  const args = [
    "--home",
    engramHome,
    "control-policy",
    "set-obligation-rule-set",
    "--input",
    input,
    "--authorized-by",
    "control-dogfood-policy-operator",
    "--idempotency-key",
    idempotencyKey,
  ];
  if (expectedPolicy !== undefined) {
    args.push("--expected-policy-hash", expectedPolicy);
  }
  return spawnSync(binary, args, { cwd: root, encoding: "utf8" });
}

function cliWork(
  engramHome,
  actorId,
  operation,
  input,
  expectedStatus = 0,
) {
  const args = [
    "--home",
    engramHome,
    "work",
    "--actor-id",
    actorId,
    "--session-id",
    actorId,
    "core",
    operation,
  ];
  if (input !== undefined) args.push("--input", JSON.stringify(input));
  const executed = spawnSync(binary, args, {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(executed.status, expectedStatus, executed.stderr);
  return JSON.parse(executed.stdout);
}

function cliWorkFocus(engramHome, actorId, workRef) {
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
      "core",
      "focus",
      workRef,
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(focused.status, 0, focused.stderr);
  return JSON.parse(focused.stdout);
}

function cliWorkAcknowledge(engramHome, actorId, page) {
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
      "core",
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

test("host control survives restart and gates turn dispatch", async (t) => {
  const engramHome = fixtureHome("engram-control-dogfood-", t);
  const actionGatedHome = fixtureHome("engram-control-action-gated-", t);
  let client;
  let peer;
  let advisory;
  let sqliteWriter;
  let successor;
  let failure;
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
        "--idempotency-key",
        "dogfood-action-gated",
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
        "--idempotency-key",
        "dogfood-action-recovery",
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
      /Control policy schema=1 id=([0-9a-f]{32}) epoch=1 required=advisory obligation_rules=([0-9a-f]{32})/,
    );
    assert.ok(initialPolicy, advisoryDoctor.stdout);
    const unknownRuleField = setObligationRuleSet(
      engramHome,
      JSON.stringify({ schema_version: 1, rules: [], typo: true }),
      "dogfood-rule-unknown-field",
      initialPolicy[1],
    );
    assert.notEqual(unknownRuleField.status, 0);
    assert.match(unknownRuleField.stderr, /unknown field `typo`/u);
    const unknownNestedRuleField = setObligationRuleSet(
      engramHome,
      JSON.stringify({
        schema_version: 1,
        rules: [
          {
            rule: { rule_id: "strict-nested-input", rule_version: 1 },
            trigger: "source_changed",
            requirement: { check_kind: "test", typo: true },
          },
        ],
      }),
      "dogfood-rule-unknown-nested-field",
      initialPolicy[1],
    );
    assert.notEqual(unknownNestedRuleField.status, 0);
    assert.match(unknownNestedRuleField.stderr, /unknown field `typo`/u);
    const oversizedRulePath = join(engramHome, "oversized-rule-set.json");
    writeFileSync(oversizedRulePath, " ".repeat(64 * 1024 + 1), "utf8");
    const oversizedRuleSet = setObligationRuleSet(
      engramHome,
      `@${oversizedRulePath}`,
      "dogfood-rule-oversized-input",
      initialPolicy[1],
    );
    assert.notEqual(oversizedRuleSet.status, 0);
    assert.match(oversizedRuleSet.stderr, /exceeds the 65536-byte limit/u);
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
    const advisorySyncTokens = [];
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

    for (const authorizedBy of ["", "   "]) {
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
          "--idempotency-key",
          `invalid-attribution-${authorizedBy || "missing"}`,
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
        "--idempotency-key",
        "dogfood-stale-policy",
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
        "--idempotency-key",
        "dogfood-policy-activation",
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
    const configuredReplay = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "control-policy",
        "set-required-assurance",
        "turn_gated",
        "--authorized-by",
        "dogfood-operator",
        "--idempotency-key",
        "dogfood-policy-activation",
        "--expected-policy-hash",
        initialPolicy[1],
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(configuredReplay.status, 0, configuredReplay.stderr);
    assert.deepEqual(JSON.parse(configuredReplay.stdout), configuredPolicy);
    const configuredConflict = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "control-policy",
        "set-required-assurance",
        "advisory",
        "--authorized-by",
        "dogfood-operator",
        "--idempotency-key",
        "dogfood-policy-activation",
        "--expected-policy-hash",
        initialPolicy[1],
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(configuredConflict.status, 0);
    assert.match(configuredConflict.stderr, /reused for a different intent/);
    const configuredDoctor = spawnSync(
      binary,
      ["--home", engramHome, "doctor"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(configuredDoctor.status, 0, configuredDoctor.stderr);
    const configuredPolicyLine = configuredDoctor.stdout.match(
      /Control policy schema=1 id=[0-9a-f]{32} epoch=2 required=turn_gated obligation_rules=([0-9a-f]{32})/,
    );
    assert.ok(configuredPolicyLine, configuredDoctor.stdout);
    assert.equal(configuredPolicyLine[1], initialPolicy[2]);
    const advisoryTokens = [];
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
    assert.equal(binding.status.phase, "ready");
    assert.ok(binding.routing_token);

    successor = new ControlClient(engramHome, "host-a");
    const successorStatus = ok(
      await successor.request({
        operation: "session_status",
        routing_token: binding.routing_token,
      }),
    );
    assert.equal(successorStatus.phase, "ready");
    const superseded = await client.request({
      operation: "session_status",
      routing_token: binding.routing_token,
    });
    assert.equal(superseded.status, "error");
    assert.equal(superseded.error.code, "control_connection_superseded");
    await client.close();
    client = successor;
    successor = undefined;

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
    // A grant carries no delivery page and none of the basis fields it used.
    assert.equal(Object.hasOwn(firstDecision.grant, "delivery"), false);
    for (const retired of [
      "purpose",
      "confirmed_cursor",
      "delivery_cursor",
      "blocking_watermark",
      "inline_delivery",
    ]) {
      assert.equal(Object.hasOwn(firstDecision.grant.basis, retired), false, retired);
    }
    const grant = firstDecision.grant;
    const issuedStatus = ok(
      await client.request({
        operation: "session_status",
        routing_token: binding.routing_token,
      }),
    );
    assert.equal(issuedStatus.open_grant_id, grant.grant_id);
    assert.equal(issuedStatus.open_grant_state, "issued");
    const issuedCheckpoint = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: binding.routing_token,
        grant_id: grant.grant_id,
        next_intent: "continue",
        idempotency_key: "checkpoint-issued-host-a",
      }),
    );
    assert.equal(issuedCheckpoint.decision, "refuse");
    assert.equal(issuedCheckpoint.code, "grant_not_begun");
    assert.equal(issuedCheckpoint.directive.target, "host");
    assert.equal(issuedCheckpoint.directive.satisfaction, "host_transition");
    await client.close();

    client = new ControlClient(engramHome, "host-a");
    const expiredBegin = ok(
      await client.request({
        operation: "turn_begin",
        routing_token: binding.routing_token,
        grant_id: grant.grant_id,
        delivery_tokens: [],
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
    const replacedDecision = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: binding.routing_token,
        idempotency_key: "turn-host-replace-issued",
        intent_fingerprint: fingerprint("turn-host-replace-issued"),
        purpose: "ordinary",
        requested_effects: ["observe", "communicate"],
      }),
    );
    assert.equal(replacedDecision.decision, "grant");
    assert.notEqual(
      replacedDecision.grant.grant_id,
      resumedDecision.grant.grant_id,
    );
    const resumedGrant = replacedDecision.grant;
    const begun = ok(
      await client.request({
        operation: "turn_begin",
        routing_token: binding.routing_token,
        grant_id: resumedGrant.grant_id,
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
    assert.equal(begunRestartStatus.open_grant_state, "begun");
    assert.equal(Object.hasOwn(begunRestartStatus, "recoverable_grant"), false);
    assert.equal(Object.hasOwn(begunRestartStatus, "confirmed_cursor"), false);
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

    for (const operation of ["lease_acquire", "lease_release"]) {
      const removed = await client.request({ operation, routing_token: binding.routing_token });
      assert.equal(removed.status, "error");
      assert.equal(removed.error.code, "invalid_request");
      assert.match(removed.error.message, /unknown variant/);
    }

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
          delivery_tokens: [],
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
    const mutationTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: binding.routing_token,
        idempotency_key: "turn-host-mutation-resource",
        intent_fingerprint: fingerprint("turn-host-mutation-resource"),
        purpose: "ordinary",
        requested_effects: ["mutate_local"],
        resource_intents: [libraryFile],
      }),
    );
    assert.equal(mutationTurn.decision, "grant");
    assert.equal(Object.hasOwn(mutationTurn.grant.basis, "leases"), false);
    // A peer's checkpoint no longer holds the session behind a page.
    assert.equal(Object.hasOwn(mutationTurn.grant, "delivery"), false);
    const mutationBegun = ok(
      await client.request({
        operation: "turn_begin",
        routing_token: binding.routing_token,
        grant_id: mutationTurn.grant.grant_id,
        delivery_tokens: [],
        idempotency_key: "begin-host-mutation-resource",
      }),
    );
    assert.equal(mutationBegun.decision, "begin");
    const mutationCheckpointed = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: binding.routing_token,
        grant_id: mutationTurn.grant.grant_id,
        next_intent: "continue",
        idempotency_key: "checkpoint-host-mutation-resource",
      }),
    );
    assert.equal(mutationCheckpointed.decision, "checkpointed");

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
    const doctorJson = spawnSync(
      binary,
      ["--home", engramHome, "doctor", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(doctorJson.status, 0, doctorJson.stderr);
    const diagnostics = JSON.parse(doctorJson.stdout);
    assert.equal(diagnostics.healthy, true);
    assert.equal(diagnostics.project_id, projectId);
    assert.equal(diagnostics.database, realpathSync(database));

    executeSql(
      database,
      "UPDATE control_turn_results SET decision_json = X'7B7D' " +
        "WHERE sequence = (SELECT MIN(sequence) FROM control_turn_results)",
    );
    const unhealthyDoctorJson = spawnSync(
      binary,
      ["--home", engramHome, "doctor", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(unhealthyDoctorJson.status, 0);
    const unhealthyDiagnostics = JSON.parse(unhealthyDoctorJson.stdout);
    assert.equal(unhealthyDiagnostics.healthy, false);
    assert.match(unhealthyDoctorJson.stderr, /CONTROL LIMITATION:/);
    assert.match(unhealthyDoctorJson.stderr, /development no-op redactor/);
  } catch (error) {
    failure = error;
  } finally {
    try {
      await closeFixtureClients(sqliteWriter, advisory, peer, client, successor);
    } catch (error) {
      failure = failure
        ? new AggregateError([failure, error], "Control fixture and shutdown failed", { cause: failure })
        : error;
    } finally {
      removeFixtureHomes(engramHome, actionGatedHome);
    }
  }
  if (failure) throw failure;
});

test("control session inspection emits scoped absence and refuses uncertainty", (t) => {
  const home = fixtureHome("engram-control-inspect-", t);
  const invoke = (args) => spawnSync(binary, ["--home", home, ...args], {
    cwd: root, encoding: "utf8", timeout: 30000,
  });
  const inspectArgs = ["control-session-inspect", "--target-session-id", "target",
    "--retained-grant-id", "retained", "--json"];
  const assertRefusal = (result) => {
    assert.notEqual(result.status, 0, result.stdout);
    const receipt = JSON.parse(result.stdout);
    assert.equal(receipt.scope, "control_session_inspect");
    assert.equal(receipt.schema_version, 1);
    assert.equal(receipt.mutation_enabled, false);
    assert.equal(receipt.code, "control_session_inspection_refused");
    for (const key of ["session_present", "session_grants_present", "retained_grant_present"]) {
      assert.equal(Object.hasOwn(receipt, key), false, key);
    }
  };
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root, encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
    const help = invoke(["control-session-inspect", "--help"]);
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /--target-session-id/);
    assert.match(help.stdout, /--retained-grant-id/);
    assertRefusal(invoke(inspectArgs));
    const init = invoke(["init"]);
    assert.equal(init.status, 0, init.stderr);
    const database = join(home, "projects", fingerprint(projectId), "engram.db");
    const before = readFileSync(database);
    const result = invoke(inspectArgs);
    assert.equal(result.status, 0, result.stderr);
    const receipt = JSON.parse(result.stdout);
    assert.equal(receipt.scope, "control_session_inspect");
    assert.equal(receipt.schema_version, 1);
    assert.equal(receipt.mutation_enabled, false);
    assert.equal(receipt.project_id, projectId);
    assert.equal(realpathSync(receipt.database), realpathSync(database));
    assert.equal(receipt.session_id, "target");
    assert.equal(receipt.retained_grant_id, "retained");
    assert.equal(receipt.host_path_policy.status, "matched");
    assert.equal(receipt.host_path_policy.stored, receipt.host_path_policy.resolved);
    assert.equal(receipt.session_present, false);
    assert.equal(receipt.session_grants_present, false);
    assert.equal(receipt.retained_grant_present, false);
    assert.equal(Object.hasOwn(receipt, "routing_token"), false);
    assert.deepEqual(readFileSync(database), before);
    const opposite = receipt.host_path_policy.stored.startsWith("case_fold") ? "case_sensitive" : "case_fold";
    assertRefusal(invoke(["--host-path-policy", opposite, ...inspectArgs]));
    assertRefusal(invoke(["--project-file", join(home, "absent-marker"), ...inspectArgs]));
    const invalid = invoke(["control-session-inspect", "--target-session-id", "x".repeat(65),
      "--retained-grant-id", "retained", "--json"]);
    assert.equal(invalid.status, 2, invalid.stderr);
    executeSql(database, "PRAGMA foreign_keys=OFF; INSERT INTO control_turn_grants " +
      "(grant_id,session_id,task_id,request_key,grant_json,state,issued_at_ms,expires_at_ms) " +
      "VALUES ('retained','elsewhere','missing','k',x'ff','invalid',0,1)");
    const present = invoke(inspectArgs);
    assert.equal(present.status, 0, present.stderr);
    assert.equal(JSON.parse(present.stdout).retained_grant_present, true);
    executeSql(database, "DROP TABLE control_turn_grants");
    assertRefusal(invoke(inspectArgs));
  } finally {
    removeFixtureHomes(home);
  }
});

test("doctor recovery reports a corrupt policy through a read-only surface", (t) => {
  const engramHome = fixtureHome("engram-control-policy-recovery-", t);
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
    const healthy = spawnSync(
      binary,
      ["--home", engramHome, "doctor", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(healthy.status, 0, healthy.stderr);
    const activePolicy = JSON.parse(healthy.stdout).control.policy;
    const database = join(
      engramHome,
      "projects",
      fingerprint(projectId),
      "engram.db",
    );
    executeSql(
      database,
      "UPDATE control_policy_versions SET policy_json = X'7B7D' " +
        `WHERE policy_id = '${activePolicy}'`,
    );
    const before = readFileSync(database);

    const ordinary = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.notEqual(ordinary.status, 0);
    const recovery = spawnSync(
      binary,
      ["--home", engramHome, "doctor", "--recover-policy", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(recovery.status, 0);
    const report = JSON.parse(recovery.stdout);
    assert.equal(report.mode, "control_policy_recovery");
    assert.equal(report.mutation_enabled, false);
    assert.equal(report.project_id, projectId);
    assert.equal(report.control_policy.checked_control_records, 2);
    assert.ok(
      report.control_policy.invalid_control_records.some(
        (finding) => finding.record === "control_policy_state:active",
      ),
    );
    assert.ok(
      report.control_policy.invalid_control_records.some(
        (finding) =>
          finding.record === `control_policy_version:${activePolicy}`,
      ),
    );
    assert.match(recovery.stderr, /store remains fail-closed and unchanged/);
    assert.deepEqual(readFileSync(database), before);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("projection repair is explicit and ordinary doctor never mutates", (t) => {
  const engramHome = fixtureHome("engram-projection-repair-", t);
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
    const database = join(
      engramHome,
      "projects",
      fingerprint(projectId),
      "engram.db",
    );
    executeSql(
      database,
      "DROP INDEX memory_heads_scope; DROP TABLE work_catalog_fts;",
    );
    const before = readFileSync(database);

    const ordinary = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.notEqual(ordinary.status, 0);
    assert.match(ordinary.stdout, /^remedy: "engram doctor --repair-projections"$/mu);
    assert.match(ordinary.stdout, /^code: "projection_repair_required"$/mu);
    assert.deepEqual(readFileSync(database), before);
    const ordinaryJson = spawnSync(binary, ["--home", engramHome, "doctor", "--json"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.notEqual(ordinaryJson.status, 0);
    const refusal = JSON.parse(ordinaryJson.stdout);
    assert.equal(refusal.healthy, false);
    assert.equal(refusal.code, "projection_repair_required");
    assert.equal(refusal.remedy, "engram doctor --repair-projections");
    assert.deepEqual(refusal.scope, ["indexes", "triggers", "fts"]);
    assert.deepEqual(readFileSync(database), before);

    const repaired = spawnSync(
      binary,
      ["--home", engramHome, "doctor", "--repair-projections", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(repaired.status, 0, repaired.stderr);
    const report = JSON.parse(repaired.stdout);
    assert.equal(report.mode, "projection_repair");
    assert.equal(report.mutation_enabled, true);
    assert.equal(report.healthy, true);
    const healthy = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(healthy.status, 0, healthy.stderr);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("work-bound control records observations and rebinds after a stale fence", async (t) => {
  const engramHome = fixtureHome("engram-control-work-bound-", t);
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
    const boundDoctor = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(boundDoctor.status, 0, boundDoctor.stderr);
    const boundInitialPolicy = boundDoctor.stdout.match(
      /Control policy schema=1 id=([0-9a-f]{32}) epoch=1 required=turn_gated obligation_rules=([0-9a-f]{32})/,
    );
    assert.ok(boundInitialPolicy, boundDoctor.stdout);
    const proposed = cliWork(engramHome, actor, "propose", {
      kind: "root",
      title: "Exercise work-bound control",
      outcome: "Host observations follow the exact live run claim",
      acceptance: ["The control binding survives only its exact fence"],
      work_kind: "chore",
      idempotency_key: "bound-root",
    });
    const claimed = cliWork(engramHome, actor, "update", {
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
    const syncTokens = [];
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
    const observedTokens = [];
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
    cliWork(engramHome, actor, "update", {
      kind: "checkpoint",
      summary: "record a contribution before releasing the claim",
      evidence: [verificationEvidence],
      idempotency_key: "bound-contribution-checkpoint",
    });
    cliWork(engramHome, actor, "update", {
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

    const reclaimed = cliWork(engramHome, actor, "update", {
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
    const reboundTokens = [];
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
    const reboundEnvironmentComponents = {
      ...boundEnvironmentComponents,
      sandbox: "control-dogfood-rebound-sandbox",
    };
    const reboundCheckpoint = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: rebound.routing_token,
        grant_id: reboundTurn.grant.grant_id,
        next_intent: "continue",
        environment_evidence: [
          {
            source_basis: {
              workspace_id: "control-dogfood-workspace",
              source_revision: "revision-2",
            },
            environment_fingerprint: canonicalFingerprint(
              reboundEnvironmentComponents,
            ),
            components: reboundEnvironmentComponents,
            observed_at: "2026-08-28T20:00:30Z",
          },
        ],
        idempotency_key: "checkpoint-bound-rebound-turn",
      }),
    );
    assert.equal(reboundCheckpoint.decision, "checkpointed");
    const reboundEnvironment =
      reboundCheckpoint.receipt.environment_evidence[0];
    const reboundEnvironmentFocus = cliWorkFocus(
      engramHome,
      actor,
      proposed.work.short_ref,
    );
    assert.deepEqual(
      reboundEnvironmentFocus.evidence_items.find(
        (item) => item.evidence === reboundEnvironment,
      ).environment_components,
      reboundEnvironmentComponents,
    );
    const pinnedCheckFingerprint = fingerprint("bound-final-verification");
    // A requirement cannot name an environment: environment evidence belongs
    // to one run and one source revision. The rule-set input refuses the
    // member by name, with a value or as null, and nothing is activated.
    for (const [index, environment] of [reboundEnvironment, null].entries()) {
      const environmentPinned = setObligationRuleSet(
        engramHome,
        JSON.stringify({
          schema_version: 1,
          rules: [
            {
              rule: {
                rule_id: "source_mutation_requires_pinned_environment",
                rule_version: 1,
              },
              trigger: "source_changed",
              requirement: {
                check_kind: "test",
                required_environment: environment,
              },
            },
          ],
        }),
        `dogfood-environment-pinned-rule-set-${index}`,
        boundInitialPolicy[1],
      );
      assert.notEqual(environmentPinned.status, 0);
      assert.match(
        environmentPinned.stderr,
        /unknown field `required_environment`/,
      );
    }
    const pinnedRuleSet = {
      schema_version: 1,
      rules: [
        {
          rule: {
            rule_id: "source_mutation_requires_pinned_test",
            rule_version: 1,
          },
          trigger: "source_changed",
          requirement: {
            check_kind: "test",
            check_fingerprint: pinnedCheckFingerprint,
          },
        },
      ],
    };
    const pinnedRuleActivation = setObligationRuleSet(
      engramHome,
      JSON.stringify(pinnedRuleSet),
      "dogfood-pinned-rule-set",
      boundInitialPolicy[1],
    );
    assert.equal(pinnedRuleActivation.status, 0, pinnedRuleActivation.stderr);
    const pinnedRuleReceipt = JSON.parse(pinnedRuleActivation.stdout);
    assert.equal(pinnedRuleReceipt.changed, true);
    assert.equal(pinnedRuleReceipt.policy_epoch, 2);
    assert.equal(pinnedRuleReceipt.previous_rule_set, boundInitialPolicy[2]);
    assert.match(
      pinnedRuleActivation.stderr,
      /asserted host context, not an authenticated identity/,
    );
    const pinnedRuleReplay = setObligationRuleSet(
      engramHome,
      JSON.stringify(pinnedRuleSet),
      "dogfood-pinned-rule-set",
      boundInitialPolicy[1],
    );
    assert.equal(pinnedRuleReplay.status, 0, pinnedRuleReplay.stderr);
    assert.deepEqual(JSON.parse(pinnedRuleReplay.stdout), pinnedRuleReceipt);
    assert.match(
      pinnedRuleReplay.stderr,
      /asserted host context, not an authenticated identity/,
    );
    const pinnedRuleConflict = setObligationRuleSet(
      engramHome,
      JSON.stringify({ schema_version: 1, rules: [] }),
      "dogfood-pinned-rule-set",
      boundInitialPolicy[1],
    );
    assert.notEqual(pinnedRuleConflict.status, 0);
    assert.match(pinnedRuleConflict.stderr, /reused for a different intent/);
    const pinnedRuleStaleCas = setObligationRuleSet(
      engramHome,
      JSON.stringify({ schema_version: 1, rules: [] }),
      "dogfood-pinned-rule-stale-cas",
      boundInitialPolicy[1],
    );
    assert.notEqual(pinnedRuleStaleCas.status, 0);
    assert.match(pinnedRuleStaleCas.stderr, /active control policy changed/);

    const stalePolicyTurn = ok(await client.request({
      operation: "turn_evaluate",
      routing_token: rebound.routing_token,
      idempotency_key: "bound-completion-stale-policy",
      intent_fingerprint: fingerprint("bound-completion-stale-policy"),
      purpose: "ordinary",
      requested_effects: ["observe"],
      resource_intents: [],
    }));
    assert.equal(stalePolicyTurn.decision, "refuse");
    assert.equal(stalePolicyTurn.directive.code, "policy_epoch_changed");
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
          delivery_tokens: [],
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
    assert.equal(Object.hasOwn(openNextItem.guidance, "host_waiver_requestable"), false);
    cliWorkAcknowledge(engramHome, actor, openNext);
    const openFocus = cliWorkFocus(
      engramHome,
      actor,
      proposed.work.short_ref,
    );
    assert.deepEqual(openFocus.obligation_page, openNext.focus.obligation_page);
    const openUpdate = cliWork(
      engramHome,
      actor,
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
      "complete",
      refusedCompletionInput,
      1,
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
    assert.equal(refusedCompletion.recovery.cause.kind, "open_obligation");
    assert.equal(
      refusedCompletion.recovery.cause.obligation_id,
      refusedCompletion.obligation_page.items[0].obligation_id,
    );
    assert.equal(
      refusedCompletion.recovery.cause.definition,
      refusedCompletion.obligation_page.items[0].definition,
    );
    assert.equal(refusedCompletion.recovery.cause.required_check, "test");
    assert.equal(
      refusedCompletion.recovery.item.work_id,
      proposed.work.work_id,
    );
    assert.equal(refusedCompletion.recovery.item.ref, proposed.work.short_ref);
    assert.equal(refusedCompletion.recovery.item.state, "open");
    assert.match(
      refusedCompletion.recovery.command,
      new RegExp(
        `^engram work done ${proposed.work.short_ref} --note "retry after host verification for obligation ${refusedCompletion.obligation_page.items[0].obligation_id}"$`,
        "u",
      ),
    );
    assert.deepEqual(
      cliWork(
        engramHome,
        actor,
        "complete",
        refusedCompletionInput,
        1,
      ),
      refusedCompletion,
    );
    // The agent-word `done` answers the same typed refusal in words plus the
    // resolving command, prints no hash, and exits 2.
    const owed = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "done",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(owed.status, 2, `${owed.stdout}\n${owed.stderr}`);
    assert.deepEqual(owed.stdout.split("\n").slice(0, 2), [
      `not done ${proposed.work.short_ref} "Exercise work-bound control": something is still owed [open; revision ${proposed.work.revision}]`,
      `full detail: engram work show '${proposed.work.short_ref}'`,
    ]);
    assert.match(
      owed.stdout,
      /- tests have not run since your last source change — run them; the host records the result/u,
    );
    assert.ok(
      owed.stdout.endsWith(
        `next:\n  ${refusedCompletion.recovery.command}\n`,
      ),
      owed.stdout,
    );
    assert.doesNotMatch(owed.stdout, /\b(?:[0-9a-f]{32}|[0-9a-f]{64})\b/u);

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
          delivery_tokens: [],
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
      1,
    );
    assert.equal(staleRefusal.code, "open_work_obligations");
    assert.equal(staleRefusal.obligation_page.items[0].state, "open");

    // A passed test of a different check leaves the pinned obligation open.
    const otherCheckTurn = ok(
      await client.request({
        operation: "turn_evaluate",
        routing_token: rebound.routing_token,
        idempotency_key: "bound-other-check-verification-turn",
        intent_fingerprint: fingerprint("bound-other-check-verification-turn"),
        purpose: "ordinary",
        requested_effects: ["observe"],
      }),
    );
    assert.equal(otherCheckTurn.decision, "grant");
    assert.equal(
      ok(
        await client.request({
          operation: "turn_begin",
          routing_token: rebound.routing_token,
          grant_id: otherCheckTurn.grant.grant_id,
          delivery_tokens: [],
          idempotency_key: "begin-bound-other-check-verification-turn",
        }),
      ).decision,
      "begin",
    );
    const otherCheckCheckpoint = ok(
      await client.request({
        operation: "turn_checkpoint",
        routing_token: rebound.routing_token,
        grant_id: otherCheckTurn.grant.grant_id,
        next_intent: "continue",
        observations: [
          {
            observation_id: "bound-other-check-verification",
            action_fingerprint: fingerprint("bound-other-check-verification"),
            effect: "observe",
            outcome: "succeeded",
            source_changed: false,
            source_basis: {
              workspace_id: "control-dogfood-workspace",
              source_revision: "revision-2",
            },
            observed_at: "2026-08-28T20:01:45Z",
          },
        ],
        verification_evidence: [
          {
            producer_observation: {
              kind: "observation_id",
              observation_id: "bound-other-check-verification",
            },
            check_kind: "test",
            summary: "a different check must leave the obligation open",
            refs: ["command:control-dogfood-other-check"],
          },
        ],
        idempotency_key: "checkpoint-bound-other-check-verification",
      }),
    );
    assert.equal(otherCheckCheckpoint.decision, "checkpointed");
    const pinnedOpenFocus = cliWorkFocus(
      engramHome,
      actor,
      proposed.work.short_ref,
    );
    const pinnedOpenItem = pinnedOpenFocus.obligation_page.items.find(
      (item) => item.state === "open",
    );
    assert.ok(pinnedOpenItem, JSON.stringify(pinnedOpenFocus.obligation_page));
    assert.equal(pinnedOpenItem.rule_set, pinnedRuleReceipt.obligation_rule_set);
    assert.equal(
      pinnedOpenItem.requirement.check_fingerprint,
      pinnedCheckFingerprint,
    );
    assert.equal(pinnedOpenItem.requirement.required_environment, undefined);

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
          delivery_tokens: [],
          idempotency_key: "begin-bound-final-verification-turn",
        }),
      ).decision,
      "begin",
    );
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
            environment: {
              kind: "object_id",
              object_id: reboundEnvironment,
            },
            summary: "host observed the final source verification",
            refs: ["command:control-dogfood-final-check"],
          },
        ],
        idempotency_key: "checkpoint-bound-final-verification-turn",
      }),
    );
    const finalVerification =
      finalVerificationCheckpoint.receipt.verification_evidence[0];
    cliWork(engramHome, actor, "update", {
      kind: "checkpoint",
      summary: "acknowledge the final typed verification",
      evidence: [finalVerification],
      idempotency_key: "bound-final-work-checkpoint",
    });
    const completed = cliWork(
      engramHome,
      actor,
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
    // Tested changes disclose nothing as untested.
    assert.equal(completed.obligation_page.untested_total, undefined);
    assert.ok(
      completed.obligation_page.items.every(
        (item) => item.untested_change === undefined,
      ),
    );
    const testedShow = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "show",
        proposed.work.short_ref,
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(testedShow.status, 0, testedShow.stderr);
    assert.doesNotMatch(testedShow.stdout, /untested source change/u);

    const stockRuleSet = {
      schema_version: 1,
      rules: [
        {
          rule: {
            rule_id: "source_mutation_requires_test",
            rule_version: 1,
          },
          trigger: "source_changed",
          requirement: { check_kind: "test" },
        },
      ],
    };
    const stockRuleSetPath = join(engramHome, "stock-obligation-rules.json");
    writeFileSync(stockRuleSetPath, JSON.stringify(stockRuleSet), "utf8");
    const rollbackRuleSet = setObligationRuleSet(
      engramHome,
      `@${stockRuleSetPath}`,
      "dogfood-rule-set-rollback",
      pinnedRuleReceipt.active_policy,
    );
    assert.equal(rollbackRuleSet.status, 0, rollbackRuleSet.stderr);
    const rollbackRuleReceipt = JSON.parse(rollbackRuleSet.stdout);
    assert.equal(rollbackRuleReceipt.changed, true);
    assert.equal(rollbackRuleReceipt.policy_epoch, 3);
    assert.equal(
      rollbackRuleReceipt.previous_rule_set,
      pinnedRuleReceipt.obligation_rule_set,
    );
    assert.equal(rollbackRuleReceipt.obligation_rule_set, boundInitialPolicy[2]);
    const rollbackReplay = setObligationRuleSet(
      engramHome,
      `@${stockRuleSetPath}`,
      "dogfood-rule-set-rollback",
      pinnedRuleReceipt.active_policy,
    );
    assert.equal(rollbackReplay.status, 0, rollbackReplay.stderr);
    assert.deepEqual(JSON.parse(rollbackReplay.stdout), rollbackRuleReceipt);
    const historicalPinnedFocus = cliWorkFocus(
      engramHome,
      actor,
      proposed.work.short_ref,
    );
    const historicalPinnedItem = historicalPinnedFocus.obligation_page.items.find(
      (item) => item.rule_set === pinnedRuleReceipt.obligation_rule_set,
    );
    assert.ok(
      historicalPinnedItem,
      JSON.stringify(historicalPinnedFocus.obligation_page),
    );
    assert.equal(historicalPinnedItem.state, "satisfied");
    assert.equal(
      historicalPinnedItem.requirement.check_fingerprint,
      pinnedCheckFingerprint,
    );
    assert.equal(
      historicalPinnedItem.requirement.required_environment,
      undefined,
    );

    const removedWaiver = await client.request({
      operation: "obligation_waive",
      routing_token: "removed-operation",
    });
    assert.equal(removedWaiver.status, "error");
    assert.equal(removedWaiver.error.code, "invalid_request");
    assert.match(removedWaiver.error.message, /obligation_waive/);

    const waiverProposed = cliWork(
      engramHome,
      actor,
      "propose",
      {
        kind: "root",
        title: "Exercise operator obligation waiver",
        outcome: "A human-attributed operator waiver resolves one exact obligation",
        acceptance: ["The typed waiver is replayable and agent-inaccessible"],
        work_kind: "chore",
        idempotency_key: "waiver-root",
      },
    );
    const waiverClaimed = cliWork(
      engramHome,
      actor,
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
        title: "Operator obligation waiver",
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
          delivery_tokens: [],
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
          delivery_tokens: [],
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
      waiverProposed.work.short_ref,
    );
    const waiverOpen = waiverOpenFocus.obligation_page.items.find(
      (item) => item.state === "open",
    );
    assert.ok(waiverOpen, JSON.stringify(waiverOpenFocus.obligation_page));
    assert.equal(waiverOpen.rule_set, rollbackRuleReceipt.obligation_rule_set);
    assert.equal(waiverOpen.requirement.check_fingerprint, undefined);
    assert.equal(waiverOpen.requirement.required_environment, undefined);

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
        "core",
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
    const waiverReason = "human reviewed the exact final mutation";
    const operatorWaive = (definition, key) =>
      spawnSync(
        binary,
        [
          "--home",
          engramHome,
          "authority",
          "waive-obligation",
          "--obligation-id",
          waiverOpen.obligation_id,
          "--expected-definition",
          definition,
          "--waived-by",
          humanOperator,
          "--reason",
          waiverReason,
          "--idempotency-key",
          key,
        ],
        { cwd: root, encoding: "utf8", windowsHide: true },
      );
    const wrongDefinitionWaiver = operatorWaive(
      rollbackRuleReceipt.obligation_rule_set,
      "operator-waiver-wrong-definition",
    );
    assert.notEqual(wrongDefinitionWaiver.status, 0);
    assert.match(wrongDefinitionWaiver.stderr, /definition changed/);
    const waived = operatorWaive(waiverOpen.definition, "operator-waiver-success");
    assert.equal(waived.status, 0, waived.stderr);
    assert.match(waived.stderr, /identity is asserted context/);
    const waiverReceipt = JSON.parse(waived.stdout);
    assert.equal(waiverReceipt.resolution.waived_by, humanOperator);
    const waiverReplay = operatorWaive(
      waiverOpen.definition,
      "operator-waiver-success",
    );
    assert.equal(waiverReplay.status, 0, waiverReplay.stderr);
    assert.deepEqual(JSON.parse(waiverReplay.stdout), waiverReceipt);
    const terminalWaiver = operatorWaive(
      waiverOpen.definition,
      "operator-waiver-terminal",
    );
    assert.notEqual(terminalWaiver.status, 0);
    assert.match(terminalWaiver.stderr, /already terminal/);

    const waivedFocus = cliWorkFocus(
      engramHome,
      actor,
      waiverProposed.work.short_ref,
    );
    assert.equal(waivedFocus.obligation_page.items[0].state, "waived");
    assert.equal(
      waivedFocus.obligation_page.items[0].waived_by,
      humanOperator,
    );
    assert.equal(JSON.stringify(waivedFocus).includes(waiverReason), false);
    const waiverCompleted = cliWork(
      engramHome,
      actor,
      "complete",
      {
        capture: {
          summary: "capture completion after the attributed waiver",
          refs: ["test:control-dogfood-host-waiver"],
        },
        acceptance: [
          { satisfied: true, note: "the operator waiver path is verified" },
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
    assert.deepEqual(waiverCompleted.obligation_page.items[0].untested_change, {
      observation_id: "host-waiver-source-mutation",
      source_revision: "waiver-revision-1",
      observed_at: "2026-08-28T20:03:00Z",
    });

    // The stock source-change rule records rather than blocks: `done` after
    // an untested change completes, and the item discloses the change.
    const untestedProposed = cliWork(engramHome, actor, "propose", {
      kind: "root",
      title: "Complete after an untested source change",
      outcome: "Completion records the untested change instead of refusing",
      acceptance: ["The untested change is disclosed after completion"],
      work_kind: "chore",
      idempotency_key: "untested-root",
    });
    const untestedClaimed = cliWork(engramHome, actor, "update", {
      kind: "claim",
      ttl_seconds: 300,
      idempotency_key: "untested-claim",
    });
    const untestedBound = ok(
      await client.request({
        operation: "session_bind",
        external_ref: "local-work:untested-change-dogfood",
        title: "Untested source change",
        assurance: "turn_gated",
        mediated_effects: ["observe", "mutate_local"],
        work_binding: untestedClaimed.receipt.control_binding,
        capability_map_revision: 1,
        idempotency_key: "bind-untested-run",
      }),
    );
    const untestedTurn = async (key, effects, checkpoint) => {
      const turn = ok(
        await client.request({
          operation: "turn_evaluate",
          routing_token: untestedBound.routing_token,
          idempotency_key: key,
          intent_fingerprint: fingerprint(key),
          purpose: "ordinary",
          requested_effects: effects,
          ...(effects.includes("mutate_local")
            ? { resource_intents: [libraryFile] }
            : {}),
        }),
      );
      assert.equal(turn.decision, "grant", JSON.stringify(turn));
      assert.equal(
        ok(
          await client.request({
            operation: "turn_begin",
            routing_token: untestedBound.routing_token,
            grant_id: turn.grant.grant_id,
            delivery_tokens: [],
            idempotency_key: `begin-${key}`,
          }),
        ).decision,
        "begin",
      );
      assert.equal(
        ok(
          await client.request({
            operation: "turn_checkpoint",
            routing_token: untestedBound.routing_token,
            grant_id: turn.grant.grant_id,
            next_intent: "continue",
            ...checkpoint,
            idempotency_key: `checkpoint-${key}`,
          }),
        ).decision,
        "checkpointed",
      );
    };
    await untestedTurn("untested-sync", ["observe"], {});
    await untestedTurn("untested-mutation", ["mutate_local"], {
      observations: [
        {
          observation_id: "untested-source-mutation",
          action_fingerprint: fingerprint("untested-source-mutation"),
          effect: "mutate_local",
          outcome: "succeeded",
          source_changed: true,
          source_basis: {
            workspace_id: "control-dogfood-untested-workspace",
            source_revision: "untested-revision-1",
          },
          observed_at: "2026-08-28T20:04:00Z",
        },
      ],
    });
    const untestedLine =
      "untested source change: untested-source-mutation (source revision untested-revision-1; detection not reported); no matching passing test followed it";
    const untestedDone = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "done",
        untestedProposed.work.short_ref,
        "the untested change is recorded",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(untestedDone.status, 0, `${untestedDone.stdout}\n${untestedDone.stderr}`);
    assert.match(untestedDone.stdout, /^done w-/u);
    assert.ok(untestedDone.stdout.includes(untestedLine), untestedDone.stdout);
    const untestedShow = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "show",
        untestedProposed.work.short_ref,
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(untestedShow.status, 0, untestedShow.stderr);
    assert.ok(untestedShow.stdout.includes(untestedLine), untestedShow.stdout);
    const untestedShowJson = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "show",
        untestedProposed.work.short_ref,
        "--json",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(untestedShowJson.status, 0, untestedShowJson.stderr);
    const untestedShown = JSON.parse(untestedShowJson.stdout);
    assert.deepEqual(untestedShown.untested_changes, [
      {
        observation_id: "untested-source-mutation",
        source_revision: "untested-revision-1",
        observed_at: "2026-08-28T20:04:00Z",
      },
    ]);
    assert.equal(untestedShown.untested_changes_omitted, undefined);
    const untestedFocus = cliWorkFocus(
      engramHome,
      actor,
      untestedProposed.work.short_ref,
    );
    assert.equal(untestedFocus.status.work.lifecycle, "completed");
    assert.equal(untestedFocus.obligation_page.items.length, 1);
    assert.equal(untestedFocus.obligation_page.items[0].state, "waived");
    assert.equal(untestedFocus.obligation_page.items[0].waived_by, actor);
    assert.equal(
      untestedFocus.obligation_page.items[0].untested_change.observation_id,
      "untested-source-mutation",
    );

    const freshSatisfied = cliWorkFocus(
      engramHome,
      "fresh-obligation-explainer",
      proposed.work.short_ref,
    );
    assert.equal(
      freshSatisfied.obligation_page.items.filter(
        (item) => item.state === "satisfied",
      ).length,
      2,
    );
    const freshWaived = cliWorkFocus(
      engramHome,
      "fresh-obligation-explainer",
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
    try {
      await client?.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});


test("evaluation admission causes survive the native CLI and host-recorded evidence", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  for (const caseName of ["eligibility", "source_root", "wrong_run", "beyond_cut", "wrong_source"]) {
    const engramHome = fixtureHome(`engram-admission-${caseName.replaceAll("_", "-")}-`, t);
    const clients = [];
    const actor = `admission-${caseName}`;
    const word = (session, ...args) => spawnSync(binary,
      ["--home", engramHome, "work", "--actor-id", session, "--session-id", session, ...args],
      { cwd: root, encoding: "utf8" });
    const jsonWord = (session, ...args) => {
      const result = word(session, ...args, "--json");
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    try {
      const init = spawnSync(binary, ["--home", engramHome, "init"], { cwd: root, encoding: "utf8" });
      assert.equal(init.status, 0, init.stderr);
      const policy = spawnSync(binary, ["--home", engramHome, "control-policy", "set-acceptance-evaluation",
        "--modes", caseName === "eligibility" ? "same-session,independent-session" : "same-session",
        "--mechanical-basis", "observed", "--authorized-by", "operator", "--idempotency-key", "admission-policy"],
      { cwd: root, encoding: "utf8" });
      assert.equal(policy.status, 0, policy.stderr);
      const bound = ["wrong_run", "beyond_cut", "wrong_source"].includes(caseName);
      const create = (session, title) => {
        const added = jsonWord(session, "add", title, "--accept", "the check supports the outcome",
          ...(bound ? ["--bind", "1=test"] : []));
        jsonWord(session, "claim", added.work.short_ref);
        return added.work.short_ref;
      };
      const ref = create(actor, "Inspect typed admission");
      const attachHost = async (session, targetRef) => {
        const binding = cliWorkFocus(engramHome, session, targetRef).control_binding;
        const client = new ControlClient(engramHome, session);
        clients.push(client);
        const control = ok(await client.request({ operation: "session_bind", external_ref: `local-work:${session}`,
          title: "Admission check host", assurance: "turn_gated", mediated_effects: ["observe", "mutate_local"],
          work_binding: binding, capability_map_revision: 1, idempotency_key: "admission-host" }));
        let turn = 0;
        const checkpoint = async (revision, check) => {
          const key = `admission-turn-${++turn}`;
          const effects = revision ? ["mutate_local"] : ["observe"];
          const granted = ok(await client.request({ operation: "turn_evaluate", routing_token: control.routing_token,
            idempotency_key: key, intent_fingerprint: fingerprint(key), purpose: "ordinary", requested_effects: effects,
            ...(revision ? { resource_intents: [libraryFile] } : {}) }));
          assert.equal(granted.decision, "grant", JSON.stringify(granted));
          assert.equal(ok(await client.request({ operation: "turn_begin", routing_token: control.routing_token,
            grant_id: granted.grant.grant_id, delivery_tokens: [], idempotency_key: `begin-${key}` })).decision, "begin");
          const source = { workspace_id: "admission-workspace", source_revision: revision };
          const time = new Date().toISOString();
          const components = { toolchain: "admission-check", sandbox: "admission-test", workspace_id: source.workspace_id,
            capability_map_revision: 1 };
          return ok(await client.request({ operation: "turn_checkpoint", routing_token: control.routing_token,
            grant_id: granted.grant.grant_id, next_intent: "continue", idempotency_key: `checkpoint-${key}`,
            ...(revision ? { observations: [{ observation_id: key, action_fingerprint: fingerprint(key), effect: "mutate_local",
              outcome: "succeeded", source_changed: true, source_basis: source, observed_at: time }] } : {}),
            ...(check ? {
              verification_evidence: [{ producer_observation: { kind: "observation_id", observation_id: key }, check_kind: "test",
                environment: { kind: "index", index: 0 }, summary: "passed host check", refs: ["command:admission-check"] }],
              environment_evidence: [{ source_basis: source, environment_fingerprint: canonicalFingerprint(components),
                components, observed_at: time }],
            } : {}),
          }));
        };
        await checkpoint(null, false);
        return { client, control, binding, checkpoint };
      };
      let citation;
      let beforeCheck;
      if (!bound) {
        jsonWord(actor, "gate", "admission-note", "--work-ref", ref);
        citation = jsonWord(actor, "show", ref, "--notes", "--gates").notes
          .find((row) => String(row.family).toLowerCase() === "gates").locator;
      }
      if (caseName === "source_root") {
        const host = await attachHost(actor, ref);
        ok(await host.client.request({ operation: "named_root_bind", routing_token: host.control.routing_token,
          claim_id: host.binding.claim_id, claim_fence: host.binding.claim_fence, workspace_id: "C:/database is locked",
          generation: 1, named_at: new Date().toISOString(), kind: "bound", idempotency_key: "named-admission-root" }));
      }
      if (caseName === "wrong_run") {
        const other = "foreign-admission-runner";
        const otherRef = create(other, "Evidence belongs to another run");
        const host = await attachHost(other, otherRef);
        citation = (await host.checkpoint("admission-R", true)).receipt.verification_evidence[0];
      }
      if (caseName === "beyond_cut" || caseName === "wrong_source") {
        const host = await attachHost(actor, ref);
        beforeCheck = jsonWord(actor, "show", ref).evidence_basis;
        citation = (await host.checkpoint("admission-R", true)).receipt.verification_evidence[0];
        if (caseName === "wrong_source") await host.checkpoint("admission-S", false);
      }
      const shown = jsonWord(actor, "show", ref);
      const cut = caseName === "beyond_cut" ? beforeCheck : shown.evidence_basis;
      const revision = caseName === "wrong_source" ? "admission-S" : "admission-R";
      const result = word(actor, "evaluate", ref, "--mode", "same-session", "--acceptance-basis", String(shown.acceptance_basis),
        "--evidence-basis", String(cut), "--verdict", `1=pass:${bound ? "observed" : "judgment"}`,
        "--rationale", "1=judge the supplied evidence", "--evidence", `1=${citation}`,
        ...(caseName === "eligibility" || caseName === "wrong_run" ? [] : ["--source-fingerprint", revision]), "--json");
      assert.equal(result.status, 1, `${caseName}: ${result.stderr}`);
      const error = JSON.parse(result.stderr).error;
      assert.equal(error.code, "acceptance_evaluation_refused");
      const cause = error.details.cause;
      assert.equal(cause.kind, bound ? "citation" : caseName);
      assert.equal(cause.mismatch, ({ eligibility: "same_session_unmarked", source_root: "no_initial_sighting",
        wrong_run: "not_on_run", beyond_cut: "beyond_cut", wrong_source: "wrong_source" })[caseName]);
      assert.equal(typeof error.details.remedy, "string");
      assert.ok(error.reminders.includes(error.details.remedy));
      assert.ok(error.next.some((command) => command.includes(ref)));
      if (caseName === "source_root") {
        assert.equal(cause.workspace_id, "C:/database is locked");
        assert.equal(cause.declared_revision, revision);
        assert.doesNotMatch(result.stderr.toLowerCase(), /database is locked/u);
      }
      if (bound) {
        assert.equal(cause.citation, citation);
        assert.equal(cause.evaluated_cut, cut);
        assert.equal(cause.requirement.check_kind, "test");
      }
    } finally {
      for (const client of clients) await client.close();
      removeFixtureHomes(engramHome);
    }
  }
});

test("a host reads what satisfied each bound criterion and lists its candidates in closed page shapes", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  const engramHome = fixtureHome("engram-binding-read-", t);
  const actor = "binding-read-runner";
  const jsonWord = (...args) => {
    const result = spawnSync(binary, ["--home", engramHome, "work", "--actor-id", actor, "--session-id", actor,
      ...args, "--json"], { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  let client;
  try {
    const init = spawnSync(binary, ["--home", engramHome, "init"], { cwd: root, encoding: "utf8" });
    assert.equal(init.status, 0, init.stderr);
    const ref = jsonWord("add", "Read bound evidence", "--accept", "1. the tests pass", "--accept", "2. the docs say so",
      "--bind", "1=test").work.short_ref;
    const revision = jsonWord("claim", ref).work.revision;
    const binding = cliWorkFocus(engramHome, actor, ref).control_binding;
    client = new ControlClient(engramHome, actor);
    const control = ok(await client.request({ operation: "session_bind", external_ref: `local-work:${actor}`,
      title: "Binding read host", assurance: "turn_gated", mediated_effects: ["observe", "mutate_local"],
      work_binding: binding, capability_map_revision: 1, idempotency_key: "binding-read-host" }));
    const key = "binding-read-check";
    const granted = ok(await client.request({ operation: "turn_evaluate", routing_token: control.routing_token,
      idempotency_key: key, intent_fingerprint: fingerprint(key), purpose: "ordinary",
      requested_effects: ["mutate_local"], resource_intents: [libraryFile] }));
    assert.equal(granted.decision, "grant", JSON.stringify(granted));
    assert.equal(ok(await client.request({ operation: "turn_begin", routing_token: control.routing_token,
      grant_id: granted.grant.grant_id, delivery_tokens: [], idempotency_key: `begin-${key}` })).decision, "begin");
    const source = { workspace_id: "binding-read-workspace", source_revision: "binding-read-R1" };
    const time = new Date().toISOString();
    const components = { toolchain: "binding-read-check", sandbox: "binding-read-test",
      workspace_id: source.workspace_id, capability_map_revision: 1 };
    const checked = ok(await client.request({ operation: "turn_checkpoint", routing_token: control.routing_token,
      grant_id: granted.grant.grant_id, next_intent: "continue", idempotency_key: `checkpoint-${key}`,
      observations: [{ observation_id: key, action_fingerprint: fingerprint(key), effect: "mutate_local",
        outcome: "succeeded", source_changed: true, source_basis: source, observed_at: time }],
      verification_evidence: [{ producer_observation: { kind: "observation_id", observation_id: key },
        check_kind: "test", environment: { kind: "index", index: 0 }, summary: "passed host check",
        refs: ["command:binding-read-check"] }],
      environment_evidence: [{ source_basis: source, environment_fingerprint: canonicalFingerprint(components),
        components, observed_at: time }] }));
    const verification = checked.receipt.verification_evidence[0];
    const request = { operation: "acceptance_binding_read", routing_token: control.routing_token,
      work_id: binding.work_id, expected_work_revision: revision, run_id: binding.run_id };
    const page = ok(await client.request(request));
    assert.deepEqual(Object.keys(page).sort(),
      ["basis", "continuation", "earlier", "omitted", "rows", "shown", "total"]);
    assert.deepEqual(Object.keys(page.basis).sort(), ["project_id", "run_cut", "run_id", "work_id", "work_revision"]);
    assert.equal(page.basis.work_id, binding.work_id);
    assert.equal(page.basis.run_id, binding.run_id);
    assert.equal(page.basis.work_revision, revision);
    assert.equal(Number.isInteger(page.basis.run_cut), true);
    assert.deepEqual([page.total, page.earlier, page.shown, page.omitted, page.continuation], [2, 0, 2, 0, null]);
    assert.deepEqual(page.rows.map((row) => Object.keys(row).sort()), [["binding", "criterion"], ["binding", "criterion"]]);
    assert.deepEqual(page.rows[1], { criterion: 2, binding: null });
    const bound = page.rows[0].binding;
    assert.deepEqual(Object.keys(bound).sort(), ["obligation", "requirement"]);
    assert.deepEqual(bound.requirement, { check_kind: "test" });
    assert.deepEqual(Object.keys(bound.obligation).sort(), ["definition", "definition_position", "obligation_id",
      "resolution", "rule", "state", "trigger_position", "triggering_observation", "work_revision"]);
    assert.equal(bound.obligation.state, "satisfied");
    assert.deepEqual(Object.keys(bound.obligation.resolution).sort(), ["kind", "position", "record", "satisfaction"]);
    assert.equal(bound.obligation.resolution.kind, "satisfied");
    const satisfaction = bound.obligation.resolution.satisfaction;
    assert.deepEqual(Object.keys(satisfaction).sort(), ["evaluated_cut", "verification"]);
    assert.deepEqual(Object.keys(satisfaction.verification).sort(), ["check_fingerprint", "check_kind", "position",
      "producer", "record", "result", "source_basis"]);
    assert.equal(satisfaction.verification.record, verification);
    assert.equal(satisfaction.verification.result, "passed");
    assert.deepEqual(satisfaction.verification.source_basis, source);
    assert.deepEqual(Object.keys(satisfaction.verification.producer).sort(), ["outcome", "position", "record"]);
    assert.equal(satisfaction.verification.producer.outcome, "succeeded");
    assert.ok(satisfaction.verification.position <= page.basis.run_cut);
    // Each refusal answers with its own code, distinct from an empty page.
    const stale = await client.request({ ...request, expected_work_revision: revision + 1 });
    assert.equal(stale.error.code, "acceptance_binding_read_wrong_revision", JSON.stringify(stale));
    const cursor = await client.request({ ...request, after: "abr1-zz" });
    assert.equal(cursor.error.code, "acceptance_binding_read_invalid_cursor", JSON.stringify(cursor));
    const caller = await client.request({ ...request, run_cut: page.basis.run_cut });
    assert.equal(caller.error.code, "invalid_request", JSON.stringify(caller));
    assert.match(caller.error.message, /run_cut/u);

    // The sibling read lists, at the same cut, every check of the bound kind.
    const candidates = { operation: "acceptance_verification_read", routing_token: control.routing_token,
      work_id: binding.work_id, expected_work_revision: revision, run_id: binding.run_id,
      run_cut: page.basis.run_cut, criterion: 1 };
    const listed = ok(await client.request(candidates));
    assert.deepEqual(Object.keys(listed).sort(), ["basis", "continuation", "criterion", "earlier", "omitted",
      "requirement", "rows", "shown", "total"]);
    assert.deepEqual(listed.basis, page.basis);
    assert.equal(listed.criterion, 1);
    assert.deepEqual(listed.requirement, { check_kind: "test" });
    assert.deepEqual([listed.total, listed.earlier, listed.shown, listed.omitted, listed.continuation],
      [1, 0, 1, 0, null]);
    assert.deepEqual(listed.rows, [satisfaction.verification]);
    // An unbound criterion lists nothing, which is not a pass.
    const unbound = ok(await client.request({ ...candidates, criterion: 2 }));
    assert.deepEqual([unbound.requirement, unbound.total, unbound.rows, unbound.continuation], [null, 0, [], null]);
    for (const [change, code] of [
      [{ criterion: 3 }, "acceptance_verification_read_invalid_criterion"],
      [{ run_cut: page.basis.run_cut + 1 }, "acceptance_verification_read_stale_cut"],
      [{ after: "abr1-zz" }, "acceptance_verification_read_invalid_cursor"],
      [{ expected_work_revision: revision + 1 }, "acceptance_verification_read_wrong_revision"],
    ]) {
      const refused = await client.request({ ...candidates, ...change });
      assert.equal(refused.error.code, code, JSON.stringify(refused));
    }
    const chosenKind = await client.request({ ...candidates, check_kind: "lint" });
    assert.equal(chosenKind.error.code, "invalid_request", JSON.stringify(chosenKind));
  } finally {
    if (client) await client.close();
    removeFixtureHomes(engramHome);
  }
});

// B77/B78 and B21/B22: native completion consumes host reports, not promises.
test("source recovery keeps a judgment through host confirmation and separates fingerprint remedies", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  const engramHome = fixtureHome("engram-source-recovery-", t);
  const actor = "source-recovery-runner";
  const word = (...args) => spawnSync(binary,
    ["--home", engramHome, "work", "--actor-id", actor, "--session-id", actor, ...args],
    { cwd: root, encoding: "utf8" });
  const jsonWord = (...args) => {
    const result = word(...args, "--json");
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  const setPolicy = (fresh) => {
    const result = spawnSync(binary, ["--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session", "--mechanical-basis", "asserted",
      ...(fresh ? ["--require-source-freshness"] : []), "--authorized-by", "operator",
      "--idempotency-key", `source-policy-${fresh}`], { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
  };
  const create = (title) => {
    const ref = jsonWord("add", title, "--accept", "the outcome is delivered").work.short_ref;
    jsonWord("claim", ref);
    jsonWord("gate", "source-evidence", "--work-ref", ref);
    return ref;
  };
  const evaluate = (ref, revision) => {
    const shown = jsonWord("show", ref);
    const citation = jsonWord("show", ref, "--notes", "--gates").notes
      .find((row) => String(row.family).toLowerCase() === "gates").locator;
    return jsonWord("evaluate", ref, "--mode", "same-session",
      "--acceptance-basis", String(shown.acceptance_basis), "--evidence-basis", String(shown.evidence_basis),
      "--verdict", "1=pass:judgment", "--rationale", "1=judge the recorded outcome", "--evidence", `1=${citation}`,
      ...(revision ? ["--source-fingerprint", revision] : []));
  };
  const refused = (ref, mismatch, action, presented) => {
    const result = word("done", ref, "Delivered", ...(presented ? ["--source-fingerprint", presented] : []), "--json");
    assert.equal(result.status, 2, result.stderr);
    const value = JSON.parse(result.stdout);
    assert.equal(value.code, "acceptance_evaluation_stale");
    assert.deepEqual(value.recovery.cause, { kind: "acceptance_evaluation_stale", reason: "source" });
    assert.equal(value.recovery.source.mismatch, mismatch);
    assert.equal(value.recovery.source.remedy, action);
    assert.ok(value.reminders.some((line) => line.includes(value.remedy)));
    assert.ok(value.next.some((command) => command.includes(ref)));
    return { result, value };
  };
  let client;
  try {
    const init = spawnSync(binary, ["--home", engramHome, "init"], { cwd: root, encoding: "utf8" });
    assert.equal(init.status, 0, init.stderr);
    setPolicy(false);
    const ref = create("Declared root confirmation");
    const binding = cliWorkFocus(engramHome, actor, ref).control_binding;
    client = new ControlClient(engramHome, actor);
    const control = ok(await client.request({ operation: "session_bind", external_ref: `local-work:${actor}`,
      title: "Source recovery host", assurance: "turn_gated", mediated_effects: ["observe", "mutate_local"],
      work_binding: binding, capability_map_revision: 1, idempotency_key: "source-host" }));
    const workspace = "C:/database is locked/源";
    ok(await client.request({ operation: "named_root_bind", routing_token: control.routing_token,
      claim_id: binding.claim_id, claim_fence: binding.claim_fence, workspace_id: workspace,
      generation: 1, named_at: new Date().toISOString(), kind: "bound", idempotency_key: "source-root" }));
    let turn = 0;
    const sight = async (revision) => {
      const key = `source-sighting-${++turn}`;
      const granted = ok(await client.request({ operation: "turn_evaluate", routing_token: control.routing_token,
        idempotency_key: key, intent_fingerprint: fingerprint(key), purpose: "ordinary",
        requested_effects: ["mutate_local"], resource_intents: [libraryFile] }));
      assert.equal(granted.decision, "grant", JSON.stringify(granted));
      assert.equal(ok(await client.request({ operation: "turn_begin", routing_token: control.routing_token,
        grant_id: granted.grant.grant_id, delivery_tokens: [], idempotency_key: `begin-${key}` })).decision, "begin");
      return ok(await client.request({ operation: "turn_checkpoint", routing_token: control.routing_token,
        grant_id: granted.grant.grant_id, next_intent: "continue", idempotency_key: `checkpoint-${key}`,
        observations: [{ observation_id: key, action_fingerprint: fingerprint(key), effect: "mutate_local",
          outcome: "succeeded", source_changed: false, source_basis: { workspace_id: workspace,
            source_revision: revision, source_root_generation: 1, source_root_state: "named" },
          observed_at: new Date().toISOString() }] }));
    };
    await sight("R1");
    const recorded = evaluate(ref, "R2");
    const pending = refused(ref, "unconfirmed_declaration", "end_turn_read_and_retry");
    assert.equal(pending.value.recovery.source.evaluation, recorded.evaluation.evaluation);
    assert.equal(pending.value.recovery.source.workspace_id, workspace);
    assert.equal(pending.value.recovery.source.declared_revision, "R2");
    assert.equal(pending.value.recovery.source.reported_revision, "R1");
    assert.doesNotMatch(pending.result.stdout.toLowerCase(), /database is locked/u);
    assert.match(pending.value.remedy, /no future report is promised/u);
    const read = jsonWord("show", ref);
    assert.deepEqual(read.acceptance_evaluation.source_recovery, pending.value.recovery.source);
    // No further report: a later retry has exactly the same knowledge and action.
    assert.deepEqual(refused(ref, "unconfirmed_declaration", "end_turn_read_and_retry").value.recovery.source,
      pending.value.recovery.source);
    await sight("R2");
    const sealed = jsonWord("done", ref, "Delivered");
    assert.equal(sealed.work.lifecycle, "completed");
    assert.equal(sealed.acceptance.evaluation, recorded.evaluation.evaluation);
    // The host reads the finished claim's named root: none, with the real
    // bound event still named, in the closed shape it consumes.
    const rootRead = ok(await client.request({ operation: "named_root_read", routing_token: control.routing_token,
      run_id: binding.run_id, claim_id: binding.claim_id }));
    assert.deepEqual(Object.keys(rootRead).sort(), ["claim", "claim_id", "latest_event", "named_root", "project_id",
      "read_cut", "root_execution_id", "run", "run_id", "work_id"]);
    assert.equal(rootRead.run_id, binding.run_id);
    assert.equal(rootRead.claim_id, binding.claim_id);
    assert.equal(rootRead.work_id, binding.work_id);
    assert.equal(rootRead.root_execution_id, binding.root_execution_id);
    assert.deepEqual(rootRead.named_root, { state: "none" });
    assert.deepEqual(Object.keys(rootRead.run).sort(), ["generation", "state"]);
    assert.equal(rootRead.run.state, "completed");
    assert.deepEqual(Object.keys(rootRead.claim).sort(), ["expires_at", "fence", "holder", "revision", "state"]);
    assert.equal(rootRead.claim.state, "completed");
    // A bound event carries no end_reason; the cut and positions are closed too.
    assert.deepEqual(Object.keys(rootRead.latest_event).sort(),
      ["event", "generation", "kind", "named_at", "position", "workspace_id"]);
    assert.deepEqual(Object.keys(rootRead.latest_event.position).sort(), ["feed", "position"]);
    assert.deepEqual(Object.keys(rootRead.read_cut).sort(), ["feed", "position"]);
    assert.equal(rootRead.latest_event.kind, "bound");
    assert.equal(rootRead.latest_event.generation, 1);
    assert.equal(rootRead.latest_event.workspace_id, workspace);
    assert.match(rootRead.latest_event.event, /^[0-9a-f]{32}$/u);
    assert.deepEqual(rootRead.latest_event.position.feed, { kind: "run_execution", id: binding.run_id });
    assert.deepEqual(rootRead.read_cut.feed, rootRead.latest_event.position.feed);
    assert.ok(rootRead.latest_event.position.position <= rootRead.read_cut.position);
    const unknownRun = await client.request({ operation: "named_root_read", routing_token: control.routing_token,
      run_id: "00000000-0000-0000-0000-000000000007", claim_id: binding.claim_id });
    assert.equal(unknownRun.status, "error", JSON.stringify(unknownRun));
    assert.equal(unknownRun.error.code, "named_root_read_refused");
    // The host reads the root's initial sighting with no routing token: the
    // finished run still reads the root recording selected, sighted at R2.
    const sighting = ok(await client.request({ operation: "named_root_sighting_read", work_ref: ref,
      run_id: binding.run_id }));
    assert.deepEqual(Object.keys(sighting).sort(), ["binding_changed", "current_binding", "head_cut", "project_id",
      "read_cut", "root", "run_id", "schema_version", "work_id"]);
    assert.equal(sighting.schema_version, 1);
    assert.equal(sighting.run_id, binding.run_id);
    assert.equal(sighting.work_id, binding.work_id);
    assert.equal(sighting.read_cut, sighting.head_cut);
    assert.equal(sighting.binding_changed, false);
    assert.equal(sighting.root.state, "bound");
    assert.equal(sighting.root.workspace_id, workspace);
    assert.equal(sighting.root.binding_event, rootRead.latest_event.event);
    assert.equal(sighting.current_binding, rootRead.latest_event.event);
    assert.equal(sighting.root.sighting.state, "present");
    assert.equal(sighting.root.sighting.revision, "R2");
    assert.match(sighting.root.sighting.record, /^[0-9a-f]{32}$/u);
    const wrongRun = await client.request({ operation: "named_root_sighting_read", work_ref: ref,
      run_id: "00000000-0000-0000-0000-000000000007" });
    assert.equal(wrongRun.status, "error", JSON.stringify(wrongRun));
    assert.equal(wrongRun.error.code, "named_root_sighting_read_wrong_run");
    await client.close();
    client = null;

    setPolicy(true);
    const measuredRef = create("Fresh measurement required");
    const measured = evaluate(measuredRef, "measured-A");
    const missing = refused(measuredRef, "completion_measurement_missing", "measure_source_and_retry");
    assert.equal(missing.value.recovery.source.evaluation, measured.evaluation.evaluation);
    assert.match(missing.value.remedy, /fresh source measurement/u);
    const mismatch = refused(measuredRef, "completion_fingerprint_mismatch", "evaluate_current_source", "measured-B");
    assert.match(mismatch.value.remedy, /new acceptance evaluation/u);
    assert.match(mismatch.value.remedy, /copying.*insufficient/u);
    assert.equal(jsonWord("show", measuredRef).acceptance_evaluation.source_recovery, undefined);
    assert.equal(jsonWord("done", measuredRef, "Delivered", "--source-fingerprint", "measured-A").work.lifecycle, "completed");
    const absentRef = create("No evaluated source basis");
    evaluate(absentRef);
    const absent = refused(absentRef, "evaluation_source_basis_missing", "evaluate_current_source", "measured-A");
    assert.equal(absent.value.recovery.source.expected_fingerprint, undefined);
    assert.match(absent.value.remedy, /new acceptance evaluation/u);
    assert.match(absent.value.remedy, /copying.*insufficient/u);
  } finally {
    if (client) await client.close();
    removeFixtureHomes(engramHome);
  }
});

test("the opt-in control phase trace numbers every frame and an unset process writes none", async (t) => {
  const engramHome = fixtureHome("engram-control-phase-trace-", t);
  const clients = [];
  let failure;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
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
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(initialized.status, 0, initialized.stderr);
    const untraced = new ControlClient(engramHome, "trace-off", { phaseTrace: false });
    const traced = new ControlClient(engramHome, "trace-on", { phaseTrace: true });
    clients.push(untraced, traced);
    const status = { operation: "session_status", routing_token: "dogfood-unknown-token" };
    // The same frames on each process, interleaved, timed by the client.
    const timings = { off: [], on: [] };
    const answers = { off: [], on: [] };
    const rounds = 20;
    for (let round = 0; round < rounds; round++) {
      for (const [name, client] of [["off", untraced], ["on", traced]]) {
        const started = performance.now();
        const response = await client.request(status);
        timings[name].push(performance.now() - started);
        answers[name].push(response);
      }
    }
    await untraced.close();
    await traced.close();
    await untraced.closed;
    await traced.closed;
    // The trace changes no response: the answers differ only by the session
    // each process speaks for.
    const normalized = (response, session) =>
      canonicalJson(response).replaceAll(session, "SESSION");
    assert.deepEqual(
      answers.on.map((response) => normalized(response, "trace-on")),
      answers.off.map((response) => normalized(response, "trace-off")),
    );
    assert.deepEqual(controlTraceLines(untraced.stderr), [], untraced.stderr);
    const lines = controlTraceLines(traced.stderr);
    const startup = lines.filter((line) => line.seq === 0 && line.state !== "in_flight");
    assert.equal(startup.length, 1, traced.stderr);
    assert.equal(startup[0].kind, "startup");
    assert.equal(startup[0].state, "complete");
    assert.equal(startup[0].pid, traced.child.pid);
    for (let seq = 1; seq <= rounds; seq++) {
      const terminal = lines.filter((line) => line.seq === seq && line.state !== "in_flight");
      assert.equal(terminal.length, 1, `seq ${seq}: ${traced.stderr}`);
      assert.equal(terminal[0].state, "complete");
      assert.equal(terminal[0].operation, "session_status");
      assert.equal(typeof terminal[0].phases.handler_total_ms, "number");
      assert.equal(typeof terminal[0].phases.response_write_flush_ms, "number");
    }
    assert.equal(lines.filter((line) => line.seq > rounds).length, 0);
    const off = median(timings.off);
    const on = median(timings.on);
    t.diagnostic(`session_status over control, interleaved ${rounds} rounds: trace on median=${on.toFixed(2)}ms, trace off median=${off.toFixed(2)}ms`);
    // A trace that is off does no added work; one that is on stays within
    // noise of it on this small workload.
    assert.ok(on < off * 2 + 5, `trace on ${on}ms against off ${off}ms`);

    // A control process whose project cannot be resolved still leaves its
    // startup record, incomplete, with the phase it stopped in.
    const refused = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "--project-file",
        join(engramHome, "missing.engram-project"),
        "control",
        "--actor-id",
        "trace-refused",
        "--session-id",
        "trace-refused",
      ],
      { cwd: root, encoding: "utf8", env: { ...process.env, [PHASE_TRACE_ENV]: "1" } },
    );
    assert.notEqual(refused.status, 0, refused.stderr);
    const refusedStartup = controlTraceLines(refused.stderr).filter(
      (line) => line.seq === 0 && line.state !== "in_flight",
    );
    assert.equal(refusedStartup.length, 1, refused.stderr);
    assert.equal(refusedStartup[0].state, "incomplete");
    assert.equal(refusedStartup[0].outcome, "startup_failed");
    assert.equal(typeof refusedStartup[0].phases.project_resolve_ms, "number", refused.stderr);
    assert.equal(refusedStartup[0].phases.connection_open_ms, undefined);
  } catch (error) {
    failure = error;
  } finally {
    for (const client of clients) {
      try { await client.close(); } catch { /* already closed */ }
    }
    removeFixtureHomes(engramHome);
  }
  if (failure) throw failure;
});

function withTimeout(promise, ms, label) {
  let timer;
  return Promise.race([
    promise.finally(() => clearTimeout(timer)),
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
    }),
  ]);
}

test("an opted-in control process whose stderr nobody drains answers every frame and still exits", async (t) => {
  const engramHome = fixtureHome("engram-control-undrained-", t);
  let child;
  let failure;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
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
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(initialized.status, 0, initialized.stderr);
    child = spawn(
      binary,
      ["--home", engramHome, "control", "--actor-id", "undrained", "--session-id", "undrained"],
      { cwd: root, env: { ...process.env, [PHASE_TRACE_ENV]: "1" }, stdio: ["pipe", "pipe", "pipe"] },
    );
    // Stderr is never read: once its buffer is full the pipe fills, and the
    // trace's writer blocks on it.
    child.stderr.pause();
    const exited = new Promise((resolvePromise) => {
      child.once("exit", (code, signal) => resolvePromise({ code, signal }));
    });
    let buffer = "";
    const waiting = [];
    child.stdout.on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      for (let newline = buffer.indexOf("\n"); newline >= 0; newline = buffer.indexOf("\n")) {
        buffer = buffer.slice(newline + 1);
        waiting.shift()?.();
      }
    });
    const frame = `${JSON.stringify({ operation: "session_status", routing_token: "undrained" })}\n`;
    // Far more trace output than a pipe and a stream buffer hold.
    const frames = 3000;
    for (let sent = 0; sent < frames; sent += 100) {
      const answered = Promise.all(
        Array.from({ length: 100 }, () => new Promise((resolvePromise) => waiting.push(resolvePromise))),
      );
      child.stdin.write(frame.repeat(100));
      await withTimeout(answered, 15000, `frames ${sent}..${sent + 100} answered with stderr undrained`);
    }
    // A broken stdout ends the service with an error, whose diagnostic must
    // not wait on the full stderr: the process exits.
    child.stdout.destroy();
    child.stdin.write(frame);
    const result = await withTimeout(exited, 15000, "exit after stdout broke with stderr undrained");
    assert.equal(result.signal, null);
    assert.notEqual(result.code, 0);
  } catch (error) {
    failure = error;
  } finally {
    if (child && child.exitCode === null) child.kill();
    removeFixtureHomes(engramHome);
  }
  if (failure) throw failure;
});
