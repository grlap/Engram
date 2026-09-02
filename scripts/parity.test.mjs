#!/usr/bin/env node

// Measured acceptance for the thirteen-word agent surface: on a fresh store, an
// agent goes from nothing to a sealed item with add -> claim -> done in at
// most three commands and at most three agent-supplied fields, typing no
// JSON, and never seeing a hash, fence, or idempotency key in text output.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");

const MAX_COMMANDS = 3;
const MAX_FIELDS = 3;
const HASH = /\b[0-9a-f]{64}\b/u;

function run(args, options = {}) {
  const executed = spawnSync(binary, args, {
    cwd: root,
    encoding: "utf8",
    ...options,
  });
  return executed;
}

function hostSetup(engramHome) {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(built.status, 0, built.stderr);
  const initialized = run(["--home", engramHome, "init"]);
  assert.equal(initialized.status, 0, initialized.stderr);
}

test("add -> claim -> done takes three commands and at most three fields", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-"));
  const actor = "parity-agent";
  try {
    hostSetup(engramHome);
    // Host context is fixed by the wrapper, not typed by the agent.
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
    ];
    let commands = 0;
    let fields = 0;
    let transcript = "";
    const agent = (word, ...agentArgs) => {
      commands += 1;
      fields += agentArgs.length;
      for (const value of agentArgs) {
        assert.doesNotMatch(value, /^[\[{]/u, `agent typed JSON: ${value}`);
        assert.doesNotMatch(value, HASH, `agent typed a hash: ${value}`);
      }
      const executed = run([...hostContext, word, ...agentArgs]);
      transcript += `${executed.stdout}\n${executed.stderr}\n`;
      assert.equal(executed.status, 0, `${word}: ${executed.stderr}`);
      return executed.stdout;
    };

    const added = agent("add", "Ship the parity test");
    const ref = added.match(/\bw-[0-9a-f]{12}\b/u)?.[0];
    assert.ok(ref, added);
    assert.match(added, /^added w-[0-9a-f]{12} "Ship the parity test"/u);
    assert.match(added, /\nnext:\n(?:.*\n)*\s+engram work claim w-/u);

    const claimed = agent("claim", ref);
    assert.match(claimed, /^claimed w-[0-9a-f]{12} "Ship the parity test" \(held by you until /u);
    assert.match(claimed, /reminders:\n\s+- you hold this item but have not noted progress yet/u);
    assert.match(claimed, /\nnext:\n(?:.*\n)*\s+engram work done w-/u);

    const done = agent("done", "Parity test shipped");
    assert.match(done, /^done w-[0-9a-f]{12} "Ship the parity test"/u);

    assert.ok(commands <= MAX_COMMANDS, `${commands} commands`);
    assert.ok(fields <= MAX_FIELDS, `${fields} agent-supplied fields`);
    assert.doesNotMatch(transcript, HASH, "text output leaked a hash");
    assert.doesNotMatch(transcript, /fence/iu, "text output leaked a fence");
    assert.doesNotMatch(transcript, /idempotency/iu, "text output leaked a key");
    assert.doesNotMatch(transcript, /"[a-z_]+":/u, "text output contained JSON");

    // Verification outside the count: the structured receipt still carries
    // everything the host reads, and the item is sealed.
    const shown = run([...hostContext, "show", ref, "--json"]);
    assert.equal(shown.status, 0, shown.stderr);
    const view = JSON.parse(shown.stdout);
    assert.equal(view.status.work.lifecycle, "completed");
    assert.ok(Array.isArray(view.reminders));
    assert.ok(Array.isArray(view.next));
    assert.ok(Array.isArray(view.allowed_next));
    assert.ok(view.obligation_page);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("project memory words create list read and permanently retire a safe key", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-memory-"));
  const actor = "memory-parity-agent";
  try {
    hostSetup(engramHome);
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
    ];
    const remembered = run([
      ...hostContext,
      "remember",
      "CLI project observation\nfull body",
      "--key",
      "cli-project-note",
      "--json",
    ]);
    assert.equal(remembered.status, 0, remembered.stderr);
    assert.equal(JSON.parse(remembered.stdout).key, "cli-project-note");

    const listed = run([...hostContext, "memories", "--json"]);
    assert.equal(listed.status, 0, listed.stderr);
    const listedValue = JSON.parse(listed.stdout);
    assert.equal(listedValue.memories[0].key, "cli-project-note");
    assert.equal(listedValue.memories[0].body, undefined);

    const full = run([
      ...hostContext,
      "memories",
      "cli-project-note",
      "--full",
      "--json",
    ]);
    assert.equal(full.status, 0, full.stderr);
    assert.equal(
      JSON.parse(full.stdout).body,
      "CLI project observation\nfull body",
    );

    const forgotten = run([
      ...hostContext,
      "forget",
      "cli-project-note",
      "--json",
    ]);
    assert.equal(forgotten.status, 0, forgotten.stderr);
    assert.equal(JSON.parse(forgotten.stdout).duplicate, false);
    const retired = run([
      ...hostContext,
      "memories",
      "cli-project-note",
      "--full",
      "--json",
    ]);
    assert.notEqual(retired.status, 0);
    assert.equal(JSON.parse(retired.stderr).error.code, "memory_retired");
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("list words stay compact while verbose and update metadata remain explicit", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-compact-"));
  const actor = "compact-agent";
  try {
    hostSetup(engramHome);
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
    ];
    const refs = [];
    for (let index = 0; index < 34; index += 1) {
      const added = run([
        ...hostContext,
        "add",
        `Budget item ${String(index).padStart(2, "0")}`,
        "--kind",
        "task",
        "--label",
        "initial",
        "--json",
      ]);
      assert.equal(added.status, 0, added.stderr);
      refs.push(JSON.parse(added.stdout).work.short_ref);
    }
    const child = run([
      ...hostContext,
      "add",
      "Measured child",
      "--under",
      refs[0],
      "--kind",
      "task",
      "--label",
      "initial",
      "--json",
    ]);
    assert.equal(child.status, 0, child.stderr);
    refs.push(JSON.parse(child.stdout).work.short_ref);

    assert.equal(run([...hostContext, "claim", refs[0]]).status, 0);
    const updated = run([
      ...hostContext,
      "update",
      refs[0],
      "--kind",
      "bug",
      "--label",
      "phoenix",
      "--label",
      "triaged",
      "--unlabel",
      "initial",
    ]);
    assert.equal(updated.status, 0, updated.stderr);
    assert.match(updated.stdout, /\(kind, labels\)/u);
    const shown = run([...hostContext, "show", refs[0], "--json"]);
    assert.equal(shown.status, 0, shown.stderr);
    assert.equal(JSON.parse(shown.stdout).status.work.kind, "bug");
    assert.deepEqual(JSON.parse(shown.stdout).status.work.labels, [
      "phoenix",
      "triaged",
    ]);
    const blocked = run([
      ...hostContext,
      "update",
      refs[1],
      "--blocked",
      "review budget blocker",
    ]);
    assert.equal(blocked.status, 0, blocked.stderr);

    const listed = run([...hostContext, "ls", "--limit", "100", "--json"]);
    assert.equal(listed.status, 0, listed.stderr);
    const compactList = JSON.parse(listed.stdout);
    assert.equal(compactList.items.length, 35);
    const itemBytes = compactList.items.map((row) =>
      Buffer.byteLength(JSON.stringify(row), "utf8"),
    );
    const maxItemBytes = Math.max(...itemBytes);
    const maxItem = compactList.items[itemBytes.indexOf(maxItemBytes)];
    assert.ok(
      maxItemBytes <= 200,
      `${maxItemBytes} max bytes/item: ${JSON.stringify(maxItem)}`,
    );
    for (const row of compactList.items) {
      assert.equal(typeof row.ref, "string");
      assert.equal(typeof row.title, "string");
      assert.equal(typeof row.lifecycle, "string");
      assert.equal(typeof row.state, "string");
      assert.equal(typeof row.blocked, "boolean");
      assert.equal(row.blocked, row.state === "blocked");
      for (const forbidden of [
        "acceptance",
        "active_run_id",
        "revision",
        "root_id",
        "updated_at",
        "work_id",
      ]) {
        assert.equal(forbidden in row, false, `${forbidden} leaked into compact row`);
      }
    }
    const heldRow = compactList.items.find(({ ref }) => ref === refs[0]);
    assert.equal(heldRow.holder, actor);
    assert.equal(typeof heldRow.held_until, "string");
    const childRow = compactList.items.find(({ ref }) => ref === refs.at(-1));
    assert.equal(childRow.parent_ref, refs[0]);
    const listedText = run([...hostContext, "ls", "--limit", "100"]);
    assert.equal(listedText.status, 0, listedText.stderr);
    assert.ok(listedText.stdout.includes(`${refs[0]} [bug]`));
    assert.ok(listedText.stdout.includes(`held by ${actor} until`));
    const blockedLine = listedText.stdout
      .split(/\r?\n/u)
      .find((line) => line.includes(refs[1]));
    assert.ok(blockedLine?.includes("[task]"));
    assert.ok(blockedLine?.includes("open/blocked"));
    assert.ok(blockedLine?.endsWith(" blocked"));
    assert.ok(listedText.stdout.includes(`${refs.at(-1)} [task]`));
    assert.ok(listedText.stdout.includes(`← ${refs[0]}`));

    const verbose = run([
      ...hostContext,
      "ls",
      "--limit",
      "1",
      "--verbose",
      "--json",
    ]);
    assert.equal(verbose.status, 0, verbose.stderr);
    assert.ok(Array.isArray(JSON.parse(verbose.stdout).items[0].work.acceptance));

    const next = run([...hostContext, "next", "--limit", "20", "--json"]);
    assert.equal(next.status, 0, next.stderr);
    assert.ok(
      Buffer.byteLength(next.stdout, "utf8") <= 4 * 1024,
      `${Buffer.byteLength(next.stdout, "utf8")} byte next receipt`,
    );
    const compactNext = JSON.parse(next.stdout);
    assert.equal("session" in compactNext, false);
    assert.equal("delivery_token" in compactNext, false);
    assert.ok(Array.isArray(compactNext.changes));
    const nextText = run([...hostContext, "next", "--limit", "20"]);
    assert.equal(nextText.status, 0, nextText.stderr);
    assert.ok(
      Buffer.byteLength(nextText.stdout, "utf8") <= 4 * 1024,
      `${Buffer.byteLength(nextText.stdout, "utf8")} byte text next receipt`,
    );
    assert.ok(nextText.stdout.includes(`${refs[0]} [bug]`));
    assert.ok(nextText.stdout.includes(`held by ${actor} until`));

    const verboseNext = run([
      ...hostContext,
      "next",
      "--limit",
      "1",
      "--verbose",
      "--json",
    ]);
    assert.equal(verboseNext.status, 0, verboseNext.stderr);
    assert.ok(JSON.parse(verboseNext.stdout).session);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("done says what is owed and exits 2 when the item cannot seal yet", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-owed-"));
  const actor = "parity-agent";
  try {
    hostSetup(engramHome);
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
    ];
    const added = run([...hostContext, "add", "Needs a note first"]);
    assert.equal(added.status, 0, added.stderr);
    const ref = added.stdout.match(/\bw-[0-9a-f]{12}\b/u)?.[0];
    assert.ok(ref, added.stdout);
    // Nothing is held yet: the answer names the command that resolves it.
    const unheld = run([...hostContext, "note", "too early"]);
    assert.notEqual(unheld.status, 0);
    assert.match(unheld.stderr, /claim it before/u);
    assert.match(unheld.stderr, new RegExp(`engram work claim ${ref}`, "u"));
    assert.doesNotMatch(unheld.stderr, HASH);
    assert.equal(run([...hostContext, "claim", ref]).status, 0);
    // Nothing noted and no summary: one sentence plus the resolving command.
    const bare = run([...hostContext, "done"]);
    assert.notEqual(bare.status, 0);
    assert.match(bare.stderr, /nothing has been noted on this item yet/u);
    assert.match(bare.stderr, new RegExp(`engram work done ${ref} "…"`, "u"));
    assert.doesNotMatch(bare.stdout + bare.stderr, HASH);
    const noted = run([...hostContext, "note", "found the missing piece", "--ref", "src/lib.rs"]);
    assert.equal(noted.status, 0, noted.stderr);
    assert.match(
      noted.stdout,
      /^noted on w-[0-9a-f]{12} "Needs a note first": found the missing piece \(held by you until (?:\d{4}-\d{2}-\d{2} )?\d{2}:\d{2} UTC\)/u,
    );
    assert.doesNotMatch(noted.stdout, HASH);
    const done = run([...hostContext, "done"]);
    assert.equal(done.status, 0, done.stderr);
    assert.match(done.stdout, /^done w-/u);
    // A typed refusal, when the host has recorded an open obligation, exits 2;
    // the code path is shared with the MCP `done` tool and covered there.
    const again = run([...hostContext, "done"]);
    assert.equal(again.status, 0, again.stderr);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("cut A gate, prerequisite, and supersession words reach the typed core", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-cut-a-"));
  const actor = "cut-a-agent";
  try {
    hostSetup(engramHome);
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
    ];
    const add = (title) => {
      const added = run([...hostContext, "add", title, "--json"]);
      assert.equal(added.status, 0, added.stderr);
      return JSON.parse(added.stdout).work.short_ref;
    };

    const dependent = add("Cut A dependent");
    const prerequisite = add("Cut A prerequisite");
    const replacement = add("Cut A replacement");
    const gated = add("Cut A gated item");

    const after = run([
      ...hostContext,
      "update",
      dependent,
      "--after",
      prerequisite,
      "--json",
    ]);
    assert.equal(after.status, 0, after.stderr);
    assert.equal(JSON.parse(after.stdout).operation, "add_prerequisite");
    const blocked = run([...hostContext, "show", dependent, "--json"]);
    assert.equal(blocked.status, 0, blocked.stderr);
    assert.equal(
      JSON.parse(blocked.stdout).next.includes(
        `engram work update ${dependent} --drop-after ${prerequisite}`,
      ),
      false,
    );
    const blockedList = run([...hostContext, "ls", "--blocked", "--json"]);
    assert.equal(blockedList.status, 0, blockedList.stderr);
    assert.equal(
      JSON.parse(blockedList.stdout).next.includes(
        `engram work update ${dependent} --drop-after ${prerequisite}`,
      ),
      false,
    );

    const cancelledPrerequisite = run([
      ...hostContext,
      "update",
      prerequisite,
      "--cancel",
      "no longer needed",
    ]);
    assert.equal(cancelledPrerequisite.status, 0, cancelledPrerequisite.stderr);
    const stale = run([...hostContext, "show", dependent, "--json"]);
    assert.equal(stale.status, 0, stale.stderr);
    assert.ok(
      JSON.parse(stale.stdout).next.includes(
        `engram work update ${dependent} --drop-after ${prerequisite}`,
      ),
    );
    const dropAfter = run([
      ...hostContext,
      "update",
      dependent,
      "--drop-after",
      prerequisite,
      "--json",
    ]);
    assert.equal(dropAfter.status, 0, dropAfter.stderr);
    assert.equal(JSON.parse(dropAfter.stdout).operation, "remove_prerequisite");

    const closedAfter = run([
      ...hostContext,
      "update",
      dependent,
      "--after",
      prerequisite,
    ]);
    assert.notEqual(closedAfter.status, 0);
    assert.match(closedAfter.stderr, /not open/u);
    assert.match(closedAfter.stderr, new RegExp(prerequisite, "u"));
    assert.doesNotMatch(closedAfter.stderr, new RegExp(`show ${dependent}`, "u"));

    const missingPrerequisite = "00000000-0000-0000-0000-000000000000";
    const missingDrop = run([
      ...hostContext,
      "update",
      dependent,
      "--drop-after",
      missingPrerequisite,
    ]);
    assert.notEqual(missingDrop.status, 0);
    assert.match(missingDrop.stderr, /no such item/u);
    assert.doesNotMatch(missingDrop.stderr, new RegExp(`show ${dependent}`, "u"));

    const missingReason = run([
      ...hostContext,
      "update",
      dependent,
      "--supersede-with",
      replacement,
    ]);
    assert.notEqual(missingReason.status, 0);
    assert.match(missingReason.stderr, /requires --reason/u);

    const superseded = run([
      ...hostContext,
      "update",
      dependent,
      "--supersede-with",
      replacement,
      "--reason",
      "replacement owns the outcome",
      "--json",
    ]);
    assert.equal(superseded.status, 0, superseded.stderr);
    assert.equal(JSON.parse(superseded.stdout).operation, "supersede");

    const claimed = run([...hostContext, "claim", gated]);
    assert.equal(claimed.status, 0, claimed.stderr);
    const gate = run([
      ...hostContext,
      "gate",
      "CARGO-TEST",
      "--failed",
      "cut_a::gate",
      "--ref",
      "target/cut-a.log",
      "--json",
    ]);
    assert.equal(gate.status, 0, gate.stderr);
    assert.deepEqual(JSON.parse(gate.stdout).gate, {
      name: "cargo-test",
      passed: false,
      failed_count: 1,
      referenced: true,
    });
    const gateShow = run([...hostContext, "show", gated, "--json"]);
    assert.equal(gateShow.status, 0, gateShow.stderr);
    assert.match(
      JSON.parse(gateShow.stdout).evidence_items.at(-1).summary,
      /^gate cargo-test failed/u,
    );
    const passed = run([...hostContext, "gate", "CARGO-TEST", "--json"]);
    assert.equal(passed.status, 0, passed.stderr);
    const passedReceipt = JSON.parse(passed.stdout);
    assert.equal(passedReceipt.gate.passed, true);
    const replayed = run([...hostContext, "gate", "cargo-test", "--json"]);
    assert.equal(replayed.status, 0, replayed.stderr);
    assert.deepEqual(
      JSON.parse(replayed.stdout).receipt.result,
      passedReceipt.receipt.result,
    );
    const failedAgain = run([
      ...hostContext,
      "gate",
      "cargo-test",
      "--failed",
      "cut_a::gate",
      "--ref",
      "target/cut-a.log",
      "--json",
    ]);
    assert.equal(failedAgain.status, 0, failedAgain.stderr);
    const passedAgain = run([...hostContext, "gate", "cargo-test", "--json"]);
    assert.equal(passedAgain.status, 0, passedAgain.stderr);
    assert.notDeepEqual(
      JSON.parse(passedAgain.stdout).receipt.result,
      passedReceipt.receipt.result,
    );

    const escapeHeavy = run([
      ...hostContext,
      "gate",
      "escape-heavy",
      ...Array.from({ length: 16 }, (_, index) => [
        "--failed",
        `${String(index).padStart(2, "0")}-${'"'.repeat(252)}`,
      ]).flat(),
      "--ref",
      "\\".repeat(2048),
      "--json",
    ]);
    assert.equal(escapeHeavy.status, 0, escapeHeavy.stderr);
    assert.ok(Buffer.byteLength(escapeHeavy.stdout, "utf8") < 12 * 1024);
    assert.deepEqual(JSON.parse(escapeHeavy.stdout).gate, {
      name: "escape-heavy",
      passed: false,
      failed_count: 16,
      referenced: true,
    });

    const textGate = run([...hostContext, "gate", "cargo-fmt"]);
    assert.equal(textGate.status, 0, textGate.stderr);
    assert.match(textGate.stdout, /recorded gate cargo-fmt passed/u);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("blank asserted work identities are refused at the shared service boundary", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-blank-identity-"));
  try {
    hostSetup(engramHome);
    for (const [actor, session] of [
      ["", "session"],
      ["agent", "   "],
    ]) {
      const refused = run([
        "--home",
        engramHome,
        "work",
        "--actor-id",
        actor,
        "--session-id",
        session,
        "next",
      ]);
      assert.notEqual(refused.status, 0);
      assert.match(refused.stderr, /non-empty asserted actor and session/u);
    }
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});
