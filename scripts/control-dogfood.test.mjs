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

test("host control survives restart and gates turn dispatch", async () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-control-dogfood-"));
  let client;
  let peer;
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

    const doctor = spawnSync(binary, ["--home", engramHome, "doctor"], {
      cwd: root,
      encoding: "utf8",
    });
    assert.equal(doctor.status, 0, doctor.stderr);
  } finally {
    await peer?.close();
    await client?.close();
    rmSync(engramHome, { recursive: true, force: true });
  }
});
