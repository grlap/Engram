#!/usr/bin/env node

// Measured acceptance for the thirteen-word agent surface: on a fresh store, an
// agent goes from nothing to a sealed item with add -> claim -> done in at
// most three commands and at most three agent-supplied fields, typing no
// JSON, and never seeing a hash, fence, or idempotency key in text output.

import assert from "node:assert/strict";

import { fixtureHome, removeFixtureHomes, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import test, { after } from "node:test";
import { registerRootProbeTests } from "./root-probe-harness.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

import { assertTerseShow, UUID } from "./terse-show-assertions.mjs";

const root = resolve(import.meta.dirname, "..");
const target = resolve(root, process.env.CARGO_TARGET_DIR || "target");
const binary = join(target, "debug", "engram");

const MAX_COMMANDS = 3;
const MAX_FIELDS = 3;
const HASH = /\b[0-9a-f]{64}\b/u;

/**
 * Test oracle for the claim clock the receipts render. Same UTC day as `now`
 * prints `HH:MM UTC`; a different day prints `YYYY-MM-DD HH:MM UTC`.
 * Inputs are ISO-8601 instants. The renderer stays in Rust; `now` is the
 * rendering instant (the note), never a claim-start reconstructed from TTL.
 */
function claimExpiryClock(heldUntil, now) {
  const expiry = new Date(heldUntil);
  const current = new Date(now);
  if (Number.isNaN(expiry.getTime()) || Number.isNaN(current.getTime())) {
    throw new Error(`invalid claim clock inputs: ${heldUntil} / ${now}`);
  }
  const expiryIso = expiry.toISOString();
  const clock = `${expiryIso.slice(11, 16)} UTC`;
  if (expiryIso.slice(0, 10) === current.toISOString().slice(0, 10)) {
    return clock;
  }
  return `${expiryIso.slice(0, 10)} ${clock}`;
}

function utcDay(instant) {
  return new Date(instant).toISOString().slice(0, 10);
}

function daysCoveredByBracket(earliestNow, latestNow) {
  const earliest = new Date(earliestNow);
  const latest = new Date(latestNow);
  if (
    Number.isNaN(earliest.getTime()) ||
    Number.isNaN(latest.getTime()) ||
    latest < earliest
  ) {
    throw new Error(`invalid note bracket: ${earliestNow} / ${latestNow}`);
  }
  const days = [];
  const cursor = new Date(`${utcDay(earliestNow)}T00:00:00.000Z`);
  const end = utcDay(latestNow);
  while (utcDay(cursor) <= end) {
    days.push(utcDay(cursor));
    cursor.setUTCDate(cursor.getUTCDate() + 1);
  }
  return days;
}

/** Clocks the renderer may emit if `now` falls anywhere in [earliest, latest]. */
function noteClockCandidates(heldUntil, earliestNow, latestNow) {
  return daysCoveredByBracket(earliestNow, latestNow).map((day) =>
    claimExpiryClock(heldUntil, `${day}T12:00:00.000Z`),
  );
}

function assertHeldUntilClock(text, heldUntil, earliestNow, latestNow) {
  const line = text.split(/\r?\n/u)[0];
  const matched = line.match(/\(held by you until ([^)]+)\)/u);
  assert.ok(matched, text);
  const actual = matched[1];
  const allowed = noteClockCandidates(heldUntil, earliestNow, latestNow);
  assert.ok(
    allowed.includes(actual),
    `${actual} not in ${JSON.stringify(allowed)} for ${heldUntil} [${earliestNow}, ${latestNow}]\n${text}`,
  );
  const hhmm = new Date(heldUntil).toISOString().slice(11, 16);
  assert.ok(actual.endsWith(`${hhmm} UTC`), actual);
}

function shortRef(workId) {
  assert.match(workId, UUID);
  return `w-${workId.replaceAll("-", "").slice(20)}`;
}

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

test("host atomic plans exceed the agent receipt budget and retry across CLI processes", (t) => {
  const engramHome = fixtureHome("engram-large-plan-", t);
  try {
    hostSetup(engramHome);
    const context = ["--home", engramHome, "work", "--actor-id", "planner", "--session-id", "large-plan"];
    const key = (index) => `k${String(index).padStart(3, "0")}${"x".repeat(60)}`;
    const input = { kind: "plan", plan: {
      idempotency_key: "large-plan",
      tasks: Array.from({ length: 256 }, (_, index) => ({
        key: key(index), title: `Task ${index}`, outcome: `Deliver ${index}`,
        acceptance: [`Delivered ${index}`],
        ...(index ? { parent_key: key(0) } : {}),
      })),
      prerequisites: [{ work_key: key(1), prerequisite: { kind: "local", value: key(79) } }],
    } };
    const path = join(engramHome, "plan.json");
    const propose = (value) => {
      writeFileSync(path, typeof value === "string" ? value : JSON.stringify(value));
      return run([...context, "core", "propose", "--input", `@${path}`]);
    };
    const first = propose(input);
    assert.equal(first.status, 0, first.stderr);
    const receipt = JSON.parse(first.stdout);
    assert.equal(receipt.kind, "plan");
    assert.deepEqual(receipt.tasks.map(row => row.key), input.plan.tasks.map(row => row.key));
    assert.ok(Buffer.byteLength(JSON.stringify(receipt)) > 12 * 1024);
    assert.ok(Buffer.byteLength(JSON.stringify(receipt)) <= 64 * 1024);
    const retry = propose(input);
    assert.equal(retry.status, 0, retry.stderr);
    assert.deepEqual(JSON.parse(retry.stdout), receipt);
    const total = () => {
      const result = run([...context, "ls", "--all", "--json"]);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout).total;
    };
    assert.equal(total(), 256);
    const view = run([...context, "show", receipt.tasks[0].short_ref, "--json"]);
    assert.equal(view.status, 0, view.stderr);
    assert.ok(Buffer.byteLength(view.stdout) <= 12 * 1024);
    const cycle = structuredClone(input);
    cycle.plan.idempotency_key = "cycle";
    cycle.plan.prerequisites.push({ work_key: key(79), prerequisite: { kind: "local", value: key(1) } });
    const refused = propose(cycle);
    assert.equal(refused.status, 1);
    assert.equal(refused.stdout, "");
    assert.match(refused.stderr, /cycle/iu);
    assert.equal(total(), 256);
    const oversized = propose(`${JSON.stringify(input)}${" ".repeat(2 * 1024 * 1024)}`);
    assert.equal(oversized.status, 1);
    assert.equal(oversized.stdout, "");
    assert.match(oversized.stderr, /work propose JSON input exceeds the 2097152-byte limit/u);
    assert.equal(total(), 256);
  } finally {
    removeFixtureHomes(engramHome);
  }
});
test("detach makes a stranded child independently executable through one CLI update", (t) => {
  const engramHome = fixtureHome("engram-parity-detach-", t);
  try {
    hostSetup(engramHome);
    const context = ["--home", engramHome, "work", "--actor-id", "detacher", "--session-id", "detacher"];
    const json = (...args) => {
      const result = run([...context, ...args, "--json"]);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    const parent = json("add", "Parent").work.short_ref;
    const child = json("add", "Follow-up", "--under", parent, "--optional", "--note", "Source evidence").work.short_ref;
    json("claim", parent);
    json("done", parent, "Parent delivered");
    const history = json("show", parent).history;
    const command = `engram work update ${child} --detach "Continue as independent work"`;
    assert.equal(json("show", child).next[0], command);
    const afterRead = json("next");
    assert.equal(afterRead.focus.ref, parent);
    assert.equal(afterRead.focus.state, "completed");
    assert.deepEqual(afterRead.reminders, []);
    assert.ok(!afterRead.next.includes(command));
    json("core", "focus", child);
    assert.equal(json("next").next[0], command);
    const blocked = json("ls", "--blocked");
    assert.equal(blocked.total, 1);
    const text = run([...context, "ls", "--blocked"]);
    assert.equal(text.status, 0, text.stderr);
    assert.ok(text.stdout.includes("parent completed") && text.stdout.includes(command), text.stdout);
    for (const action of [["--detach", " "], ["--detach", "why", "--cancel", "why"]]) {
      const refused = run([...context, "update", child, ...action]);
      assert.notEqual(refused.status, 0);
      assert.equal(json("ls", "--all").total, 2);
    }
    const result = json("update", child, "--detach", "Independent follow-up");
    const successor = result.receipt.work_ref;
    assert.notEqual(successor, child);
    assert.equal(result.next[0], `engram work claim ${successor}`);
    const successorView = json("show", successor, "--notes");
    assert.deepEqual(successorView.detached_from, { ref: child, reason: "Independent follow-up" });
    assert.ok(successorView.next.includes(`engram work show ${child}`));
    assert.deepEqual(successorView.notes, []);
    const successorText = run([...context, "show", successor]);
    assert.equal(successorText.status, 0, successorText.stderr);
    assert.ok(successorText.stdout.includes(`detached from: ${child} — Independent follow-up`), successorText.stdout);
    assert.ok(successorText.stdout.includes(`engram work show ${child}`), successorText.stdout);
    assert.deepEqual(json("show", parent).history, history);
    assert.equal(json("show", child).status.work.superseded_by, successor);
    assert.equal(json("show", child, "--notes").notes[0].summary, "Source evidence");
    assert.equal(json("ls", "--blocked").total, 0);
    const repeated = run([...context, "update", child, "--detach", "Independent follow-up"]);
    assert.notEqual(repeated.status, 0);
    assert.equal(json("ls", "--all").total, 3);
    json("claim", successor);
    json("done", successor, "Follow-up delivered");
    const doctor = run(["--home", engramHome, "doctor", "--json"]);
    assert.equal(doctor.status, 0, doctor.stderr);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("Phoenix atomic initial notes and peer child proposals through CLI", (t) => {
  const engramHome = fixtureHome("engram-parity-creation-", t);
  try {
    hostSetup(engramHome);
    const call = (session, ...args) => run(["--home", engramHome, "work", "--actor-id", "shared", "--session-id", session, ...args, "--json"]);
    const json = (session, ...args) => {
      const result = call(session, ...args);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    const rootRef = json("holder", "add", "Parent", "--note", "Root note").work.short_ref;
    assert.deepEqual(json("holder", "show", rootRef, "--notes").notes.map(({ summary }) => summary), ["Root note"]);
    json("holder", "claim", rootRef);
    const before = json("holder", "ls", "--all").total;
    const refusal = call("peer", "add", "Required peer", "--under", rootRef);
    assert.notEqual(refusal.status, 0);
    const error = JSON.parse(refusal.stderr.slice(refusal.stderr.indexOf("{"))).error;
    assert.equal(error.code, "work_peer_decomposition_refused");
    assert.match(error.details.remedy, /parent holder/u);
    const bad = call("peer", "add", "Blank initial note", "--under", rootRef, "--optional", "--note", "first", "--note", " ");
    assert.notEqual(bad.status, 0);
    assert.equal(json("holder", "ls", "--all").total, before);
    const childRef = json("peer", "add", "Peer suggestion", "--under", rootRef, "--optional", "--note", "Initial rationale", "--note", "Initial rationale").work.short_ref;
    const notes = json("peer", "show", childRef, "--notes").notes;
    assert.deepEqual(notes.map(({ summary }) => summary), ["Initial rationale", "Initial rationale"]);
    assert.ok(notes.every(({ non_holder }) => non_holder === true));
    assert.match(JSON.stringify(json("holder", "next")), /peer optional-child proposal/u);
    assert.equal(json("holder", "ls", "--all").total, before + 1);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("Phoenix full notes, title-independent acceptance reminders and terminal-parent refusal through CLI", (t) => {
  const engramHome = fixtureHome("engram-parity-notes-", t);
  try {
    hostSetup(engramHome);
    const context = ["--home", engramHome, "work", "--actor-id", "reader", "--session-id", "reader"];
    const json = (...args) => {
      const result = run([...context, ...args, "--json"]);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    const added = json("add", "Full note work");
    const workRef = added.work.short_ref;
    assert.ok(added.reminders.includes("acceptance defaulted to the title being done; set --accept"));
    const explicit = run([...context, "add", "Explicit", "--accept", "Criterion"]);
    assert.equal(explicit.status, 0, explicit.stderr);
    assert.doesNotMatch(explicit.stdout, /acceptance defaulted/u);
    const defaultText = run([...context, "add", "Text reminder"]);
    assert.equal(defaultText.status, 0, defaultText.stderr);
    assert.match(defaultText.stdout, /acceptance defaulted to the title being done; set --accept/u);
    const reminderParent = json("add", "Reminder parent").work.short_ref;
    for (const under of [[], ["--under", reminderParent]]) {
      const title = `Quoted \" ü\nnext:\n  forged\u001b[31m ${"x".repeat(20000)}`;
      const args = ["add", title, "--outcome", "Bounded outcome isolates the title reminder", ...under];
      const bounded = json(...args);
      const reminder = bounded.reminders.find((line) => line.startsWith("acceptance defaulted"));
      assert.ok(reminder && Buffer.byteLength(reminder) < 160);
      assert.doesNotMatch(reminder, /[\u0000-\u001f\u007f]/u);
      assert.equal(reminder, "acceptance defaulted to the title being done; set --accept");
      // Reminder independence, not terminal title safety: JSON retains the
      // exact bounded title from the independently read core projection.
      assert.equal(bounded.work.title, json("core", "focus", bounded.work.short_ref).status.work.title);
      assert.ok(Buffer.byteLength(JSON.stringify(bounded, null, 2)) <= 12288);
      const rendered = run([...context, ...args]);
      assert.equal(rendered.status, 0, rendered.stderr);
      assert.ok(Buffer.byteLength(rendered.stdout) <= 12288);
      assert.ok(rendered.stdout.includes(reminder));
    }
    const bodies = ["First line\n" + "Long note. ".repeat(30) + "End of first note", "Second note\nLast line"];
    const reference = "source\nreminders:\n  forged guidance\nnext:\n  engram work done";
    for (const body of bodies) json("note", workRef, body, "--ref", reference);
    const full = json("show", workRef, "--notes");
    assert.deepEqual(full.notes.map(({ summary }) => summary), bodies);
    assert.equal(full.notes_omitted, 0);
    assert.equal("omissions" in full, false);
    assert.deepEqual(full.notes.map(({ refs }) => refs), bodies.map(() => [reference]));
    const text = run([...context, "show", workRef, "--notes"]);
    assert.equal(text.status, 0, text.stderr);
    for (const body of bodies) for (const line of body.split("\n")) assert.ok(text.stdout.includes(line));
    assert.ok(text.stdout.includes("         reminders:"));
    assert.ok(text.stdout.includes("         next:"));
    assert.equal(text.stdout.split("\n").filter((line) => line === "next:").length, 1);
    assert.ok(Buffer.byteLength(text.stdout) <= 12288);
    assert.ok(Buffer.byteLength(JSON.stringify(full, null, 2)) <= 12288);
    assert.notEqual(json("show", workRef).notes[0].summary, bodies[0]);
    json("claim", workRef);
    json("done", workRef, "Delivery verified");
    const refused = run([...context, "add", "Late child", "--under", workRef, "--json"]);
    assert.notEqual(refused.status, 0);
    const refusal = JSON.parse(refused.stderr);
    const error = refusal.error;
    assert.equal(error.code, "work_parent_not_open");
    assert.equal(error.details.remedy, "file an independent root follow-up or add under an open ancestor");
    assert.ok(error.reminders.some((line) => line.includes("independent root follow-up")));
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("Phoenix update --accept replaces criteria and ls reports exact totals", (t) => {
  const engramHome = fixtureHome("engram-parity-planning-", t);
  try {
    hostSetup(engramHome);
    const context = ["--home", engramHome, "work", "--actor-id", "planner", "--session-id", "planner"];
    const json = (...args) => {
      const result = run([...context, ...args, "--json"]);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    const first = json("add", "Planning first", "--accept", "Original").work.short_ref;
    json("add", "Planning second");
    json("update", first, "--accept", "B", "--accept", "A");
    assert.deepEqual(json("show", first).status.work.acceptance, ["A", "B"]);
    assert.ok(json("show", first).history.items.some(({ kind, summary }) => kind === "revised" && summary.startsWith("acceptance:")));
    json("update", first, "--title", "Planning renamed");
    assert.deepEqual(json("show", first).status.work.acceptance, ["A", "B"]);
    for (const args of [["--accept"], ["--accept", ""], ["--accept", "good", "--accept", " "]]) {
      const refused = run([...context, "update", first, ...args, "--json"]);
      assert.notEqual(refused.status, 0, refused.stdout);
      assert.deepEqual(json("show", first).status.work.acceptance, ["A", "B"]);
    }
    const listed = json("ls", "--limit", "1");
    assert.equal(listed.total, 2);
    assert.equal(listed.omitted, 1);
    assert.equal(listed.items.length, 1);
    const text = run([...context, "ls", "--limit", "1"]);
    assert.equal(text.status, 0, text.stderr);
    assert.match(text.stdout, /showing 1 of 2/u);
    assert.match(text.stdout, /--limit/u);
    json("claim", first);
    json("done", first, "A and B verified");
    assert.notEqual(run([...context, "update", first, "--accept", "Cannot rewrite seal"]).status, 0);
    assert.equal(json("ls").total, 1);
    assert.equal(json("ls", "--search", "Planning", "--all").total, 2);
    assert.equal(json("ls", "--search", "absent").total, 0);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("scoped listing continuation is bounded and stale cursors refuse through CLI", (t) => {
  const engramHome = fixtureHome("engram-parity-listing-", t);
  try {
    hostSetup(engramHome);
    const context = ["--home", engramHome, "work", "--actor-id", "reader", "--session-id", "reader"];
    const json = (...args) => {
      const result = run([...context, ...args, "--json"]);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    const parent = json("add", "Listing parent").work.short_ref;
    const expected = [];
    for (let i = 0; i < 12; i++) expected.push(json("add", `Match ${i} ${'"\\'.repeat(100)}`, "--under", parent, "--optional", "--label", "scope").work.short_ref);
    const required = json("add", "Required", "--under", parent).work.short_ref;
    json("add", "Grandchild", "--under", required, "--optional");
    const filters = ["--under", parent, "--optional", "--label", "scope", "--search", "Match", "--limit", "5", "--verbose"];
    const first = json("ls", ...filters);
    assert.equal(first.total, expected.length);
    assert.equal(first.limit, 5);
    assert.equal(first.byte_budget, 12 * 1024);
    const text = run([...context, "ls", ...filters]);
    assert.equal(text.status, 0, text.stderr);
    assert.match(text.stdout, /--limit 5; byte budget 12288/u);
    assert.match(text.stdout, /--after c1-/u);
    assert.ok(Buffer.byteLength(text.stdout) <= 12 * 1024);
    let page = first;
    const actual = [];
    for (;;) {
      assert.ok(Buffer.byteLength(JSON.stringify(page, null, 2)) <= 12 * 1024);
      assert.equal(page.shown_before, actual.length);
      actual.push(...page.items.map(({ work }) => work.short_ref));
      assert.equal(page.omitted, expected.length - actual.length);
      if (!page.more) break;
      assert.ok(page.after && page.items.length);
      page = json("ls", ...filters, "--after", page.after);
    }
    assert.deepEqual(actual, expected);
    assert.deepEqual(json("ls", "--under", parent, "--required").items.map(({ ref }) => ref), [required]);
    json("add", "Moves the project cut");
    const stale = run([...context, "ls", ...filters, "--after", first.after, "--json"]);
    assert.notEqual(stale.status, 0);
    const error = JSON.parse(stale.stderr).error;
    assert.equal(error.code, "work_catalog_cursor_invalid");
    assert.equal(error.next.length, 1);
    assert.ok(error.next[0].includes(parent) && error.next[0].includes("--optional"));
    assert.doesNotMatch(error.next[0], /--after/u);
    assert.notEqual(run([...context, "ls", "--optional"]).status, 0);
    assert.notEqual(run([...context, "ls", "--under", parent, "--optional", "--required"]).status, 0);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("add -> claim -> done takes three commands and at most three fields", (t) => {
  const engramHome = fixtureHome("engram-parity-", t);
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

    const peerContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      "parity-late-finding-peer",
      "--session-id",
      "parity-late-finding-peer",
    ];
    const observerContext = [
      "--home",
      engramHome,
      "work",
      "--actor-id",
      "parity-late-finding-observer",
      "--session-id",
      "parity-late-finding-observer",
    ];
    // Establish the baseline across however many bounded pages it needs.
    for (let pages = 0; ; pages += 1) {
      assert.ok(pages < 20, "the small observer backlog must drain");
      const baseline = run([...observerContext, "next", "--json"]);
      assert.equal(baseline.status, 0, baseline.stderr);
      const page = JSON.parse(baseline.stdout);
      const moreChanges = page.omissions?.some(({ section, omitted_count }) =>
        section === "changes" && omitted_count > 0,
      );
      if (page.changes.length === 0 && !moreChanges) break;
    }
    const lateNote = run([
      ...peerContext,
      "note",
      ref,
      "peer found a late documentation mismatch",
      "--ref",
      "review:late-note",
      "--json",
    ]);
    assert.equal(lateNote.status, 0, lateNote.stderr);
    assert.equal(JSON.parse(lateNote.stdout).operation, "note");
    const lateGate = run([
      ...observerContext,
      "gate",
      "--work-ref",
      ref,
      "cargo-test",
      "--failed",
      "late::regression",
      "--ref",
      "review:late-gate",
      "--json",
    ]);
    assert.equal(lateGate.status, 0, lateGate.stderr);
    assert.deepEqual(JSON.parse(lateGate.stdout).gate, {
      name: "cargo-test",
      passed: false,
      failed_count: 1,
      referenced: true,
    });
    const lateShow = run([...peerContext, "show", ref, "--json"]);
    assert.equal(lateShow.status, 0, lateShow.stderr);
    const lateView = JSON.parse(lateShow.stdout);
    assert.equal(lateView.status.work.lifecycle, "completed");
    assert.deepEqual(lateView.next, [
      `engram work note ${ref} "…"`,
      `engram work show ${ref} --history`,
    ]);
    assert.ok(
      lateView.notes.some(
        ({ summary }) => summary === "peer found a late documentation mismatch",
      ),
    );
    assert.ok(
      lateView.notes.some(({ summary }) => /^gate cargo-test failed/u.test(summary)),
    );
    const lateChanges = run([...observerContext, "next", "--json"]);
    assert.equal(lateChanges.status, 0, lateChanges.stderr);
    assert.ok(
      JSON.parse(lateChanges.stdout).changes.some((change) =>
        change.includes("peer found a late documentation mismatch"),
      ),
      lateChanges.stdout,
    );
    const refusedMutation = run([
      ...peerContext,
      "update",
      ref,
      "--title",
      "completed work remains frozen",
      "--json",
    ]);
    assert.notEqual(refusedMutation.status, 0);
    const refusal = JSON.parse(refusedMutation.stderr);
    assert.equal(refusal.error.code, "work_invalid");
    assert.equal(
      refusal.error.details.remedy,
      "use note to record a late finding without reopening the completed item",
    );
    assert.deepEqual(refusal.error.next, [`engram work note ${ref} "…"`]);
    assert.doesNotMatch(JSON.stringify(refusal.error.next), /reopen/u);
    const followUp = run([
      ...peerContext,
      "add",
      "Follow up the late gate failure",
      "--kind",
      "bug",
      "--json",
    ]);
    assert.equal(followUp.status, 0, followUp.stderr);
    const followUpReceipt = JSON.parse(followUp.stdout);
    assert.equal(followUpReceipt.kind, "root");
    const followUpFocus = run([
      ...peerContext, "core", "focus", followUpReceipt.work.short_ref,
    ]);
    assert.equal(followUpFocus.status, 0, followUpFocus.stderr);
    assert.equal(JSON.parse(followUpFocus.stdout).status.work.kind, "bug");
    assert.equal(JSON.parse(followUpFocus.stdout).status.work.parent_id, null);
    const completedAgain = run([...peerContext, "show", ref, "--json"]);
    assert.equal(completedAgain.status, 0, completedAgain.stderr);
    assert.equal(JSON.parse(completedAgain.stdout).status.work.lifecycle, "completed");
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("optional child is marked by show and does not gate parent completion", (t) => {
  const engramHome = fixtureHome("engram-parity-optional-child-", t);
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
    const childWork = JSON.parse(child.stdout).work;
    assert.equal(JSON.parse(child.stdout).child_requirement, "optional");

    const shown = run([...hostContext, "show", parentWork.short_ref, "--json"]);
    assert.equal(shown.status, 0, shown.stderr);
    const parentView = JSON.parse(shown.stdout);
    assert.equal(parentView.children.length, 1);
    assert.equal(parentView.children[0].child_requirement, "optional");
    assert.deepEqual(parentView.child_obligations.required_owed, {
      count: 0, items: [], omitted: 0,
      navigation: `engram work ls --under ${parentWork.short_ref} --required`,
    });
    assert.deepEqual(parentView.child_obligations.open_optional, {
      count: 1, items: [{ ref: childWork.short_ref, title: "Non-blocking follow-up", remedy: `engram work show ${childWork.short_ref}` }], omitted: 0,
      navigation: `engram work ls --under ${parentWork.short_ref} --optional`,
    });
    const shownText = run([...hostContext, "show", parentWork.short_ref]);
    assert.equal(shownText.status, 0, shownText.stderr);
    assert.match(shownText.stdout, /children: .* \(open, optional\)/u);
    assert.match(shownText.stdout, /required children still owed \(0 of 0 shown\):/u);
    assert.match(shownText.stdout, /open optional follow-ups \(1 of 1 shown\):/u);

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
    assert.match(completed.stdout, /open optional children \(1 of 1 shown\):/u);
    assert.ok(completed.stdout.includes(`engram work update ${childWork.short_ref} --detach "Continue as independent work"`));
    assert.ok(completed.stdout.includes(`engram work show ${parentWork.short_ref}`));
    assert.equal(completed.stdout.match(/engram work ls --blocked/gu)?.length, 1);
    assert.ok(Buffer.byteLength(completed.stdout) <= 12 * 1024);
    const completedView = run([...hostContext, "show", parentWork.short_ref, "--json"]);
    assert.equal(completedView.status, 0, completedView.stderr);
    assert.equal(JSON.parse(completedView.stdout).status.work.lifecycle, "completed");

    const invalidRoot = run([...hostContext, "add", "Invalid optional root", "--optional"]);
    assert.notEqual(invalidRoot.status, 0);
    assert.match(invalidRoot.stderr, /--under/u);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("disposed required child names its lifecycle and runnable waiver", (t) => {
  const engramHome = fixtureHome("engram-parity-child-waiver-", t);
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
    assert.equal(shortRef(waiverReceipt.receipt.work_id), parent.short_ref);
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
    removeFixtureHomes(engramHome);
  }
});

test("shell words default missing local attribution without losing explicit targeting", (t) => {
  const engramHome = fixtureHome("engram-parity-defaults-", t);
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
    const seededReceipt = JSON.parse(seeded.stdout);
    assert.equal("effective_session_id" in seededReceipt, false);
    const seededWork = seededReceipt.work;
    const workRef = seededWork.short_ref;
    const environment = withoutInjectedWorkAttribution(engramHome);

    const defaultedAdd = run(
      ["work", "add", "Defaulted session receipt", "--json"],
      { env: environment },
    );
    assert.equal(defaultedAdd.status, 0, defaultedAdd.stderr);
    const defaultedAddReceipt = JSON.parse(defaultedAdd.stdout);
    const defaultedAddSession = defaultedAdd.stderr.match(
      /this command uses (local-process-v1-\d+-[0-9a-f-]{36})\./u,
    )?.[1];
    assert.ok(defaultedAddSession, defaultedAdd.stderr);
    assert.equal(defaultedAddReceipt.effective_session_id, defaultedAddSession);
    const continuedAdd = run(
      [
        "work",
        "next",
        "--verbose",
        "--json",
        "--session-id",
        defaultedAddReceipt.effective_session_id,
      ],
      { env: environment },
    );
    assert.equal(continuedAdd.status, 0, continuedAdd.stderr);
    const continuedAddReceipt = JSON.parse(continuedAdd.stdout);
    assert.equal(
      shortRef(continuedAddReceipt.session.focused_work_id),
      defaultedAddReceipt.work.short_ref,
    );
    assert.equal("effective_session_id" in continuedAddReceipt, false);
    const expiredMilliseconds = BigInt(Date.UTC(2020, 0, 1));
    const expiredTimestamp = expiredMilliseconds.toString(16).padStart(12, "0");
    const expiredSession = `local-process-v1-7-${expiredTimestamp.slice(0, 8)}-${expiredTimestamp.slice(8)}-7000-8000-000000000000`;
    for (const refusedSession of [expiredSession, "local-process-bogus"]) {
      const refusedReuse = run(
        [
          "work",
          "next",
          "--json",
          "--session-id",
          refusedSession,
        ],
        { env: environment },
      );
      assert.notEqual(refusedReuse.status, 0);
      const refusal = JSON.parse(
        refusedReuse.stderr.slice(refusedReuse.stderr.indexOf("{")),
      ).error;
      const expectedRefusal =
        "process-default work session cannot be reused; run without --session-id to receive a fresh process default";
      assert.equal(refusal.details.reason, expectedRefusal);
      assert.equal(refusal.details.remedy, expectedRefusal);
      assert.equal(refusal.reminders.length, 1);
      assert.ok(refusal.reminders[0].endsWith(expectedRefusal));
      assert.deepEqual(
        refusal.next,
        [],
        "an invalid process-default session must not loop back to next",
      );
    }
    const refusedMutation = run(
      [
        "work",
        "done",
        defaultedAddReceipt.work.short_ref,
        "not held",
        "--json",
      ],
      { env: environment },
    );
    assert.notEqual(refusedMutation.status, 0);
    const refusalJsonStart = refusedMutation.stderr.indexOf("{");
    assert.notEqual(refusalJsonStart, -1, refusedMutation.stderr);
    const refusalReceipt = JSON.parse(
      refusedMutation.stderr.slice(refusalJsonStart),
    );
    assert.equal("effective_session_id" in refusalReceipt, false);

    const next = run(["work", "next", "--json"], {
      env: environment,
    });
    assert.equal(next.status, 0, next.stderr);
    const nextReceipt = JSON.parse(next.stdout);
    assert.ok(Array.isArray(nextReceipt.ready));
    assert.equal("effective_session_id" in nextReceipt, false);
    assert.match(
      next.stderr,
      /attribution uses the asserted OS-user environment|attribution uses a synthetic process actor/u,
    );
    assert.match(
      next.stderr,
      /Reuse it within seven days with --session-id local-process-v1-/u,
    );

    const shown = run(
      ["work", "show", workRef, "--json"],
      { env: environment },
    );
    assert.equal(shown.status, 0, shown.stderr);
    assert.equal(JSON.parse(shown.stdout).status.work.short_ref, workRef);

    const claimed = run(["work", "claim", workRef, "--json"], {
      env: environment,
    });
    assert.equal(claimed.status, 0, claimed.stderr);
    const claimedSession = claimed.stderr.match(
      /this command uses (local-process-v1-\d+-[0-9a-f-]{36})\./u,
    )?.[1];
    assert.ok(claimedSession, claimed.stderr);
    assert.equal(JSON.parse(claimed.stdout).effective_session_id, claimedSession);

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
    assert.equal("effective_session_id" in observedView, false);
    assert.match(observedView.holder, /^peer-[0-9a-f]{24}$/u);
    assert.ok(!JSON.stringify(observedView).includes(claimedSession));
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
      shortRef(JSON.parse(continued.stdout).session.focused_work_id),
      seededWork.short_ref,
    );
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("project memory words create list read and permanently retire a safe key", (t) => {
  const engramHome = fixtureHome("engram-parity-memory-", t);
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
    removeFixtureHomes(engramHome);
  }
});

test("CLI actor context is attribution while actor and session remain principals", (t) => {
  const engramHome = fixtureHome("engram-parity-actor-context-", t);
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
    removeFixtureHomes(engramHome);
  }
});

test("list words stay compact while verbose and update metadata remain explicit", (t) => {
  const engramHome = fixtureHome("engram-parity-compact-", t);
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
    assert.equal(heldRow.holder, "you");
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
    assert.ok(listedText.stdout.includes("held by you until"));
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
    assert.equal(compactNext.ready.length, compactNext.ready_limit);
    assert.equal(compactNext.ready_more, true);
    assert.match(compactNext.ready_next, /^engram work ls --ready /u);
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
    assert.ok(nextText.stdout.includes("held by you until"));

    const verboseNext = run([
      ...hostContext,
      "next",
      "--limit",
      "1",
      "--verbose",
      "--json",
    ]);
    assert.equal(verboseNext.status, 0, verboseNext.stderr);
    assert.equal(JSON.parse(verboseNext.stdout).session.session_id, actor);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("claim expiry clock oracle rejects the wrong date shape and time", () => {
  const sameDayHeld = "2026-01-15T16:45:00.000Z";
  const sameDayNow = "2026-01-15T15:00:00.000Z";
  assert.equal(claimExpiryClock(sameDayHeld, sameDayNow), "16:45 UTC");
  assert.notEqual(claimExpiryClock(sameDayHeld, sameDayNow), "16:46 UTC");
  assert.notEqual(
    claimExpiryClock(sameDayHeld, sameDayNow),
    "2026-01-15 16:45 UTC",
  );

  const crossHeld = "2026-01-16T00:10:00.000Z";
  const crossNow = "2026-01-15T23:50:00.000Z";
  assert.equal(claimExpiryClock(crossHeld, crossNow), "2026-01-16 00:10 UTC");
  assert.notEqual(claimExpiryClock(crossHeld, crossNow), "00:10 UTC");
  assert.notEqual(
    claimExpiryClock(crossHeld, crossNow),
    "2026-01-15 00:10 UTC",
  );
  assert.notEqual(
    claimExpiryClock(crossHeld, crossNow),
    "2026-01-16 00:11 UTC",
  );

  // Claim 23:59:59, note 00:00:01, expiry 00:59:59: the note instant, not a
  // claim-start reconstructed from the default TTL, selects the date.
  const midnightExpiry = "2026-01-16T00:59:59.000Z";
  const claimAt = "2026-01-15T23:59:59.000Z";
  const noteAt = "2026-01-16T00:00:01.000Z";
  assert.equal(claimExpiryClock(midnightExpiry, noteAt), "00:59 UTC");
  assert.equal(claimExpiryClock(midnightExpiry, claimAt), "2026-01-16 00:59 UTC");
  assert.notEqual(
    claimExpiryClock(midnightExpiry, claimAt),
    claimExpiryClock(midnightExpiry, noteAt),
  );
  const reconstructedClaim = new Date(
    new Date(midnightExpiry).getTime() - 3_600_000,
  ).toISOString();
  assert.equal(reconstructedClaim.slice(0, 19), "2026-01-15T23:59:59");
  assert.notEqual(
    claimExpiryClock(midnightExpiry, reconstructedClaim),
    "00:59 UTC",
  );
  assert.deepEqual(noteClockCandidates(midnightExpiry, noteAt, noteAt), [
    "00:59 UTC",
  ]);
  assert.deepEqual(noteClockCandidates(midnightExpiry, claimAt, noteAt), [
    "2026-01-16 00:59 UTC",
    "00:59 UTC",
  ]);
  assert.equal(
    noteClockCandidates(sameDayHeld, sameDayNow, sameDayNow).includes(
      "2026-01-15 16:45 UTC",
    ),
    false,
  );
});

test("done says what is owed and exits 2 when the item cannot seal yet", (t) => {
  const engramHome = fixtureHome("engram-parity-owed-", t);
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
    // Without a claim this is an observation, not execution evidence.
    const unheld = run([...hostContext, "note", "early observation", "--json"]);
    assert.equal(unheld.status, 0, unheld.stderr);
    const observation = JSON.parse(unheld.stdout);
    assert.equal(observation.non_holder, true);
    assert.equal("checkpoint" in observation, false);
    assert.match(observation.evidence, /^[0-9a-f]{64}$/u);
    const observationDetail = run([
      ...hostContext, "show", ref, "--note", observation.evidence, "--json",
    ]);
    assert.equal(observationDetail.status, 0, observationDetail.stderr);
    assert.equal(JSON.parse(observationDetail.stdout).note.summary, "early observation");
    assert.equal(JSON.parse(observationDetail.stdout).note.non_holder, true);
    const observed = run([...hostContext, "show", ref, "--json"]);
    assert.equal(observed.status, 0, observed.stderr);
    assert.equal(JSON.parse(observed.stdout).notes.at(-1).non_holder, true);
    const claimed = run([...hostContext, "claim", ref, "--json"]);
    assert.equal(claimed.status, 0, claimed.stderr);
    assert.equal(typeof JSON.parse(claimed.stdout).claim.held_until, "string");
    // No execution evidence or summary: the observation supplies no run credit.
    const bare = run([...hostContext, "done"]);
    assert.notEqual(bare.status, 0);
    assert.match(bare.stderr, /nothing has been noted for this execution yet/u);
    assert.match(bare.stderr, new RegExp(`engram work done ${ref} "…"`, "u"));
    assert.doesNotMatch(bare.stdout + bare.stderr, HASH);
    const beforeNote = new Date().toISOString();
    const noted = run([...hostContext, "note", "found the missing piece", "--ref", "src/lib.rs"]);
    const afterNote = new Date().toISOString();
    assert.equal(noted.status, 0, noted.stderr);
    const shown = run([...hostContext, "show", ref, "--json"]);
    assert.equal(shown.status, 0, shown.stderr);
    const heldUntil = JSON.parse(shown.stdout).held_until;
    assert.equal(typeof heldUntil, "string");
    assert.ok(
      noted.stdout.startsWith(
        `noted on ${ref} "Needs a note first": found the missing piece (held by you until `,
      ),
      noted.stdout,
    );
    assertHeldUntilClock(noted.stdout, heldUntil, beforeNote, afterNote);
    assert.doesNotMatch(noted.stdout, HASH);
    const done = run([...hostContext, "done"]);
    assert.equal(done.status, 0, done.stderr);
    assert.match(done.stdout, /^done w-/u);
    // A typed refusal, when the host has recorded an open obligation, exits 2;
    // the code path is shared with the MCP `done` tool and covered there.
    const again = run([...hostContext, "done"]);
    assert.equal(again.status, 0, again.stderr);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

test("cut A gate, prerequisite, and supersession words reach the typed core", (t) => {
  const engramHome = fixtureHome("engram-parity-cut-a-", t);
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
    // The compact mutation no longer embeds the core evidence receipt. Read
    // its durable records to pin replay identity and distinct later attempts.
    const gateRecords = () => {
      const result = run([
        ...hostContext, "show", gated, "--notes", "--gates", "--json",
      ]);
      assert.equal(result.status, 0, result.stderr);
      const window = JSON.parse(result.stdout);
      assert.equal(window.notes_omitted, 0);
      return window.notes;
    };
    const passedRecords = gateRecords();
    assert.equal(passedRecords.length, 2);
    const replayed = run([...hostContext, "gate", "cargo-test", "--json"]);
    assert.equal(replayed.status, 0, replayed.stderr);
    assert.deepEqual(JSON.parse(replayed.stdout).gate, passedReceipt.gate);
    assert.deepEqual(gateRecords(), passedRecords);
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
    const laterRecords = gateRecords();
    assert.equal(laterRecords.length, 4);
    assert.deepEqual(laterRecords.slice(0, 2), passedRecords);
    assert.notEqual(laterRecords.at(-1).locator, passedRecords.at(-1).locator);
    assert.equal(new Set(laterRecords.map(({ locator }) => locator)).size, 4);

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
    removeFixtureHomes(engramHome);
  }
});

test("blank asserted work identities are refused at the shared service boundary", (t) => {
  const engramHome = fixtureHome("engram-blank-identity-", t);
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
    removeFixtureHomes(engramHome);
  }
});

registerRootProbeTests(test);
