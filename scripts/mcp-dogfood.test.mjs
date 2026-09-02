#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");

const AGENT_TOOLS = [
  "next",
  "ls",
  "show",
  "add",
  "claim",
  "update",
  "gate",
  "note",
  "done",
  "search",
  "handoff",
  "remember",
  "memories",
  "forget",
];
const HASH = /\b[0-9a-f]{64}\b/u;

class McpClient {
  constructor(engramHome, sessionId) {
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
    this.args = [...args];
    this.child = spawn(binary, args, {
      cwd: root,
      env: process.env,
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
    const initialized = await this.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "engram-dogfood", version: "1" },
    });
    this.instructions = initialized.instructions;
    this.notify("notifications/initialized");
    return initialized;
  }

  async toolNames() {
    return new Set((await this.tools()).map(({ name }) => name));
  }

  async tools() {
    const listed = await this.request("tools/list", {});
    return listed.tools;
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

function cliWord(engramHome, actorId, word, ...agentArgs) {
  const args = [
    "--home",
    engramHome,
    "work",
    "--actor-id",
    actorId,
    "--session-id",
    actorId,
    word,
    ...agentArgs,
  ];
  return spawnSync(binary, args, { cwd: root, encoding: "utf8" });
}

function cliText(engramHome, actorId, word, ...agentArgs) {
  const executed = cliWord(engramHome, actorId, word, ...agentArgs);
  assert.equal(executed.status, 0, executed.stderr);
  assert.doesNotMatch(executed.stdout, HASH, executed.stdout);
  assert.doesNotMatch(executed.stdout, /fence|idempotency/iu, executed.stdout);
  return executed.stdout;
}

function cliJson(engramHome, actorId, word, ...agentArgs) {
  const executed = cliWord(engramHome, actorId, word, ...agentArgs, "--json");
  assert.equal(executed.status, 0, executed.stderr);
  return JSON.parse(executed.stdout);
}

test("CLI words translate the same ambient lifecycle service", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-work-cli-"));
  try {
    buildAndInit(engramHome);
    const actor = "cli-work-agent";
    const added = cliText(
      engramHome,
      actor,
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

    const next = cliJson(engramHome, actor, "next", "--verbose");
    assert.equal(next.session.focused_work_id, next.focus.status.work.work_id);
    assert.equal(next.focus.status.work.short_ref, workRef);
    assert.ok(Array.isArray(next.changes));
    const listed = cliText(engramHome, actor, "ls", "--label", "dogfood");
    assert.match(
      listed,
      /^1 item\(s\):\n\s+w-[0-9a-f]{12} \[chore\] p1 open\/ready "Dogfood work CLI" labels:dogfood/u,
    );
    const nothingMine = cliText(engramHome, actor, "ls", "--mine");
    assert.match(nothingMine, /^0 item\(s\):/u);

    const claimed = cliText(engramHome, actor, "claim", workRef, "--ttl", "300");
    assert.match(claimed, /^claimed w-[0-9a-f]{12} "Dogfood work CLI" \(held by you until \d{2}:\d{2} UTC\)/u);
    const coreRefusal = spawnSync(
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
        "complete",
        "--work-ref",
        workRef,
        "--input",
        JSON.stringify({
          capture: null,
          evidence: [],
          acceptance: [],
          idempotency_key: "core-refusal-current-contract",
        }),
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(coreRefusal.status, 1, coreRefusal.stderr);
    assert.equal(coreRefusal.stderr, "");
    const coreRefusalReceipt = JSON.parse(coreRefusal.stdout);
    assert.equal(coreRefusalReceipt.code, "missing_acceptance");
    assert.equal(coreRefusalReceipt.work_id, coreRefusalReceipt.recovery.item.work_id);
    assert.equal(coreRefusalReceipt.recovery.cause.kind, "missing_acceptance");
    assert.equal(coreRefusalReceipt.recovery.item.ref, workRef);
    const mine = cliText(engramHome, actor, "ls", "--mine");
    assert.match(mine, /^1 item\(s\):/u);

    const shown = cliText(engramHome, actor, "show", workRef);
    assert.match(shown, /^w-[0-9a-f]{12} "Dogfood work CLI" — held by you until/u);
    assert.match(shown, /kind: chore  priority: 1  labels: dogfood/u);
    assert.match(shown, /outcome: The shell completes an ambient local lifecycle/u);
    assert.match(shown, /acceptance:\n\s+- CLI completion is sealed/u);
    assert.match(shown, /reminders:\n\s+- you hold this item but have not noted progress yet/u);
    assert.match(shown, new RegExp(`\\s+engram work note ${workRef} "…"`, "u"));
    assert.doesNotMatch(shown, new RegExp(`engram work show ${workRef}`, "u"));

    const blocked = cliText(engramHome, actor, "update", "--blocked", "waiting on a review");
    assert.match(blocked, /^blocked w-[0-9a-f]{12} "Dogfood work CLI": waiting on a review/u);
    assert.match(blocked, /reminders:\n(?:.*\n)*\s+- blocked: waiting on a review/u);
    assert.match(blocked, new RegExp(`\\s+engram work update ${workRef} --unblock`, "u"));
    const blockedList = cliText(engramHome, actor, "ls", "--blocked");
    assert.match(blockedList, /^1 item\(s\):/u);
    const unblocked = cliText(engramHome, actor, "update", workRef, "--unblock");
    assert.match(unblocked, /^unblocked w-/u);

    const noted = cliText(
      engramHome,
      actor,
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
      "note",
      workRef,
      "CLI lifecycle assertions passed",
      "--ref",
      "test:cli-work-dogfood",
    );
    assert.equal(notedJson.operation, "note");
    assert.match(notedJson.evidence.result, HASH);
    assert.ok(Array.isArray(notedJson.allowed_next));

    const done = cliText(engramHome, actor, "done");
    assert.match(done, /^done w-[0-9a-f]{12} "Dogfood work CLI"\nreminders: none\nnext:\n/u);
    assert.match(done, /\s+engram work next/u);
    const doneJson = cliJson(engramHome, actor, "done");
    assert.match(doneJson.seal, HASH);
    const focused = cliJson(engramHome, actor, "show", workRef);
    assert.equal(focused.status.work.lifecycle, "completed");
    assert.equal(focused.evidence.length, 1);
    const closedList = cliText(engramHome, actor, "ls");
    assert.match(closedList, /^0 item\(s\):/u);
    const allList = cliText(engramHome, actor, "ls", "--all", "--search", "dogfood work");
    assert.match(
      allList,
      /^1 item\(s\):\n\s+w-[0-9a-f]{12} \[chore\] p1 completed\/closed/u,
    );

    // `add --under` translates to a one-child decomposition and focuses the
    // new child; the text receipt names both items and no hash.
    const parentPlan = cliText(engramHome, actor, "add", "Parent plan");
    const parentRef = parentPlan.match(/\bw-[0-9a-f]{12}\b/u)?.[0];
    assert.ok(parentRef, parentPlan);
    const child = cliWord(
      engramHome,
      actor,
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
    const coreFocus = spawnSync(
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
    assert.equal(coreFocus.status, 0, coreFocus.stderr);
    assert.equal(JSON.parse(coreFocus.stdout).status.work.lifecycle, "completed");
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
    // Both sessions receive the same fourteen-tool MCP surface with only
    // project and asserted actor/session bindings.
    a = new McpClient(engramHome, "work-agent-a");
    b = new McpClient(engramHome, "work-agent-b");
    await Promise.all([a.initialize(), b.initialize()]);
    assert.match(
      a.instructions,
      /Thirteen words: next, ls, show, add, claim, update, gate, note, done, handoff, remember, memories, forget \(plus search\)/u,
    );
    assert.doesNotMatch(a.instructions, /Ten words/u);
    const aToolDefinitions = await a.tools();
    const aTools = new Set(aToolDefinitions.map(({ name }) => name));
    assert.deepEqual([...aTools].sort(), [...AGENT_TOOLS].sort());
    for (const tool of aToolDefinitions) {
      assert.doesNotMatch(
        JSON.stringify(tool.inputSchema),
        /"(?:idempotency_key|fence)"/u,
        `${tool.name} exposes a host-owned protocol field`,
      );
    }
    const nextProperties = aToolDefinitions.find(
      ({ name }) => name === "next",
    ).inputSchema.properties;
    assert.ok(nextProperties.verbose);
    assert.equal(nextProperties.context_generation.maxLength, 256);
    assert.ok(
      aToolDefinitions.find(({ name }) => name === "ls").inputSchema.properties
        .verbose,
    );
    const updateProperties = aToolDefinitions.find(
      ({ name }) => name === "update",
    ).inputSchema.properties;
    for (const field of [
      "kind",
      "labels",
      "unlabels",
      "prerequisite",
      "replacement",
    ]) {
      assert.ok(updateProperties[field], `update is missing ${field}`);
    }
    const gateProperties = aToolDefinitions.find(
      ({ name }) => name === "gate",
    ).inputSchema.properties;
    for (const field of ["name", "failed", "evidence_ref"]) {
      assert.ok(gateProperties[field], `gate is missing ${field}`);
    }
    assert.match(
      gateProperties.evidence_ref.description,
      /opaque external-evidence reference/u,
    );
    const memoriesProperties = aToolDefinitions.find(
      ({ name }) => name === "memories",
    ).inputSchema.properties;
    const rememberProperties = aToolDefinitions.find(
      ({ name }) => name === "remember",
    ).inputSchema.properties;
    const forgetProperties = aToolDefinitions.find(
      ({ name }) => name === "forget",
    ).inputSchema.properties;
    assert.equal(rememberProperties.text.maxLength, 8192);
    assert.equal(rememberProperties.key.maxLength, 64);
    assert.equal(memoriesProperties.query.maxLength, 256);
    assert.equal(memoriesProperties.after.maxLength, 64);
    assert.equal(forgetProperties.key.maxLength, 64);

    const remembered = receipt(
      await a.call("remember", {
        text: "MCP project observation\nfull body",
        key: "mcp-project-note",
      }),
    );
    assert.equal(remembered.key, "mcp-project-note");
    assert.equal(remembered.duplicate, false);
    const memorySignal = receipt(
      await a.call("next", { context_generation: "mcp-context-1" }),
    ).memories;
    assert.equal(memorySignal.count, 1);
    assert.equal(memorySignal.changed, true);
    const peerMemories = receipt(await b.call("memories", {}));
    assert.equal(peerMemories.memories[0].key, "mcp-project-note");
    assert.equal(peerMemories.memories[0].body, undefined);
    const fullMemory = receipt(
      await b.call("memories", { query: "mcp-project-note", full: true }),
    );
    assert.equal(fullMemory.body, "MCP project observation\nfull body");
    structuredError(
      await a.call("remember", {
        text: "different content",
        key: "mcp-project-note",
      }),
      "memory_exists",
    );
    assert.equal(
      receipt(await a.call("forget", { key: "mcp-project-note" })).duplicate,
      false,
    );
    structuredError(
      await b.call("memories", {
        query: "mcp-project-note",
        full: true,
      }),
      "memory_retired",
    );

    const mcpDependent = receipt(
      await a.call("add", { title: "MCP prerequisite dependent" }),
    ).work;
    const mcpPrerequisite = receipt(
      await a.call("add", { title: "MCP prerequisite source" }),
    ).work;
    const mcpReplacement = receipt(
      await a.call("add", { title: "MCP supersession replacement" }),
    ).work;
    const afterReceipt = receipt(
      await a.call("update", {
        work_ref: mcpDependent.short_ref,
        action: "after",
        prerequisite: mcpPrerequisite.short_ref,
      }),
    );
    assert.equal(afterReceipt.operation, "add_prerequisite");
    assert.equal(
      receipt(
        await a.call("show", { work_ref: mcpDependent.short_ref }),
      ).prerequisites[0].short_ref,
      mcpPrerequisite.short_ref,
    );
    const missingPrerequisite = structuredError(
      await a.call("update", {
        work_ref: mcpDependent.short_ref,
        action: "after",
      }),
      "work_invalid",
    );
    assert.match(missingPrerequisite.message, /needs the prerequisite item ref/u);
    const dropAfterReceipt = receipt(
      await a.call("update", {
        work_ref: mcpDependent.short_ref,
        action: "drop_after",
        prerequisite: mcpPrerequisite.short_ref,
      }),
    );
    assert.equal(dropAfterReceipt.operation, "remove_prerequisite");
    assert.deepEqual(
      receipt(
        await a.call("show", { work_ref: mcpDependent.short_ref }),
      ).prerequisites,
      [],
    );
    assert.match(
      structuredError(
        await a.call("update", {
          work_ref: mcpDependent.short_ref,
          action: "supersede",
          reason: "missing replacement",
        }),
        "work_invalid",
      ).message,
      /supersession needs the replacement item ref/u,
    );
    assert.match(
      structuredError(
        await a.call("update", {
          work_ref: mcpDependent.short_ref,
          action: "supersede",
          replacement: mcpReplacement.short_ref,
        }),
        "work_invalid",
      ).message,
      /supersession needs a reason/u,
    );
    const supersedeReceipt = receipt(
      await a.call("update", {
        work_ref: mcpDependent.short_ref,
        action: "supersede",
        replacement: mcpReplacement.short_ref,
        reason: "MCP replacement owns this outcome",
      }),
    );
    assert.equal(supersedeReceipt.operation, "supersede");
    assert.equal(
      supersedeReceipt.receipt.result.superseded_by,
      mcpReplacement.work_id,
    );

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

    const next = receipt(await a.call("next", { limit: 20, verbose: true }));
    assert.equal(next.session.focused_work_id, added.work.work_id);
    assert.ok(next.delivered_through > 0);
    assert.match(next.delivery_token, /^[0-9a-f-]{36}$/u);
    assert.equal(next.session.confirmed_project_cursor, 0);
    // The previous page counts as delivered once the session asks again; no
    // agent-side acknowledgement exists.
    const following = receipt(
      await a.call("next", { limit: 20, verbose: true }),
    );
    assert.equal(following.session.confirmed_project_cursor, next.delivered_through);
    assert.equal(following.delivered_through, next.delivered_through);
    assert.deepEqual(following.changes, []);
    const compactNext = receipt(await a.call("next", { limit: 20 }));
    assert.equal(compactNext.focus.ref, workRef);
    assert.equal(compactNext.focus.title, "Dogfood local work");
    assert.equal(compactNext.focus.lifecycle, "open");
    assert.equal("acceptance" in compactNext.focus, false);
    assert.equal("work_id" in compactNext.focus, false);
    assert.equal("session" in compactNext, false);
    assert.equal("delivery_token" in compactNext, false);
    // An identical keyless call replays instead of duplicating.
    const keyless = receipt(await a.call("add", { title: "Keyless root" }));
    assert.equal(keyless.kind, "root");
    assert.equal(keyless.work.outcome, "Keyless root");
    assert.deepEqual(keyless.work.acceptance, ["Keyless root is done"]);
    const keylessReplay = receipt(await a.call("add", { title: "Keyless root" }));
    assert.equal(keylessReplay.work.work_id, keyless.work.work_id);
    const keylessCatalog = receipt(await a.call("ls", { search: "keyless root" }));
    assert.equal(keylessCatalog.items.length, 1);
    assert.equal(keylessCatalog.items[0].ref, keyless.work.short_ref);
    assert.equal("work" in keylessCatalog.items[0], false);
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
    const gate = receipt(
      await a.call("gate", {
        name: "CARGO-TEST",
        failed: ["work::one", "work::two"],
        evidence_ref: "target/test.log",
      }),
    );
    assert.equal(gate.operation, "gate");
    assert.deepEqual(gate.gate, {
      name: "cargo-test",
      passed: false,
      failed_count: 2,
      referenced: true,
    });
    assert.match(
      receipt(await a.call("show", { work_ref: workRef })).evidence_items.at(-1)
        .summary,
      /^gate cargo-test failed/u,
    );
    const replayedClaim = receipt(
      await a.call("claim", { work_ref: workRef, ttl_seconds: 300 }),
    );
    assert.equal(replayedClaim.operation, "claim");
    assert.equal(replayedClaim.receipt.work_id, added.work.work_id);
    assert.equal("focus" in replayedClaim, false);
    assert.ok(Array.isArray(replayedClaim.obligations));
    assert.ok(Array.isArray(replayedClaim.allowed_next));
    const metadataRevised = receipt(
      await a.call("update", {
        action: "revise",
        kind: "bug",
        labels: ["triaged", "phoenix"],
        unlabels: ["dogfood"],
      }),
    );
    assert.equal(metadataRevised.operation, "revise");
    const metadataShow = receipt(await a.call("show", { work_ref: workRef }));
    assert.equal(metadataShow.status.work.kind, "bug");
    assert.deepEqual(metadataShow.status.work.labels, ["phoenix", "triaged"]);
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
    assert.equal(completedCatalog.items[0].ref, workRef);
    assert.equal(completedCatalog.items[0].lifecycle, "completed");
    assert.equal("work" in completedCatalog.items[0], false);
    assert.equal("changes" in completedCatalog, false);
    assert.equal("delivered_through" in completedCatalog, false);
    const verboseCompletedCatalog = receipt(
      await b.call("ls", {
        search: "dogfood local work",
        all: true,
        verbose: true,
      }),
    );
    assert.equal(
      verboseCompletedCatalog.items[0].work.work_id,
      added.work.work_id,
    );
    const searched = receipt(await b.call("search", { query: "dogfood local work" }));
    assert.equal(searched.items.length, 1);

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
