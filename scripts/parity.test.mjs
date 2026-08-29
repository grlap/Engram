#!/usr/bin/env node

// Measured acceptance for the eight-word agent surface: on a fresh store, an
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
const binary = join(root, "target", "debug", "engram");

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

function hostSetup(engramHome, actor) {
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(built.status, 0, built.stderr);
  const initialized = run(["--home", engramHome, "init"]);
  assert.equal(initialized.status, 0, initialized.stderr);
  const granted = run([
    "--home",
    engramHome,
    "authority",
    "grant",
    "--subject-actor-id",
    actor,
    "--issued-by",
    "parity-host",
    "--reason",
    "parity acceptance",
  ]);
  assert.equal(granted.status, 0, granted.stderr);
  return JSON.parse(granted.stdout).grant;
}

test("add -> claim -> done takes three commands and at most three fields", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-"));
  const actor = "parity-agent";
  try {
    const grant = hostSetup(engramHome, actor);
    // Host context is fixed by the wrapper, not typed by the agent.
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
      "--authority-grant",
      grant,
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

test("done says what is owed and exits 2 when the item cannot seal yet", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-owed-"));
  const actor = "parity-agent";
  try {
    const grant = hostSetup(engramHome, actor);
    const hostContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      actor,
      "--session-id",
      actor,
      "--authority-grant",
      grant,
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
    assert.match(noted.stdout, /^noted on w-[0-9a-f]{12} "Needs a note first": found the missing piece/u);
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
