#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const binary = join(root, "target", "debug", "engram");

const AGENT_TOOLS = [
  "next",
  "ls",
  "show",
  "add",
  "claim",
  "update",
  "note",
  "done",
  "search",
  "handoff",
];
const LEGACY_TOOLS = [
  "task_start",
  "task_join",
  "memory_note",
  "memory_contradict",
  "memory_context",
  "memory_delta",
  "memory_search",
  "memory_show",
  "context_explain",
  "task_claim",
  "work_next",
  "work_focus",
  "work_propose",
  "work_update",
  "work_complete",
  "work_handoff",
];
const CONTROL_TOOLS = [
  "session_bind",
  "session_status",
  "lease_acquire",
  "lease_release",
  "turn_evaluate",
  "turn_begin",
  "turn_checkpoint",
];
const HASH = /\b[0-9a-f]{64}\b/u;

class McpClient {
  constructor(engramHome, sessionId, options = {}) {
    const {
      workAuthorityGrant,
      environmentAuthorityGrant,
      legacyTools = false,
    } = options;
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = "";
    this.buffer = "";
    const args = [
      "--home",
      engramHome,
      "mcp",
      "--actor-id",
      sessionId,
      "--session-id",
      sessionId,
      "--source-skill",
      "engram-dogfood",
    ];
    if (workAuthorityGrant) {
      args.push("--work-authority-grant", workAuthorityGrant);
    }
    if (legacyTools) {
      args.push("--legacy-tools");
    }
    const environment = { ...process.env };
    delete environment.ENGRAM_WORK_AUTHORITY_GRANT;
    if (environmentAuthorityGrant !== undefined) {
      environment.ENGRAM_WORK_AUTHORITY_GRANT = environmentAuthorityGrant;
    }
    this.args = [...args];
    this.child = spawn(binary, args, {
      cwd: root,
      env: environment,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
    this.closed = new Promise((resolvePromise) => {
      this.child.once("close", (code, signal) => {
        resolvePromise({ code, signal });
      });
    });
    this.child.on("exit", (code, signal) => {
      const error = new Error(
        `MCP server exited code=${code} signal=${signal}: ${this.stderr}`,
      );
      for (const { reject } of this.pending.values()) reject(error);
      this.pending.clear();
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
      const message = JSON.parse(line);
      if (message.id === undefined) continue;
      const pending = this.pending.get(String(message.id));
      if (!pending) continue;
      this.pending.delete(String(message.id));
      if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
      else pending.resolve(message.result);
    }
  }

  request(method, params) {
    const id = this.nextId++;
    const message = { jsonrpc: "2.0", id, method };
    if (params !== undefined) message.params = params;
    return new Promise((resolvePromise, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(String(id));
        reject(new Error(`MCP request timed out: ${method}; stderr=${this.stderr}`));
      }, 5000);
      this.pending.set(String(id), {
        resolve: (value) => {
          clearTimeout(timer);
          resolvePromise(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.child.stdin.write(`${JSON.stringify(message)}\n`);
    });
  }

  notify(method, params) {
    const message = { jsonrpc: "2.0", method };
    if (params !== undefined) message.params = params;
    this.child.stdin.write(`${JSON.stringify(message)}\n`);
  }

  async initialize() {
    await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "engram-dogfood", version: "1" },
    });
    this.notify("notifications/initialized");
  }

  async toolNames() {
    const listed = await this.request("tools/list", {});
    return new Set(listed.tools.map(({ name }) => name));
  }

  async call(name, arguments_ = {}) {
    const started = performance.now();
    const result = await this.request("tools/call", {
      name,
      arguments: arguments_,
    });
    assert.ok(performance.now() - started < 1000, `${name} exceeded one second`);
    return result;
  }

  async close() {
    if (!this.child.stdin.destroyed) this.child.stdin.end();
    let timer;
    try {
      const { code, signal } = await Promise.race([
        this.closed,
        new Promise((_, reject) => {
          timer = setTimeout(() => {
            this.child.kill();
            reject(new Error(`MCP server did not close: ${this.stderr}`));
          }, 5000);
        }),
      ]);
      assert.equal(signal, null, `MCP server terminated by ${signal}`);
      assert.equal(code, 0, this.stderr);
    } finally {
      clearTimeout(timer);
    }
  }
}

function structured(result) {
  assert.equal(result.isError ?? false, false, JSON.stringify(result));
  assert.ok(result.structuredContent, JSON.stringify(result));
  return result.structuredContent;
}

function receipt(result) {
  const value = structured(result);
  assert.ok(Array.isArray(value.reminders), JSON.stringify(value));
  assert.ok(Array.isArray(value.next), JSON.stringify(value));
  for (const line of [...value.reminders, ...value.next]) {
    assert.equal(typeof line, "string");
    assert.doesNotMatch(line, HASH, line);
    assert.doesNotMatch(line, /fence|idempotency/iu, line);
  }
  return value;
}

function structuredError(result, code) {
  assert.equal(result.isError, true, JSON.stringify(result));
  assert.equal(result.structuredContent.error.code, code);
  return result.structuredContent.error;
}

async function wait(milliseconds) {
  await new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

function buildAndInit(engramHome) {
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
      "dogfood-host",
      "--reason",
      "MCP local-work dogfood",
    ],
    { cwd: root, encoding: "utf8" },
  );
  assert.equal(granted.status, 0, granted.stderr);
  return JSON.parse(granted.stdout).grant;
}

test("legacy memory loop still works for two sessions under --legacy-tools", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-mcp-dogfood-"));
  let a;
  let b;
  try {
    buildAndInit(engramHome);

    a = new McpClient(engramHome, "eval-a", { legacyTools: true });
    b = new McpClient(engramHome, "eval-b", { legacyTools: true });
    await Promise.all([a.initialize(), b.initialize()]);

    const listed = await a.request("tools/list", {});
    const toolNames = new Set(listed.tools.map(({ name }) => name));
    for (const name of [...AGENT_TOOLS, ...LEGACY_TOOLS]) {
      assert.ok(toolNames.has(name), `missing MCP tool ${name}`);
    }
    for (const name of [
      "work_propose",
      "work_update",
      "work_complete",
      "work_handoff",
    ]) {
      const tool = listed.tools.find((candidate) => candidate.name === name);
      const schema = JSON.stringify(tool.inputSchema);
      assert.ok(schema.includes("idempotency_key"), `${name} has a generic input schema`);
      assert.ok(schema.includes("required"), `${name} does not expose required fields`);
    }
    for (const name of AGENT_TOOLS) {
      const tool = listed.tools.find((candidate) => candidate.name === name);
      const schema = JSON.stringify(tool.inputSchema);
      assert.equal(schema.includes("idempotency_key"), false, `${name} asks for a key`);
      assert.equal(schema.includes("fence"), false, `${name} asks for a fence`);
    }
    for (const name of CONTROL_TOOLS) {
      assert.ok(!toolNames.has(name), `control tool leaked through MCP: ${name}`);
    }

    const started = structured(
      await a.call("task_start", {
        external_ref: "dummy:TASK-7",
        title: "Dogfood Engram",
      }),
    );
    const joined = structured(
      await b.call("task_join", { external_ref: "dummy:TASK-7" }),
    );
    assert.equal(joined.task.task_id, started.task.task_id);

    const first = structured(
      await a.call("memory_note", {
        prose:
          "Decided: report freeze binds one payload to one idempotency key — retries must be byte-identical.",
        idempotency_key: "dogfood-note-1",
      }),
    );
    assert.equal(first.kind, "decision");
    assert.equal(first.scope.kind, "task");
    assert.ok(first.classification_reason.includes("prefix"));

    const packet = structured(await b.call("memory_context"));
    assert.ok(packet.index.some(({ version }) => version === first.version));
    assert.ok(packet.header.packet_hash);
    const firstCursor = packet.header.event_cursor;

    const second = structured(
      await a.call("memory_note", {
        prose: "Decision: attach retry-test evidence to the frozen report.",
        refs: ["repo:test/mcp-dogfood"],
        idempotency_key: "dogfood-note-2",
      }),
    );
    const delta = structured(
      await b.call("memory_delta", { after: firstCursor, limit: 100 }),
    );
    assert.equal(delta.changes.length, 1);
    assert.equal(delta.changes[0].memory.version, second.version);
    assert.ok(delta.cursor > firstCursor);

    const privateNote = structured(
      await a.call("memory_note", {
        prose: "scratch: half-formed hypothesis Z",
        private: true,
        idempotency_key: "dogfood-private",
      }),
    );
    assert.equal(privateNote.scope.kind, "agent");
    assert.equal(privateNote.cursor, null);
    const peerSearch = structured(
      await b.call("memory_search", { query: "hypothesis Z" }),
    );
    assert.deepEqual(peerSearch, []);
    const peerAfterPrivate = structured(
      await b.call("memory_delta", { after: delta.cursor }),
    );
    assert.equal(peerAfterPrivate.changes.length, 0);
    structuredError(
      await b.call("memory_show", { hash: privateNote.version }),
      "memory_access_denied",
    );

    const beforeRestart = JSON.stringify(delta);
    await b.close();
    b = new McpClient(engramHome, "eval-b", { legacyTools: true });
    await b.initialize();
    const afterRestart = structured(
      await b.call("memory_delta", { after: firstCursor, limit: 100 }),
    );
    assert.equal(JSON.stringify(afterRestart), beforeRestart);
    const shown = structured(
      await b.call("memory_show", { hash: first.version }),
    );
    assert.equal(shown.version.actor.session_id, "eval-a");
    assert.equal(shown.version.actor.source_tool, "mcp:memory_note");
    assert.ok(shown.version.classification_reason.includes("prefix"));
    const explanation = structured(
      await b.call("context_explain", { hash: packet.header.packet_hash }),
    );
    assert.equal(explanation.event_cursor, firstCursor);
    assert.ok(explanation.index.some(({ version }) => version === first.version));

    const retry = structured(
      await a.call("memory_note", {
        prose: "Decision: attach retry-test evidence to the frozen report.",
        refs: ["repo:test/mcp-dogfood"],
        idempotency_key: "dogfood-note-2",
      }),
    );
    assert.equal(retry.version, second.version);
    assert.equal(retry.duplicate, true);
    const afterRetry = structured(
      await b.call("memory_delta", { after: delta.cursor }),
    );
    assert.equal(afterRetry.changes.length, 0);
    structuredError(
      await a.call("memory_note", {
        prose: "Decision: changed meaning under a reused key.",
        refs: ["repo:test/mcp-dogfood"],
        idempotency_key: "dogfood-note-2",
      }),
      "note_idempotency_conflict",
    );

    const claimA = structured(
      await a.call("task_claim", {
        ttl_seconds: 1,
        idempotency_key: "claim-a",
      }),
    );
    const claimReplay = structured(
      await a.call("task_claim", {
        ttl_seconds: 1,
        idempotency_key: "claim-a",
      }),
    );
    assert.deepEqual(claimReplay, claimA);
    structuredError(
      await a.call("task_claim", {
        ttl_seconds: 2,
        idempotency_key: "claim-a",
      }),
      "claim_idempotency_conflict",
    );
    const held = structuredError(
      await b.call("task_claim", {
        ttl_seconds: 1,
        idempotency_key: "claim-b-live",
      }),
      "task_claim_held",
    );
    assert.equal(held.details.holder, "eval-a");
    assert.ok(held.details.expires_at_ms);
    assert.match(held.details.expires_at, /^\d{4}-\d{2}-\d{2}T/u);
    await wait(1100);
    const claimB = structured(
      await b.call("task_claim", {
        ttl_seconds: 1,
        idempotency_key: "claim-b-expired",
      }),
    );
    assert.equal(claimB.revision, claimA.revision + 1);
    const claimDelta = structured(
      await a.call("memory_delta", { after: delta.cursor }),
    );
    assert.deepEqual(
      claimDelta.changes.map(({ object_kind }) => object_kind),
      ["task_claim_event", "task_claim_event"],
    );

    const naturalConstraint = structured(
      await a.call("memory_note", {
        prose: "Never publish before every participant is ready.",
        idempotency_key: "dogfood-constraint-a",
      }),
    );
    assert.equal(naturalConstraint.kind, "constraint");
    assert.equal(naturalConstraint.authority, "firm");
    assert.equal(naturalConstraint.delivery, "pinned");
    assert.match(naturalConstraint.classification_reason, /rule cue/u);
    const conflictingConstraint = structured(
      await a.call("memory_note", {
        prose: "Always publish immediately after local validation passes.",
        idempotency_key: "dogfood-constraint-b",
      }),
    );
    const contradiction = structured(
      await a.call("memory_contradict", {
        first_version: naturalConstraint.version,
        second_version: conflictingConstraint.version,
        reason: "the two publication timing rules cannot both be followed",
        idempotency_key: "dogfood-contradiction",
      }),
    );
    assert.ok(contradiction.contradiction);
    const contradictionReplay = structured(
      await a.call("memory_contradict", {
        first_version: conflictingConstraint.version,
        second_version: naturalConstraint.version,
        reason: "the two publication timing rules cannot both be followed",
        idempotency_key: "dogfood-contradiction",
      }),
    );
    assert.equal(contradictionReplay.contradiction, contradiction.contradiction);
    assert.equal(contradictionReplay.duplicate, true);
    const unsafePacket = structuredError(
      await b.call("memory_context"),
      "pinned_contradiction",
    );
    assert.equal(
      unsafePacket.details.contradiction_hash,
      contradiction.contradiction,
    );

    const doctor = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(doctor.status, 0, doctor.stderr);
    assert.match(doctor.stdout, /store is healthy/u);
    assert.match(doctor.stderr, /no-op redactor/u);
  } finally {
    await Promise.all([a?.close(), b?.close()]);
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("legacy tools are opt-in and still answer one call", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-mcp-legacy-"));
  let plain;
  let legacy;
  try {
    buildAndInit(engramHome);
    plain = new McpClient(engramHome, "legacy-probe");
    legacy = new McpClient(engramHome, "legacy-probe", { legacyTools: true });
    await Promise.all([plain.initialize(), legacy.initialize()]);
    const plainTools = await plain.toolNames();
    for (const name of AGENT_TOOLS) {
      assert.ok(plainTools.has(name), `missing agent tool ${name}`);
    }
    for (const name of [...LEGACY_TOOLS, ...CONTROL_TOOLS]) {
      assert.ok(!plainTools.has(name), `${name} registered without --legacy-tools`);
    }
    const legacyTools = await legacy.toolNames();
    for (const name of [...AGENT_TOOLS, ...LEGACY_TOOLS]) {
      assert.ok(legacyTools.has(name), `missing MCP tool ${name} under --legacy-tools`);
    }
    const started = structured(
      await legacy.call("task_start", {
        external_ref: "dummy:LEGACY-1",
        title: "Legacy tools answer",
      }),
    );
    assert.ok(started.task.task_id);
    const emptyNext = receipt(await plain.call("next", {}));
    assert.equal(emptyNext.focus, undefined);
    assert.deepEqual(emptyNext.next, ['engram work add "…"']);
  } finally {
    await Promise.all([plain?.close(), legacy?.close()]);
    rmSync(engramHome, { recursive: true, force: true });
  }
});

function cliWord(engramHome, actorId, grant, word, ...agentArgs) {
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
    word,
    ...agentArgs,
  ];
  return spawnSync(binary, args, { cwd: root, encoding: "utf8" });
}

function cliText(engramHome, actorId, grant, word, ...agentArgs) {
  const executed = cliWord(engramHome, actorId, grant, word, ...agentArgs);
  assert.equal(executed.status, 0, executed.stderr);
  assert.doesNotMatch(executed.stdout, HASH, executed.stdout);
  assert.doesNotMatch(executed.stdout, /fence|idempotency/iu, executed.stdout);
  return executed.stdout;
}

function cliJson(engramHome, actorId, grant, word, ...agentArgs) {
  const executed = cliWord(engramHome, actorId, grant, word, ...agentArgs, "--json");
  assert.equal(executed.status, 0, executed.stderr);
  return JSON.parse(executed.stdout);
}

test("CLI words translate the same ambient lifecycle service", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-work-cli-"));
  try {
    buildAndInit(engramHome);
    const actor = "cli-work-agent";
    const grant = installWorkGrant(engramHome, actor);
    const added = cliText(
      engramHome,
      actor,
      grant,
      "add",
      "Dogfood work CLI",
      "--outcome",
      "The shell completes an ambient local lifecycle",
      "--accept",
      "CLI completion is sealed",
      "--kind",
      "chore",
      "--label",
      "dogfood",
    );
    const workRef = added.match(/\bw-[0-9a-f]{12}\b/u)?.[0];
    assert.ok(workRef, added);
    assert.match(added, /^added w-[0-9a-f]{12} "Dogfood work CLI"\n/u);
    assert.match(added, /reminders:\n\s+- unclaimed: claim it before you change anything/u);
    assert.match(added, new RegExp(`next:\\n(?:.*\\n)*\\s+engram work claim ${workRef}`, "u"));

    const next = cliJson(engramHome, actor, grant, "next");
    assert.equal(next.session.focused_work_id, next.focus.status.work.work_id);
    assert.equal(next.focus.status.work.short_ref, workRef);
    assert.ok(Array.isArray(next.changes));
    const listed = cliText(engramHome, actor, grant, "ls", "--label", "dogfood");
    assert.match(listed, /^1 item\(s\):\n\s+w-[0-9a-f]{12}\s+p1\s+ready\s+"Dogfood work CLI"\s+\[dogfood\]/u);
    const nothingMine = cliText(engramHome, actor, grant, "ls", "--mine");
    assert.match(nothingMine, /^0 item\(s\):/u);

    const claimed = cliText(engramHome, actor, grant, "claim", workRef, "--ttl", "300");
    assert.match(claimed, /^claimed w-[0-9a-f]{12} "Dogfood work CLI" \(held by you until \d{2}:\d{2} UTC\)/u);
    const mine = cliText(engramHome, actor, grant, "ls", "--mine");
    assert.match(mine, /^1 item\(s\):/u);

    const shown = cliText(engramHome, actor, grant, "show", workRef);
    assert.match(shown, /^w-[0-9a-f]{12} "Dogfood work CLI" — held by you until/u);
    assert.match(shown, /kind: chore  priority: 1  labels: dogfood/u);
    assert.match(shown, /outcome: The shell completes an ambient local lifecycle/u);
    assert.match(shown, /acceptance:\n\s+- CLI completion is sealed/u);
    assert.match(shown, /reminders:\n\s+- you hold this item but have not noted progress yet/u);
    assert.match(shown, new RegExp(`\\s+engram work note ${workRef} "…"`, "u"));
    assert.doesNotMatch(shown, new RegExp(`engram work show ${workRef}`, "u"));

    const blocked = cliText(engramHome, actor, grant, "update", "--blocked", "waiting on a review");
    assert.match(blocked, /^blocked w-[0-9a-f]{12} "Dogfood work CLI": waiting on a review/u);
    assert.match(blocked, /reminders:\n(?:.*\n)*\s+- blocked: waiting on a review/u);
    assert.match(blocked, new RegExp(`\\s+engram work update ${workRef} --unblock`, "u"));
    const blockedList = cliText(engramHome, actor, grant, "ls", "--blocked");
    assert.match(blockedList, /^1 item\(s\):/u);
    const unblocked = cliText(engramHome, actor, grant, "update", workRef, "--unblock");
    assert.match(unblocked, /^unblocked w-/u);

    const noted = cliText(
      engramHome,
      actor,
      grant,
      "note",
      "CLI lifecycle assertions passed",
      "--ref",
      "test:cli-work-dogfood",
    );
    assert.match(noted, /^noted on w-[0-9a-f]{12} "Dogfood work CLI": CLI lifecycle assertions passed/u);
    assert.doesNotMatch(noted, /have not noted progress/u);
    const notedJson = cliJson(
      engramHome,
      actor,
      grant,
      "note",
      workRef,
      "CLI lifecycle assertions passed",
      "--ref",
      "test:cli-work-dogfood",
    );
    assert.equal(notedJson.operation, "note");
    assert.match(notedJson.evidence.result, HASH);
    assert.ok(Array.isArray(notedJson.allowed_next));

    const done = cliText(engramHome, actor, grant, "done");
    assert.match(done, /^done w-[0-9a-f]{12} "Dogfood work CLI"\nreminders: none\nnext:\n/u);
    assert.match(done, /\s+engram work next/u);
    const doneJson = cliJson(engramHome, actor, grant, "done");
    assert.match(doneJson.seal, HASH);
    const focused = cliJson(engramHome, actor, grant, "show", workRef);
    assert.equal(focused.status.work.lifecycle, "completed");
    assert.equal(focused.evidence.length, 1);
    const closedList = cliText(engramHome, actor, grant, "ls");
    assert.match(closedList, /^0 item\(s\):/u);
    const allList = cliText(engramHome, actor, grant, "ls", "--all", "--search", "dogfood work");
    assert.match(allList, /^1 item\(s\):\n\s+w-[0-9a-f]{12}\s+p1\s+completed/u);

    // `add --under` translates to a one-child decomposition and focuses the
    // new child; the text receipt names both items and no hash.
    const parentPlan = cliText(engramHome, actor, grant, "add", "Parent plan");
    const parentRef = parentPlan.match(/\bw-[0-9a-f]{12}\b/u)?.[0];
    assert.ok(parentRef, parentPlan);
    const child = cliWord(
      engramHome,
      actor,
      grant,
      "add",
      "Follow-up step",
      "--under",
      parentRef,
    );
    assert.equal(child.status, 0, child.stderr);
    assert.match(
      child.stdout,
      new RegExp(`^added w-[0-9a-f]{12} "Follow-up step" under ${parentRef} "Parent plan"`, "u"),
    );
    assert.doesNotMatch(child.stdout, HASH);
    const legacyFocus = spawnSync(
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
        "focus",
        workRef,
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(legacyFocus.status, 0, legacyFocus.stderr);
    assert.equal(JSON.parse(legacyFocus.stdout).status.work.lifecycle, "completed");
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("two MCP sessions complete ambient work through a fenced handoff", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-work-dogfood-"));
  let a;
  let b;
  try {
    buildAndInit(engramHome);
    const grantA = installWorkGrant(engramHome, "work-agent-a");
    const grantB = installWorkGrant(engramHome, "work-agent-b");

    const mcpArgs = (sessionId) => [
      "--home",
      engramHome,
      "mcp",
      "--actor-id",
      sessionId,
      "--session-id",
      sessionId,
    ];
    const grantlessError = async (environmentAuthorityGrant, suffix) => {
      const client = new McpClient(engramHome, `grantless-${suffix}`, {
        environmentAuthorityGrant,
      });
      try {
        await client.initialize();
        return structuredError(
          await client.call("add", { title: "Grantless environment probe" }),
          "work_invalid",
        );
      } finally {
        await client.close();
      }
    };
    const unsetError = await grantlessError(undefined, "unset");
    assert.match(unsetError.details.reason, /did not bind a work-authority grant/u);
    assert.deepEqual(unsetError.reminders, [
      "the host has not granted this session work authority",
    ]);
    for (const [suffix, emptyGrant] of [
      ["empty", ""],
      ["whitespace", " \t "],
    ]) {
      const emptyError = await grantlessError(emptyGrant, suffix);
      assert.deepEqual(emptyError, unsetError);
    }
    const malformedValue = "not-a-work-authority-secret";
    const malformed = spawnSync(binary, mcpArgs("malformed-env"), {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        ENGRAM_WORK_AUTHORITY_GRANT: malformedValue,
      },
      input: "",
    });
    assert.notEqual(malformed.status, 0);
    assert.match(malformed.stderr, /invalid work-authority grant/u);
    assert.equal(malformed.stderr.includes(malformedValue), false);
    const helpSecret = "secret-help-value-that-must-not-appear";
    const help = spawnSync(binary, ["mcp", "--help"], {
      cwd: root,
      encoding: "utf8",
      env: { ...process.env, ENGRAM_WORK_AUTHORITY_GRANT: helpSecret },
    });
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /ENGRAM_WORK_AUTHORITY_GRANT/u);
    assert.match(help.stdout, /--legacy-tools/u);
    assert.equal(help.stdout.includes(helpSecret), false);

    // `a` speaks only the eight words; `b` is a host that also opted into the
    // legacy tools and passes its grant through argv over a malformed
    // environment value.
    a = new McpClient(engramHome, "work-agent-a", {
      environmentAuthorityGrant: grantA,
    });
    b = new McpClient(engramHome, "work-agent-b", {
      workAuthorityGrant: grantB,
      environmentAuthorityGrant: malformedValue,
      legacyTools: true,
    });
    assert.equal(a.args.includes(grantA), false);
    assert.equal(b.args.includes(grantB), true);
    await Promise.all([a.initialize(), b.initialize()]);
    const aTools = await a.toolNames();
    assert.equal(aTools.has("work_focus"), false);
    assert.equal(aTools.has("memory_note"), false);

    const added = receipt(
      await a.call("add", {
        title: "Dogfood local work",
        outcome: "Two MCP sessions finish through an ambient handoff",
        acceptance: ["recipient seals the validated result"],
        kind: "feature",
        priority: 1,
        labels: ["dogfood"],
      }),
    );
    assert.equal(added.kind, "root");
    const workRef = added.work.short_ref;
    assert.ok(added.reminders.includes("unclaimed: claim it before you change anything"));
    assert.ok(added.next.includes(`engram work claim ${workRef}`));

    const next = receipt(await a.call("next", { limit: 20 }));
    assert.equal(next.session.focused_work_id, added.work.work_id);
    assert.ok(next.delivered_through > 0);
    assert.match(next.delivery_token, /^[0-9a-f-]{36}$/u);
    assert.equal(next.session.confirmed_project_cursor, 0);
    // The previous page counts as delivered once the session asks again; no
    // agent-side acknowledgement exists.
    const following = receipt(await a.call("next", { limit: 20 }));
    assert.equal(following.session.confirmed_project_cursor, next.delivered_through);
    assert.equal(following.delivered_through, next.delivered_through);
    assert.deepEqual(following.changes, []);
    // An identical keyless call replays instead of duplicating.
    const keyless = receipt(await a.call("add", { title: "Keyless root" }));
    assert.equal(keyless.kind, "root");
    assert.equal(keyless.work.outcome, "Keyless root");
    assert.deepEqual(keyless.work.acceptance, ["Keyless root is done"]);
    const keylessReplay = receipt(await a.call("add", { title: "Keyless root" }));
    assert.equal(keylessReplay.work.work_id, keyless.work.work_id);
    const keylessCatalog = receipt(await a.call("ls", { search: "keyless root" }));
    assert.equal(keylessCatalog.items.length, 1);
    assert.ok(next.focus.allowed_next.includes("work_update:claim"));

    // work_ref selects the target in the same call: focus is still the keyless
    // root here, and the claim lands on the first root.
    const claimed = receipt(
      await a.call("claim", { work_ref: workRef, ttl_seconds: 300 }),
    );
    assert.equal(claimed.receipt.work_id, added.work.work_id);
    assert.ok(claimed.receipt.control_binding, JSON.stringify(claimed));
    assert.ok(
      claimed.reminders.includes("you hold this item but have not noted progress yet"),
      JSON.stringify(claimed.reminders),
    );
    assert.ok(claimed.next.includes(`engram work note ${workRef} "…"`));
    assert.ok(claimed.next.includes(`engram work done ${workRef} "…"`));
    const replayedClaim = receipt(
      await a.call("claim", { work_ref: workRef, ttl_seconds: 300 }),
    );
    assert.equal(replayedClaim.operation, "claim");
    assert.equal(replayedClaim.receipt.work_id, added.work.work_id);
    assert.equal("focus" in replayedClaim, false);
    assert.ok(Array.isArray(replayedClaim.obligations));
    assert.ok(Array.isArray(replayedClaim.allowed_next));
    receipt(
      await a.call("update", {
        action: "blocked",
        text: "Dogfood the agent-visible blocker identity",
      }),
    );
    const blockedShow = receipt(await a.call("show", { work_ref: workRef }));
    assert.equal(blockedShow.blockers.length, 1);
    assert.ok(blockedShow.blockers[0].blocker_id.length > 0);
    assert.ok(
      blockedShow.reminders.includes("blocked: Dogfood the agent-visible blocker identity"),
      JSON.stringify(blockedShow.reminders),
    );
    assert.ok(blockedShow.next.includes(`engram work update ${workRef} --unblock`));
    receipt(await a.call("update", { action: "unblock" }));
    assert.equal(
      receipt(await a.call("show", { work_ref: workRef })).blockers.length,
      0,
    );

    // The legacy-enabled host session captures work-scoped memory; the
    // eight-word session sees shared memory and never the private scratch.
    receipt(await b.call("show", { work_ref: workRef }));
    structured(
      await b.call("task_start", {
        external_ref: "dummy:WORK-MIXED-7",
        title: "Dogfood explicit mixed-context routing",
      }),
    );
    structuredError(
      await b.call("memory_note", {
        prose: "Decision: ambiguous mixed task and work capture must be refused.",
        idempotency_key: "work-ambiguous-memory",
      }),
      "invalid_argument",
    );
    const sharedWorkMemory = structured(
      await b.call("memory_note", {
        prose: "Decision: the focused work uses one local identity for task tracking and shared execution memory.",
        idempotency_key: "work-shared-memory",
        target: "work",
      }),
    );
    assert.equal(sharedWorkMemory.scope.kind, "work");
    assert.equal(sharedWorkMemory.scope.work, added.work.work_id);
    assert.ok(sharedWorkMemory.work_positions.length >= 2);
    const privateWorkMemory = structured(
      await b.call("memory_note", {
        prose: "scratch: private focused implementation hypothesis",
        private: true,
        idempotency_key: "work-private-memory",
        target: "work",
      }),
    );
    assert.equal(privateWorkMemory.scope.kind, "agent");
    assert.equal(privateWorkMemory.scope.work, added.work.work_id);
    assert.deepEqual(privateWorkMemory.work_positions, []);
    const authorMemoryFocus = structured(
      await b.call("work_focus", { work_ref: workRef }),
    );
    assert.ok(
      authorMemoryFocus.memories.some(
        ({ version }) => version === sharedWorkMemory.version,
      ),
    );
    assert.ok(
      authorMemoryFocus.memories.some(
        ({ version }) => version === privateWorkMemory.version,
      ),
    );
    const peerMemoryFocus = receipt(await a.call("show", { work_ref: workRef }));
    assert.ok(
      peerMemoryFocus.memories.some(
        ({ version }) => version === sharedWorkMemory.version,
      ),
    );
    assert.equal(
      peerMemoryFocus.memories.some(
        ({ version }) => version === privateWorkMemory.version,
      ),
      false,
    );
    const peerWorkChanges = receipt(await a.call("next", { limit: 100 })).changes;
    assert.ok(
      peerWorkChanges.some(
        ({ entry }) => entry.object_hash === sharedWorkMemory.version,
      ),
    );
    assert.equal(
      peerWorkChanges.some(
        ({ entry }) => entry.object_hash === privateWorkMemory.version,
      ),
      false,
    );
    const shownWorkMemory = structured(
      await b.call("memory_show", { hash: sharedWorkMemory.version }),
    );
    assert.equal(shownWorkMemory.version.scope.work, added.work.work_id);
    assert.equal(shownWorkMemory.version.authority, "firm");

    const held = structuredError(
      await b.call("claim", { work_ref: workRef, ttl_seconds: 300 }),
      "work_claim_held",
    );
    assert.equal(held.details.holder_session_id, "work-agent-a");
    assert.match(held.reminders[0], /^held by another session until /u);
    assert.deepEqual(held.next, [`engram work show ${workRef}`]);
    const noted = receipt(
      await a.call("note", {
        text: "MCP ambient lifecycle assertions passed",
        refs: ["test:mcp-work-dogfood"],
      }),
    );
    assert.equal(noted.operation, "note");
    assert.match(noted.evidence.result, HASH);
    assert.equal(
      noted.reminders.includes("you hold this item but have not noted progress yet"),
      false,
    );

    receipt(
      await a.call("handoff", {
        action: "offer",
        to: "work-agent-b",
        ttl_seconds: 240,
        summary: "handoff after MCP evidence capture",
      }),
    );
    const recipientFocus = receipt(await b.call("show", { work_ref: workRef }));
    assert.ok(recipientFocus.allowed_next.includes("work_handoff:accept"));
    assert.equal(recipientFocus.next[0], `engram work handoff ${workRef} --accept`);
    const accepted = receipt(await b.call("handoff", { action: "accept" }));
    assert.equal(accepted.operation, "accept");
    const stale = structuredError(
      await a.call("note", { text: "must be rejected after handoff" }),
      "work_claim_mismatch",
    );
    assert.deepEqual(stale.next, [`engram work claim ${workRef}`]);
    receipt(
      await b.call("note", {
        text: "recipient validated evidence and completion criterion",
      }),
    );
    structuredError(
      await b.call("work_complete", {
        input: {
          acceptance: [
            {
              satisfied: true,
              assurance: "signed",
              note: "agent must not self-assert signed assurance",
            },
          ],
          idempotency_key: "work-forged-assurance",
        },
      }),
      "invalid_argument",
    );
    const seal = receipt(
      await b.call("done", { summary: "validated by the receiving MCP session" }),
    );
    assert.equal(seal.work_id, added.work.work_id);
    assert.match(seal.seal, HASH);
    assert.deepEqual(seal.reminders, []);
    assert.ok(seal.next.includes("engram work next"));
    const completed = receipt(await b.call("show", { work_ref: workRef }));
    assert.equal(completed.status.work.lifecycle, "completed");
    assert.ok(completed.history.items.length > 0);
    assert.equal(completed.reminders.length, 0);
    const openOnly = receipt(await b.call("ls", { search: "dogfood local work" }));
    assert.equal(openOnly.items.length, 0);
    const completedCatalog = receipt(
      await b.call("ls", { search: "dogfood local work", all: true }),
    );
    assert.equal(completedCatalog.items.length, 1);
    assert.equal(completedCatalog.items[0].work.work_id, added.work.work_id);
    assert.equal(JSON.stringify(completedCatalog).includes(grantB), false);
    assert.equal("changes" in completedCatalog, false);
    assert.equal("delivered_through" in completedCatalog, false);
    const searched = receipt(await b.call("search", { query: "dogfood local work" }));
    assert.equal(searched.items.length, 1);

    const replacement = receipt(
      await b.call("add", {
        title: "MCP replacement plan",
        outcome: "A replacement remains visible in the local catalog",
        acceptance: ["replacement is evaluated"],
      }),
    ).work;
    const obsolete = receipt(
      await b.call("add", {
        title: "MCP obsolete plan",
        outcome: "The obsolete plan is not falsely completed",
        acceptance: ["obsolete plan is disposed honestly"],
      }),
    ).work;
    const superseded = structured(
      await b.call("work_update", {
        input: {
          kind: "supersede",
          replacement: replacement.short_ref,
          reason: "the replacement captures the revised plan",
          idempotency_key: "work-supersede-obsolete",
        },
      }),
    );
    assert.equal(superseded.receipt.result.lifecycle, "superseded");
    assert.equal(superseded.receipt.result.superseded_by, replacement.work_id);
    const supersededCatalog = receipt(
      await b.call("ls", { search: "obsolete plan", all: true }),
    );
    assert.equal(supersededCatalog.items.length, 1);
    assert.equal(supersededCatalog.items[0].work.work_id, obsolete.work_id);

    const disposable = receipt(
      await b.call("add", {
        title: "MCP disposable plan",
        outcome: "Cancellation remains distinct from completion",
        acceptance: ["cancellation is audited"],
      }),
    ).work;
    const cancelled = receipt(
      await b.call("update", {
        action: "cancel",
        reason: "the experiment is no longer needed",
      }),
    );
    assert.equal(cancelled.receipt.result.lifecycle, "cancelled");
    assert.equal(cancelled.receipt.work_id, disposable.work_id);
    assert.ok(cancelled.reminders.includes("this item was cancelled"));

    const compact = receipt(
      await b.call("add", {
        title: "MCP compact completion",
        outcome: "A normal local task closes with one evidence-backed completion call",
        acceptance: ["compact completion is sealed"],
      }),
    ).work;
    receipt(await b.call("claim", { work_ref: compact.short_ref }));
    const compactSeal = receipt(
      await b.call("done", {
        summary: "validated compact completion through the MCP lifecycle",
      }),
    );
    assert.equal(compactSeal.work_id, compact.work_id);
    const compactSealReplay = receipt(
      await b.call("done", {
        summary: "validated compact completion through the MCP lifecycle",
      }),
    );
    assert.equal(compactSealReplay.seal, compactSeal.seal);
    const compactFocus = receipt(await b.call("show", { work_ref: compact.short_ref }));
    assert.equal(compactFocus.status.work.lifecycle, "completed");
    assert.equal(compactFocus.evidence.length, 1);

    // `add` with `under` translates to a one-child decomposition and focuses
    // the new required child.
    const singleChild = receipt(
      await a.call("add", { title: "Child step", under: keyless.work.short_ref }),
    );
    assert.equal(singleChild.work.title, "Child step");
    assert.equal(singleChild.work.parent_id, keyless.work.work_id);
    assert.ok(Array.isArray(singleChild.reminders));
    assert.ok(Array.isArray(singleChild.next));
    const parentShow = receipt(await a.call("show", { work_ref: keyless.work.short_ref }));
    assert.equal(parentShow.children.length, 1);
    assert.equal(parentShow.children[0].title, "Child step");
    // Planning fields revise in one call, and deferral shows up as words.
    const rootRef = keyless.work.short_ref;
    const revised = receipt(
      await a.call("update", {
        work_ref: rootRef,
        action: "revise",
        title: "Keyless root (renamed)",
        priority: 2,
        defer: "2030-01-01",
      }),
    );
    assert.equal(revised.operation, "revise");
    assert.ok(revised.reminders.includes("deferred: its wake time has not arrived"));
    const revisedShow = receipt(await a.call("show", { work_ref: rootRef }));
    assert.equal(revisedShow.status.work.title, "Keyless root (renamed)");
    assert.equal(revisedShow.status.work.priority, 2);
    assert.equal(revisedShow.status.availability, "deferred");
    structuredError(
      await a.call("update", { work_ref: rootRef, action: "revise" }),
      "work_invalid",
    );
    structuredError(
      await a.call("update", { work_ref: rootRef, action: "revise", priority: 9 }),
      "work_invalid",
    );
  } finally {
    await Promise.all([a?.close(), b?.close()]);
    rmSync(engramHome, { recursive: true, force: true });
  }
});
