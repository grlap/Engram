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
  constructor(engramHome, sessionId, workAuthorityGrant) {
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
    this.child = spawn(
      binary,
      args,
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
      "work_next",
      "work_focus",
      "work_propose",
      "work_update",
      "work_complete",
      "work_handoff",
    ]) {
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
    for (const name of [
      "session_bind",
      "session_status",
      "lease_acquire",
      "lease_release",
      "turn_evaluate",
      "turn_begin",
      "turn_checkpoint",
    ]) {
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

test("CLI translates the same ambient lifecycle service", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-work-cli-"));
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
    const actor = "cli-work-agent";
    const grant = installWorkGrant(engramHome, actor);
    const proposed = cliWork(engramHome, actor, grant, "propose", {
      kind: "root",
      title: "Dogfood work CLI",
      outcome: "The shell completes an ambient local lifecycle",
      acceptance: ["CLI completion is sealed"],
      work_kind: "chore",
      idempotency_key: "cli-root",
    });
    assert.equal(proposed.kind, "root");
    const next = cliWork(engramHome, actor, grant, "next");
    assert.equal(next.session.focused_work_id, proposed.work.work_id);
    cliWork(engramHome, actor, grant, "update", {
      kind: "claim",
      ttl_seconds: 300,
      idempotency_key: "cli-claim",
    });
    const evidence = cliWork(engramHome, actor, grant, "update", {
      kind: "evidence",
      summary: "CLI lifecycle assertions passed",
      refs: ["test:cli-work-dogfood"],
      idempotency_key: "cli-evidence",
    }).receipt.result;
    cliWork(engramHome, actor, grant, "update", {
      kind: "checkpoint",
      summary: "CLI evidence and acceptance validated",
      evidence: [evidence],
      idempotency_key: "cli-checkpoint",
    });
    const seal = cliWork(engramHome, actor, grant, "complete", {
      acceptance: [
        { satisfied: true, note: "validated by the CLI dogfood session" },
      ],
      idempotency_key: "cli-complete",
    });
    assert.equal(seal.work_id, proposed.work.work_id);
    const focused = spawnSync(
      binary,
      [
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        actor,
        "focus",
        proposed.work.short_ref,
      ],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(focused.status, 0, focused.stderr);
    assert.equal(JSON.parse(focused.stdout).status.work.lifecycle, "completed");
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("two MCP sessions complete ambient work through a fenced handoff", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-work-dogfood-"));
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
    const grantA = installWorkGrant(engramHome, "work-agent-a");
    const grantB = installWorkGrant(engramHome, "work-agent-b");

    a = new McpClient(engramHome, "work-agent-a", grantA);
    b = new McpClient(engramHome, "work-agent-b", grantB);
    await Promise.all([a.initialize(), b.initialize()]);

    const proposed = structured(
      await a.call("work_propose", {
        input: {
          kind: "root",
          title: "Dogfood local work",
          outcome: "Two MCP sessions finish through an ambient handoff",
          acceptance: ["recipient seals the validated result"],
          work_kind: "feature",
          priority: 1,
          labels: ["dogfood"],
          idempotency_key: "work-root",
        },
      }),
    );
    assert.equal(proposed.kind, "root");
    const workRef = proposed.work.short_ref;

    const next = structured(await a.call("work_next", { limit: 20 }));
    assert.equal(next.session.focused_work_id, proposed.work.work_id);
    assert.ok(next.delivered_through > 0);
    assert.match(next.delivery_token, /^[0-9a-f-]{36}$/u);
    assert.equal(next.session.confirmed_project_cursor, 0);
    const replayed = structured(await a.call("work_next", { limit: 20 }));
    assert.equal(replayed.delivered_through, next.delivered_through);
    assert.equal(replayed.delivery_token, next.delivery_token);
    const pendingRootInput = {
      kind: "root",
      title: "Pending-delivery recovery root",
      outcome: "The refusal teaches a one-call recovery",
      acceptance: ["the same idempotency key completes after acknowledgement"],
      idempotency_key: "pending-delivery-root",
    };
    const pendingDelivery = structuredError(
      await a.call("work_propose", { input: pendingRootInput }),
      "work_delivery_pending",
    );
    assert.equal("delivered_through" in pendingDelivery.details, false);
    assert.equal("delivery_token" in pendingDelivery.details, false);
    assert.equal(JSON.stringify(pendingDelivery.details).includes(next.delivery_token), false);
    assert.match(pendingDelivery.details.remedy, /replay the pending page/u);
    assert.match(pendingDelivery.details.remedy, /sections excluding changes/u);
    assert.match(pendingDelivery.details.remedy, /same idempotency key/u);
    const recoveryReplay = structured(await a.call("work_next", { limit: 20 }));
    assert.equal(recoveryReplay.delivered_through, next.delivered_through);
    assert.equal(recoveryReplay.delivery_token, next.delivery_token);
    const wrongCursor = structuredError(
      await a.call("work_next", {
        limit: 20,
        acknowledge_through: 999,
        acknowledge_token: "wrong-token",
        sections: ["focus"],
      }),
      "work_invalid",
    );
    assert.equal(wrongCursor.details.reason.includes(next.delivery_token), false);
    assert.equal(/\d/u.test(wrongCursor.details.reason), false);
    const wrongToken = structuredError(
      await a.call("work_next", {
        limit: 20,
        acknowledge_through: recoveryReplay.delivered_through,
        acknowledge_token: "wrong-token",
        sections: ["focus"],
      }),
      "work_invalid",
    );
    assert.equal(wrongToken.details.reason.includes(next.delivery_token), false);
    const recoveryReplayAfterRefusal = structured(
      await a.call("work_next", { limit: 20 }),
    );
    assert.equal(
      recoveryReplayAfterRefusal.delivered_through,
      recoveryReplay.delivered_through,
    );
    assert.equal(
      recoveryReplayAfterRefusal.delivery_token,
      recoveryReplay.delivery_token,
    );
    const acknowledged = structured(
      await a.call("work_next", {
        limit: 20,
        acknowledge_through: recoveryReplay.delivered_through,
        acknowledge_token: recoveryReplay.delivery_token,
        sections: ["focus"],
      }),
    );
    assert.equal(
      acknowledged.session.confirmed_project_cursor,
      next.delivered_through,
    );
    assert.equal("delivered_through" in acknowledged, false);
    structured(await a.call("work_propose", { input: pendingRootInput }));
    structured(await a.call("work_focus", { work_ref: workRef }));
    assert.ok(next.focus.allowed_next.includes("work_update:claim"));

    structured(
      await a.call("work_update", {
        input: {
          kind: "claim",
          ttl_seconds: 300,
          idempotency_key: "work-claim-a",
        },
      }),
    );
    const replayedClaim = structured(
      await a.call("work_update", {
        input: {
          kind: "claim",
          ttl_seconds: 300,
          idempotency_key: "work-claim-a",
        },
      }),
    );
    assert.equal(replayedClaim.operation, "claim");
    assert.equal("focus" in replayedClaim, false);
    assert.ok(Array.isArray(replayedClaim.obligations));
    assert.ok(Array.isArray(replayedClaim.allowed_next));
    structuredError(
      await a.call("work_update", {
        input: {
          kind: "claim",
          ttl_seconds: 301,
          idempotency_key: "work-claim-a",
        },
      }),
      "work_idempotency_conflict",
    );
    structured(
      await a.call("work_update", {
        input: {
          kind: "block",
          blocker_kind: "external_input",
          detail: "Dogfood the agent-visible blocker identity",
          idempotency_key: "work-block-a",
        },
      }),
    );
    const blockedFocus = structured(
      await a.call("work_focus", { work_ref: workRef }),
    );
    assert.equal(blockedFocus.blockers.length, 1);
    assert.ok(blockedFocus.blockers[0].blocker_id.length > 0);
    structured(
      await a.call("work_update", {
        input: {
          kind: "unblock",
          idempotency_key: "work-unblock-a",
        },
      }),
    );
    assert.equal(
      structured(await a.call("work_focus", { work_ref: workRef })).blockers
        .length,
      0,
    );
    structured(await b.call("work_focus", { work_ref: workRef }));
    structured(
      await a.call("task_start", {
        external_ref: "dummy:WORK-MIXED-7",
        title: "Dogfood explicit mixed-context routing",
      }),
    );
    structuredError(
      await a.call("memory_note", {
        prose: "Decision: ambiguous mixed task and work capture must be refused.",
        idempotency_key: "work-ambiguous-memory",
      }),
      "invalid_argument",
    );
    const sharedWorkMemory = structured(
      await a.call("memory_note", {
        prose: "Decision: the focused work uses one local identity for task tracking and shared execution memory.",
        idempotency_key: "work-shared-memory",
        target: "work",
      }),
    );
    assert.equal(sharedWorkMemory.scope.kind, "work");
    assert.equal(sharedWorkMemory.scope.work, proposed.work.work_id);
    assert.ok(sharedWorkMemory.work_positions.length >= 2);
    const privateWorkMemory = structured(
      await a.call("memory_note", {
        prose: "scratch: private focused implementation hypothesis",
        private: true,
        idempotency_key: "work-private-memory",
        target: "work",
      }),
    );
    assert.equal(privateWorkMemory.scope.kind, "agent");
    assert.equal(privateWorkMemory.scope.work, proposed.work.work_id);
    assert.deepEqual(privateWorkMemory.work_positions, []);
    const shownPrivateWorkMemory = structured(
      await a.call("memory_show", { hash: privateWorkMemory.version }),
    );
    assert.equal(shownPrivateWorkMemory.version.scope.work, proposed.work.work_id);
    const authorMemoryFocus = structured(
      await a.call("work_focus", { work_ref: workRef }),
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
    const peerMemoryFocus = structured(
      await b.call("work_focus", { work_ref: workRef }),
    );
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
    const peerWorkChanges = structured(
      await b.call("work_next", { limit: 100 }),
    ).changes;
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
    assert.equal(shownWorkMemory.version.scope.work, proposed.work.work_id);
    assert.equal(shownWorkMemory.version.authority, "firm");
    structuredError(
      await b.call("memory_show", { hash: privateWorkMemory.version }),
      "memory_access_denied",
    );
    const held = structuredError(
      await b.call("work_update", {
        input: {
          kind: "claim",
          ttl_seconds: 300,
          idempotency_key: "work-claim-b-held",
        },
      }),
      "work_claim_held",
    );
    assert.equal(held.details.holder_session_id, "work-agent-a");
    const evidence = structured(
      await a.call("work_update", {
        input: {
          kind: "evidence",
          summary: "MCP ambient lifecycle assertions passed",
          refs: ["test:mcp-work-dogfood"],
          idempotency_key: "work-evidence-a",
        },
      }),
    ).receipt.result;
    assert.match(evidence, /^[0-9a-f]{64}$/u);

    structured(
      await a.call("work_handoff", {
        input: {
          kind: "offer",
          to: "work-agent-b",
          ttl_seconds: 240,
          checkpoint_summary: "handoff after MCP evidence capture",
          idempotency_key: "work-offer-b",
        },
      }),
    );
    const recipientFocus = structured(
      await b.call("work_focus", { work_ref: workRef }),
    );
    assert.ok(recipientFocus.allowed_next.includes("work_handoff:accept"));
    structured(
      await b.call("work_handoff", {
        input: { kind: "accept", idempotency_key: "work-accept-b" },
      }),
    );
    structuredError(
      await a.call("work_update", {
        input: {
          kind: "evidence",
          summary: "must be rejected after handoff",
          idempotency_key: "work-stale-a",
        },
      }),
      "work_claim_mismatch",
    );
    structured(
      await b.call("work_update", {
        input: {
          kind: "checkpoint",
          summary: "recipient validated evidence and completion criterion",
          evidence: [evidence],
          idempotency_key: "work-checkpoint-b",
        },
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
    const seal = structured(
      await b.call("work_complete", {
        input: {
          acceptance: [
            {
              satisfied: true,
              note: "validated by the receiving MCP session",
            },
          ],
          idempotency_key: "work-complete-b",
        },
      }),
    );
    assert.equal(seal.work_id, proposed.work.work_id);
    assert.match(seal.seal, /^[0-9a-f]{64}$/u);
    const completed = structured(
      await b.call("work_focus", { work_ref: workRef }),
    );
    assert.equal(completed.status.work.lifecycle, "completed");
    assert.ok(completed.history.items.length > 0);
    const completedCatalog = structured(
      await b.call("work_next", {
        limit: 20,
        search: "dogfood local work",
        lifecycles: ["completed"],
      }),
    );
    assert.equal(completedCatalog.catalog.items.length, 1);
    assert.equal(completedCatalog.catalog.items[0].work.work_id, proposed.work.work_id);
    assert.equal(JSON.stringify(completedCatalog).includes(grantB), false);
    const catalogOnly = structured(
      await b.call("work_next", {
        limit: 20,
        acknowledge_through: completedCatalog.delivered_through,
        acknowledge_token: completedCatalog.delivery_token,
        sections: ["catalog"],
        search: "dogfood local work",
      }),
    );
    assert.equal("changes" in catalogOnly, false);
    assert.equal("delivered_through" in catalogOnly, false);
    assert.equal("focus" in catalogOnly, false);
    assert.equal(catalogOnly.catalog.items.length, 1);

    const replacement = structured(
      await b.call("work_propose", {
        input: {
          kind: "root",
          title: "MCP replacement plan",
          outcome: "A replacement remains visible in the local catalog",
          acceptance: ["replacement is evaluated"],
          idempotency_key: "work-replacement",
        },
      }),
    ).work;
    const obsolete = structured(
      await b.call("work_propose", {
        input: {
          kind: "root",
          title: "MCP obsolete plan",
          outcome: "The obsolete plan is not falsely completed",
          acceptance: ["obsolete plan is disposed honestly"],
          idempotency_key: "work-obsolete",
        },
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
    const supersededCatalog = structured(
      await b.call("work_next", {
        limit: 20,
        search: "obsolete plan",
        lifecycles: ["superseded"],
      }),
    );
    assert.equal(supersededCatalog.catalog.items.length, 1);
    assert.equal(supersededCatalog.catalog.items[0].work.work_id, obsolete.work_id);
    structured(
      await b.call("work_next", {
        acknowledge_through: supersededCatalog.delivered_through,
        acknowledge_token: supersededCatalog.delivery_token,
        sections: ["catalog"],
        search: "obsolete plan",
        lifecycles: ["superseded"],
      }),
    );

    const disposable = structured(
      await b.call("work_propose", {
        input: {
          kind: "root",
          title: "MCP disposable plan",
          outcome: "Cancellation remains distinct from completion",
          acceptance: ["cancellation is audited"],
          idempotency_key: "work-disposable",
        },
      }),
    ).work;
    const cancelled = structured(
      await b.call("work_update", {
        input: {
          kind: "cancel",
          reason: "the experiment is no longer needed",
          idempotency_key: "work-cancel-disposable",
        },
      }),
    );
    assert.equal(cancelled.receipt.result.lifecycle, "cancelled");
    assert.equal(cancelled.receipt.work_id, disposable.work_id);

    const compact = structured(
      await b.call("work_propose", {
        input: {
          kind: "root",
          title: "MCP compact completion",
          outcome: "A normal local task closes with one evidence-backed completion call",
          acceptance: ["compact completion is sealed"],
          idempotency_key: "work-compact-root",
        },
      }),
    ).work;
    structured(
      await b.call("work_update", {
        input: {
          kind: "claim",
          idempotency_key: "work-compact-claim",
        },
      }),
    );
    const compactSeal = structured(
      await b.call("work_complete", {
        input: {
          capture: {
            summary: "validated compact completion through the MCP lifecycle",
            refs: ["test:mcp-work-dogfood"],
          },
          acceptance: [
            {
              satisfied: true,
              note: "the evidence and checkpoint were captured with this call",
            },
          ],
          idempotency_key: "work-compact-complete",
        },
      }),
    );
    assert.equal(compactSeal.work_id, compact.work_id);
    const compactFocus = structured(
      await b.call("work_focus", { work_ref: compact.short_ref }),
    );
    assert.equal(compactFocus.status.work.lifecycle, "completed");
    assert.equal(compactFocus.evidence.length, 1);
  } finally {
    a?.close();
    b?.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});
