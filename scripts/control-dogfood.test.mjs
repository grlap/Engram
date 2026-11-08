#!/usr/bin/env node

import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { EventEmitter } from "node:events";
import {
  readFileSync,
  existsSync,
  mkdirSync,
  realpathSync,
  writeFileSync,
} from "node:fs";
import { fixtureHome, removeFixtureHomes, registerFixtureChild, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { FixtureChild, finishFixture } from "./fixture-child.mjs";
import { observeHistoricalSource, recordCheck, ensureRoot, gitRevision } from "./work-session-fixture.mjs";
import { join, resolve } from "node:path";
import test, { after } from "node:test";

const tempBefore = tempSnapshot();
const machineHome = fixtureHome("control-machine-");
const machineTemp = join(machineHome, "tmp");
mkdirSync(machineTemp);
after(() => {
  removeFixtureHomes(machineHome);
  assertTempClean(tempBefore);
});

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");
const projectId = readFileSync(join(root, ".engram-project"), "utf8").trim();

// Native CLI and control processes execute the production source reader.
// Keep its machine policy and temporary files separate from the developer's;
// Cargo retains its ordinary toolchain/cache environment.
function nativeEnvironment(overrides = {}) {
  const environment = { ...process.env, ...overrides };
  for (const key of Object.keys(environment)) {
    if (/^GIT_/iu.test(key)) delete environment[key];
  }
  delete environment.ENGRAM_HOME;
  delete environment.ENGRAM_PROJECT_FILE;
  Object.assign(environment, {
    HOME: machineHome, USERPROFILE: machineHome, XDG_CONFIG_HOME: machineHome,
    GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : "/dev/null",
    GIT_CEILING_DIRECTORIES: process.env.GIT_CEILING_DIRECTORIES,
    TMP: machineTemp, TEMP: machineTemp, TMPDIR: machineTemp,
  });
  return environment;
}

function spawnNative(args, options) {
  return spawnSync(binary, args, { ...options, env: nativeEnvironment(options?.env) });
}

function spawnNativeControl(args, options) {
  return spawn(binary, args, { ...options, env: nativeEnvironment(options?.env) });
}

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

async function holdSqliteWriter(database, home) {
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
  const lifecycle = new FixtureChild(child, "SQLite writer helper", () => stderr);
  registerFixtureChild(home, lifecycle);
  child.stderr.on("data", (chunk) => {
    stderr += chunk.toString("utf8");
  });
  try {
    await new Promise((resolvePromise, reject) => {
      let output = "";
      const timer = setTimeout(() => {
        reject(lifecycle.error("readiness timed out"));
      }, HANG_GUARD_MS);
      lifecycle.onFailure = (error) => { clearTimeout(timer); reject(error); };
      child.stdout.on("data", (chunk) => {
        output += chunk.toString("utf8");
        if (!output.includes("ready\n")) return;
        clearTimeout(timer);
        resolvePromise();
      });
    });
  } catch (error) {
    await finishFixture(error, [lifecycle], () => {});
  }
  return lifecycle;
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

// The opt-in phase trace: bounded JSON lines on the control process's
// stderr, keyed by this marker with the process id and frame number.
const PHASE_TRACE_ENV = "ENGRAM_MCP_PHASE_TRACE";
const CONTROL_TRACE_KEY = "engram_control_phase_trace";
// Ends a wait for what will never come. It says nothing about speed.
const HANG_GUARD_MS = 60000;

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
  constructor(engramHome, sessionId, { phaseTrace = false, command = binary, args } = {}) {
    this.pending = [];
    this.buffer = "";
    this.stderr = "";
    const environment = nativeEnvironment();
    if (phaseTrace) environment[PHASE_TRACE_ENV] = "1";
    else delete environment[PHASE_TRACE_ENV];
    this.child = spawn(
      command,
      args ?? [
        "--home",
        engramHome,
        "control",
        "--work-session",
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
    this.lifecycle = new FixtureChild(this.child, `control server ${sessionId}`, () => this.stderr);
    registerFixtureChild(engramHome, this.lifecycle);
    this.lifecycle.onFailure = (error) => this.#rejectPending(error);
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    this.closed = this.lifecycle.closed;
  }

  #rejectPending(error) {
    const pending = this.pending.splice(0);
    for (const request of pending) request.reject(error);
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
      if (!pending) {
        this.lifecycle.fail("response_error", new Error(`unexpected control response: ${line}`));
        continue;
      }
      try { pending.resolve(JSON.parse(line)); }
      catch (error) {
        pending.reject(error);
        this.lifecycle.fail("response_error", error);
      }
    }
  }

  request(request) {
    if (this.lifecycle.closePromise || this.lifecycle.hasExited || this.lifecycle.hasClosed || this.lifecycle.firstError) {
      return Promise.reject(this.lifecycle.error(`request refused after shutdown or failure: ${request.operation}`));
    }
    return new Promise((resolvePromise, reject) => {
      const timer = setTimeout(() => {
        // FIFO responses cannot be attributed after one request times out.
        this.lifecycle.fail("request_error", new Error(`request timed out: ${request.operation}`));
      }, HANG_GUARD_MS);
      this.pending.push({
        resolve: (response) => {
          clearTimeout(timer);
          this.lifecycle.request(request.operation, "answered", this.pending.length);
          resolvePromise(response);
        },
        reject: (error) => {
          clearTimeout(timer);
          this.lifecycle.request(request.operation, "failed", this.pending.length);
          reject(error);
        },
      });
      this.lifecycle.request(request.operation, "written", this.pending.length);
      try {
        this.child.stdin.write(`${JSON.stringify(request)}\n`, (error) => {
          if (error) this.lifecycle.fail("stdin_error", error);
        });
      } catch (error) { this.lifecycle.fail("stdin_error", error); }
    });
  }

  close() {
    return this.lifecycle.close();
  }
}

function ok(response) {
  assert.equal(response.status, "ok", JSON.stringify(response));
  return response.result;
}

function childEvents({ pid = 42 } = {}) {
  const child = new EventEmitter();
  child.pid = pid;
  child.exitCode = null;
  child.signalCode = null;
  child.stdin = new EventEmitter();
  child.endCalls = 0;
  child.killCalls = 0;
  child.stdin.end = () => { child.endCalls += 1; child.stdin.emit("finish"); };
  child.kill = () => { child.killCalls += 1; return false; };
  child.exit = (code, signal = null) => {
    child.exitCode = code;
    child.signalCode = signal;
    child.emit("exit", code, signal);
  };
  child.closeStdio = () => child.emit("close", child.exitCode, child.signalCode);
  return child;
}

test("fixture child shutdown waits for stdio and reuses its one close result", async () => {
  const child = childEvents();
  const lifecycle = new FixtureChild(child, "event child");
  child.emit("spawn");
  const first = lifecycle.close();
  assert.equal(lifecycle.close(), first);
  assert.equal(child.endCalls, 1);
  child.exit(0);
  let resolved = false;
  first.then(() => { resolved = true; });
  await Promise.resolve();
  assert.equal(resolved, false, "exit does not establish stdio closure");
  child.closeStdio();
  await first;
  assert.equal(lifecycle.close(), first);
  assert.equal(child.killCalls, 0);
  assert.equal(lifecycle.hasClosed, true);
  assert.ok(lifecycle.events.close >= lifecycle.events.exit);
});

test("fixture child shutdown handles observed exit, signals and absent PID without guessing closure", async () => {
  for (const [pid, code, signal] of [[42, 0, null], [42, null, "SIGTERM"], [undefined, null, null]]) {
    const child = childEvents({ pid });
    child.pid = pid;
    const lifecycle = new FixtureChild(child, "event child");
    if (pid) child.exit(code, signal);
    const closing = lifecycle.close();
    assert.equal(lifecycle.hasClosed, false);
    assert.equal(child.endCalls, pid ? 0 : 1);
    child.closeStdio();
    await closing;
    assert.equal(child.killCalls, 0);
    assert.equal(lifecycle.snapshot().signal, signal);
  }
  const child = childEvents();
  const lifecycle = new FixtureChild(child, "closed child");
  child.exit(0);
  child.closeStdio();
  await lifecycle.close();
  assert.equal(child.endCalls, 0);
});

test("fixture child shutdown enforces its elapsed bound before a delayed timer fires", async (t) => {
  let now = 0;
  t.mock.method(performance, "now", () => now);
  const shutdownGuardMs = 5000;
  for (const elapsed of [shutdownGuardMs, shutdownGuardMs + 1]) {
    const child = childEvents();
    const lifecycle = new FixtureChild(child, "delayed timer child", undefined, { shutdownGuardMs });
    const closing = lifecycle.close();
    // Advance observation time without yielding to the real timeout callback.
    now += elapsed;
    child.closeStdio();
    if (elapsed === shutdownGuardMs) {
      await closing;
      assert.equal(lifecycle.events.timeout, undefined);
    } else {
      const error = await closing.then(() => assert.fail("expected elapsed timeout"), (error) => error);
      assert.match(error.message, /shutdown timed out/);
      assert.equal(error.lifecycle.stdio_closed, true);
      assert.equal(error.lifecycle.kill, null);
      assert.equal(error.lifecycle.events.close - error.lifecycle.events.close_call, elapsed);
      assert.equal(lifecycle.close(), closing);
      await assert.rejects(lifecycle.close(), (again) => again === error);
    }
    assert.equal(child.endCalls, 1);
    assert.equal(child.killCalls, 0);
  }
});

test("fixture child shutdown timeout stays rejected after late close with bounded kill evidence", async () => {
  const child = childEvents();
  const lifecycle = new FixtureChild(child, "timeout child", () => "retained stderr", { shutdownGuardMs: 50 });
  lifecycle.request("work_session_read", "answered", 0);
  const first = lifecycle.close();
  const error = await first.then(() => assert.fail("expected timeout"), (error) => error);
  assert.match(error.message, /shutdown timed out: retained stderr/);
  assert.equal(error.lifecycle.stdio_closed, false);
  assert.equal(error.lifecycle.latest_request.operation, "work_session_read");
  assert.equal(error.lifecycle.pending, 0);
  assert.equal(error.lifecycle.kill.returned, false, "kill refusal is not termination proof");
  assert.equal(child.endCalls, 1);
  assert.equal(child.killCalls, 1);
  child.exit(0);
  child.closeStdio();
  assert.equal(lifecycle.close(), first);
  await assert.rejects(lifecycle.close(), (again) => again === error);
  assert.equal(error.lifecycle.stdio_closed, false, "the failure snapshot is immutable");
});

test("fixture child shutdown captures spawn and pipe failures before readiness", async () => {
  for (const event of ["child_error", "stdin_error"]) {
    const child = childEvents({ pid: undefined });
    const lifecycle = new FixtureChild(child, "failed child");
    let received;
    lifecycle.onFailure = (error) => { received = error; };
    const cause = new Error(event === "child_error" ? "spawn ENOENT" : "write EPIPE");
    (event === "child_error" ? child : child.stdin).emit("error", cause);
    assert.equal(received.cause, cause);
    const closing = lifecycle.close();
    child.closeStdio();
    await assert.rejects(closing, (error) => error.cause === cause);
    assert.equal(typeof lifecycle.events[event], "number");
  }
});

test("fixture child normal EOF retains final stderr and rejects new control requests", async (t) => {
  const home = fixtureHome("engram-child-eof-", t);
  const client = new ControlClient(home, "node-fixture", {
    command: process.execPath,
    args: ["-e", 'process.stdin.resume(); process.stdin.on("data", () => process.stdout.write("{\\"status\\":\\"ok\\"}\\n")); process.stdin.on("end", () => process.stderr.write("final stderr\\n"));'],
  });
  let failure;
  try {
    assert.equal((await client.request({ operation: "probe" })).status, "ok");
    const closing = client.close();
    await assert.rejects(client.request({ operation: "too_late" }), /request refused after shutdown/);
    await closing;
    assert.equal(client.stderr, "final stderr\n");
    assert.equal(client.lifecycle.latestRequest.outcome, "answered");
    assert.equal(client.lifecycle.pending, 0);
    assert.equal(client.lifecycle.hasClosed, true);
  } catch (error) { failure = error; }
  finally { await finishFixture(failure, [client], () => removeFixtureHomes(home)); }
});

test("fixture control spawn and asynchronous pipe errors settle pending requests", async (t) => {
  const home = fixtureHome("engram-child-errors-", t);
  const missing = new ControlClient(home, "missing-fixture", { command: join(home, "missing-executable") });
  await assert.rejects(missing.request({ operation: "probe" }), /ENOENT/);
  await assert.rejects(missing.close(), /shutdown failed/);
  assert.equal(missing.pending.length, 0);
  assert.equal(missing.lifecycle.hasClosed, true);
  const client = new ControlClient(home, "pipe-fixture", {
    command: process.execPath, args: ["-e", "process.stdin.resume();"],
  });
  const pending = client.request({ operation: "probe" });
  client.child.stdin.destroy(Object.assign(new Error("injected EPIPE"), { code: "EPIPE" }));
  await assert.rejects(pending, /EPIPE/);
  await assert.rejects(client.close(), /shutdown failed/);
  assert.equal(client.pending.length, 0);
  assert.equal(client.lifecycle.hasClosed, true);
  await assert.rejects(client.request({ operation: "after_error" }), /request refused/);
  removeFixtureHomes(home);
});

test("fixture cleanup preserves the first error and attempts every client", async (t) => {
  const body = new Error("body assertion");
  const cleanup = new Error("child shutdown");
  const called = [];
  const diagnostics = [];
  t.mock.method(console, "error", (error) => diagnostics.push(error));
  const clients = [
    { close() { called.push("first"); throw cleanup; } },
    { close() { called.push("second"); } },
  ];
  await assert.rejects(finishFixture(body, clients, () => called.push("remove")), (error) => error === body);
  assert.deepEqual(called, ["first", "second", "remove"]);
  assert.equal(diagnostics[0].errors[0], cleanup);
  await assert.rejects(finishFixture(null, clients, () => {}), AggregateError);
});

test("fixture cleanup guards explicit and automatic home removal until stdio closes", async (t) => {
  let automatic;
  const home = fixtureHome("engram-child-removal-", { after(fn) { automatic = fn; } });
  const lifecycle = new FixtureChild(childEvents(), "unclosed child");
  registerFixtureChild(home, lifecycle);
  const diagnostics = [];
  t.mock.method(console, "error", (error) => diagnostics.push(error));
  removeFixtureHomes(home);
  assert.equal(existsSync(home), true);
  automatic();
  assert.equal(existsSync(home), true);
  assert.equal(diagnostics.length, 2);
  assert.match(diagnostics[0].errors[0].message, /Owned child stdio remains open/);
  lifecycle.child.exit(0);
  lifecycle.child.closeStdio();
  automatic();
  assert.equal(existsSync(home), false);
});

test("fixture SQLite writer rolls back and cleans up setup failure", async (t) => {
  const home = fixtureHome("engram-writer-lifecycle-", t);
  const database = join(home, "writer.db");
  executeSql(database, "CREATE TABLE value (n INTEGER)");
  const writer = await holdSqliteWriter(database, home);
  await writer.close();
  assert.equal(writer.hasClosed, true);
  executeSql(database, "BEGIN IMMEDIATE; INSERT INTO value VALUES (1); ROLLBACK");
  await assert.rejects(holdSqliteWriter(join(home, "absent", "writer.db"), home), /SQLite writer helper exited/);
  removeFixtureHomes(home);
  assert.equal(existsSync(home), false, "failed setup's owned child was closed too");
});

function readPolicy(home) {
  const result = spawnNative(["--home", home, "control-policy", "show"], { cwd: root, encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr);
  return JSON.parse(result.stdout);
}

test("obligation rule policy CLI rejects malformed input and preserves pinned replay", (t) => {
  const home = fixtureHome("engram-rule-policy-input-", t);
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  const initialized = spawnNative(["--home", home, "init"], { cwd: root, encoding: "utf8" });
  assert.equal(initialized.status, 0, initialized.stderr);
  const initial = readPolicy(home);
  const set = (input, key, expected = initial.policy) => {
    const args = ["--home", home, "control-policy", "set-obligation-rule-set", "--input", input,
      "--authorized-by", "rule-policy-operator", "--idempotency-key", key,
      "--expected-policy-hash", expected];
    return spawnNative(args, { cwd: root, encoding: "utf8" });
  };
  const rule = {
    rule: { rule_id: "source_mutation_requires_pinned_test", rule_version: 1 },
    trigger: "source_changed",
    requirement: { check_kind: "test", check_fingerprint: fingerprint("the requested test command") },
  };
  const valid = { schema_version: 1, rules: [rule] };
  const badInputs = [
    { schema_version: 1, rules: [], typo: true },
    { schema_version: 1, rules: [{ ...rule, typo: true }] },
    { schema_version: 1, rules: [{ ...rule, requirement: { ...rule.requirement, typo: true } }] },
    ...[null, { toolchain: "unsupported" }].map((required_environment) => ({
      schema_version: 1, rules: [{ ...rule, requirement: { ...rule.requirement, required_environment } }],
    })),
  ];
  for (const [index, input] of badInputs.entries()) {
    const rejected = set(JSON.stringify(input), `malformed-${index}`);
    assert.notEqual(rejected.status, 0, rejected.stdout);
    assert.match(rejected.stderr, /unknown field/);
    assert.deepEqual(readPolicy(home), initial);
  }
  const oversized = join(home, "oversized-rules.json");
  writeFileSync(oversized, " ".repeat(64 * 1024 + 1));
  const rejectedFile = set(`@${oversized}`, "oversized-file");
  assert.notEqual(rejectedFile.status, 0, rejectedFile.stdout);
  assert.match(rejectedFile.stderr, /65536-byte limit/);
  assert.deepEqual(readPolicy(home), initial);
  const applied = set(JSON.stringify(valid), "pinned-rule");
  assert.equal(applied.status, 0, applied.stderr);
  const receipt = JSON.parse(applied.stdout);
  assert.equal(receipt.changed, true);
  assert.equal(receipt.policy_epoch, initial.epoch + 1);
  assert.equal(receipt.previous_rule_set, initial.obligation_rules);
  const current = readPolicy(home);
  const replay = set(JSON.stringify(valid), "pinned-rule");
  assert.equal(replay.status, 0, replay.stderr);
  assert.deepEqual(JSON.parse(replay.stdout), receipt);
  const conflicting = set(JSON.stringify({ schema_version: 1, rules: [] }), "pinned-rule");
  assert.notEqual(conflicting.status, 0, conflicting.stdout);
  assert.match(conflicting.stderr, /reused for a different intent/);
  const stale = set(JSON.stringify(valid), "stale-policy");
  assert.notEqual(stale.status, 0, stale.stdout);
  assert.match(stale.stderr, /active control policy changed/);
  assert.deepEqual(readPolicy(home), current);
});

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
  const executed = spawnNative(args, {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(executed.status, expectedStatus, executed.stderr);
  return JSON.parse(executed.stdout);
}

function cliWorkFocus(engramHome, actorId, workRef) {
  const focused = spawnNative([
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

test("doctor recovery reports a corrupt policy through a read-only surface", (t) => {
  const engramHome = fixtureHome("engram-control-policy-recovery-", t);
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
    const initialized = spawnNative(["--home", engramHome, "init"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(initialized.status, 0, initialized.stderr);
    const healthy = spawnNative(["--home", engramHome, "doctor", "--json"],
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

    const ordinary = spawnNative(["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.notEqual(ordinary.status, 0);
    const recovery = spawnNative(["--home", engramHome, "doctor", "--recover-policy", "--json"],
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
    const initialized = spawnNative(["--home", engramHome, "init"], {
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

    const ordinary = spawnNative(["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.notEqual(ordinary.status, 0);
    assert.match(ordinary.stdout, /^remedy: "engram doctor --repair-projections"$/mu);
    assert.match(ordinary.stdout, /^code: "projection_repair_required"$/mu);
    assert.deepEqual(readFileSync(database), before);
    const ordinaryJson = spawnNative(["--home", engramHome, "doctor", "--json"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.notEqual(ordinaryJson.status, 0);
    const refusal = JSON.parse(ordinaryJson.stdout);
    assert.equal(refusal.healthy, false);
    assert.equal(refusal.code, "projection_repair_required");
    assert.equal(refusal.remedy, "engram doctor --repair-projections");
    assert.deepEqual(refusal.scope, [
      "indexes", "triggers", "fts", "derived_tables",
    ]);
    assert.deepEqual(readFileSync(database), before);

    const readiness = spawnNative(["--home", engramHome, "readiness", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.notEqual(readiness.status, 0);
    const readinessRefusal = JSON.parse(readiness.stdout);
    assert.equal(readinessRefusal.code, "projection_repair_required");
    assert.equal(readinessRefusal.scope, "readiness");
    assert.deepEqual(readinessRefusal.detail.scope, [
      "indexes", "triggers", "fts", "derived_tables",
    ]);
    assert.deepEqual(readFileSync(database), before);

    const repaired = spawnNative(["--home", engramHome, "doctor", "--repair-projections", "--json"],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(repaired.status, 0, repaired.stderr);
    const report = JSON.parse(repaired.stdout);
    assert.equal(report.mode, "projection_repair");
    assert.equal(report.mutation_enabled, true);
    assert.equal(report.healthy, true);
    const healthy = spawnNative(["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(healthy.status, 0, healthy.stderr);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("retained ordinary observations owe checks and registered evidence satisfies them", async (t) => {
  const home = fixtureHome("engram-observation-work-", t);
  const actor = "ordinary-runner";
  let client;
  let failure;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
    assert.equal(built.status, 0, built.stderr);
    const initialized = spawnNative(["--home", home, "init"], { cwd: root, encoding: "utf8" });
    assert.equal(initialized.status, 0, initialized.stderr);
    const item = cliWork(home, actor, "propose", { kind: "root", title: "Observe ordinary work",
      outcome: "Evidence follows the exact live claim", acceptance: ["the recorded check passes"],
      work_kind: "chore", idempotency_key: "ordinary-root" });
    const binding = cliWork(home, actor, "update", { kind: "claim", ttl_seconds: 300,
      idempotency_key: "ordinary-claim" }).receipt.control_binding;
    client = new ControlClient(home, actor);
    assert.equal(ok(await client.request({ operation: "work_session_read" })).work_session, true);
    const request = async (frame) => ok(await client.request(frame));
    const policy = () => readPolicy(home);
    const source = { workspace_id: "ordinary-workspace", source_revision: gitRevision("ordinary-R1"),
      source_root_generation: 1, source_root_state: "named" };
    await ensureRoot({ request, binding, key: "ordinary", source });
    await observeHistoricalSource({ home, request, policy, binding, key: "ordinary-mutation", source, changed: true });
    let focus = cliWorkFocus(home, actor, item.work.short_ref);
    assert.equal(focus.obligation_page.items.filter((row) => row.state === "open").length, 1);
    const recorded = await recordCheck({ request, binding, session: actor, key: "ordinary-check", source });
    const verification = recorded.verifications[0].record_id;
    focus = cliWorkFocus(home, actor, item.work.short_ref);
    assert.equal(focus.obligation_page.items[0].state, "satisfied");
    assert.equal(focus.obligation_page.items[0].evidence, verification);
    const attached = cliWork(home, actor, "update", { kind: "evidence", attach: { evidence: verification },
      idempotency_key: "ordinary-attach" }).receipt.result;
    assert.equal(attached.attached, true);
    assert.equal(attached.evidence_kind, "verification");
    cliWork(home, actor, "update", { kind: "checkpoint", summary: "The host check supports the contribution",
      evidence: [verification], idempotency_key: "ordinary-contribution" });
    cliWork(home, actor, "update", { kind: "release", reason: "continue with the next claim",
      idempotency_key: "ordinary-release" });
    const successor = cliWork(home, actor, "update", { kind: "claim", ttl_seconds: 300,
      idempotency_key: "ordinary-reclaim" }).receipt.control_binding;
    assert.ok(successor.claim_fence > binding.claim_fence);
    const doctor = spawnNative(["--home", home, "doctor"], { cwd: root, encoding: "utf8" });
    assert.equal(doctor.status, 0, doctor.stderr);
  } catch (error) { failure = error; }
  finally { await finishFixture(failure, [client], () => removeFixtureHomes(home)); }
});

test("evaluation admission causes survive the native CLI and host-recorded evidence", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  // These transport cases retain the historical source/cut admission rules.
  // Committed input's live Git decision is exercised separately below.
  const revisionFor = (label) => gitRevision(label).replace("git-commit:", "git-tree:");
  for (const caseName of ["eligibility", "source_root", "wrong_run", "beyond_cut", "wrong_source"]) {
    const engramHome = fixtureHome(`engram-admission-${caseName.replaceAll("_", "-")}-`, t);
    const clients = [];
    let failure;
    const actor = `admission-${caseName}`;
    const word = (session, ...args) => spawnNative(["--home", engramHome, "work", "--actor-id", session, "--session-id", session, ...args],
      { cwd: root, encoding: "utf8" });
    const jsonWord = (session, ...args) => {
      const result = word(session, ...args, "--json");
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    try {
      const init = spawnNative(["--home", engramHome, "init"], { cwd: root, encoding: "utf8" });
      assert.equal(init.status, 0, init.stderr);
      const policy = spawnNative(["--home", engramHome, "control-policy", "set-acceptance-evaluation",
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
        assert.equal(ok(await client.request({ operation: "work_session_read" })).work_session, true);
        const request = async (frame) => ok(await client.request(frame));
        const policy = () => readPolicy(engramHome);
        if (bound) await ensureRoot({ request, binding, key: "admission",
          source: { workspace_id: "admission-workspace" } });
        let sequence = 0;
        const report = async (revision, check) => {
          const key = `admission-report-${++sequence}`;
          if (!revision) return;
          const source = { workspace_id: "admission-workspace", source_revision: revisionFor(revision),
            source_root_generation: 1, source_root_state: "named" };
          if (check) return recordCheck({ request, binding, session, key, source,
            input: { kind: "staged_tree", tree: source.source_revision.slice("git-tree:".length),
              base_commit: gitRevision("admission-base").slice("git-commit:".length) } });
          return observeHistoricalSource({ home: engramHome, request, policy, binding, key, source, changed: true });
        };
        return { client, binding, report };
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
        ok(await host.client.request({ operation: "named_root_bind",           claim_id: host.binding.claim_id, claim_fence: host.binding.claim_fence, workspace_id: "C:/database is locked",
          generation: 1, named_at: new Date().toISOString(), kind: "bound", idempotency_key: "named-admission-root" }));
      }
      if (caseName === "wrong_run") {
        const other = "foreign-admission-runner";
        const otherRef = create(other, "Evidence belongs to another run");
        const host = await attachHost(other, otherRef);
        citation = (await host.report("admission-R", true)).verifications[0].record_id;
      }
      if (caseName === "beyond_cut" || caseName === "wrong_source") {
        const host = await attachHost(actor, ref);
        beforeCheck = jsonWord(actor, "show", ref).evidence_basis;
        citation = (await host.report("admission-R", true)).verifications[0].record_id;
        if (caseName === "wrong_source") await host.report("admission-S", false);
      }
      const shown = jsonWord(actor, "show", ref);
      const cut = caseName === "beyond_cut" ? beforeCheck : shown.evidence_basis;
      const revision = revisionFor(caseName === "wrong_source" ? "admission-S" : "admission-R");
      const result = word(actor, "evaluate", ref, "--mode", "same-session", "--acceptance-basis", String(shown.acceptance_basis),
        "--evidence-basis", String(cut), "--verdict", `1=pass:${bound ? "observed" : "judgment"}`,
        "--rationale", "1=judge the supplied evidence", "--evidence", `1=${citation}`,
        ...(caseName === "eligibility" || caseName === "wrong_run" ? [] : ["--source-fingerprint", revision]), "--json");
      assert.equal(result.status, 1, `${caseName}: ${result.stderr}`);
      const error = JSON.parse(result.stderr).error;
      if (caseName === "beyond_cut") {
        assert.equal(error.code, "acceptance_evaluation_resubmit", result.stderr);
        assert.ok(error.details.reason.includes(`a host check was recorded after evidence basis ${cut}`), result.stderr);
        assert.match(error.details.remedy, /re-read show/u);
        assert.equal(jsonWord(actor, "show", ref).evidence_basis, shown.evidence_basis);
        assert.deepEqual(jsonWord(actor, "show", ref, "--evaluations").evaluations, []);
        continue;
      }
      assert.equal(error.code, "acceptance_evaluation_refused", `${caseName}: ${result.stderr}`);
      const cause = error.details.cause;
      assert.equal(cause.kind, bound ? "citation" : caseName);
      assert.equal(cause.mismatch, ({ eligibility: "same_session_unmarked", source_root: "no_initial_sighting",
        wrong_run: "not_on_run", wrong_source: "wrong_source" })[caseName]);
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
    } catch (error) {
      failure = error;
    } finally {
      await finishFixture(failure, clients, () => removeFixtureHomes(engramHome));
    }
  }
});

test("committed input admission reads real Git through the native CLI", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  for (const state of ["clean", "dirty", "new-commit", "missing"]) {
    const home = fixtureHome("engram-live-admission-", t);
    const sourceRoot = fixtureHome("engram-live-source-", t);
    const actor = `live-admission-${state}`;
    const word = (...args) => spawnNative(["--home", home, "work", "--actor-id", actor, "--session-id", actor, ...args],
      { cwd: root, encoding: "utf8" });
    const jsonWord = (...args) => {
      const result = word(...args, "--json");
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    const git = (...args) => {
      const result = spawnSync("git", args, { cwd: sourceRoot, env: nativeEnvironment(), encoding: "utf8" });
      assert.equal(result.status, 0, result.stderr);
      return result.stdout.trim();
    };
    const commit = () => git("-c", "user.name=Test", "-c", "user.email=test@example.invalid",
      "commit", "--no-gpg-sign", "--allow-empty", "-qm", "fixture");
    let client;
    let failure;
    try {
      git("init", "-q");
      writeFileSync(join(sourceRoot, "tracked"), "checked\n");
      git("add", "tracked");
      commit();
      const revision = `git-commit:${git("rev-parse", "HEAD")}`;
      const initialized = spawnNative(["--home", home, "init"], { cwd: root, encoding: "utf8" });
      assert.equal(initialized.status, 0, initialized.stderr);
      const policy = spawnNative(["--home", home, "control-policy", "set-acceptance-evaluation",
        "--modes", "same-session", "--mechanical-basis", "observed", "--authorized-by", "operator",
        "--idempotency-key", "live-admission-policy"], { cwd: root, encoding: "utf8" });
      assert.equal(policy.status, 0, policy.stderr);
      const ref = jsonWord("add", "Live commit admission", "--accept", "the test passed", "--bind", "1=test").work.short_ref;
      jsonWord("claim", ref);
      const binding = cliWorkFocus(home, actor, ref).control_binding;
      client = new ControlClient(home, actor);
      assert.equal(ok(await client.request({ operation: "work_session_read" })).work_session, true);
      const request = async (frame) => ok(await client.request(frame));
      const source = { workspace_id: realpathSync.native(sourceRoot), source_revision: revision,
        source_root_generation: 1, source_root_state: "named" };
      await ensureRoot({ request, binding, key: "live-admission", source });
      const before = jsonWord("show", ref).evidence_basis;
      const citation = (await recordCheck({ request, binding, session: actor, key: "live-admission", source })).verifications[0].record_id;
      const shown = jsonWord("show", ref);
      const evaluate = (cut) => word("evaluate", ref, "--mode", "same-session",
        "--acceptance-basis", String(shown.acceptance_basis), "--evidence-basis", String(cut),
        "--verdict", "1=pass:observed", "--rationale", "1=registered check on this commit",
        "--evidence", `1=${citation}`, "--source-fingerprint", revision, "--json");
      // No check inside the cut cannot mint a live source decision, even
      // when the declared commit is currently clean and its check is later.
      const staleCut = evaluate(before);
      assert.equal(staleCut.status, 1, staleCut.stderr);
      assert.equal(JSON.parse(staleCut.stderr).error.code, "work_completion_refused");
      assert.match(staleCut.stderr, /no registered check at the evaluated cut/u);
      assert.deepEqual(jsonWord("show", ref, "--evaluations").evaluations, []);
      if (state === "dirty") writeFileSync(join(sourceRoot, "tracked"), "changed\n");
      if (state === "new-commit") commit();
      if (state === "missing") removeFixtureHomes(sourceRoot);
      const result = evaluate(shown.evidence_basis);
      if (state === "clean") {
        assert.equal(result.status, 0, result.stderr);
        const sealed = jsonWord("done", ref, "Delivered", "--source-fingerprint", revision);
        assert.equal(sealed.work.lifecycle, "completed");
      } else {
        assert.equal(result.status, 1, `${state}: ${result.stderr}`);
        assert.equal(JSON.parse(result.stderr).error.code, "work_completion_refused");
        assert.match(result.stderr, state === "dirty" ? /dirty/u : state === "missing" ? /unavailable/u : /named root is at/u);
        assert.deepEqual(jsonWord("show", ref, "--evaluations").evaluations, []);
      }
      const doctor = spawnNative(["--home", home, "doctor"], { cwd: root, encoding: "utf8" });
      assert.equal(doctor.status, 0, doctor.stderr);
    } catch (error) { failure = error; }
    finally { await finishFixture(failure, [client], () => removeFixtureHomes(home, sourceRoot)); }
  }
});

test("a host reads what satisfied each bound criterion and lists its candidates in closed page shapes", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  const engramHome = fixtureHome("engram-binding-read-", t);
  const actor = "binding-read-runner";
  const jsonWord = (...args) => {
    const result = spawnNative(["--home", engramHome, "work", "--actor-id", actor, "--session-id", actor,
      ...args, "--json"], { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  let client;
  let failure;
  try {
    const init = spawnNative(["--home", engramHome, "init"], { cwd: root, encoding: "utf8" });
    assert.equal(init.status, 0, init.stderr);
    const ref = jsonWord("add", "Read bound evidence", "--accept", "1. the tests pass", "--accept", "2. the docs say so",
      "--bind", "1=test").work.short_ref;
    const revision = jsonWord("claim", ref).work.revision;
    const binding = cliWorkFocus(engramHome, actor, ref).control_binding;
    client = new ControlClient(engramHome, actor);
    assert.equal(ok(await client.request({ operation: "work_session_read" })).work_session, true);
    const source = { workspace_id: "binding-read-workspace", source_revision: gitRevision("binding-read-R1"),
      source_root_generation: 1, source_root_state: "named" };
    const checked = await recordCheck({ request: async (frame) => ok(await client.request(frame)),
      binding, session: actor, key: "binding-read-check", source });
    const verification = checked.verifications[0].record_id;
    const request = { operation: "acceptance_binding_read",       work_id: binding.work_id, expected_work_revision: revision, run_id: binding.run_id };
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
    const candidates = { operation: "acceptance_verification_read",       work_id: binding.work_id, expected_work_revision: revision, run_id: binding.run_id,
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
  } catch (error) {
    failure = error;
  } finally {
    await finishFixture(failure, [client], () => removeFixtureHomes(engramHome));
  }
});

// B77/B78 and B21/B22: native completion consumes host reports, not promises.
test("source recovery keeps a judgment through host confirmation and separates fingerprint remedies", async (t) => {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
  assert.equal(built.status, 0, built.stderr);
  const engramHome = fixtureHome("engram-source-recovery-", t);
  const actor = "source-recovery-runner";
  let failure;
  const word = (...args) => spawnNative(["--home", engramHome, "work", "--actor-id", actor, "--session-id", actor, ...args],
    { cwd: root, encoding: "utf8" });
  const jsonWord = (...args) => {
    const result = word(...args, "--json");
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  const setPolicy = (fresh) => {
    const result = spawnNative(["--home", engramHome, "control-policy", "set-acceptance-evaluation",
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
    const init = spawnNative(["--home", engramHome, "init"], { cwd: root, encoding: "utf8" });
    assert.equal(init.status, 0, init.stderr);
    setPolicy(false);
    const ref = create("Declared root confirmation");
    const binding = cliWorkFocus(engramHome, actor, ref).control_binding;
    client = new ControlClient(engramHome, actor);
    assert.equal(ok(await client.request({ operation: "work_session_read" })).work_session, true);
    const workspace = "C:/database is locked/源";
    ok(await client.request({ operation: "named_root_bind",
      claim_id: binding.claim_id, claim_fence: binding.claim_fence, workspace_id: workspace,
      generation: 1, named_at: new Date().toISOString(), kind: "bound", idempotency_key: "source-root" }));
    let sightingCounter = 0;
    const sight = async (revision) => observeHistoricalSource({ home: engramHome, request: async (frame) => ok(await client.request(frame)),
      policy: () => readPolicy(engramHome), binding, key: `source-sighting-${++sightingCounter}`,
      source: { workspace_id: workspace, source_revision: revision,
        source_root_generation: 1, source_root_state: "named" } });
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
    const rootRead = ok(await client.request({ operation: "named_root_read",       run_id: binding.run_id, claim_id: binding.claim_id }));
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
    const unknownRun = await client.request({ operation: "named_root_read",       run_id: "00000000-0000-0000-0000-000000000007", claim_id: binding.claim_id });
    assert.equal(unknownRun.status, "error", JSON.stringify(unknownRun));
    assert.equal(unknownRun.error.code, "named_root_read_refused");
    assert.equal(unknownRun.error.reason, "run_unknown");
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
  } catch (error) {
    failure = error;
  } finally {
    await finishFixture(failure, [client], () => removeFixtureHomes(engramHome));
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
    const initialized = spawnNative([
        "--home",
        engramHome,
        "init",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(initialized.status, 0, initialized.stderr);
    const untraced = new ControlClient(engramHome, "trace-off", { phaseTrace: false });
    const traced = new ControlClient(engramHome, "trace-on", { phaseTrace: true });
    clients.push(untraced, traced);
    const status = { operation: "work_session_read" };
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
      assert.equal(terminal[0].operation, "work_session_read");
      assert.equal(typeof terminal[0].phases.handler_total_ms, "number");
      assert.equal(typeof terminal[0].phases.response_write_flush_ms, "number");
    }
    assert.equal(lines.filter((line) => line.seq > rounds).length, 0);
    const off = median(timings.off);
    const on = median(timings.on);
    t.diagnostic(`work_session_read over control, interleaved ${rounds} rounds: trace on median=${on.toFixed(2)}ms, trace off median=${off.toFixed(2)}ms`);
    // A comparison report, not a benchmark. What the trace costs is counted in
    // Rust: tracing_adds_no_sql_work_to_a_frame,
    // each_trace_line_is_encoded_exactly_once and
    // an_undrained_sink_never_stalls_a_traced_frame. An unset process wrote
    // nothing above. Left to these reported medians alone: building a line's
    // fields, an encoding outside the trace's one encoder, and writer-thread time.

    // A control process whose project cannot be resolved still leaves its
    // startup record, incomplete, with the phase it stopped in.
    const refused = spawnNative([
        "--home",
        engramHome,
        "--project-file",
        join(engramHome, "missing.engram-project"),
        "control",
        "--work-session",
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
    await finishFixture(failure, clients, () => removeFixtureHomes(engramHome));
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

test("failed control startup answers one frame, exits, and never dispatches mutations", async (t) => {
  const home = fixtureHome("engram-control-startup-", t);
  let writer;
  let lifecycle;
  let failure;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], { cwd: root, encoding: "utf8" });
    assert.equal(built.status, 0, built.stderr);
    const initialized = spawnNative(["--home", home, "init"], { cwd: root, encoding: "utf8" });
    assert.equal(initialized.status, 0, initialized.stderr);
    const database = join(home, "projects", fingerprint(projectId), "engram.db");
    const { DatabaseSync } = await import("node:sqlite");
    const snapshot = () => {
      const connection = new DatabaseSync(database);
      try {
        return ["control_connections", "objects", "work_operation_results"].map((table) =>
          connection.prepare(`SELECT * FROM ${table}`).all(),
        );
      } finally { connection.close(); }
    };
    const args = ["--home", home, "control", "--work-session", "--actor-id", "startup-agent", "--session-id", "startup-session"];
    const frame = JSON.stringify({ operation: "work_session_read" }) + "\n";
    const assertRefusal = (result, code, message) => {
      assert.equal(result.signal, null);
      assert.notEqual(result.status, 0, result.stderr);
      assert.equal(result.stdout.trim().split("\n").length, 1);
      assert.deepEqual(JSON.parse(result.stdout), { status: "error", error: { code, message } });
      assert.match(result.stderr, /failed to start Engram host-control service/);
    };
    const before = snapshot();
    writer = await holdSqliteWriter(database, home);
    const busy = spawnNative(args, { cwd: root, encoding: "utf8", input: frame + frame, timeout: HANG_GUARD_MS });
    assert.equal(busy.error, undefined);
    assertRefusal(busy, "control_startup_busy", "Engram control startup could not acquire the store.");
    await writer.close();
    writer = undefined;
    assert.deepEqual(snapshot(), before);

    executeSql(database, "DROP INDEX work_items_ready");
    for (const input of [frame + frame, "malformed\n", "partial"]) {
      const refused = spawnNative(args, { cwd: root, encoding: "utf8", input, timeout: HANG_GUARD_MS });
      assert.equal(refused.error, undefined);
      assertRefusal(refused, "control_startup_refused", "Engram control startup was refused; inspect the startup diagnostic.");
      assert.deepEqual(snapshot(), before);
    }
    const empty = spawnNative(args, { cwd: root, encoding: "utf8", input: "", timeout: HANG_GUARD_MS });
    assert.equal(empty.error, undefined);
    assert.notEqual(empty.status, 0);
    assert.equal(empty.stdout, "");

    // Keep stdin open after oversized, newline-free input: draining would
    // wait forever. Closing it only in finally must not be needed for exit.
    const child = spawnNativeControl(args, { cwd: root, stdio: ["pipe", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    lifecycle = new FixtureChild(child, "failed startup control server", () => stderr);
    registerFixtureChild(home, lifecycle);
    child.stdout.on("data", (chunk) => { stdout += chunk; });
    child.stderr.on("data", (chunk) => { stderr += chunk; });
    child.stdin.write("x".repeat(256 * 1024 + 1));
    await withTimeout(lifecycle.closed, HANG_GUARD_MS, "oversize failed startup without EOF");
    assert.equal(lifecycle.signalCode, null);
    assert.notEqual(lifecycle.exitCode, 0, stderr);
    assert.equal(stdout, "");
    assert.match(stderr, /startup frame exceeds/);
    assert.deepEqual(snapshot(), before);
  } catch (error) {
    failure = error;
  } finally {
    lifecycle?.killIfRunning();
    await finishFixture(failure, [writer, lifecycle], () => removeFixtureHomes(home));
  }
});

test("an opted-in control process whose stderr nobody drains answers every frame and still exits", async (t) => {
  const engramHome = fixtureHome("engram-control-undrained-", t);
  let child;
  let lifecycle;
  let failure;
  try {
    const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(built.status, 0, built.stderr);
    const initialized = spawnNative([
        "--home",
        engramHome,
        "init",
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(initialized.status, 0, initialized.stderr);
    child = spawnNativeControl(["--home", engramHome, "control", "--work-session", "--actor-id", "undrained", "--session-id", "undrained"],
      { cwd: root, env: { ...process.env, [PHASE_TRACE_ENV]: "1" }, stdio: ["pipe", "pipe", "pipe"] },
    );
    lifecycle = new FixtureChild(child, "undrained control server");
    registerFixtureChild(engramHome, lifecycle);
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
    const frame = `${JSON.stringify({ operation: "work_session_read" })}\n`;
    // Far more trace output than a pipe and a stream buffer hold.
    const frames = 3000;
    for (let sent = 0; sent < frames; sent += 100) {
      const answered = Promise.all(
        Array.from({ length: 100 }, () => new Promise((resolvePromise) => waiting.push(resolvePromise))),
      );
      child.stdin.write(frame.repeat(100));
      await withTimeout(answered, HANG_GUARD_MS, `frames ${sent}..${sent + 100} answered with stderr undrained`);
    }
    // A broken stdout ends the service with an error, whose diagnostic must
    // not wait on the full stderr: the process exits.
    child.stdout.destroy();
    child.stdin.write(frame);
    const result = await withTimeout(exited, HANG_GUARD_MS, "exit after stdout broke with stderr undrained");
    assert.equal(result.signal, null);
    assert.notEqual(result.code, 0);
  } catch (error) {
    failure = error;
  } finally {
    // Keep this test's intentionally undrained stderr distinct. On teardown
    // alone discard its buffered bytes so stdio can close after the proven
    // process exit (or after killing our own child on a body failure).
    child?.stderr.destroy();
    lifecycle?.killIfRunning();
    await finishFixture(failure, [lifecycle], () => removeFixtureHomes(engramHome));
  }
  if (failure) throw failure;
});
