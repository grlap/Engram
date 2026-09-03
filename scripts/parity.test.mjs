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

import { assertTerseShow, UUID } from "./terse-show-assertions.mjs";

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");

const MAX_COMMANDS = 3;
const MAX_FIELDS = 3;
const HASH = /\b[0-9a-f]{64}\b/u;

function run(args, options = {}) {
  const environment = { ...process.env };
  delete environment.ENGRAM_ACTOR_CONTEXT;
  const executed = spawnSync(binary, args, {
    cwd: root,
    encoding: "utf8",
    env: environment,
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

function withoutInjectedWorkAttribution(engramHome) {
  const environment = { ...process.env, ENGRAM_HOME: engramHome };
  delete environment.ENGRAM_ACTOR_ID;
  delete environment.ENGRAM_SESSION_ID;
  delete environment.ENGRAM_ACTOR_CONTEXT;
  return environment;
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

    // Verification outside the count: the agent detail view remains terse,
    // while the completed lifecycle stays directly readable.
    const shown = run([...hostContext, "show", ref, "--json"]);
    assert.equal(shown.status, 0, shown.stderr);
    const view = JSON.parse(shown.stdout);
    assert.equal(view.status.work.lifecycle, "completed");
    assert.ok(Array.isArray(view.reminders));
    assert.ok(Array.isArray(view.next));
    assert.ok(Array.isArray(view.allowed_next));
    assertTerseShow(view);
    const shownText = run([...hostContext, "show", ref]);
    assert.equal(shownText.status, 0, shownText.stderr);
    assert.doesNotMatch(shownText.stdout, HASH);
    assert.doesNotMatch(shownText.stdout, UUID);
    assert.doesNotMatch(
      shownText.stdout,
      /completion_seal|control_binding|obligation_page|\bfence\b|\brevision\b/iu,
    );
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("optional child is marked by show and does not gate parent completion", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-optional-child-"));
  const actor = "optional-child-agent";
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
    const parent = run([...hostContext, "add", "Optional parent", "--json"]);
    assert.equal(parent.status, 0, parent.stderr);
    const parentWork = JSON.parse(parent.stdout).work;
    const child = run([
      ...hostContext,
      "add",
      "Non-blocking follow-up",
      "--under",
      parentWork.short_ref,
      "--optional",
      "--json",
    ]);
    assert.equal(child.status, 0, child.stderr);
    assert.equal(JSON.parse(child.stdout).work.child_requirement, "optional");

    const shown = run([...hostContext, "show", parentWork.short_ref, "--json"]);
    assert.equal(shown.status, 0, shown.stderr);
    const parentView = JSON.parse(shown.stdout);
    assert.equal(parentView.children.length, 1);
    assert.equal(parentView.children[0].child_requirement, "optional");
    const shownText = run([...hostContext, "show", parentWork.short_ref]);
    assert.equal(shownText.status, 0, shownText.stderr);
    assert.match(shownText.stdout, /children: .* \(open, optional\)/u);

    const claimed = run([...hostContext, "claim", parentWork.short_ref]);
    assert.equal(claimed.status, 0, claimed.stderr);
    const completed = run([
      ...hostContext,
      "done",
      parentWork.short_ref,
      "Parent complete without optional follow-up",
    ]);
    assert.equal(completed.status, 0, completed.stderr);
    assert.match(completed.stdout, /^done /u);
    const completedView = run([...hostContext, "show", parentWork.short_ref, "--json"]);
    assert.equal(completedView.status, 0, completedView.stderr);
    assert.equal(JSON.parse(completedView.stdout).status.work.lifecycle, "completed");

    const invalidRoot = run([...hostContext, "add", "Invalid optional root", "--optional"]);
    assert.notEqual(invalidRoot.status, 0);
    assert.match(invalidRoot.stderr, /--under/u);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("disposed required child names its lifecycle and runnable waiver", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-child-waiver-"));
  const actor = "child-waiver-agent";
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
    const parentResult = run([
      ...hostContext,
      "add",
      "Waiver parent",
      "--json",
    ]);
    assert.equal(parentResult.status, 0, parentResult.stderr);
    const parent = JSON.parse(parentResult.stdout).work;
    const childResult = run([
      ...hostContext,
      "add",
      "Disposed required child",
      "--under",
      parent.short_ref,
      "--json",
    ]);
    assert.equal(childResult.status, 0, childResult.stderr);
    const child = JSON.parse(childResult.stdout).work;
    const cancelled = run([
      ...hostContext,
      "update",
      child.short_ref,
      "--cancel",
      "child outcome is no longer needed",
    ]);
    assert.equal(cancelled.status, 0, cancelled.stderr);
    const claimed = run([...hostContext, "claim", parent.short_ref]);
    assert.equal(claimed.status, 0, claimed.stderr);

    const refused = run([
      ...hostContext,
      "done",
      parent.short_ref,
      "parent implementation complete",
    ]);
    assert.equal(refused.status, 2, refused.stderr);
    assert.match(
      refused.stdout,
      new RegExp(`required child ${child.short_ref} .* is cancelled without`, "u"),
    );
    const waiverCommand = `engram work update ${parent.short_ref} --waive ${child.short_ref} --reason "account for disposed required child"`;
    assert.ok(refused.stdout.includes(waiverCommand), refused.stdout);
    assert.doesNotMatch(refused.stdout, /engram work core/u);

    const waived = run([
      ...hostContext,
      "update",
      parent.short_ref,
      "--waive",
      child.short_ref,
      "--reason",
      "the cancelled child is explicitly accounted for",
      "--json",
    ]);
    assert.equal(waived.status, 0, waived.stderr);
    const waiverReceipt = JSON.parse(waived.stdout);
    assert.equal(waiverReceipt.operation, "waive_required_child");
    assert.equal(waiverReceipt.receipt.work_id, parent.work_id);
    assert.equal(typeof waiverReceipt.receipt.result.work_revision, "number");

    const completed = run([
      ...hostContext,
      "done",
      parent.short_ref,
      "parent implementation complete",
    ]);
    assert.equal(completed.status, 0, completed.stderr);
    assert.match(completed.stdout, /^done /u);
  } finally {
    rmSync(engramHome, { recursive: true, force: true });
  }
});

test("shell words default missing local attribution without losing explicit targeting", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-defaults-"));
  try {
    hostSetup(engramHome);
    const seeded = run([
      "--home",
      engramHome,
      "work",
      "--actor-id",
      "injected-actor",
      "--session-id",
      "injected-session",
      "add",
      "Default attribution fixture",
      "--json",
    ]);
    assert.equal(seeded.status, 0, seeded.stderr);
    const seededWork = JSON.parse(seeded.stdout).work;
    const workRef = seededWork.short_ref;
    const environment = withoutInjectedWorkAttribution(engramHome);

    const next = run(["work", "next", "--json"], {
      env: environment,
    });
    assert.equal(next.status, 0, next.stderr);
    assert.ok(Array.isArray(JSON.parse(next.stdout).ready));
    assert.match(
      next.stderr,
      /attribution uses the asserted OS-user environment|attribution uses a synthetic process actor/u,
    );
    assert.match(next.stderr, /Reuse it with --session-id local-process-/u);

    const shown = run(
      ["work", "show", workRef, "--json"],
      { env: environment },
    );
    assert.equal(shown.status, 0, shown.stderr);
    assert.equal(JSON.parse(shown.stdout).status.work.short_ref, workRef);

    const claimed = run(["work", "claim", workRef], { env: environment });
    assert.equal(claimed.status, 0, claimed.stderr);
    const claimedSession = claimed.stderr.match(
      /this command uses (local-process-\d+-[0-9a-f-]{36})\./u,
    )?.[1];
    assert.ok(claimedSession, claimed.stderr);

    const observed = run(
      [
        "work",
        "--actor-id",
        "observer",
        "--session-id",
        "observer",
        "show",
        workRef,
        "--json",
      ],
      { env: environment },
    );
    assert.equal(observed.status, 0, observed.stderr);
    const observedView = JSON.parse(observed.stdout);
    assert.equal(observedView.holder, "another session");
    assertTerseShow(observedView);
    const observedText = run(
      [
        "work",
        "--actor-id",
        "observer",
        "--session-id",
        "observer",
        "show",
        workRef,
      ],
      { env: environment },
    );
    assert.equal(observedText.status, 0, observedText.stderr);
    assert.equal(observedText.stdout.includes(claimedSession), false);

    const continued = run(
      [
        "work",
        "next",
        "--verbose",
        "--json",
        "--session-id",
        claimedSession,
      ],
      { env: environment },
    );
    assert.equal(continued.status, 0, continued.stderr);
    assert.equal(
      JSON.parse(continued.stdout).session.focused_work_id,
      seededWork.work_id,
    );
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
    assert.equal(listedValue.memories[0].actor_context, undefined);

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
    assert.equal(JSON.parse(full.stdout).actor_context, undefined);

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

test("CLI actor context is attribution while actor and session remain principals", () => {
  const engramHome = mkdtempSync(join(tmpdir(), "engram-parity-actor-context-"));
  const actor = "greg/codex";
  const assignee = "planning-owner";
  const session = "actor-context-source";
  const recipientSession = "actor-context-recipient";
  const actorContext = "model=opus-4.1;reasoning=high";
  const environment = {
    ...process.env,
    ENGRAM_HOME: engramHome,
    ENGRAM_ACTOR_ID: actor,
    ENGRAM_SESSION_ID: session,
    ENGRAM_ACTOR_CONTEXT: actorContext,
  };
  const word = (...args) => run(["work", ...args], { env: environment });
  const recipientEnvironment = {
    ...environment,
    ENGRAM_ACTOR_ID: "peer",
    ENGRAM_SESSION_ID: recipientSession,
  };
  delete recipientEnvironment.ENGRAM_ACTOR_CONTEXT;
  const recipientWord = (...args) =>
    run(["work", ...args], { env: recipientEnvironment });
  const assigneeEnvironment = {
    ...environment,
    ENGRAM_ACTOR_ID: assignee,
    ENGRAM_SESSION_ID: "actor-context-assignee",
  };
  delete assigneeEnvironment.ENGRAM_ACTOR_CONTEXT;
  const assigneeWord = (...args) =>
    run(["work", ...args], { env: assigneeEnvironment });
  try {
    hostSetup(engramHome);
    const added = word(
      "add",
      "Attribute CLI execution context",
      "--assignee",
      assignee,
      "--json",
    );
    assert.equal(added.status, 0, added.stderr);
    const workRef = JSON.parse(added.stdout).work.short_ref;
    const mine = word("ls", "--mine", "--json");
    assert.equal(mine.status, 0, mine.stderr);
    assert.ok(!JSON.parse(mine.stdout).items.some(({ ref }) => ref === workRef));
    const assigned = assigneeWord("ls", "--mine", "--json");
    assert.equal(assigned.status, 0, assigned.stderr);
    assert.ok(JSON.parse(assigned.stdout).items.some(({ ref }) => ref === workRef));
    assert.equal(word("claim", workRef).status, 0);
    const noted = word("note", workRef, "context follows attribution");
    assert.equal(noted.status, 0, noted.stderr);

    const shown = word("show", workRef, "--json");
    assert.equal(shown.status, 0, shown.stderr);
    const showValue = JSON.parse(shown.stdout);
    assert.equal(showValue.notes.at(-1).by, `you (${actorContext})`);
    assert.ok(
      showValue.history.items.some(
        ({ by }) => by === `you (${actorContext})`,
      ),
    );
    const showText = word("show", workRef);
    assert.equal(showText.status, 0, showText.stderr);
    assert.match(showText.stdout, new RegExp(`latest note by you \\(${actorContext}\\)`, "u"));

    const remembered = word(
      "remember",
      "Actor context is retained on project memories",
      "--key",
      "actor-context",
    );
    assert.equal(remembered.status, 0, remembered.stderr);
    const memories = word("memories", "--json");
    assert.equal(memories.status, 0, memories.stderr);
    assert.equal(JSON.parse(memories.stdout).memories[0].actor_id, actor);
    assert.equal(
      JSON.parse(memories.stdout).memories[0].actor_context,
      actorContext,
    );
    const memoryText = word("memories");
    assert.equal(memoryText.status, 0, memoryText.stderr);
    assert.match(memoryText.stdout, new RegExp(`by ${actor} \\(${actorContext}\\)`, "u"));
    const fullMemoryText = word("memories", "actor-context", "--full");
    assert.equal(fullMemoryText.status, 0, fullMemoryText.stderr);
    assert.match(
      fullMemoryText.stdout,
      new RegExp(`by ${actor} \\(${actorContext}\\)`, "u"),
    );

    const offered = word(
      "handoff",
      workRef,
      "--to",
      recipientSession,
      "--json",
    );
    assert.equal(offered.status, 0, offered.stderr);
    assert.equal(JSON.parse(offered.stdout).operation, "offer");
    const accepted = recipientWord("handoff", workRef, "--accept", "--json");
    assert.equal(accepted.status, 0, accepted.stderr);
    assert.equal(JSON.parse(accepted.stdout).operation, "accept");
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
      const title =
        index === 0
          ? `Budget item 00 ${"x".repeat(100)}`
          : index === 1
            ? `Unicode budget ${"界".repeat(40)}`
          : `Budget item ${String(index).padStart(2, "0")}`;
      const added = run([
        ...hostContext,
        "add",
        title,
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
      maxItemBytes <= 256,
      `${maxItemBytes} max bytes/item: ${JSON.stringify(maxItem)}`,
    );
    for (const row of compactList.items) {
      assert.equal(typeof row.ref, "string");
      assert.equal(typeof row.title, "string");
      assert.equal(typeof row.state, "string");
      for (const forbidden of [
        "acceptance",
        "active_run_id",
        "blocked",
        "lifecycle",
        "revision",
        "root_id",
        "updated_at",
        "work_id",
      ]) {
        assert.equal(forbidden in row, false, `${forbidden} leaked into compact row`);
      }
    }
    const heldRow = compactList.items.find(({ ref }) => ref === refs[0]);
    assert.equal(Buffer.byteLength(heldRow.title, "utf8"), 80);
    assert.match(heldRow.title, /…$/u);
    assert.equal(heldRow.holder, actor);
    assert.equal(typeof heldRow.held_until, "string");
    const unicodeRow = compactList.items.find(({ ref }) => ref === refs[1]);
    assert.ok(Buffer.byteLength(unicodeRow.title, "utf8") <= 80);
    assert.match(unicodeRow.title, /…$/u);
    assert.equal(unicodeRow.title.includes("\uFFFD"), false);
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
    assert.ok(blockedLine?.includes(" blocked \""));
    assert.equal(blockedLine?.includes("open/blocked"), false);
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
    // Keep the realistic 20-row compact fixture materially below the shared
    // 12 KiB hard ceiling while still requiring every requested row.
    assert.ok(
      Buffer.byteLength(next.stdout, "utf8") <= 8 * 1024,
      `${Buffer.byteLength(next.stdout, "utf8")} byte next receipt`,
    );
    const compactNext = JSON.parse(next.stdout);
    assert.equal(compactNext.ready.length, 20);
    assert.equal("session" in compactNext, false);
    assert.equal("delivery_token" in compactNext, false);
    assert.ok(Array.isArray(compactNext.changes));
    const nextText = run([...hostContext, "next", "--limit", "20"]);
    assert.equal(nextText.status, 0, nextText.stderr);
    assert.ok(
      Buffer.byteLength(nextText.stdout, "utf8") <= 8 * 1024,
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
    // Nothing is held yet: inspect exact claim guidance before mutating it.
    const unheld = run([...hostContext, "note", "too early"]);
    assert.notEqual(unheld.status, 0);
    assert.match(unheld.stderr, /claim it before/u);
    assert.match(unheld.stderr, new RegExp(`engram work show ${ref}`, "u"));
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
    const supersededShow = run([
      ...hostContext,
      "show",
      dependent,
      "--json",
    ]);
    assert.equal(supersededShow.status, 0, supersededShow.stderr);
    assert.equal(
      JSON.parse(supersededShow.stdout).status.work.superseded_by,
      replacement,
    );
    assertTerseShow(JSON.parse(supersededShow.stdout));
    const supersededText = run([...hostContext, "show", dependent]);
    assert.equal(supersededText.status, 0, supersededText.stderr);
    assert.match(supersededText.stdout, new RegExp(`successor: ${replacement}`, "u"));

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
      JSON.parse(gateShow.stdout).notes.at(-1).summary,
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
