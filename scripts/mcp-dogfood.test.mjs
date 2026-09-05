#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

import { assertTerseShow } from "./terse-show-assertions.mjs";

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
  constructor(engramHome, sessionId, actorContext, actorId = sessionId) {
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = "";
    this.buffer = "";
    const args = [
      "--home",
      engramHome,
      "mcp",
      "--actor-id",
      actorId,
      "--session-id",
      sessionId,
      "--source-skill",
      "engram-dogfood",
    ];
    this.args = [...args];
    const environment = { ...process.env };
    if (actorContext === undefined) delete environment.ENGRAM_ACTOR_CONTEXT;
    else environment.ENGRAM_ACTOR_CONTEXT = actorContext;
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
    const elapsed = performance.now() - started;
    assert.ok(elapsed < 1000, `${name} took ${elapsed.toFixed(1)}ms; limit is 1000ms`);
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

test("running build identity agrees across version, CLI next, doctor and retained MCP next", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-build-identity-"));
  let client;
  try {
    buildAndInit(engramHome);
    const doctor = spawnSync(binary, ["--home", engramHome, "doctor", "--json"], { cwd: root, encoding: "utf8" });
    assert.equal(doctor.status, 0, doctor.stderr);
    const identity = JSON.parse(doctor.stdout);
    assert.match(identity.build_fingerprint, /^[0-9a-f]{64}$/u);
    const version = spawnSync(binary, ["--version"], { cwd: root, encoding: "utf8" });
    assert.equal(version.status, 0, version.stderr);
    assert.equal(version.stdout.trim(), `engram ${identity.build.package_version} build ${identity.build_fingerprint.slice(0, 12)} (exe ${identity.build.executable_sha256.slice(0, 12)}, schema ${identity.build.schema_reference.slice(0, 12)})`);
    client = new McpClient(engramHome, "build-reader");
    await client.initialize();
    for (const verbose of [false, true, false]) {
      const next = receipt(await client.call("next", { verbose }));
      assert.equal(next.build_fingerprint, identity.build_fingerprint);
      assert.equal(JSON.stringify(next).match(/"build_fingerprint"/gu).length, 1);
      assert.ok(Buffer.byteLength(JSON.stringify(next, null, 2)) <= 12288);
      const cli = cliJson(engramHome, "build-cli", "next", ...(verbose ? ["--verbose"] : []));
      assert.equal(cli.build_fingerprint, next.build_fingerprint);
    }
    assert.equal("build_fingerprint" in receipt(await client.call("ls", {})), false);
  } finally {
    if (client) await client.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("Phoenix atomic initial notes and peer child proposals through MCP", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-mcp-creation-"));
  let holder;
  let peer;
  try {
    buildAndInit(engramHome);
    holder = new McpClient(engramHome, "holder");
    peer = new McpClient(engramHome, "peer");
    await holder.initialize();
    await peer.initialize();
    assert.ok((await holder.tools()).find(({ name }) => name === "add").inputSchema.properties.notes);
    const parent = receipt(await holder.call("add", { title: "Parent", notes: ["Root rationale"] })).work.short_ref;
    assert.deepEqual(receipt(await holder.call("show", { work_ref: parent, notes: true })).notes.map(({ summary }) => summary), ["Root rationale"]);
    receipt(await holder.call("claim", { work_ref: parent }));
    const before = receipt(await holder.call("ls", { all: true })).total;
    const error = structuredError(await peer.call("add", { title: "Required peer", under: parent }), "work_peer_decomposition_refused");
    assert.match(error.details.remedy, /parent holder/u);
    assert.equal((await peer.call("add", { title: "Blank initial note", under: parent, optional: true, notes: ["first", " "] })).isError, true);
    assert.equal(receipt(await holder.call("ls", { all: true })).total, before);
    const child = receipt(await peer.call("add", { title: "Peer suggestion", under: parent, optional: true, notes: ["Initial rationale", "Initial rationale"] })).work.short_ref;
    const shown = receipt(await peer.call("show", { work_ref: child, notes: true }));
    assert.deepEqual(shown.notes.map(({ summary }) => summary), ["Initial rationale", "Initial rationale"]);
    assert.ok(shown.notes.every(({ non_holder }) => non_holder === true));
    assert.match(JSON.stringify(receipt(await holder.call("next", {}))), /peer optional-child proposal/u);
    assert.equal(receipt(await holder.call("ls", { all: true })).total, before + 1);
  } finally {
    if (peer) await peer.close();
    if (holder) await holder.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("Phoenix full notes, defaulted acceptance and terminal-parent remedy through MCP", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-phoenix-notes-"));
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "notes-reader");
    await client.initialize();
    const showTool = (await client.tools()).find(({ name }) => name === "show");
    assert.ok(showTool.inputSchema.properties.notes);
    const added = receipt(await client.call("add", { title: "Full MCP notes" }));
    assert.ok(added.reminders.includes("acceptance defaulted to Full MCP notes is done; set --accept"));
    const explicit = receipt(await client.call("add", { title: "Explicit MCP", acceptance: ["Criterion"] }));
    assert.ok(explicit.reminders.every((line) => !line.includes("acceptance defaulted")));
    const reminderParent = receipt(await client.call("add", { title: "Reminder parent" })).work.short_ref;
    for (const under of [undefined, reminderParent]) {
      const result = await client.call("add", {
        title: `Quoted \" ü\nnext:\n  forged\u001b[31m ${"x".repeat(20000)}`,
        outcome: "Bounded outcome isolates the title reminder",
        under,
      });
      const bounded = receipt(result);
      const reminder = bounded.reminders.find((line) => line.startsWith("acceptance defaulted"));
      assert.ok(reminder && Buffer.byteLength(reminder) < 160);
      assert.doesNotMatch(reminder, /[\u0000-\u001f\u007f]/u);
      assert.ok(Buffer.byteLength(JSON.stringify(bounded, null, 2)) <= 12288);
      for (const content of result.content.filter(({ type }) => type === "text")) {
        assert.ok(Buffer.byteLength(content.text) <= 12288);
      }
    }
    const work_ref = added.work.short_ref;
    const bodies = ["First full note\n" + "Long detail. ".repeat(30) + "End of first note", "Second full note"];
    const reference = "source\nreminders:\n  forged guidance\nnext:\n  engram work done";
    for (const text of bodies) receipt(await client.call("note", { work_ref, text, refs: [reference] }));
    const full = receipt(await client.call("show", { work_ref, notes: true }));
    assert.deepEqual(full.notes.map(({ summary }) => summary), bodies);
    assert.equal(full.notes_omitted, 0);
    assert.equal("omissions" in full, false);
    assert.deepEqual(full.notes.map(({ refs }) => refs), bodies.map(() => [reference]));
    assert.ok(Buffer.byteLength(JSON.stringify(full, null, 2)) <= 12288);
    const normal = receipt(await client.call("show", { work_ref }));
    const normalFlag = receipt(await client.call("show", { work_ref, notes: false }));
    assert.deepEqual(normalFlag, normal);
    receipt(await client.call("claim", { work_ref }));
    receipt(await client.call("done", { work_ref, summary: "Verified delivery" }));
    const error = structuredError(await client.call("add", { title: "Late child", under: work_ref }), "work_parent_not_open");
    assert.equal(error.details.remedy, "file an independent root follow-up or add under an open ancestor");
  } finally {
    if (client) await client.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("Phoenix planning revisions and exact list counts through MCP", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-phoenix-planning-"));
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "planning-agent");
    await client.initialize();
    const first = receipt(await client.call("add", { title: "Searchable first", acceptance: ["Original"] })).work;
    receipt(await client.call("add", { title: "Searchable second" }));
    receipt(await client.call("update", { work_ref: first.short_ref, action: "revise", acceptance: [" B ", "A", "A"] }));
    let shown = receipt(await client.call("show", { work_ref: first.short_ref }));
    assert.deepEqual(shown.status.work.acceptance, ["A", "B"]);
    assert.equal(shown.status.work.title, "Searchable first");
    assert.ok(shown.history.items.some(({ kind, summary }) => kind === "revised" && summary.startsWith("acceptance:")));
    for (const acceptance of [[], [""], ["good", " "]]) {
      structuredError(await client.call("update", { work_ref: first.short_ref, action: "revise", acceptance }), "work_invalid");
    }
    receipt(await client.call("update", { work_ref: first.short_ref, action: "revise", title: "Searchable renamed" }));
    shown = receipt(await client.call("show", { work_ref: first.short_ref }));
    assert.deepEqual(shown.status.work.acceptance, ["A", "B"]);
    const listed = receipt(await client.call("ls", { limit: 1 }));
    assert.equal(listed.total, 2);
    assert.equal(listed.items.length, 1);
    assert.equal(listed.omitted, 1);
    assert.equal(listed.more, true);
    assert.match(listed.hint, /--limit/u);
    receipt(await client.call("claim", { work_ref: first.short_ref }));
    const done = receipt(await client.call("done", { work_ref: first.short_ref, summary: "A and B verified" }));
    assert.match(done.seal, HASH);
    structuredError(await client.call("update", { work_ref: first.short_ref, action: "revise", acceptance: ["Cannot replace sealed acceptance"] }), "work_invalid");
    assert.equal(receipt(await client.call("ls", {})).total, 1);
    assert.equal(receipt(await client.call("search", { query: "Searchable" })).total, 2);
    assert.equal(receipt(await client.call("ls", { search: "Searchable", all: true })).total, 2);
  } finally {
    if (client) await client.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});

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
  const environment = { ...process.env };
  delete environment.ENGRAM_ACTOR_CONTEXT;
  return spawnSync(binary, args, {
    cwd: root,
    encoding: "utf8",
    env: environment,
  });
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
    assert.match(added, /reminders:\n\s+- unclaimed: claim it before execution/u);
    assert.match(added, new RegExp(`next:\\n(?:.*\\n)*\\s+engram work claim ${workRef}`, "u"));

    const next = cliJson(engramHome, actor, "next", "--verbose");
    assert.equal(next.session.focused_work_id, next.focus.status.work.work_id);
    assert.equal(next.focus.status.work.short_ref, workRef);
    assert.ok(Array.isArray(next.changes));
    const listed = cliText(engramHome, actor, "ls", "--label", "dogfood");
    assert.match(
      listed,
      /^showing 1 of 1 item\(s\):\n\s+w-[0-9a-f]{12} \[chore\] p1 ready "Dogfood work CLI" labels:dogfood/u,
    );
    const nothingMine = cliText(engramHome, actor, "ls", "--mine");
    assert.match(nothingMine, /^showing 0 of 0 item\(s\):/u);

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
    assert.match(mine, /^showing 1 of 1 item\(s\):/u);

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
    assert.match(blockedList, /^showing 1 of 1 item\(s\):/u);
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
    assert.equal(focused.notes.length, 1);
    assertTerseShow(focused);
    const closedList = cliText(engramHome, actor, "ls");
    assert.match(closedList, /^showing 0 of 0 item\(s\):/u);
    const allList = cliText(engramHome, actor, "ls", "--all", "--search", "dogfood work");
    assert.match(
      allList,
      /^showing 1 of 1 item\(s\):\n\s+w-[0-9a-f]{12} \[chore\] p1 completed/u,
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
    const richFocus = JSON.parse(coreFocus.stdout);
    assert.equal(richFocus.status.work.lifecycle, "completed");
    assert.equal(typeof richFocus.status.work.work_id, "string");
    assert.ok(richFocus.run);
    assert.ok(richFocus.obligation_page);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("two MCP sessions complete ambient work through a fenced handoff", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-work-dogfood-"));
  const sessionA = "work-agent-a-123e4567-e89b-42d3-a456-426614174000";
  const sessionB = "work-agent-b-123e4567-e89b-42d3-a456-426614174001";
  let a;
  let b;
  try {
    buildAndInit(engramHome);
    // Both sessions receive the same fourteen-tool MCP surface with only
    // project and asserted actor/session bindings.
    a = new McpClient(engramHome, sessionA);
    b = new McpClient(engramHome, sessionB);
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
      "acceptance",
      "prerequisite",
      "child",
      "replacement",
    ]) {
      assert.ok(updateProperties[field], `update is missing ${field}`);
    }
    const gateProperties = aToolDefinitions.find(
      ({ name }) => name === "gate",
    ).inputSchema.properties;
    for (const field of ["work_ref", "name", "failed", "evidence_ref"]) {
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
    assert.equal(peerMemories.memories[0].actor_id, sessionA);
    assert.equal(peerMemories.memories[0].actor_context, undefined);
    const fullMemory = receipt(
      await b.call("memories", { query: "mcp-project-note", full: true }),
    );
    assert.equal(fullMemory.body, "MCP project observation\nfull body");
    assert.equal(fullMemory.actor_context, undefined);
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
    const supersededShow = receipt(
      await a.call("show", { work_ref: mcpDependent.short_ref }),
    );
    assert.equal(
      supersededShow.status.work.superseded_by,
      mcpReplacement.short_ref,
    );
    assertTerseShow(supersededShow);

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
    assert.equal("effective_session_id" in added, false);
    const workRef = added.work.short_ref;
    assert.ok(added.reminders.includes("unclaimed: claim it before execution"));
    assert.ok(added.next.includes(`engram work claim ${workRef}`));
    const typicalShow = receipt(await a.call("show", { work_ref: workRef }));
    assertTerseShow(typicalShow);
    assert.ok(
      Buffer.byteLength(JSON.stringify(typicalShow), "utf8") < 1024,
      `${Buffer.byteLength(JSON.stringify(typicalShow), "utf8")} byte MCP show`,
    );
    const createdHistory = typicalShow.history.items.find(({ kind }) => kind === "created");
    assert.match(createdHistory.summary, /without prerequisites/u);
    assert.equal(createdHistory.by, "you");

    const next = receipt(await a.call("next", { limit: 20, verbose: true }));
    assert.equal(next.session.focused_work_id, added.work.work_id);
    assert.ok(next.delivered_through > 0);
    assert.match(next.delivery_token, /^[0-9a-f-]{36}$/u);
    assert.equal(next.session.confirmed_project_cursor, 0);
    // The previous page counts as delivered once the session asks again; no
    // agent-side acknowledgement exists.
    let following = receipt(
      await a.call("next", { limit: 20, verbose: true }),
    );
    assert.equal(following.session.confirmed_project_cursor, next.delivered_through);
    // Session-relative attribution adds a field to the bounded staged page;
    // the initial backlog may require another page. Assert every exact ack
    // before asserting the empty page, rather than assuming one page fits.
    for (let pages = 0; following.changes.length > 0; pages += 1) {
      assert.ok(pages < 20, "the small fixture must drain its bounded backlog");
      const delivered = following.delivered_through;
      following = receipt(await a.call("next", { limit: 20, verbose: true }));
      assert.equal(following.session.confirmed_project_cursor, delivered);
      assert.ok(following.delivered_through >= delivered);
    }
    assert.equal(following.delivered_through, following.session.confirmed_project_cursor);
    assert.deepEqual(following.changes, []);
    const compactNext = receipt(await a.call("next", { limit: 20 }));
    assert.equal(compactNext.focus.ref, workRef);
    assert.equal(compactNext.focus.title, "Dogfood local work");
    assert.equal("lifecycle" in compactNext.focus, false);
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
      receipt(await a.call("show", { work_ref: workRef })).notes.at(-1)
        .summary,
      /^gate cargo-test failed/u,
    );
    assert.equal(
      receipt(await a.call("show", { work_ref: workRef })).notes.at(-1).kind,
      "generic",
    );
    assert.equal(
      receipt(await a.call("show", { work_ref: workRef })).notes.at(-1).by,
      "you",
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
    assert.equal(blockedShow.blockers[0].kind, "manual");
    assert.equal(blockedShow.blockers[0].detail, "Dogfood the agent-visible blocker identity");
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
    assert.equal(held.details.holder_session_id, sessionA);
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
        to: sessionB,
        ttl_seconds: 240,
        summary: "handoff after MCP evidence capture",
      }),
    );
    const recipientFocus = receipt(await b.call("show", { work_ref: workRef }));
    assert.ok(recipientFocus.allowed_next.includes("work_handoff:accept"));
    assert.equal(recipientFocus.next[0], `engram work handoff ${workRef} --accept`);
    const accepted = receipt(await b.call("handoff", { action: "accept" }));
    assert.equal(accepted.operation, "accept");
    const acceptedFocus = receipt(await b.call("show", { work_ref: workRef }));
    assert.ok(
      acceptedFocus.history.items.some(
        ({ kind, summary }) =>
          kind === "handed_off" && summary.includes("from one session to another"),
      ),
      JSON.stringify(acceptedFocus.history),
    );
    // A prior holder is now a non-holder: explicit MCP work_ref records an
    // observation without regaining the recipient's execution authority.
    const observationInput = { work_ref: workRef, text: "peer observation after handoff" };
    const observation = receipt(await a.call("note", observationInput));
    assert.equal(observation.operation, "note");
    assert.equal(observation.non_holder, true);
    assert.equal(observation.receipt.result, observation.evidence.result);
    assert.deepEqual(receipt(await a.call("note", observationInput)), observation);
    const observationShown = receipt(await b.call("show", { work_ref: workRef }));
    assert.equal(observationShown.held_until, acceptedFocus.held_until);
    assert.equal(observationShown.notes.filter(({ summary }) => summary === observationInput.text).length, 1);
    assert.equal(observationShown.notes.at(-1).non_holder, true);
    assertTerseShow(observationShown);
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
    assert.ok(
      completed.history.items.some(
        ({ kind, summary }) => kind === "completed" && summary === '"Dogfood local work"',
      ),
      JSON.stringify(completed.history),
    );
    assertTerseShow(completed);
    assert.equal(completed.reminders.length, 0);

    // Reproduce Phoenix's exact surface: same MCP process/session, explicit
    // work_ref, immediately after done. No claim or reopen is needed.
    const sameSessionLate = receipt(await b.call("note", {
      work_ref: workRef,
      text: "completing session records a late finding",
    }));
    assert.equal(sameSessionLate.operation, "note");
    assert.equal(sameSessionLate.receipt.result, sameSessionLate.evidence.result);
    assert.equal(sameSessionLate.non_holder, undefined);
    assert.equal(receipt(await b.call("done", { summary: "validated by the receiving MCP session" })).seal, seal.seal);
    const lateNote = receipt(
      await a.call("note", {
        work_ref: workRef,
        text: "peer found a late MCP documentation mismatch",
        refs: ["review:mcp-late-note"],
      }),
    );
    assert.equal(lateNote.operation, "note");
    assert.equal(lateNote.receipt.result, lateNote.evidence.result);
    assert.equal(lateNote.non_holder, undefined);
    const lateGate = receipt(
      await a.call("gate", {
        work_ref: workRef,
        name: "cargo-test",
        failed: ["late::mcp-regression"],
        evidence_ref: "review:mcp-late-gate",
      }),
    );
    assert.equal(lateGate.operation, "gate");
    assert.equal(lateGate.gate.passed, false);
    const afterLateFindings = receipt(await a.call("show", { work_ref: workRef }));
    assert.equal(afterLateFindings.status.work.lifecycle, "completed");
    assert.deepEqual(afterLateFindings.next, [`engram work note ${workRef} "…"`]);
    assert.ok(
      afterLateFindings.notes.some(
        ({ summary }) => summary === "peer found a late MCP documentation mismatch",
      ),
      JSON.stringify(afterLateFindings.notes),
    );
    assert.ok(
      afterLateFindings.notes.some(({ summary }) => /^gate cargo-test failed/u.test(summary)),
      JSON.stringify(afterLateFindings.notes),
    );
    const completedMutation = structuredError(
      await a.call("update", {
        work_ref: workRef,
        action: "revise",
        title: "completed work remains frozen",
      }),
      "work_invalid",
    );
    assert.equal(
      completedMutation.details.remedy,
      "use note to record a late finding without reopening the completed item",
    );
    assert.deepEqual(completedMutation.next, [`engram work note ${workRef} "…"`]);
    assert.doesNotMatch(JSON.stringify(completedMutation.next), /reopen/u);

    const openOnly = receipt(await b.call("ls", { search: "dogfood local work" }));
    assert.equal(openOnly.items.length, 0);
    const completedCatalog = receipt(
      await b.call("ls", { search: "dogfood local work", all: true }),
    );
    assert.equal(completedCatalog.items.length, 1);
    assert.equal(completedCatalog.items[0].ref, workRef);
    assert.equal(completedCatalog.items[0].state, "completed");
    assert.equal("lifecycle" in completedCatalog.items[0], false);
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
    assert.equal(compactFocus.notes.length, 1);

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
    assert.equal("child_requirement" in parentShow.children[0], false);

    // The same one-child path exposes the optional requirement, and the open
    // child is retained for audit without blocking its parent's seal.
    const optionalParent = receipt(await a.call("add", { title: "Optional parent" })).work;
    const optionalChild = receipt(
      await a.call("add", {
        title: "Optional follow-up",
        under: optionalParent.short_ref,
        optional: true,
      }),
    );
    assert.equal(optionalChild.work.child_requirement, "optional");
    const optionalParentShow = receipt(
      await a.call("show", { work_ref: optionalParent.short_ref }),
    );
    assert.equal(optionalParentShow.children.length, 1);
    assert.equal(optionalParentShow.children[0].child_requirement, "optional");
    receipt(await a.call("claim", { work_ref: optionalParent.short_ref }));
    const optionalSeal = receipt(
      await a.call("done", {
        work_ref: optionalParent.short_ref,
        summary: "parent is complete without the optional follow-up",
      }),
    );
    assert.match(optionalSeal.seal, HASH);
    assert.equal(
      receipt(await a.call("show", { work_ref: optionalParent.short_ref })).status.work.lifecycle,
      "completed",
    );
    structuredError(
      await a.call("add", { title: "Invalid optional root", optional: true }),
      "work_invalid",
    );

    // A disposed required child is an explicit, agent-runnable waiver flow:
    // done names the lifecycle and command, update records the waiver, and the
    // unchanged completion intent then seals the parent.
    const waiverParent = receipt(
      await a.call("add", { title: "Required-child waiver parent" }),
    ).work;
    const waiverChild = receipt(
      await a.call("add", {
        title: "Disposed required child",
        under: waiverParent.short_ref,
      }),
    ).work;
    receipt(
      await a.call("update", {
        work_ref: waiverChild.short_ref,
        action: "cancel",
        reason: "child outcome is no longer needed",
      }),
    );
    receipt(await a.call("claim", { work_ref: waiverParent.short_ref }));
    const refusedWaiverParent = receipt(
      await a.call("done", {
        work_ref: waiverParent.short_ref,
        summary: "parent implementation complete",
      }),
    );
    assert.ok(
      refusedWaiverParent.reminders.some((line) =>
        line.includes("is cancelled without a completion seal or waiver"),
      ),
      JSON.stringify(refusedWaiverParent),
    );
    assert.deepEqual(refusedWaiverParent.next, [
      `engram work update ${waiverParent.short_ref} --waive ${waiverChild.short_ref} --reason "account for disposed required child"`,
    ]);
    const waiver = receipt(
      await a.call("update", {
        work_ref: waiverParent.short_ref,
        action: "waive",
        child: waiverChild.short_ref,
        reason: "the cancelled child is explicitly accounted for",
      }),
    );
    assert.equal(waiver.operation, "waive_required_child");
    assert.equal(waiver.receipt.work_id, waiverParent.work_id);
    assert.equal(typeof waiver.receipt.result.work_revision, "number");
    const waiverParentSeal = receipt(
      await a.call("done", {
        work_ref: waiverParent.short_ref,
        summary: "parent implementation complete",
      }),
    );
    assert.match(waiverParentSeal.seal, HASH);

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

test("MCP actor context stays attribution-only across words and handoff", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-mcp-actor-context-"));
  const actorContext = "model=opus-4.1;reasoning=high";
  let author;
  let assignee;
  let recipient;
  try {
    buildAndInit(engramHome);
    author = new McpClient(
      engramHome,
      "actor-context-source",
      actorContext,
      "greg/codex",
    );
    recipient = new McpClient(
      engramHome,
      "actor-context-recipient",
      undefined,
      "peer",
    );
    assignee = new McpClient(
      engramHome,
      "actor-context-assignee",
      undefined,
      "planning-owner",
    );
    await Promise.all([
      author.initialize(),
      assignee.initialize(),
      recipient.initialize(),
    ]);

    const added = receipt(
      await author.call("add", {
        title: "Attribute MCP execution context",
        assignee: "planning-owner",
      }),
    ).work;
    const mine = receipt(await author.call("ls", { mine: true }));
    assert.ok(!mine.items.some(({ ref }) => ref === added.short_ref));
    const assigned = receipt(await assignee.call("ls", { mine: true }));
    assert.ok(assigned.items.some(({ ref }) => ref === added.short_ref));
    receipt(await author.call("claim", { work_ref: added.short_ref }));
    receipt(
      await author.call("note", {
        work_ref: added.short_ref,
        text: "MCP attribution context retained",
      }),
    );
    const shown = receipt(
      await author.call("show", { work_ref: added.short_ref }),
    );
    assert.equal(shown.notes.at(-1).by, `you (${actorContext})`);
    assert.ok(
      shown.history.items.some(({ by }) => by === `you (${actorContext})`),
    );

    receipt(
      await author.call("remember", {
        text: "MCP context memory",
        key: "mcp-actor-context",
      }),
    );
    const memories = receipt(await recipient.call("memories", {}));
    assert.equal(memories.memories[0].actor_id, "greg/codex");
    assert.equal(memories.memories[0].actor_context, actorContext);

    receipt(
      await author.call("handoff", {
        action: "offer",
        work_ref: added.short_ref,
        to: "actor-context-recipient",
      }),
    );
    assert.equal(
      receipt(
        await recipient.call("handoff", {
          action: "accept",
          work_ref: added.short_ref,
        }),
      ).operation,
      "accept",
    );
  } finally {
    await Promise.all([author?.close(), assignee?.close(), recipient?.close()]);
    rmSync(engramHome, { recursive: true, force: true });
  }
});
