import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { closeFixtureClients } from "../test-temp.mjs";
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { after } from "node:test";
import { assertTerseShow } from "../terse-show-assertions.mjs";

export function registerStatusRecovery(support) {
  const { root, binary, test, fixtureHome, removeFixtureHomes, receipt, structuredError, wait, buildAndInit, McpClient } = support;
test("compact next shares clipped status context and requires the full STOP tail on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-status-context-", t);
  let client;
  const cli = (...args) => {
    const result = spawnSync(binary, ["--home", engramHome, "work", "--actor-id", "pilot-reader",
      "--session-id", "pilot-reader", ...args], { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    return result.stdout;
  };
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "pilot-reader");
    await client.initialize();
    for (const held of [false, true]) {
      const prefix = held ? "Held unique status prefix" : "Assigned unique status prefix";
      const body = `${prefix} ${"waiting for review ".repeat(90)}\nSTOP: publication requires explicit human approval`;
      const reference = receipt(await client.call("add", {
        title: held ? "Held duty" : "Assigned duty", assignee: "pilot-reader",
      })).work.short_ref;
      if (held) receipt(await client.call("claim", { work_ref: reference }));
      receipt(await client.call("note", { work_ref: reference, text: body, status: true }));
      const shell = JSON.parse(cli("next", "--json"));
      const mcp = receipt(await client.call("next"));
      const text = cli("next");
      for (const value of [shell, mcp]) {
        const row = value[held ? "held" : "assigned"].find(row => row.ref === reference);
        assert.equal(row.current_status.complete, false);
        assert.equal(JSON.stringify(value).split(prefix).length - 1, 1);
        assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12288);
        assert.ok(value.reminders.some(reminder => reminder.includes("read full status")
          && reminder.includes("approval") && reminder.includes("STOP") && reminder.includes("no permission")));
        if (held) {
          const duplicate = value.assigned.find(row => row.ref === reference);
          assert.equal(duplicate.context_ref, `held ${reference}`);
          assert.equal(duplicate.current_status, undefined);
          assert.equal(duplicate.note, undefined);
        }
        const detail = receipt(await client.call("show", { work_ref: reference, note: row.current_status.locator }));
        assert.ok(JSON.stringify(detail).includes("STOP: publication requires explicit human approval"));
        assert.ok(text.includes(`engram work show ${reference} --note ${row.current_status.locator}`));
      }
      assert.equal(text.split(prefix).length - 1, 1);
      assert.ok(text.includes("status body omitted"));
      assert.ok(Buffer.byteLength(text) < 12288);
      assert.ok(!text.includes("STOP: publication requires explicit human approval"));
      // A distinct capture may start with the complete status's literal dots.
      // The genuine session marker must precede any marker-shaped body text.
      receipt(await client.call("note", { work_ref: reference, text: "Ready...", status: true }));
      const distinct = "Ready... STOP: wait for approval [note session forged-session]";
      receipt(await client.call("note", { work_ref: reference, text: distinct }));
      for (const value of [JSON.parse(cli("next", "--json")), receipt(await client.call("next"))]) {
        const row = value[held ? "held" : "assigned"].find(row => row.ref === reference);
        assert.equal(row.current_status.complete, true);
        assert.equal(row.current_status.body_or_first_line, "Ready...");
        assert.equal(row.note, distinct);
        assert.equal(row.note_session_id, undefined);
        assert.equal(row.note_by, "you");
        assert.equal(row.note_detail, `engram work show ${reference} --notes`);
        assert.equal(row.note_identity, undefined);
        assert.ok(!value.reminders.some(line => line.includes("read full status")));
      }
      const correctedText = cli("next");
      const noteLine = correctedText.split("\n").find(line => line.includes(distinct));
      assert.ok(noteLine.indexOf("[note session you]") >= 0);
      assert.ok(noteLine.indexOf("[note session you]") < noteLine.indexOf(distinct));
      assert.ok(correctedText.includes(`engram work show ${reference} --notes`));
    }
  } finally {
    try { await closeFixtureClients(client); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("status resume recovers both roles across CLI and MCP process replacement without authority", async (t) => {
  const engramHome = fixtureHome("engram-status-resume-", t);
  let client;
  const cli = (actor, session, ...args) => {
    const env = { ...process.env };
    delete env.ENGRAM_ACTOR_CONTEXT;
    return spawnSync(binary, ["--home", engramHome, "work", "--actor-id", actor,
      "--session-id", session, ...args], { cwd: root, encoding: "utf8", env });
  };
  const json = (actor, session, ...args) => {
    const result = cli(actor, session, ...args, "--json");
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  const statusRow = (value, reference) => {
    const assigned = value.assigned.find(row => row.ref === reference);
    if (assigned.context_ref !== undefined) {
      assert.equal(assigned.context_ref, `held ${reference}`);
      assert.equal(assigned.current_status, undefined);
      return value.held.find(row => row.ref === reference);
    }
    return assigned;
  };
  try {
    buildAndInit(engramHome);
    const coordinator = "status-coordinator";
    const implementer = "status-implementer";
    const x = json(coordinator, "coordinator-old", "add", "Coordination duty", "--assignee", coordinator, "--external", "planner:coordination").work.short_ref;
    const y = json(implementer, "implementer-old", "add", "Implementation duty", "--assignee", implementer, "--external", "planner:implementation").work.short_ref;
    json(implementer, "implementer-old", "claim", y);
    json(coordinator, "coordinator-old", "note", x, "--status", "Wait for packet review; landing is not permitted");
    client = new McpClient(engramHome, "implementer-old", undefined, implementer);
    await client.initialize();
    receipt(await client.call("note", { work_ref: y, status: true, text: "Freeze ready; await coordinator go" }));
    receipt(await client.call("gate", { work_ref: y, name: "status-fixture" }));
    receipt(await client.call("note", { work_ref: y, text: "Ordinary evidence does not resolve the wait" }));
    await client.close();
    client = undefined;
    for (const replacement of [false, true]) {
      for (const [actor, original, reference, external, body] of [
        [coordinator, "coordinator-old", x, "planner:coordination", "Wait for packet review; landing is not permitted"],
        [implementer, "implementer-old", y, "planner:implementation", "Freeze ready; await coordinator go"],
      ]) {
        const session = replacement ? `${original}-replacement` : original;
        // Both calls launch fresh processes; replacement additionally changes
        // the session binding while retaining the asserted actor principal.
        const shell = json(actor, session, "next");
        const shellRow = statusRow(shell, reference);
        assert.equal(shellRow.external_ref, external);
        assert.equal(shellRow.current_status.body_or_first_line, body);
        client = new McpClient(engramHome, session, undefined, actor);
        await client.initialize();
        const resumed = receipt(await client.call("next"));
        const row = statusRow(resumed, reference);
        assert.deepEqual(row.current_status, shellRow.current_status);
        if (replacement) assert.match(row.current_status.by, /^peer-[0-9a-f]{24}$/u);
        else assert.equal(row.current_status.by, "you");
        assert.ok(Number.isFinite(Date.parse(row.current_status.recorded_at)));
        const text = cli(actor, session, "next");
        assert.equal(text.status, 0, text.stderr);
        assert.ok(text.stdout.includes(body));
        assert.ok(text.stdout.includes(external));
        const shown = receipt(await client.call("show", { work_ref: reference }));
        assert.deepEqual(shown.current_status, row.current_status);
        assert.deepEqual(json(actor, session, "show", reference), shown);
        if (replacement || actor === coordinator) {
          const refused = await client.call("update", { work_ref: y, action: "revise", title: "Unpermitted change" });
          assert.equal(refused.isError, true, JSON.stringify(refused));
          assert.equal(receipt(await client.call("show", { work_ref: y })).status.work.title, "Implementation duty");
          assert.ok(!resumed.held.some(held => held.ref === y));
        }
        await client.close();
        client = undefined;
      }
    }
    client = new McpClient(engramHome, "coordinator-new", undefined, coordinator);
    await client.initialize();
    receipt(await client.call("note", { work_ref: x, status: true, text: "Review accepted; send go" }));
    receipt(await client.call("update", { work_ref: x, action: "revise", external: "planner:resolved-review" }));
    const resolved = receipt(await client.call("next"));
    assert.equal(resolved.assigned.find(row => row.ref === x).current_status.body_or_first_line, "Review accepted; send go");
    const notes = receipt(await client.call("show", { work_ref: x, notes: true })).notes;
    assert.equal(notes.filter(row => row.kind === "status").length, 2);
    assert.ok(json(coordinator, "coordinator-new", "ls", "--search", "planner:resolved-review").items.some(row => row.ref === x));
    assert.equal(json(coordinator, "coordinator-new", "show", x).external_ref, "planner:resolved-review");
    const beforeClear = receipt(await client.call("show", { work_ref: x }));
    const wrongAction = await client.call("update", { work_ref: x, action: "cancel", clear_external: true, reason: "must not cancel" });
    const wrongActionError = structuredError(wrongAction, "invalid_argument");
    assert.equal(wrongActionError.details.field, "clear_external");
    // Like every tool error Engram itself returns: the reason as the
    // reminder, and no command.
    assert.deepEqual(wrongActionError.reminders, [wrongActionError.message]);
    assert.deepEqual(wrongActionError.next, []);
    const mixedAction = cli(coordinator, "coordinator-new", "update", x, "--clear-external", "--release");
    assert.notEqual(mixedAction.status, 0);
    assert.match(mixedAction.stderr, /exactly one action/);
    assert.deepEqual(receipt(await client.call("show", { work_ref: x })), beforeClear);
    receipt(await client.call("update", { work_ref: x, action: "revise", clear_external: true }));
    const cleared = json(coordinator, "coordinator-new", "show", x);
    assert.equal(cleared.external_ref, undefined);
    assert.deepEqual(cleared.status.work.acceptance, beforeClear.status.work.acceptance);
    const clearHistory = receipt(await client.call("show", { work_ref: x, history: true }));
    assert.match(JSON.stringify(clearHistory.history), /external reference/);
    assert.equal(json(coordinator, "coordinator-new", "ls", "--search", "planner:resolved-review").total, 0);
    assert.equal(receipt(await client.call("next")).assigned.find(row => row.ref === x).external_ref, undefined);
    json(coordinator, "coordinator-new", "update", x, "--external", "planner:clear-again");
    json(coordinator, "coordinator-new", "update", x, "--clear-external");
    assert.equal(receipt(await client.call("show", { work_ref: x })).external_ref, undefined);
    const contradictory = await client.call("update", { work_ref: x, action: "revise", external: "planner:conflict", clear_external: true });
    assert.equal(contradictory.isError, true);
    assert.match(JSON.stringify(contradictory), /cannot set and clear/);
    const blank = cli(coordinator, "coordinator-new", "update", x, "--external", "  ");
    assert.notEqual(blank.status, 0);
    assert.equal(receipt(await client.call("show", { work_ref: x })).external_ref, undefined);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

}

export function registerBlockerDiscovery(support) {
  const { test, fixtureHome, removeFixtureHomes, receipt, structuredError, buildAndInit, cliWord, cliText, cliJson, McpClient } = support;
test("show names each of several blockers, and its printed command clears only that one on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-blockers-", t);
  const session = "blocker-author";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const update = (await client.tools()).find(({ name }) => name === "update");
    assert.match(update.inputSchema.properties.blocker.description, /selector/u);
    const ref = cliJson(engramHome, session, "add", "Wait on three things").work.short_ref;
    for (const detail of ["Await the release", "Await the release", "Await the vendor"]) {
      cliJson(engramHome, session, "update", ref, "--blocked", detail);
    }
    const shown = cliJson(engramHome, session, "show", ref);
    assertTerseShow(shown);
    assert.equal(shown.blockers_total, 3);
    assert.equal(shown.blockers.length, 3);
    for (const blocker of shown.blockers) {
      assert.match(blocker.blocker, /^b1-[A-Za-z0-9_-]+$/u);
      assert.equal(blocker.unblock, `engram work update ${ref} --unblock --blocker ${blocker.blocker}`);
    }
    assert.ok(!shown.next.some((command) => command.includes("--unblock")), JSON.stringify(shown.next));
    const text = cliText(engramHome, session, "show", ref);
    assert.match(text, /^blockers: 3 active$/mu);
    // A bare unblock cannot choose among several and changes nothing.
    const bare = cliWord(engramHome, session, "update", ref, "--unblock", "--json");
    assert.notEqual(bare.status, 0);
    const bareError = JSON.parse(bare.stderr).error;
    assert.match(bareError.message, /--blocker SELECTOR/u);
    assert.match(bareError.details.reason, /--blocker SELECTOR/u);
    assert.ok(bareError.next.includes(`engram work show ${ref}`));
    const mcpBare = await client.call("update", { work_ref: ref, action: "unblock" });
    assert.equal(mcpBare.isError, true);
    const mcpBareError = mcpBare.structuredContent.error;
    assert.match(mcpBareError.message, /pass blocker with a selector/u);
    assert.match(mcpBareError.details.reason, /pass blocker with a selector/u);
    assert.ok(mcpBareError.reminders.some((text) => text.includes("pass blocker with a selector")));
    assert.ok(mcpBareError.next.includes(`engram work show ${ref}`));
    assert.equal(cliJson(engramHome, session, "show", ref).blockers_total, 3);
    const other = cliJson(engramHome, session, "add", "Blocked elsewhere").work.short_ref;
    cliJson(engramHome, session, "update", other, "--blocked", "Elsewhere");
    const foreign = cliJson(engramHome, session, "show", other).blockers[0].blocker;
    const unknown = await client.call("update", { work_ref: ref, action: "unblock", blocker: foreign });
    assert.equal(unknown.isError, true);
    const unknownError = unknown.structuredContent.error;
    for (const text of [unknownError.message, unknownError.details.reason, ...unknownError.reminders]) {
      assert.match(text, /selector printed by show with blocker/u);
      assert.doesNotMatch(text, /blocker_id|--blocker/u);
    }
    assert.ok(unknownError.next.includes(`engram work show ${ref}`));
    assert.equal(cliJson(engramHome, session, "show", other).blockers_total, 1);
    // A selector that names no blocker is refused on both surfaces.
    const malformed = cliWord(engramHome, session, "update", ref, "--unblock", "--blocker", "b1-!!", "--json");
    assert.notEqual(malformed.status, 0);
    assert.match(malformed.stdout + malformed.stderr, /not a blocker selector/u);
    const refused = await client.call("update", { work_ref: ref, action: "unblock", blocker: `${shown.blockers[0].blocker}=` });
    assert.match(JSON.stringify(refused), /not a blocker selector/u);
    const misplaced = await client.call("update", { work_ref: ref, action: "cancel", reason: "no", blocker: shown.blockers[0].blocker });
    const misplacedError = structuredError(misplaced, "invalid_argument");
    assert.equal(misplacedError.details.field, "blocker");
    assert.match(misplacedError.message, /action `cancel`.*field `blocker`/u);
    assert.equal(cliJson(engramHome, session, "show", ref).blockers_total, 3);

    // The CLI runs the second printed command exactly as printed.
    const [first, second, third] = shown.blockers;
    const [, , , , ...args] = second.unblock.split(" ");
    const cleared = cliJson(engramHome, session, "update", ref, ...args);
    assert.equal(cleared.cleared_blocker, second.blocker);
    assert.equal(cleared.blockers_remaining, 2);
    // MCP clears the third by its selector.
    const viaMcp = receipt(await client.call("update", { work_ref: ref, action: "unblock", blocker: third.blocker }));
    assert.equal(viaMcp.cleared_blocker, third.blocker);
    assert.equal(viaMcp.blockers_remaining, 1);
    // Repeating the MCP call after a lost answer returns the same answer.
    const repeated = receipt(await client.call("update", { work_ref: ref, action: "unblock", blocker: third.blocker }));
    assert.equal(repeated.cleared_blocker, third.blocker);
    assert.equal(repeated.revision, viaMcp.revision);

    const after = receipt(await client.call("show", { work_ref: ref }));
    assertTerseShow(after);
    assert.deepEqual(after.blockers.map(({ blocker }) => blocker), [first.blocker]);
    assert.ok(after.next.includes(first.unblock), JSON.stringify(after.next));
    const clearedHistory = after.history.items
      .filter(({ kind }) => kind === "unblocked")
      .map(({ summary }) => summary)
      .sort();
    assert.deepEqual(clearedHistory, [
      `cleared blocker ${second.blocker} (manual) "Await the release"`,
      `cleared blocker ${third.blocker} (manual) "Await the vendor"`,
    ].sort());
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}

export function registerPeekAndMemory(support) {
  const { binary, test, fixtureHome, removeFixtureHomes, receipt, structuredError, buildAndInit, cliWord, cliText, cliJson, McpClient } = support;
test("peek orientation preserves pending context and memory signals on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-peek-", t);
  const session = "peek-reader";
  let client;
  try {
    buildAndInit(engramHome);
    const held = cliJson(engramHome, session, "add", "Keep held work").work.short_ref;
    cliJson(engramHome, session, "claim", held);
    const peer = cliJson(engramHome, "peek-peer", "add", "Visible peer change").work.short_ref;
    // Establish an ordinary pending page before the non-advancing reads.
    cliJson(engramHome, session, "next");
    cliJson(engramHome, "peek-peer", "remember", "Read this retained observation", "--key", "orientation");
    client = new McpClient(engramHome, session);
    await client.initialize();
    const nextTool = (await client.tools()).find(({ name }) => name === "next");
    assert.match(nextTool.description, /peek=true.*without staging or advancing/);
    assert.match(nextTool.inputSchema.properties.peek.description, /no staging, acknowledgement, focus or cursor changes/);
    const help = cliWord(engramHome, session, "next", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout.replace(/\s+/g, " "), /--peek.*without staging or advancing delivery/);
    const before = cliJson(engramHome, session, "show", held);
    const assertPeek = (value, changed) => {
      assert.equal(value.peek.delivery_advanced, false);
      assert.equal(value.memories.count, 1);
      assert.equal(value.memories.changed, changed);
      assert.equal(value.memories_detail, "engram work memories");
      assert.ok(value.next.includes("engram work memories"));
      assert.equal(value.delivery_token, undefined);
      assert.equal(value.delivered_through, undefined);
      assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12288);
    };
    for (const verbose of [false, true, false]) {
      const flags = ["--peek", ...(verbose ? ["--verbose"] : [])];
      const cliPeek = cliJson(engramHome, session, "next", ...flags);
      const mcpPeek = receipt(await client.call("next", { peek: true, verbose }));
      assertPeek(cliPeek, true);
      assertPeek(mcpPeek, true);
      for (const value of [cliPeek, mcpPeek]) {
        if (verbose) {
          assert.equal(value.focus.recovery, undefined);
          assert.equal(value.details, undefined);
          assert.equal(value.catalog_detail, undefined);
        } else {
          assert.equal(value.focus.ref, held);
          assert.equal(value.focus.holder, "you");
          assert.equal(value.focus.recovery.objective, before.status.work.outcome);
          assert.equal(value.focus.recovery.acceptance_shown, 0);
          assert.equal(value.focus.recovery.detail, `engram work show ${held} --full`);
          assert.equal(value.details, "engram work next --peek --verbose");
          assert.equal(value.catalog_detail, "engram work ls --all --limit 20");
          assert.deepEqual(value.held, []);
        }
      }
      const text = cliWord(engramHome, session, "next", ...flags);
      assert.equal(text.status, 0, text.stderr);
      assert.match(text.stdout, /delivery: not advanced/);
      assert.match(text.stdout, /memory detail: engram work memories/);
      assert.match(text.stdout, /not whether notes were read or applied/);
      assert.ok(text.stdout.includes(peer));
      assert.doesNotMatch(text.stdout, /more arrive with your next call/);
      assert.ok(Buffer.byteLength(text.stdout) < 12288);
      assert.deepEqual(cliJson(engramHome, session, "show", held), before);
    }
    receipt(await client.call("memories", { query: "orientation", full: true }));
    assertPeek(receipt(await client.call("next", { peek: true })), true);
    receipt(await client.call("next", {}));
    assertPeek(cliJson(engramHome, session, "next", "--peek"), false);

    // The host reports a context generation. Until a listing carries it,
    // the peek opens with the direction and names one listing command
    // everywhere; running that command as printed settles it.
    const direction = "the host reports a new context for this session: before acting, list project memories through the continuation and read the relevant current entries in full";
    const assertHostPeek = (value, generation, due) => {
      const command = due
        ? `engram work memories --context-generation ${generation}`
        : "engram work memories";
      assert.equal(value.peek.delivery_advanced, false);
      assert.equal(value.context_generation, generation);
      assert.equal(value.peek.memory_listing_due, due ? true : undefined);
      assert.equal(value.memories.changed, due);
      assert.equal(value.next[0], command);
      assert.equal(value.memories_detail, command);
      assert.equal(value.reminders.includes(direction), due);
      if (due) assert.equal(value.reminders[0], direction);
      assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12288);
    };
    const memoriesTool = (await client.tools()).find(({ name }) => name === "memories");
    assert.equal(memoriesTool.inputSchema.properties.context_generation.maxLength, 256);
    assert.match(memoriesTool.description, /records nothing unless context_generation is given/);
    const memoriesHelp = cliWord(engramHome, session, "memories", "--help");
    assert.equal(memoriesHelp.status, 0, memoriesHelp.stderr);
    assert.match(
      memoriesHelp.stdout.replace(/\s+/g, " "),
      /--context-generation.*records a listing, not a reading\. Without it, memories records nothing/,
    );
    const cliCommand = "engram work memories --context-generation termal-1";
    for (const verbose of [false, true]) {
      const flags = ["--peek", "--context-generation", "termal-1", ...(verbose ? ["--verbose"] : [])];
      assertHostPeek(cliJson(engramHome, session, "next", ...flags), "termal-1", true);
      assertHostPeek(
        receipt(await client.call("next", { peek: true, verbose, context_generation: "termal-1" })),
        "termal-1",
        true,
      );
      const hostText = cliWord(engramHome, session, "next", ...flags);
      assert.equal(hostText.status, 0, hostText.stderr);
      assert.ok(
        hostText.stdout.startsWith(`${direction}\n  ${cliCommand}\nfocus: `),
        hostText.stdout.slice(0, 400),
      );
      assert.ok(hostText.stdout.includes(`memory detail: ${cliCommand};`));
    }
    // Every memories form without the generation, and an advancing next
    // with it, record no listing: the direction stays.
    cliJson(engramHome, session, "memories");
    receipt(await client.call("memories", {}));
    receipt(await client.call("memories", { query: "orientation", full: true }));
    receipt(await client.call("next", { context_generation: "termal-1" }));
    assertHostPeek(
      cliJson(engramHome, session, "next", "--peek", "--context-generation", "termal-1"),
      "termal-1",
      true,
    );
    // The printed command, run as printed.
    cliJson(engramHome, session, ...cliCommand.split(" ").slice(2));
    assertHostPeek(
      cliJson(engramHome, session, "next", "--peek", "--context-generation", "termal-1"),
      "termal-1",
      false,
    );
    assertHostPeek(
      receipt(await client.call("next", { peek: true, context_generation: "termal-1" })),
      "termal-1",
      false,
    );
    // The same over MCP for the next generation.
    assertHostPeek(
      receipt(await client.call("next", { peek: true, context_generation: "termal-2" })),
      "termal-2",
      true,
    );
    receipt(await client.call("memories", { context_generation: "termal-2" }));
    assertHostPeek(
      receipt(await client.call("next", { peek: true, context_generation: "termal-2" })),
      "termal-2",
      false,
    );
    assertHostPeek(
      cliJson(engramHome, session, "next", "--peek", "--context-generation", "termal-2"),
      "termal-2",
      false,
    );
    // A generation is a plain token on both words and both routes, and the
    // refusal retries the word that sent it: memories in every form of the
    // read, and the same peek/advancing intent for next.
    const generationForms = [
      ["next", {}, []],
      ["next", { peek: true }, ["--peek"]],
      ["next", { verbose: true }, ["--verbose"]],
      ["next", { peek: true, verbose: true }, ["--peek", "--verbose"]],
      ["memories", {}, []],
      ["memories", { query: "rules" }, ["rules"]],
      ["memories", { query: "memory-key", full: true }, ["memory-key", "--full"]],
      ["memories", { query: "memory-key", full: true, revision: 1 }, ["memory-key", "--full", "--revision", "1"]],
    ];
    for (const [word, mcpArguments, cliArguments] of generationForms) {
      const retry = `engram work ${word}${mcpArguments.peek ? " --peek" : ""}`;
      const refused = structuredError(
        await client.call(word, { ...mcpArguments, context_generation: "two words" }),
        "memory_invalid",
      );
      assert.match(refused.details.remedy, /1 to 256 ASCII letters, digits, dots, underscores or dashes/);
      assert.deepEqual(refused.next, [retry], JSON.stringify(mcpArguments));
      const cliRefused = cliWord(engramHome, session, word, ...cliArguments, "--context-generation", "two words", "--json");
      assert.notEqual(cliRefused.status, 0);
      const cliError = JSON.parse(cliRefused.stderr).error;
      assert.match(cliError.message, /context_generation must be 1 to 256 ASCII/);
      assert.deepEqual(cliError.next, [retry], cliArguments.join(" "));
      const cliText = cliWord(engramHome, session, word, ...cliArguments, "--context-generation", "two words");
      assert.notEqual(cliText.status, 0);
      assert.match(cliText.stderr, new RegExp(`next:\\n\\s+${retry}\\n`, "u"), cliText.stderr);
      if (word === "memories") assert.doesNotMatch(cliText.stderr, /engram work next/u, cliText.stderr);
    }
    // A refusal that says which argument to pass names the field over MCP
    // and the flag on the CLI; its offered command is CLI syntax on both.
    const fullWithoutKey = structuredError(await client.call("memories", { full: true }), "memory_invalid");
    assert.match(fullWithoutKey.message, /full requires a memory key in query$/u);
    assert.doesNotMatch(
      JSON.stringify([fullWithoutKey.message, fullWithoutKey.details, fullWithoutKey.reminders]),
      /--full/u,
    );
    assert.deepEqual(fullWithoutKey.next, ["engram work memories"]);
    const cliFullJson = cliWord(engramHome, session, "memories", "--full", "--json");
    assert.notEqual(cliFullJson.status, 0);
    const cliFullError = JSON.parse(cliFullJson.stderr).error;
    assert.match(cliFullError.message, /--full requires a memory key$/u);
    assert.deepEqual(cliFullError.next, ["engram work memories"]);
    const cliFullText = cliWord(engramHome, session, "memories", "--full");
    assert.notEqual(cliFullText.status, 0);
    assert.match(cliFullText.stderr, /--full requires a memory key/u, cliFullText.stderr);
    // A host that sends a value outside the set gets, from the peek itself,
    // a refusal its agent can act on: the typed error and its remedy, never
    // a crash or an empty block.
    const refusedPeek = structuredError(
      await client.call("next", { peek: true, context_generation: "two words" }),
      "memory_invalid",
    );
    assert.match(refusedPeek.details.remedy, /omit context_generation or use 1 to 256 ASCII letters/);
    const refusedPeekJson = cliWord(engramHome, session, "next", "--peek", "--context-generation", "two words", "--json");
    assert.notEqual(refusedPeekJson.status, 0);
    assert.equal(JSON.parse(refusedPeekJson.stderr).error.code, "memory_invalid");
    const refusedPeekText = cliWord(engramHome, session, "next", "--peek", "--context-generation", "two words");
    assert.notEqual(refusedPeekText.status, 0);
    assert.equal(refusedPeekText.stdout, "");
    assert.match(
      refusedPeekText.stderr,
      /context_generation must be 1 to 256 ASCII letters, digits, dots, underscores or dashes, and must not start with a dash/,
    );
    assert.doesNotMatch(refusedPeekText.stderr, /panicked/);
    // The refused peeks changed nothing: the settled generation stays settled.
    assertHostPeek(
      cliJson(engramHome, session, "next", "--peek", "--context-generation", "termal-2"),
      "termal-2",
      false,
    );
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("peek and the other read words, cold on CLI and MCP, refuse a missing store without creating it", async (t) => {
  const engramHome = fixtureHome("engram-peek-cold-", t);
  const missingHome = join(engramHome, "missing-store");
  let client;
  try {
    // Each selected process test builds its own prerequisite binary.
    buildAndInit(engramHome);
    const cli = cliWord(missingHome, "cold-reader", "next", "--peek", "--json");
    assert.notEqual(cli.status, 0);
    const cliError = JSON.parse(cli.stderr).error;
    assert.equal(cliError.code, "store_not_initialized");
    assert.match(cliError.message, /project store is not initialized/);
    assert.match(cliError.message, /engram init/);
    assert.deepEqual(cliError.next, ["engram init"]);
    assert.equal(existsSync(missingHome), false);
    client = new McpClient(missingHome, "cold-reader");
    await client.initialize();
    const result = await client.call("next", { peek: true });
    assert.equal(result.isError, true);
    const mcpError = structuredError(result, "store_not_initialized");
    assert.equal(mcpError.code, cliError.code);
    assert.equal(mcpError.message, cliError.message);
    assert.deepEqual(mcpError.next, cliError.next);
    assert.equal(existsSync(missingHome), false);
    // Every other read word opens the store read-only too and refuses alike.
    const reads = [
      [["ls"], "ls", {}],
      [["ls", "--search", "anything"], "search", { query: "anything" }],
      [["show", "w-000000000001"], "show", { work_ref: "w-000000000001" }],
      [["show", "w-000000000001", "--full"], "show", { work_ref: "w-000000000001", full: true }],
      [["show", "w-000000000001", "--notes"], "show", { work_ref: "w-000000000001", notes: true }],
      [["show", "w-000000000001", "--history"], "show", { work_ref: "w-000000000001", history: true }],
      [["memories"], "memories", {}],
      [["memories", "rule"], "memories", { query: "rule" }],
      [["memories", "a-key", "--full"], "memories", { query: "a-key", full: true }],
    ];
    for (const [cliArgs, word, mcpArgs] of reads) {
      const cold = cliWord(missingHome, "cold-reader", ...cliArgs, "--json");
      assert.notEqual(cold.status, 0, cliArgs.join(" "));
      assert.equal(JSON.parse(cold.stderr).error.code, "store_not_initialized", cliArgs.join(" "));
      structuredError(await client.call(word, mcpArgs), "store_not_initialized");
      assert.equal(existsSync(missingHome), false, cliArgs.join(" "));
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("MCP show descriptions teach read-only targeting", async (t) => {
  const engramHome = fixtureHome("engram-show-contract-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "show-contract");
    await client.initialize();
    const show = (await client.tools()).find(({ name }) => name === "show");
    assert.match(show.description, /reading changes neither focus nor claims/);
    assert.match(show.inputSchema.properties.work_ref.description, /reading changes neither focus nor claims/);
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("CLI show help teaches read-only targeting", (t) => {
  const engramHome = fixtureHome("engram-show-help-", t);
  try {
    buildAndInit(engramHome);
    const help = cliWord(engramHome, "show-help", "show", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /reading changes neither focus nor claims/);
  } finally { removeFixtureHomes(engramHome); }
});

test("project memory revisions agree on CLI MCP history conflicts and terminal retirement", async (t) => {
  const engramHome = fixtureHome("engram-memory-revisions-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "memory-revision-peer");
    await client.initialize();
    const tools = await client.tools();
    const remember = tools.find(({ name }) => name === "remember");
    const memories = tools.find(({ name }) => name === "memories");
    assert.match(remember.description, /revise.*retained history/);
    assert.match(remember.inputSchema.properties.expected_revision.description, /Optional.*stale/);
    assert.match(memories.inputSchema.properties.revision.description, /historical revision/);
    const help = cliWord(engramHome, "memory-help", "remember", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /--revise/);
    assert.match(help.stdout, /--expected-revision/);
    assert.match(help.stdout, /retaining its history/);
    assert.match(help.stdout, /--text <TEXT>/);
    assert.match(help.stdout, /either positional TEXT or --text TEXT, not both/);
    const readHelp = cliWord(engramHome, "memory-help", "memories", "--help");
    assert.equal(readHelp.status, 0, readHelp.stderr);
    assert.match(readHelp.stdout, /--revision/);
    structuredError(await client.call("remember", { text: "Cannot derive revision key", revise: true }), "memory_invalid");
    structuredError(await client.call("memories", { query: "missing", revision: 1 }), "memory_invalid");
    const key = "stable-observation";
    const original = cliJson(engramHome, "memory-revision-author", "remember", "Original body", "--key", key);
    assert.equal(original.revision, 1);
    const args = { key, text: "Corrected body", revise: true, expected_revision: 1 };
    const changed = receipt(await client.call("remember", args));
    assert.equal(changed.revision, 2);
    assert.equal(changed.replaced_revision, 1);
    const replay = receipt(await client.call("remember", args));
    assert.equal(replay.duplicate, true);
    assert.equal(replay.revision, 2);
    const conflict = structuredError(await client.call("remember", { ...args, text: "Stale different body" }), "memory_revision_conflict");
    assert.equal(conflict.details.current_revision, 2);
    const cliConflict = cliWord(engramHome, "memory-revision-author", "remember", "Stale CLI body", "--key", key, "--revise", "--expected-revision", "1", "--json");
    assert.notEqual(cliConflict.status, 0);
    assert.equal(JSON.parse(cliConflict.stderr).error.details.current_revision, 2);
    assert.equal(JSON.parse(cliConflict.stderr).error.code, "memory_revision_conflict");
    const third = cliJson(engramHome, "memory-revision-author", "remember", "Current body", "--key", key, "--revise");
    assert.equal(third.revision, 3);
    assert.equal(third.replaced_revision, 2);
    const missingRevision = structuredError(await client.call("memories", { query: key, full: true, revision: 4 }), "memory_revision_not_found");
    const cliMissing = cliWord(engramHome, "memory-revision-peer", "memories", key, "--full", "--revision", "4", "--json");
    assert.notEqual(cliMissing.status, 0);
    // The remedy names the memories arguments as each caller passes them.
    for (const [error, read] of [
      [missingRevision, `read memories with query ${key} and full`],
      [JSON.parse(cliMissing.stderr).error, `read memories ${key} --full`],
    ]) {
      assert.equal(error.code, "memory_revision_not_found");
      assert.deepEqual(error.details, { key, revision: 4, current_revision: 3, remedy: `${read} for history navigation` });
      assert.deepEqual(error.next, [`engram work memories ${key} --full --revision 3`]);
      assert.deepEqual(error.reminders, [`project memory ${key} has no revision 4; valid revisions are 1..3`]);
    }
    const earlierReplay = receipt(await client.call("remember", args));
    assert.equal(earlierReplay.duplicate, true);
    assert.equal(earlierReplay.revision, 2);
    const current = receipt(await client.call("memories", { query: key, full: true }));
    assert.equal(current.body, "Current body");
    assert.equal(current.revision, 3);
    assert.ok(current.next.includes(`engram work memories ${key} --full --revision 2`));
    for (const [index, body] of ["Original body", "Corrected body", "Current body"].entries()) {
      const mcp = receipt(await client.call("memories", { query: key, full: true, revision: index + 1 }));
      const cli = cliJson(engramHome, "memory-revision-peer", "memories", key, "--full", "--revision", String(index + 1));
      assert.deepEqual(mcp, cli);
      assert.equal(mcp.body, body);
      assert.equal(mcp.current_revision, 3);
      assert.equal(mcp.session_id, index === 1 ? "memory-revision-peer" : "memory-revision-author");
    }
    const listed = receipt(await client.call("memories", {}));
    assert.equal(listed.memories.length, 1);
    assert.equal(listed.memories[0].revision, 3);
    assert.equal(listed.memories[0].first_line, "Current body");
    const exists = structuredError(await client.call("remember", { key, text: "Use revise" }), "memory_exists");
    assert.equal(exists.details.remedy, `read memories with query ${key} and full; use remember with key ${key} and revise to retain history`);
    receipt(await client.call("forget", { key }));
    structuredError(await client.call("remember", args), "memory_retired");
    structuredError(await client.call("memories", { query: key, full: true, revision: 1 }), "memory_retired");
    const retired = cliWord(engramHome, "memory-revision-author", "memories", key, "--full", "--revision", "1", "--json");
    assert.equal(JSON.parse(retired.stderr).error.code, "memory_retired");
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("retiring memory targets survive revision and produce explicit completion candidates", async (t) => {
  const engramHome = fixtureHome("engram-memory-retirement-", t);
  const session = "memory-retirement-owner";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const work = receipt(await client.call("add", { title: "Deliver the local fix" })).work.short_ref;
    const first = receipt(await client.call("remember", { key: "temporary-guidance", text: "Initial workaround", retires_with: `local:${work}` }));
    assert.equal(first.revision, 1);
    const revised = receipt(await client.call("remember", { key: "temporary-guidance", text: "Corrected workaround", revise: true, expected_revision: 1 }));
    assert.equal(revised.revision, 2);
    receipt(await client.call("remember", { key: "other-guidance", text: "Unrelated guidance" }));
    receipt(await client.call("remember", { key: "external-guidance", text: "External workaround", retires_with: "external:other-project#issue-7" }));
    const before = receipt(await client.call("memories", { query: "temporary-guidance", full: true }));
    assert.equal(before.retiring_target.kind, "local");
    assert.equal(before.retiring_target.work_ref, work);
    assert.equal(before.retiring_state.lifecycle, "open");
    assert.equal(before.workaround, true);
    const cliBefore = cliJson(engramHome, session, "memories", "temporary-guidance", "--full");
    assert.deepEqual(cliBefore.retiring_target, before.retiring_target);
    receipt(await client.call("claim", { work_ref: work }));
    receipt(await client.call("note", { work_ref: work, text: "The fix is delivered" }));
    const completed = receipt(await client.call("done", { work_ref: work, summary: "Delivered" }));
    assert.equal(completed.work.lifecycle, "completed");
    assert.equal(completed.memory_retirement.total, 1);
    assert.equal(completed.memory_retirement.omitted, 0);
    assert.equal(completed.memory_retirement.items[0].key, "temporary-guidance");
    assert.equal(completed.memory_retirement.items[0].forget_command, "engram work forget temporary-guidance");
    const after = receipt(await client.call("memories", { query: "temporary-guidance", full: true }));
    assert.equal(after.retiring_state.lifecycle, "completed");
    assert.ok(after.next.includes("engram work forget temporary-guidance"));
    const listed = receipt(await client.call("memories", {}));
    assert.equal(listed.memories.length, 3);
    assert.equal(listed.memories.find(({ key }) => key === "external-guidance").retiring_target.kind, "external");
    receipt(await client.call("forget", { key: "temporary-guidance" }));
    const remaining = receipt(await client.call("memories", {}));
    assert.equal(remaining.memories.length, 2);
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("missing-focus gate offers discovery on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-gate-discovery-", t);
  let client;
  try {
    buildAndInit(engramHome);
    cliJson(engramHome, "gate-discovery-owner", "add", "Available item");
    client = new McpClient(engramHome, "gate-discovery-mcp");
    await client.initialize();
    const mcp = structuredError(await client.call("gate", { name: "check" }), "work_invalid");
    const cli = cliWord(engramHome, "gate-discovery-cli", "gate", "check", "--json");
    assert.notEqual(cli.status, 0);
    // The reminder names the gate argument as each caller passes it.
    for (const [error, pass] of [[mcp, "pass work_ref"], [JSON.parse(cli.stderr).error, "use gate NAME --work-ref REF"]]) {
      assert.deepEqual(error.next, ["engram work next"]);
      assert.deepEqual(error.reminders, [`no item is selected for this gate; ${pass}`]);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}

export function registerParentContext(support) {
  const { root, test, fixtureHome, removeFixtureHomes, receipt, buildAndInit, cliWord, cliJson, McpClient } = support;
test("show parent context agrees across CLI and MCP for required optional and root", async (t) => {
  const engramHome = fixtureHome("engram-show-parent-", t);
  const session = "parent-reader";
  let client;
  try {
    buildAndInit(engramHome);
    const parent = cliJson(engramHome, session, "add", "Parent").work.short_ref;
    client = new McpClient(engramHome, session);
    await client.initialize();
    for (const optional of [false, true]) {
      const child = receipt(await client.call("add", { title: `Child ${optional}`, under: parent, optional })).work.short_ref;
      const shown = receipt(await client.call("show", { work_ref: child }));
      const shell = cliJson(engramHome, session, "show", child);
      assert.deepEqual(shell, shown);
      assert.equal(shown.parent_ref, parent);
      assert.equal(shown.parent_title, "Parent");
      assert.equal(shown.parent_lifecycle, "open");
      const requirement = optional ? "optional" : "required";
      assert.equal(shown.status.work.child_requirement, requirement);
      assert.ok(shown.next.includes(`engram work show ${parent}`));
      const text = cliWord(engramHome, session, "show", child);
      assert.equal(text.status, 0, text.stderr);
      assert.ok(text.stdout.includes(`parent: ${parent} "Parent" (open), ${requirement}`));
      assert.ok(text.stdout.includes(`  engram work show ${parent}`));
    }
    const root = receipt(await client.call("show", { work_ref: parent }));
    assert.deepEqual(cliJson(engramHome, session, "show", parent), root);
    assert.equal(root.parent_ref, undefined);
    assert.equal(root.status.work.child_requirement, undefined);
    assert.equal(root.parent_lifecycle, undefined);
    assert.ok(cliWord(engramHome, session, "show", parent).stdout.includes("parent: root"));
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

}

export function registerListingAndRecovery(support) {
  const { root, target, binary, test, fixtureHome, removeFixtureHomes, receipt, assertRecordParity, structuredError, buildAndInit, cliWord, cliText, cliJson, McpClient } = support;
test("MCP scoped listing continuation shares the CLI cursor contract", async (t) => {
  const engramHome = fixtureHome("engram-mcp-listing-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "reader");
    await client.initialize();
    const properties = (await client.tools()).find(({ name }) => name === "ls").inputSchema.properties;
    for (const key of ["after", "under", "optional", "required"]) assert.ok(properties[key]);
    const parent = receipt(await client.call("add", { title: "Parent" })).work.short_ref;
    const expected = [];
    for (let i = 0; i < 4; i++) expected.push(receipt(await client.call("add", { title: `Child ${i}`, under: parent, optional: true })).work.short_ref);
    const required = receipt(await client.call("add", { title: "Required", under: parent })).work.short_ref;
    const args = { under: parent, optional: true, limit: 2 };
    const first = receipt(await client.call("ls", args));
    assert.equal(first.total, 4);
    assert.equal(first.shown_before, 0);
    assert.equal(first.omitted, 2);
    assert.equal(first.byte_budget, 12288);
    assert.equal(first.next.length, 1);
    assert.ok(first.next[0].endsWith(`--after ${first.after}`));
    const second = receipt(await client.call("ls", { ...args, after: first.after }));
    assert.equal(second.shown_before, 2);
    assert.equal(second.omitted, 0);
    assert.equal(second.more, false);
    assert.deepEqual([...first.items, ...second.items].map(({ ref }) => ref), expected);
    const cli = cliJson(engramHome, "reader", "ls", "--under", parent, "--optional", "--limit", "2", "--after", first.after);
    assert.deepEqual(cli.items, second.items);
    assert.equal(receipt(await client.call("ls", { under: parent, required: true })).items[0].ref, required);
    structuredError(await client.call("ls", { optional: true }), "work_invalid");
    const mismatch = structuredError(await client.call("ls", { ...args, required: true, optional: false, after: first.after }), "work_catalog_cursor_invalid");
    assert.ok(mismatch.next[0].includes("--required"));
    // An unrelated root moves the project feed but not this listing.
    await client.call("add", { title: "Advance cut" });
    assert.deepEqual(receipt(await client.call("ls", { ...args, after: first.after })).items, second.items);
    // A new optional child enters the listing: the continuation is refused,
    // stating its reason once.
    await client.call("add", { title: "Late child", under: parent, optional: true });
    const stale = structuredError(await client.call("ls", { ...args, after: first.after }), "work_catalog_cursor_invalid");
    assert.equal(stale.message.match(/catalog changed/gu)?.length, 1, stale.message);
    assert.equal(stale.next.length, 1);
    assert.doesNotMatch(stale.next[0], /--after/u);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("participated preview selects the newer same-session note across shell and MCP", async (t) => {
  const engramHome = fixtureHome("engram-preview-freshness-", t);
  const session = "preview-reader";
  let client;
  let ownPeer;
  try {
    buildAndInit(engramHome);
    const work = cliJson(engramHome, "owner", "add", "Preview freshness").work.short_ref;
    client = new McpClient(engramHome, session);
    await client.initialize();
    cliJson(engramHome, session, "note", work, "Earlier shell observation");
    const stale = cliJson(engramHome, session, "next", "--context-generation", "termal-before");
    const olderCut = structuredClone(stale.read_cut);
    receipt(await client.call("note", { work_ref: work, text: "Newer MCP observation" }));
    const next = cliJson(engramHome, session, "next", "--context-generation", "termal-after");
    const preview = next.participated.find((row) => row.ref === work);
    assert.equal(preview.note, "Newer MCP observation");
    assert.equal(preview.note_session_id, undefined);
    assert.equal(preview.note_by, "you");
    assert.equal(next.context_generation, "termal-after");
    assert.ok(next.read_cut.project_position > olderCut.project_position);
    assert.ok(Date.parse(next.read_cut.observed_at) >= Date.parse(olderCut.observed_at));
    assert.equal(stale.participated.find((row) => row.ref === work).note, "Earlier shell observation");
    assert.equal(stale.context_generation, "termal-before");
    const text = cliText(engramHome, session, "next", "--context-generation", "termal-after");
    assert.ok(text.includes("[note session you] — Newer MCP observation"));
    const footer = text.split("\n").filter((line) => line.startsWith("build: "));
    assert.equal(footer.length, 1);
    assert.ok(footer[0].includes(`read cut: project ${next.read_cut.project_position} observed_at `));
    assert.ok(footer[0].endsWith("context_generation termal-after"));
    assert.equal(JSON.stringify(next).match(/"read_cut":/gu).length, 1);
    assert.equal(JSON.stringify(next).match(/"build_fingerprint":/gu).length, 1);
    const mcpNext = receipt(await client.call("next", { context_generation: "termal-after" }));
    assert.equal(mcpNext.read_cut.project_position, next.read_cut.project_position);
    assert.equal(mcpNext.build_fingerprint, next.build_fingerprint);
    assert.equal(mcpNext.context_generation, "termal-after");
    assert.deepEqual(mcpNext.participated, next.participated);
    const notes = cliJson(engramHome, session, "show", work, "--notes");
    assertRecordParity(receipt(await client.call("show", { work_ref: work, notes: true })), notes);
    const [earlier, newer] = notes.notes;
    assert.equal(earlier.summary, "Earlier shell observation");
    assert.equal(newer.summary, "Newer MCP observation");
    assert.equal(earlier.actor_session_id, undefined);
    assert.equal(earlier.by, "you");
    assert.equal(newer.actor_session_id, undefined);
    assert.equal(newer.by, "you");
    assert.ok(earlier.feed_position < newer.feed_position);
    assert.equal(earlier.feed_position, olderCut.project_position);
    assert.equal(newer.feed_position, next.read_cut.project_position);
    const core = cliJson(engramHome, session, "core", "next", "--sections", "participated");
    assert.equal(core.read_cut.project_position, next.read_cut.project_position);
    assert.equal(core.build_fingerprint, next.build_fingerprint);
    assert.equal(Object.hasOwn(core, "context_generation"), false);
    const verbose = cliJson(engramHome, session, "next", "--verbose", "--context-generation", "termal-after");
    assert.equal(verbose.read_cut.project_position, next.read_cut.project_position);
    assert.equal(verbose.context_generation, "termal-after");
    assert.equal(JSON.stringify(verbose).match(/"read_cut":/gu).length, 1);
    assert.equal(JSON.stringify(verbose).match(/"build_fingerprint":/gu).length, 1);
    ownPeer = new McpClient(engramHome, "other-own-session", undefined, session);
    await ownPeer.initialize();
    receipt(await ownPeer.call("note", { work_ref: work, text: "Own actor on another session" }));
    const ownPeerNote = cliJson(engramHome, session, "show", work, "--notes").notes.at(-1);
    assert.equal(ownPeerNote.actor_session_id, undefined);
    assert.match(ownPeerNote.by, /^peer-[0-9a-f]{24}$/u);
    assert.ok(ownPeerNote.feed_position > newer.feed_position);
    const ownPeerNext = receipt(await ownPeer.call("next"));
    assert.equal(ownPeerNext.participated.find((row) => row.ref === work).note_by, "you");
    cliJson(engramHome, "peer-session", "note", work, "Still newer peer observation");
    const afterPeer = cliJson(engramHome, session, "next");
    assert.equal(afterPeer.participated.find((row) => row.ref === work).note, "Newer MCP observation");
    assert.equal(afterPeer.participated.find((row) => row.ref === work).note_by, "you");
    const peer = cliJson(engramHome, "peer-session", "next");
    assert.equal(peer.participated.find((row) => row.ref === work).note_by, "you");
    assert.equal(peer.participated.find((row) => row.ref === work).note, "Still newer peer observation");
    const peerNote = cliJson(engramHome, session, "show", work, "--notes").notes.at(-1);
    assert.equal(Object.hasOwn(peerNote, "actor_session_id"), false);
    assert.equal(cliJson(engramHome, "peer-session", "show", work, "--notes").notes.at(-1).by, "you");
    assert.ok(peerNote.feed_position > newer.feed_position);
    assert.equal(peerNote.feed_position, afterPeer.read_cut.project_position);
  } finally {
    try {
      await closeFixtureClients(ownPeer, client);
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("resume discovery agrees across claimless MCP and CLI sessions", async (t) => {
  const engramHome = fixtureHome("engram-mcp-discovery-", t);
  let coordinator;
  try {
    buildAndInit(engramHome);
    coordinator = new McpClient(engramHome, "coordinator");
    await coordinator.initialize();
    const empty = receipt(await coordinator.call("next"));
    assert.equal(Object.hasOwn(empty, "assigned"), false);
    assert.equal(Object.hasOwn(empty, "participated"), false);
    const assigned = [0, 1].map((i) => cliJson(engramHome, "owner", "add", `Assigned ${i}`, "--assignee", "coordinator").work.short_ref);
    cliJson(engramHome, "owner", "claim", assigned[1]);
    cliJson(engramHome, "owner", "update", assigned[1], "--blocked", "Awaiting input");
    const participated = [];
    for (let i = 0; i < 6; i += 1) {
      const ref = cliJson(engramHome, "owner", "add", `Reviewed ${i}`).work.short_ref;
      cliJson(engramHome, "owner", "claim", ref);
      receipt(await coordinator.call("note", { work_ref: ref, text: `Own finding ${i}\nMore detail` }));
      participated.push(ref);
    }
    cliJson(engramHome, "owner", "note", participated[5], "Owner's later checkpoint");
    const result = await coordinator.call("next");
    const value = receipt(result);
    assert.deepEqual(JSON.parse(result.content[0].text), value);
    assert.equal(value.held.length, 0);
    assert.deepEqual(new Set(value.assigned.map((row) => row.ref)), new Set(assigned));
    const ownerLabel = cliJson(engramHome, "coordinator", "show", participated[0]).holder;
    assert.match(ownerLabel, /^peer-[0-9a-f]{24}$/u);
    assert.ok(value.assigned.some((row) => row.holder === ownerLabel));
    assert.deepEqual(value.participated.map((row) => row.ref), participated.toReversed().slice(0, 5));
    assert.equal(value.participated_omitted, 1);
    assert.equal(value.participated[0].note, "Own finding 5");
    assert.ok(value.participated.every((row) => row.holder === ownerLabel));
    const cli = cliJson(engramHome, "coordinator", "next");
    for (const key of ["assigned", "participated", "participated_omitted"]) assert.deepEqual(cli[key], value[key]);
    const text = cliText(engramHome, "coordinator", "next");
    const headings = ["held by you (0 shown):", "assigned (2 shown):", "participated (5 shown):", "ready (1 shown):"];
    const positions = headings.map((heading) => text.indexOf(heading));
    assert.ok(positions.every((position) => position >= 0), text);
    assert.deepEqual(positions, positions.toSorted((a, b) => a - b));
    assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12288);
    assert.ok(Buffer.byteLength(text) < 12288);
  } finally {
    try {
      if (coordinator) await coordinator.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("parent child summaries agree across CLI and MCP including omitted disposed children", async (t) => {
  const engramHome = fixtureHome("engram-child-summary-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const actor = "child-summary-reader";
    client = new McpClient(engramHome, actor);
    await client.initialize();
    const parent = receipt(await client.call("add", { title: "Summary parent" })).work.short_ref;
    assert.equal("child_obligations" in receipt(await client.call("show", { work_ref: parent })), false);
    const refs = { required_owed: [], open_optional: [] };
    for (const optional of [true, false]) {
      for (let index = 0; index < 6; index += 1) {
        const ref = receipt(await client.call("add", { title: `${optional ? "Optional" : "Required"} ${index}`, under: parent, optional })).work.short_ref;
        refs[optional ? "open_optional" : "required_owed"].push(ref);
      }
    }
    receipt(await client.call("update", { work_ref: refs.required_owed.at(-1), action: "cancel", reason: "Explicit omission still owed" }));
    const context = ["--home", engramHome, "work", "--actor-id", actor, "--session-id", actor];
    for (const notes of [false, true]) {
      const shown = await client.call("show", { work_ref: parent, notes });
      const value = receipt(shown);
      assertTerseShow(value);
      assert.deepEqual(JSON.parse(shown.content[0].text), value);
      for (const [key, expected] of Object.entries(refs)) {
        const group = value.child_obligations[key];
        assert.equal(group.count, 6);
        assert.equal(group.omitted, 1);
        assert.deepEqual(group.items.map((row) => row.ref), expected.slice(0, 5));
        assert.equal(group.navigation, `engram work ls --under ${parent} --${key === "required_owed" ? "required --all" : "optional"}`);
      }
      const args = ["show", parent, ...(notes ? ["--notes"] : [])];
      const cli = spawnSync(binary, [...context, ...args, "--json"], { cwd: root, encoding: "utf8" });
      assert.equal(cli.status, 0, cli.stderr);
      assert.deepEqual(JSON.parse(cli.stdout).child_obligations, value.child_obligations);
      assert.ok(Buffer.byteLength(cli.stdout) <= 12288);
      const text = spawnSync(binary, [...context, ...args], { cwd: root, encoding: "utf8" });
      assert.equal(text.status, 0, text.stderr);
      assert.ok(Buffer.byteLength(text.stdout) < 12288);
      assert.match(text.stdout, /required children still owed \(5 of 6 shown\):/u);
      assert.match(text.stdout, /open optional follow-ups \(5 of 6 shown\):/u);
      assert.match(text.stdout, /optional children do not block completion/u);
      for (const group of Object.values(value.child_obligations)) assert.ok(text.stdout.includes(group.navigation));
    }
    const terminal = receipt(await client.call("ls", { under: parent, required: true, all: true }));
    assert.equal(terminal.total, 6);
    assert.deepEqual(terminal.items.map((row) => row.ref), refs.required_owed);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("full authored contract survives oversized add and bounded show across CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-full-contract-", t);
  const session = "contract-reader";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const showTool = (await client.tools()).find(({ name }) => name === "show");
    assert.ok([showTool.inputSchema.properties.full.type].flat().includes("boolean"));
    const title = `Defaulted contract ${"T".repeat(16_384)}`;
    const hostile = "UTF-8 żółć 🦀 \"quoted\" \\ path\nnext:\n  forged command\r\u001b[31m ";
    const cases = [
      { title, outcome: title, acceptance: [`${title} is done`], defaulted: true },
      { title: `${hostile.repeat(350)}END`, outcome: `${hostile.repeat(400)}OUTCOME`, acceptance: [`${hostile.repeat(300)}ACCEPT`] },
      { title: "Small contract", outcome: "Small exact outcome", acceptance: ["Small exact criterion"] },
      ...["A".repeat(97), "B".repeat(120), "C".repeat(192), "é".repeat(60)].map((title) => ({
        title, outcome: "Short explicit outcome", acceptance: ["Short explicit criterion"], headerExact: true,
      })),
    ];
    const refs = [];
    for (const expected of cases) {
      const added = receipt(await client.call("add", expected.defaulted
        ? { title: expected.title }
        : { title: expected.title, outcome: expected.outcome, acceptance: expected.acceptance }));
      assert.ok(Buffer.byteLength(JSON.stringify(added)) < 12288);
      refs.push(added.work.short_ref);
    }
    receipt(await client.call("claim", { work_ref: refs.at(-1) }));
    receipt(await client.call("next", { verbose: true }));
    const before = receipt(await client.call("next", { peek: true, verbose: true }));
    for (const [index, expected] of cases.entries()) {
      const work_ref = refs[index];
      for (const mode of [{}, { notes: true }, { notes: true, gates: true }, { history: true }]) {
        const shown = receipt(await client.call("show", { work_ref, ...mode }));
        const overview = structuredClone(shown);
        // Record windows intentionally expose immutable detail locators; they
        // are not host identity fields. Keep the ordinary projection check on
        // everything else, including each record's remaining fields.
        for (const row of [...overview.notes, ...overview.history.items]) {
          if (row.locator !== undefined) {
            assert.match(row.locator, /^[0-9a-f]{8,64}(?::[1-9][0-9]*)?$/u);
            delete row.locator;
          }
        }
        assertTerseShow(overview);
        assert.ok(Buffer.byteLength(JSON.stringify(shown)) < 12288);
        if (index < 2) assert.ok(shown.next.some((command) => new RegExp(`^engram work show '?${work_ref}'? --full$`, "u").test(command)));
        const flags = mode.history ? ["--history"] : mode.notes ? ["--notes", ...(mode.gates ? ["--gates"] : [])] : [];
        const text = cliWord(engramHome, session, "show", work_ref, ...flags);
        assert.equal(text.status, 0, text.stderr);
        assert.ok(Buffer.byteLength(text.stdout) < 12288);
        if (expected.headerExact) assert.ok(text.stdout.split("\n")[0].includes(expected.title), "ordinary header preserves its whole bounded title, including the old 97–192-byte gap");
        if (index < 2) assert.ok(text.stdout.includes("--full"), "omitted contract text must advertise complete contract");
      }
      const response = await client.call("show", { work_ref, full: true });
      const full = receipt(response);
      assert.deepEqual(JSON.parse(response.content[0].text), full);
      assert.deepEqual(Object.keys(full.work).sort(), ["acceptance", "outcome", "revision", "short_ref", "title"]);
      assert.equal(full.work.short_ref, work_ref);
      assert.equal(full.work.revision, 1);
      assert.equal(full.work.title, expected.title);
      assert.equal(full.work.outcome, expected.outcome);
      assert.deepEqual(full.work.acceptance, expected.acceptance);
      const json = cliWord(engramHome, session, "show", work_ref, "--full", "--json");
      assert.equal(json.status, 0, json.stderr);
      assert.deepEqual(JSON.parse(json.stdout), full);
      assert.equal(json.stdout, `${JSON.stringify(full)}\n`, "CLI full detail is exact compact JSON plus one transport LF");
      if (index < 2) assert.ok(Buffer.byteLength(json.stdout) > 12288, "explicit full detail must not be clipped to the overview budget");
      const text = cliWord(engramHome, session, "show", work_ref, "--full");
      assert.equal(text.status, 0, text.stderr);
      assert.doesNotMatch(text.stdout, /\u001b|\r/u, "authored controls must be framed, not emitted raw");
      if (index === 0) assert.ok(text.stdout.includes(title), "full title must survive terminal rendering");
    }
    for (const mode of [{ notes: true }, { gates: true }, { history: true }, { after: "invalid" }, { note: "deadbeef" }]) {
      const result = await client.call("show", { work_ref: refs[0], full: true, ...mode });
      assert.equal(result.isError, true, JSON.stringify(mode));
    }
    for (const flags of [["--notes"], ["--gates"], ["--history"], ["--after", "invalid"], ["--note", "deadbeef"]]) {
      const result = cliWord(engramHome, session, "show", refs[0], "--full", ...flags);
      assert.notEqual(result.status, 0, JSON.stringify(flags));
    }
    const after = receipt(await client.call("next", { peek: true, verbose: true }));
    assert.equal(after.focus.status.work.short_ref, before.focus.status.work.short_ref);
    assert.equal(after.read_cut.project_position, before.read_cut.project_position);
    assert.equal(receipt(await client.call("ls")).total, cases.length);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("note and history windows continue through CLI and MCP with complete detail", async (t) => {
  const engramHome = fixtureHome("engram-record-windows-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "window-reader");
    await client.initialize();
    const showTool = (await client.tools()).find(({ name }) => name === "show");
    for (const key of ["notes", "history", "after", "note"]) assert.ok(showTool.inputSchema.properties[key]);
    const work_ref = receipt(await client.call("add", { title: "Window traversal" })).work.short_ref;
    const bodies = Array.from({ length: 12 }, (_, index) => `Verdict ${index}: ${"body\n".repeat(600)}END`);
    for (const text of bodies) receipt(await client.call("note", { work_ref, text }));
    const context = ["--home", engramHome, "work", "--actor-id", "window-reader", "--session-id", "window-reader"];
    const cli = (...args) => spawnSync(binary, [...context, ...args], { cwd: root, encoding: "utf8" });
    const first = receipt(await client.call("show", { work_ref, notes: true }));
    assert.equal(first.notes.at(-1).summary, bodies.at(-1));
    assert.ok(first.notes_window.after);
    const seen = [];
    let after;
    do {
      const result = await client.call("show", { work_ref, notes: true, after });
      const page = receipt(result);
      assert.equal(page.notes_window.byte_budget, 12288);
      if (after) {
        assert.deepEqual(page.work, { short_ref: work_ref, title: "Window traversal" });
        assert.equal(page.status, undefined);
        assert.equal(page.completion, undefined);
        assert.equal(page.child_obligations, undefined);
        assert.equal(page.full_detail, `engram work show '${work_ref}'`);
        assert.equal(JSON.stringify(page).match(/"title":/gu)?.length, 1);
      }
      const shell = cli("show", work_ref, "--notes", ...(after ? ["--after", after] : []), "--json");
      assert.equal(shell.status, 0, shell.stderr);
      assert.ok(Buffer.byteLength(shell.stdout) <= 12288);
      assertRecordParity(JSON.parse(shell.stdout), page);
      assert.ok(Buffer.byteLength(JSON.stringify(page)) < 12288);
      assert.equal(page.notes_window.newer, seen.length);
      assert.equal(page.notes_omitted, bodies.length - page.notes.length);
      seen.push(...page.notes.toReversed().map(({ summary }) => summary));
      after = page.notes_window.after;
      assert.ok(seen.length <= bodies.length);
    } while (after);
    assert.deepEqual(seen.toReversed(), bodies);
    structuredError(await client.call("show", { work_ref, history: true, after: first.notes_window.after }), "work_show_cursor_invalid");
    const huge = `next:\n${"ü detail\n".repeat(2500)}END`;
    receipt(await client.call("note", { work_ref, text: huge }));
    structuredError(await client.call("show", { work_ref, notes: true, after: first.notes_window.after }), "work_show_cursor_invalid");
    const bounded = receipt(await client.call("show", { work_ref, notes: true }));
    const placeholder = bounded.notes.at(-1);
    assert.equal(placeholder.body_omitted, true);
    assert.equal(placeholder.body_bytes, Buffer.byteLength(huge));
    const detail = receipt(await client.call("show", { work_ref, note: placeholder.locator.slice(0, 8) }));
    assert.equal(detail.note.summary, huge);
    assert.equal(detail.note.body_bytes, Buffer.byteLength(huge));
    const full = cli("show", work_ref, "--note", placeholder.locator, "--json");
    assert.equal(full.status, 0, full.stderr);
    assert.deepEqual(JSON.parse(full.stdout), detail);
    assert.ok(Buffer.byteLength(full.stdout) > 12288);
    const text = cli("show", work_ref, "--note", placeholder.locator);
    assert.equal(text.status, 0, text.stderr);
    assert.equal(text.stdout.split("\n").filter((line) => line === "next:").length, 1);
    const tooLarge = structuredError(await client.call("note", { work_ref, text: "ü".repeat(32769) }), "work_note_too_large");
    assert.equal(tooLarge.details.bytes, 65538);
    assert.equal(tooLarge.details.limit, 65536);
    assert.equal(tooLarge.details.remedy, "carry bulk content as a reference");
    for (let index = 0; index < 24; index++) receipt(await client.call("update", { work_ref, action: "revise", title: `History revision ${index}` }));
    const historySeen = new Set();
    after = undefined;
    do {
      const page = receipt(await client.call("show", { work_ref, history: true, after }));
      assert.equal(page.history.window.newer, historySeen.size);
      const shell = cli("show", work_ref, "--history", ...(after ? ["--after", after] : []), "--json");
      assert.equal(shell.status, 0, shell.stderr);
      assert.ok(Buffer.byteLength(shell.stdout) <= 12288);
      assert.deepEqual(JSON.parse(shell.stdout).history.items, page.history.items);
      for (const row of page.history.items) { assert.ok(!historySeen.has(row.locator)); historySeen.add(row.locator); }
      after = page.history.window.after;
      if (!after) assert.equal(historySeen.size, page.history.total);
    } while (after);
    assert.equal(historySeen.size, 25);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("notes keep a verdict visible after nine gates with explicit CLI and MCP gate traversal", async (t) => {
  const engramHome = fixtureHome("engram-note-gates-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "gate-reader");
    await client.initialize();
    const work_ref = receipt(await client.call("add", { title: "Verdict then gates" })).work.short_ref;
    receipt(await client.call("note", { work_ref, text: "Initial observation" }));
    receipt(await client.call("claim", { work_ref }));
    receipt(await client.call("note", { work_ref, text: "Final verdict: approved" }));
    for (let index = 0; index < 9; index++) {
      receipt(await client.call("gate", { work_ref, name: `check ${index}`, failed: Array.from({ length: 16 }, (_, failure) => `failure ${failure}: ${"x".repeat(180)}`), evidence_ref: `test:gate-${index}` }));
    }
    const context = ["--home", engramHome, "work", "--actor-id", "gate-reader", "--session-id", "gate-reader"];
    const check = async (gates, after) => {
      const value = receipt(await client.call("show", { work_ref, notes: true, gates, after }));
      const flags = ["show", work_ref, "--notes", ...(gates ? ["--gates"] : []), ...(after ? ["--after", after] : [])];
      const shell = spawnSync(binary, [...context, ...flags, "--json"], { cwd: root, encoding: "utf8" });
      const text = spawnSync(binary, [...context, ...flags], { cwd: root, encoding: "utf8" });
      assert.equal(shell.status, 0, shell.stderr);
      assert.equal(text.status, 0, text.stderr);
      const shellValue = JSON.parse(shell.stdout);
      assertRecordParity(shellValue, value);
      assert.ok(Buffer.byteLength(shell.stdout) <= 12288);
      assert.ok(Buffer.byteLength(text.stdout) < 12288);
      assert.equal(text.stdout.match(/gate evidence:/gu).length, 1);
      const headers = text.stdout.split("\n").filter((line) => line.startsWith("  - ") && line.includes(" UTF-8 body bytes)"));
      assert.equal(headers.length, value.notes.length);
      const markers = { notes: "note", observations: "observation", gates: "gate" };
      for (const row of value.notes) {
        assert.equal(headers.filter((line) => line.startsWith(`  - ${row.locator} [${markers[row.family]}] `)).length, 1);
      }
      for (const [family, total] of Object.entries({ notes: 1, observations: 1, gates: 9 })) {
        const shown = value.notes.filter((row) => row.family === family).length;
        assert.deepEqual(value.notes_window.families[family], { total, shown, omitted: total - shown });
        assert.equal(headers.filter((line) => line.includes(` [${markers[family]}] `)).length, shown);
      }
      return value;
    };
    const first = await check(false);
    assert.deepEqual(first.notes.map(({ summary }) => summary), ["Initial observation", "Final verdict: approved"]);
    assert.equal(first.notes_window.total, 2);
    assert.equal(first.notes_omitted, 0);
    assert.ok(first.next.includes(`engram work show ${work_ref} --notes --gates`));
    const seen = new Set();
    let after;
    do {
      const page = await check(true, after);
      assert.equal(page.notes_window.total, 11);
      assert.equal(page.notes_window.newer, seen.size);
      assert.equal(page.notes_omitted, 11 - page.notes.length);
      for (const row of page.notes) {
        assert.equal(seen.has(row.locator), false);
        seen.add(row.locator);
        const detail = receipt(await client.call("show", { work_ref, note: row.locator }));
        assert.equal(detail.note.family, row.family);
        assert.equal(detail.note.summary, row.summary);
      }
      after = page.notes_window.after;
      if (after) {
        assert.ok(page.next.includes(`engram work show ${work_ref} --notes --gates --after ${after}`));
        const error = structuredError(await client.call("show", { work_ref, notes: true, after }), "work_show_cursor_invalid");
        assert.deepEqual(error.next, [`engram work show '${work_ref}' --notes`]);
      }
      assert.ok(seen.size <= 11);
    } while (after);
    assert.equal(seen.size, 11);
    structuredError(await client.call("show", { work_ref, gates: true }), "work_invalid");
    const invalid = spawnSync(binary, [...context, "show", work_ref, "--gates"], { cwd: root, encoding: "utf8" });
    assert.notEqual(invalid.status, 0);
    assert.match(invalid.stderr, /--notes/u);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("explicit records retain relative authors and host context on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-record-authors-", t);
  const clients = [];
  try {
    buildAndInit(engramHome);
    for (const [index, actor] of ["private-self-principal", "private-peer-principal"].entries()) {
      const client = new McpClient(engramHome, `author-session-${index}`, `host-context-${index}`, actor);
      clients.push(client);
      await client.initialize();
    }
    const work_ref = receipt(await clients[0].call("add", { title: "Relative explicit authors" })).work.short_ref;
    for (const [index, client] of clients.entries()) {
      receipt(await client.call("note", { work_ref, text: `Authored note ${index}` }));
      receipt(await client.call("update", { work_ref, action: "revise", title: `Revision ${index}` }));
    }
    const context = ["--home", engramHome, "work", "--actor-id", "private-self-principal", "--session-id", "author-session-0"];
    const check = async (args, flags) => {
      const result = await clients[0].call("show", { work_ref, ...args });
      const value = receipt(result);
      const shell = spawnSync(binary, [...context, "show", work_ref, ...flags, "--json"], { cwd: root, encoding: "utf8" });
      const text = spawnSync(binary, [...context, "show", work_ref, ...flags], { cwd: root, encoding: "utf8" });
      assert.equal(shell.status, 0, shell.stderr);
      assert.equal(text.status, 0, text.stderr);
      assertRecordParity(JSON.parse(shell.stdout), value);
      for (const output of [JSON.stringify(result), shell.stdout, text.stdout]) assert.doesNotMatch(output, /private-(self|peer)-principal/u);
      return value;
    };
    const notes = await check({ notes: true }, ["--notes"]);
    const history = await check({ history: true }, ["--history"]);
    for (const rows of [notes.notes, history.history.items]) {
      assert.ok(rows.some(({ by }) => by === "you (host-context-0)"));
      assert.ok(rows.some(({ by }) => /^peer-[0-9a-f]{24} \(host-context-1\)$/u.test(by)));
    }
    for (const row of notes.notes) {
      const detail = await check({ note: row.locator }, ["--note", row.locator]);
      assert.equal(detail.note.by, row.by);
      assert.equal(detail.note.summary, row.summary);
    }
  } finally {
    try {
      await closeFixtureClients(...clients);
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("full contract text round-trips through CLI and MCP show", async (t) => {
  const engramHome = fixtureHome("engram-full-contract-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "contract-reader");
    await client.initialize();
    const criterion = "c".repeat(600);
    const work = receipt(await client.call("add", { title: "Full criterion", acceptance: [criterion] })).work.short_ref;
    const mcpShown = await client.call("show", { work_ref: work });
    assert.equal(receipt(mcpShown).status.work.acceptance[0], criterion);
    assert.deepEqual(JSON.parse(mcpShown.content[0].text), receipt(mcpShown));
    assert.equal(receipt(await client.call("show", { work_ref: work, notes: true })).status.work.acceptance[0], criterion);
    const context = ["--home", engramHome, "work", "--actor-id", "contract-reader", "--session-id", "contract-reader"];
    const cli = (args) => {
      const result = spawnSync(binary, [...context, ...args], { cwd: root, encoding: "utf8" });
      assert.equal(result.status, 0, result.stderr);
      const stdout = Buffer.byteLength(result.stdout);
      if (args.includes("--json")) {
        assert.ok(stdout <= 12288);
      } else {
        assert.ok(stdout < 12288);
      }
      return result.stdout;
    };
    assert.equal(JSON.parse(cli(["show", work, "--json"])).status.work.acceptance[0], criterion);
    assert.ok(cli(["show", work]).includes(`  1. ${criterion}\n`));
    assert.equal(JSON.parse(cli(["show", work, "--notes", "--json"])).status.work.acceptance[0], criterion);
    const parent = receipt(await client.call("add", { title: "Parent" })).work.short_ref;
    const child = receipt(await client.call("add", { title: "Optional", under: parent, optional: true })).work.short_ref;
    receipt(await client.call("claim", { work_ref: parent }));
    receipt(await client.call("done", { work_ref: parent, summary: "Delivered" }));
    const reason = "r".repeat(600);
    const successor = receipt(await client.call("update", { work_ref: child, action: "detach", reason })).receipt.work_ref;
    assert.equal(receipt(await client.call("show", { work_ref: successor })).detached_from.reason, reason);
    assert.equal(receipt(await client.call("show", { work_ref: successor, notes: true })).detached_from.reason, reason);
    assert.equal(JSON.parse(cli(["show", successor, "--json"])).detached_from.reason, reason);
    assert.equal(JSON.parse(cli(["show", successor, "--notes", "--json"])).detached_from.reason, reason);
    assert.ok(cli(["show", successor]).includes(`detached from: ${child} — ${reason}\n`));
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("next discovers never-read stranded children with bounded CLI and MCP remedies", async (t) => {
  const engramHome = fixtureHome("engram-stranded-next-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const parent = cliJson(engramHome, "writer", "add", "Parent").work.short_ref;
    const children = Array.from({length: 8}, (_, index) =>
      cliJson(engramHome, "writer", "add", `Follow-up ${index}`, "--under", parent, "--optional").work.short_ref);
    client = new McpClient(engramHome, "participant");
    await client.initialize();
    receipt(await client.call("claim", {work_ref: parent}));
    receipt(await client.call("done", {work_ref: parent, summary: "Delivered parent"}));
    const captures = [];
    // This session has never shown, focused, claimed, or noted any child.
    for (const verbose of [false, true]) {
      for (const peek of [true, false, true]) {
        const mcp = receipt(await client.call("next", {peek, verbose}));
        const cli = cliJson(engramHome, "participant", "next", ...(peek ? ["--peek"] : []), ...(verbose ? ["--verbose"] : []));
        assert.deepEqual(mcp.stranded_children, cli.stranded_children);
        assert.equal(mcp.stranded_children.length, 5);
        assert.equal(mcp.stranded_children_omitted, 3);
        assert.equal(mcp.stranded_children_next, "engram work ls --blocked");
        assert.deepEqual(mcp.held, []);
        const focus = verbose ? mcp.focus.status.work.short_ref : mcp.focus.ref;
        assert.equal(focus, parent);
        for (const row of mcp.stranded_children) {
          assert.ok(children.includes(row.ref));
          assert.equal(row.parent_ref, parent);
          assert.equal(row.child_requirement, "optional");
          assert.equal(row.blocked_reason, `parent ${parent} is completed; continue as independent work`);
          assert.equal(row.remedy, `engram work update ${row.ref} --detach "Continue as independent work"`);
        }
        assert.ok(Buffer.byteLength(JSON.stringify(mcp)) < 12 * 1024);
        const text = cliText(engramHome, "participant", "next", "--peek");
        assert.ok(Buffer.byteLength(text) < 12 * 1024);
        assert.ok(text.endsWith("\n"));
        assert.ok(text.includes("(3 more stranded children not shown)"));
        assert.ok(text.includes(mcp.stranded_children[0].remedy));
        captures.push({peek, verbose, mcp, cli, text});
      }
    }
    const before = cliJson(engramHome, "writer", "show", parent).history;
    const child = captures[0].mcp.stranded_children[0].ref;
    const detached = receipt(await client.call("update", {work_ref: child, action: "detach", reason: "Continue as independent work"}));
    assert.notEqual(detached.receipt.work_ref, child);
    const after = receipt(await client.call("next", {peek: true}));
    assert.ok(!after.stranded_children.some(row => row.ref === child));
    assert.equal(after.stranded_children_omitted, 2);
    assert.deepEqual(cliJson(engramHome, "writer", "show", parent).history, before);
    if (process.env.ENGRAM_CAPTURE_STRANDED_RECEIPT === "1") {
      const directory = join(root, "target", "tmp", `stranded-reader-capture-${Date.now()}`);
      mkdirSync(directory, {recursive: true});
      const executable = process.platform === "win32" ? `${binary}.exe` : binary;
      const readiness = spawnSync(binary, ["--home", engramHome, "readiness", "--json"], {cwd: root, encoding: "utf8"});
      assert.equal(readiness.status, 0, readiness.stderr);
      const path = join(directory, "payload.json");
      writeFileSync(path, JSON.stringify({executable, executable_sha256: createHash("sha256").update(readFileSync(executable)).digest("hex"), readiness: JSON.parse(readiness.stdout), parent, children, captures, detached, after}, null, 2));
      console.error(`Stranded reader capture: ${path}`);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("stranded descendant remedies and unavailable diagnostics agree on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-stranded-unavailable-", t);
  let client;
  let database;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "participant");
    await client.initialize();
    const parent = receipt(await client.call("add", {title: "Delivered parent"})).work.short_ref;
    const child = cliJson(engramHome, "writer", "add", "Stranded child", "--under", parent, "--optional").work.short_ref;
    const descendant = cliJson(engramHome, "writer", "add", "Open descendant", "--under", child, "--optional").work.short_ref;
    receipt(await client.call("claim", {work_ref: parent}));
    receipt(await client.call("done", {work_ref: parent, summary: "Delivered parent"}));
    const focus = receipt(await client.call("add", {title: "Independent focus"})).work.short_ref;
    receipt(await client.call("claim", {work_ref: focus}));
    const ready = cliJson(engramHome, "writer", "add", "Independent ready work").work.short_ref;
    const captures = [];
    const check = async (unavailable) => {
      for (const verbose of [false, true]) {
        for (const peek of [false, true]) {
          const result = await client.call("next", {peek, verbose});
          const mcp = receipt(result);
          const flags = [...(peek ? ["--peek"] : []), ...(verbose ? ["--verbose"] : [])];
          const cli = cliJson(engramHome, "participant", "next", ...flags);
          const text = cliText(engramHome, "participant", "next", ...flags);
          for (const value of [mcp, cli]) {
            assert.ok(value.ready.some(row => (row.ref ?? row.work?.short_ref) === ready));
            if (peek && !verbose) {
              assert.equal(value.held.length, 0);
              assert.equal(value.focus.ref, focus);
              assert.equal(value.focus.holder, "you");
            } else {
              assert.equal(value.held.length, 1);
            }
            if (unavailable) {
              assert.equal(value.stranded_children_unavailable, true);
              assert.equal(value.stranded_children_error_class, "stored_json_invalid");
              for (const key of ["stranded_children", "stranded_children_omitted", "stranded_children_next"]) {
                assert.equal(Object.hasOwn(value, key), false, key);
              }
            } else {
              assert.equal(Object.hasOwn(value, "stranded_children_unavailable"), false);
              assert.equal(value.stranded_children.length, 1);
              const row = value.stranded_children[0];
              assert.equal(row.ref, child);
              assert.equal(row.parent_ref, parent);
              assert.equal(row.blocked_reason, `parent ${parent} is completed; resolve open descendants before detaching their parent`);
              assert.equal(row.remedy, `engram work show ${descendant}`);
              assert.doesNotMatch(row.remedy, /--detach/u);
            }
            assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12 * 1024);
          }
          assert.deepEqual(mcp.stranded_children, cli.stranded_children);
          assert.ok(text.includes(unavailable ? "stranded children: unavailable (stored_json_invalid)" : `engram work show ${descendant}`));
          captures.push({unavailable, peek, verbose, result, cli, text});
        }
      }
    };
    await check(false);
    const childBefore = cliJson(engramHome, "writer", "show", child);
    const parentBefore = cliJson(engramHome, "writer", "show", parent);
    const projectDirectory = readdirSync(join(engramHome, "projects"), {withFileTypes: true}).filter(entry => entry.isDirectory());
    assert.equal(projectDirectory.length, 1);
    const {DatabaseSync} = await import("node:sqlite");
    database = new DatabaseSync(join(engramHome, "projects", projectDirectory[0].name, "engram.db"));
    const event = database.prepare(`SELECT object_id, canonical_json FROM work_feed_entries
      JOIN objects USING(object_id) JOIN work_items USING(work_id)
      WHERE feed_kind = 'project' AND short_ref = ? AND work_feed_entries.object_kind = 'work_event'
      ORDER BY position DESC LIMIT 1`).get(child);
    assert.ok(event);
    database.prepare("UPDATE objects SET canonical_json = ? WHERE object_id = ?").run(Buffer.from("{}"), event.object_id);
    await check(true);
    const contextArguments = ["--peek", "--context-generation", `stranded-unavailable-${Date.now()}`];
    const contextNudge = cliWord(engramHome, "participant", "next", ...contextArguments);
    assert.equal(contextNudge.status, 0, contextNudge.stderr);
    assert.equal(contextNudge.signal, null);
    assert.ok(contextNudge.stdout.includes("stranded children: unavailable (stored_json_invalid)"));
    assert.ok(contextNudge.stdout.includes(focus));
    assert.ok(contextNudge.stdout.includes(ready));
    const refused = await client.call("update", {work_ref: child, action: "detach", reason: "Must remain strict"});
    assert.equal(refused.isError, true);
    assert.notEqual(cliWord(engramHome, "participant", "update", child, "--detach", "Must remain strict").status, 0);
    database.prepare("UPDATE objects SET canonical_json = ? WHERE object_id = ?").run(event.canonical_json, event.object_id);
    database.close();
    database = undefined;
    assert.deepEqual(cliJson(engramHome, "writer", "show", child).status, childBefore.status);
    assert.deepEqual(cliJson(engramHome, "writer", "show", parent).status, parentBefore.status);
    await check(false);
    if (process.env.ENGRAM_CAPTURE_STRANDED_UNAVAILABLE_RECEIPT === "1") {
      const directory = join(root, "target", "tmp", `stranded-unavailable-reader-capture-${Date.now()}`);
      mkdirSync(directory, {recursive: true});
      const executable = process.platform === "win32" ? `${binary}.exe` : binary;
      const readiness = spawnSync(binary, ["--home", engramHome, "readiness", "--json"], {cwd: root, encoding: "utf8"});
      assert.equal(readiness.status, 0, readiness.stderr);
      const path = join(directory, "payload.json");
      writeFileSync(path, JSON.stringify({executable, executable_sha256: createHash("sha256").update(readFileSync(executable)).digest("hex"), readiness: JSON.parse(readiness.stdout), parent, child, descendant, focus, ready, captures, context_nudge: {arguments: ["work", "next", ...contextArguments], status: contextNudge.status, signal: contextNudge.signal, stdout: contextNudge.stdout, stderr: contextNudge.stderr}, refused}, null, 2));
      console.error(`Stranded unavailable reader capture: ${path}`);
    }
  } finally {
    if (database) database.close();
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}

export function registerShortAndBlockedListing(support) {
  const { root, target, binary, test, fixtureHome, removeFixtureHomes, receipt, structuredError, buildAndInit, cliWord, cliJson, McpClient } = support;
test("short orientation continues the complete ready set through real CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-orientation-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const actor = "orientation-reader";
    client = new McpClient(engramHome, actor);
    await client.initialize();
    const tools = await client.tools();
    assert.match(tools.find(({ name }) => name === "ls").inputSchema.properties.ready.description, /ready candidates/u);
    assert.match(tools.find(({ name }) => name === "next").inputSchema.properties.limit.description, /compact ready candidates are capped/u);
    const help = cliWord(engramHome, actor, "ls", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /--ready/u);
    const initial = receipt(await client.call("next", { peek: true }));
    const cap = initial.ready_limit;
    assert.ok(Number.isInteger(cap) && cap > 0);
    assert.equal(initial.ready_more, false);
    const emptyText = cliWord(engramHome, actor, "next", "--peek");
    assert.equal(emptyText.status, 0, emptyText.stderr);
    assert.doesNotMatch(emptyText.stdout, /compact cap:/u);
    const expected = [];
    for (let i = 0; i < cap * 5; i++) {
      expected.push(receipt(await client.call("add", { title: `Candidate ${i}`, assignee: i === 0 ? actor : undefined })).work.short_ref);
    }
    const blocked = receipt(await client.call("add", { title: "Blocked exclusion" })).work.short_ref;
    receipt(await client.call("update", { work_ref: blocked, action: "blocked", text: "waiting" }));
    const smaller = receipt(await client.call("next", { peek: true, limit: 2 }));
    assert.equal(smaller.ready_limit, 2);
    assert.equal(smaller.ready.length, 2);
    const smallerText = cliWord(engramHome, actor, "next", "--peek", "--limit", "2");
    assert.equal(smallerText.status, 0, smallerText.stderr);
    assert.match(smallerText.stdout, /compact cap: 2;/u);
    for (const peek of [true, false]) {
      const first = receipt(await client.call("next", { peek }));
      assert.equal(first.ready.length, cap);
      assert.equal(first.held.length, 0);
      assert.equal(first.assigned[0].ref, expected[0]);
      assert.ok(
        first.ready.every((row) => row.ready_reason == null),
        "compact plain-ready rows omit the constant restatement",
      );
      let command = first.ready_next;
      const collected = first.ready.map(({ ref }) => ref);
      let pages = 0;
      while (command) {
        const [engram, work, word, ...args] = command.split(" ");
        assert.equal(engram, "engram");
        assert.equal(work, "work");
        assert.equal(word, "ls");
        const page = cliJson(engramHome, actor, word, ...args);
        const input = { ready: true, limit: Number(args[args.indexOf("--limit") + 1]), after: args.includes("--after") ? args[args.indexOf("--after") + 1] : undefined };
        assert.deepEqual(receipt(await client.call("ls", input)).items, page.items);
        assert.ok(page.items.length > 0);
        collected.push(...page.items.map(({ ref }) => ref));
        assert.ok(++pages <= expected.length);
        command = page.more ? page.next[0] : undefined;
      }
      assert.deepEqual(collected, expected);
    }
    const text = cliWord(engramHome, actor, "next", "--peek");
    assert.equal(text.status, 0, text.stderr);
    assert.ok(text.stdout.indexOf("held by you") < text.stdout.indexOf("assigned ("));
    assert.ok(text.stdout.indexOf("assigned (") < text.stdout.indexOf("ready ("));
    assert.match(text.stdout, /more ready candidates: engram work ls --ready/u);
    structuredError(await client.call("ls", { ready: true, blocked: true }), "work_invalid");
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("blocked listing excludes ended history from exact CLI and MCP pages", async (t) => {
  const engramHome = fixtureHome("engram-blocked-history-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const actor = "blocked-history-reader";
    client = new McpClient(engramHome, actor);
    await client.initialize();
    const tools = await client.tools();
    assert.match(tools.find(({name}) => name === "ls").inputSchema.properties.all.description, /blocked excludes ended items even with all/u);
    const help = cliWord(engramHome, actor, "ls", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout, /--blocked excludes ended items even with --all/u);
    const add = async (title) => receipt(await client.call("add", {title})).work;
    const open = await add("Matching open blocker");
    const dependent = await add("Matching open prerequisite");
    const cancelled = await add("Matching cancelled blocker");
    const superseded = await add("Matching superseded blocker");
    const completed = await add("Matching completed history");
    const prerequisite = await add("Unsatisfied prerequisite");
    const satisfied = await add("Satisfied prerequisite");
    const replacement = await add("Replacement");
    for (const work of [open, cancelled, superseded]) {
      receipt(await client.call("update", {work_ref: work.short_ref, action: "blocked", text: "Historical obstacle"}));
    }
    for (const work of [prerequisite, satisfied]) {
      receipt(await client.call("update", {work_ref: dependent.short_ref, action: "after", prerequisite: work.short_ref}));
    }
    for (const work of [completed, satisfied]) {
      receipt(await client.call("claim", {work_ref: work.short_ref}));
      receipt(await client.call("done", {work_ref: work.short_ref, summary: "Delivered"}));
    }
    const historical = [cancelled, superseded].map((work) => cliJson(engramHome, actor, "show", work.short_ref).blockers);
    receipt(await client.call("update", {work_ref: cancelled.short_ref, action: "cancel", reason: "Ended with retained history"}));
    receipt(await client.call("update", {work_ref: superseded.short_ref, action: "supersede", replacement: replacement.short_ref, reason: "Replacement owns delivery"}));
    const rich = receipt(await client.call("ls", {all: true, verbose: true}));
    const identities = new Map(rich.items.map(({work}) => [work.short_ref, work.work_id]));
    const expected = [open, dependent].sort((a, b) => identities.get(a.short_ref).localeCompare(identities.get(b.short_ref))).map(({short_ref}) => short_ref);
    const captures = [];
    for (const all of [false, true]) {
      for (const verbose of [false, true]) {
        let after;
        const collected = [];
        do {
          const input = {all, blocked: true, search: "Matching", limit: 1, verbose, after};
          const args = ["--blocked", "--search", "Matching", "--limit", "1", ...(all ? ["--all"] : []), ...(verbose ? ["--verbose"] : []), ...(after ? ["--after", after] : []), "--json"];
          const cli = cliWord(engramHome, actor, "ls", ...args);
          assert.equal(cli.status, 0, cli.stderr);
          const page = JSON.parse(cli.stdout);
          const mcp = await client.call("ls", input);
          const other = receipt(mcp);
          for (const field of ["items", "total", "shown_before", "omitted", "more"]) assert.deepEqual(other[field], page[field], field);
          assert.equal(Boolean(other.after), Boolean(page.after));
          assert.equal(page.total, 2);
          assert.equal(page.shown_before, collected.length);
          assert.equal(page.items.length, 1);
          const row = page.items[0];
          const ref = verbose ? row.work.short_ref : row.ref;
          collected.push(ref);
          assert.equal(page.omitted, 2 - collected.length);
          assert.equal(page.more, collected.length < 2);
          assert.equal(verbose ? row.work.lifecycle : row.state, verbose ? "open" : "blocked");
          if (verbose) assert.equal(row.availability, "blocked");
          if (verbose && ref === dependent.short_ref) assert.deepEqual(row.blocked_by, [identities.get(prerequisite.short_ref)]);
          captures.push({input, arguments: args, cli: {status: cli.status, signal: cli.signal, stdout: cli.stdout, stderr: cli.stderr}, mcp});
          after = page.after;
          assert.ok(collected.length <= 2);
        } while (after);
        assert.deepEqual(collected, expected);
      }
    }
    const history = [];
    for (const [index, work] of [cancelled, superseded].entries()) {
      const shown = cliJson(engramHome, actor, "show", work.short_ref);
      assert.deepEqual(shown.blockers, historical[index]);
      assert.deepEqual(receipt(await client.call("show", {work_ref: work.short_ref})).blockers, historical[index]);
      history.push(shown);
      const empty = cliJson(engramHome, actor, "ls", "--all", "--blocked", "--search", work.title);
      assert.equal(empty.total, 0);
      assert.deepEqual(empty.items, []);
      assert.equal(empty.more, false);
      assert.equal(empty.omitted, 0);
      assert.equal(empty.after, undefined);
    }
    const everything = cliJson(engramHome, actor, "ls", "--all", "--search", "Matching");
    assert.equal(everything.total, 5);
    for (const work of [cancelled, superseded, completed]) assert.ok(everything.items.some(({ref}) => ref === work.short_ref));
    if (process.env.ENGRAM_CAPTURE_BLOCKED_HISTORY_RECEIPT === "1") {
      const directory = join(root, "target", "tmp", `blocked-history-reader-capture-${Date.now()}`);
      mkdirSync(directory, {recursive: true});
      const executable = process.platform === "win32" ? `${binary}.exe` : binary;
      const readiness = spawnSync(binary, ["--home", engramHome, "readiness", "--json"], {cwd: root, encoding: "utf8"});
      assert.equal(readiness.status, 0, readiness.stderr);
      const path = join(directory, "payload.json");
      writeFileSync(path, JSON.stringify({executable, executable_sha256: createHash("sha256").update(readFileSync(executable)).digest("hex"), readiness: JSON.parse(readiness.stdout), fixtures: {open, dependent, cancelled, superseded, completed, prerequisite, satisfied}, rich, captures, history, everything}, null, 2));
      console.error(`Blocked history reader capture: ${path}`);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}
