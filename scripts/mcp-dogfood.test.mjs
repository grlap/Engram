#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const binary = join(root, "target", "debug", "engram");

class McpClient {
  constructor(engramHome, sessionId) {
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = "";
    this.buffer = "";
    this.child = spawn(
      binary,
      [
        "--home",
        engramHome,
        "mcp",
        "--actor-id",
        sessionId,
        "--session-id",
        sessionId,
        "--source-skill",
        "engram-dogfood",
      ],
      { cwd: root, stdio: ["pipe", "pipe", "pipe"] },
    );
    this.child.stderr.on("data", (chunk) => {
      this.stderr += chunk.toString("utf8");
    });
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
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

  async call(name, arguments_ = {}) {
    const started = performance.now();
    const result = await this.request("tools/call", {
      name,
      arguments: arguments_,
    });
    assert.ok(performance.now() - started < 1000, `${name} exceeded one second`);
    return result;
  }

  close() {
    this.child.stdin.end();
  }
}

function structured(result) {
  assert.equal(result.isError ?? false, false, JSON.stringify(result));
  assert.ok(result.structuredContent, JSON.stringify(result));
  return result.structuredContent;
}

function structuredError(result, code) {
  assert.equal(result.isError, true, JSON.stringify(result));
  assert.equal(result.structuredContent.error.code, code);
  return result.structuredContent.error;
}

async function wait(milliseconds) {
  await new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

test("two MCP sessions voluntarily usable memory loop", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-mcp-dogfood-"));
  let a;
  let b;
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

    a = new McpClient(engramHome, "eval-a");
    b = new McpClient(engramHome, "eval-b");
    await Promise.all([a.initialize(), b.initialize()]);

    const listed = await a.request("tools/list", {});
    const toolNames = new Set(listed.tools.map(({ name }) => name));
    for (const name of [
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
    ]) {
      assert.ok(toolNames.has(name), `missing MCP tool ${name}`);
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
    b.close();
    await wait(50);
    b = new McpClient(engramHome, "eval-b");
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
    a?.close();
    b?.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});
