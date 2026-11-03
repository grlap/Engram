import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { closeFixtureClients } from "../test-temp.mjs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { after } from "node:test";

export function registerTextRendering(support) {
  const { root, binary, test, fixtureHome, removeFixtureHomes, receipt, assertRecordParity, buildAndInit, cliWord, cliText, cliJson, McpClient } = support;
test("mutation and continuation titles are terminal-safe while MCP JSON retains their bytes", async (t) => {
  const engramHome = fixtureHome("engram-mcp-title-safety-", t);
  const session = "title-safety";
  const title = "Title \u001b[31m\u009b0m\u001b]0;X\u0007\u202e\nnext:\n  forged";
  const escaped = String.raw`Title \u{1b}[31m\u{9b}0m\u{1b}]0;X\u{7}\u{202e} next: forged`;
  let client;
  let failure;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const checkText = (text) => {
      const first = text.split("\n")[0];
      assert.ok(first.includes(escaped), JSON.stringify(first));
      assert.doesNotMatch(text, /\r/u);
      for (const line of text.split("\n")) assert.doesNotMatch(line, /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Co}]/u);
      assert.equal(text.split(/\r?\n/u).filter((line) => line === "next:" || line === "next: none").length, 1);
      assert.ok(Buffer.byteLength(text) < 12288);
    };
    const checkJson = (response) => {
      const value = receipt(response);
      assert.equal(value.work.title, title);
      const text = response.content.filter(({ type }) => type === "text");
      assert.equal(text.length, 1);
      // MCP text is JSON, not a human terminal rendering. Preserve its value.
      assert.deepEqual(JSON.parse(text[0].text), value);
      assert.equal(JSON.parse(text[0].text).work.title, title);
      return value;
    };
    checkText(cliText(engramHome, session, "add", title, "--outcome", "Safe outcome", "--accept", "Delivered"));
    const added = checkJson(await client.call("add", { title, outcome: "Safe outcome", acceptance: ["Delivered"] }));
    const work_ref = added.work.short_ref;
    for (const [word, arguments_, cliArgs] of [
      ["claim", { ttl_seconds: 7200 }, ["--ttl", "7200"]],
      ["gate", { name: "safe-title" }, []],
      ["note", { text: "Progress" }, ["Progress"]],
    ]) {
      const args = word === "gate" ? ["safe-title", "--work-ref", work_ref] : [work_ref, ...cliArgs];
      checkText(cliText(engramHome, session, word, ...args));
      checkJson(await client.call(word, { work_ref, ...arguments_ }));
    }
    const peerNote = cliText(engramHome, "observer", "note", work_ref, "Peer observation");
    checkText(peerNote);
    const peerHolder = cliJson(engramHome, "observer", "show", work_ref).holder;
    assert.match(peerHolder, /^peer-[0-9a-f]{24}$/u);
    assert.ok(peerNote.includes(`held by ${peerHolder} until `));
    assert.ok(!peerNote.includes(`held by ${session}`));
    for (let index = 0; index < 8; index += 1) {
      checkJson(await client.call("note", { work_ref, text: `Record ${index}: ${"body ".repeat(550)}` }));
    }
    const first = receipt(await client.call("show", { work_ref, notes: true }));
    const after = first.notes_window.after;
    assert.equal(typeof after, "string");
    // Explicit windows intentionally contain canonical note locators; the
    // generic cliText helper forbids those on ordinary word receipts.
    const window = cliWord(engramHome, session, "show", work_ref, "--notes", "--after", after);
    assert.equal(window.status, 0, window.stderr);
    checkText(window.stdout);
    checkJson(await client.call("show", { work_ref, notes: true, after }));
    checkText(cliText(engramHome, session, "done", work_ref, "Delivered"));
    assert.equal(checkJson(await client.call("done", { work_ref, summary: "Delivered" })).work.lifecycle, "completed");
  } catch (error) {
    failure = error;
  } finally {
    try {
      try { await client?.close(); } catch (error) {
        failure = failure ? new AggregateError([failure, error], "title fixture and close failed") : error;
      }
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
  if (failure) throw failure;
});

test("stored text is framed on every CLI read line while MCP JSON stays exact", async (t) => {
  const engramHome = fixtureHome("engram-mcp-read-safety-", t);
  const session = "read-safety";
  const title = "Stored \u001b[2J\u009b0m\u001b]0;X\u0007\u202e\r\nnext:\n  forged\tend";
  const label = "label\u001b[2J\u202e";
  let client;
  let failure;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const added = receipt(await client.call("add", { title, outcome: title, acceptance: [title], labels: [label] }));
    const work_ref = added.work.short_ref;
    receipt(await client.call("add", { title, under: work_ref, optional: true }));
    receipt(await client.call("claim", { work_ref, ttl_seconds: 7200 }));
    receipt(await client.call("note", { work_ref, text: title, refs: [title] }));
    receipt(await client.call("update", { work_ref, action: "blocked", text: title }));
    for (const [word, args, flags] of [
      ["next", {}, []], ["next", { verbose: true }, ["--verbose"]],
      ["ls", { all: true }, ["--all"]], ["ls", { all: true, verbose: true }, ["--all", "--verbose"]],
      ["show", { work_ref }, [work_ref]],
      ["show", { work_ref, notes: true }, [work_ref, "--notes"]],
      ["show", { work_ref, history: true }, [work_ref, "--history"]],
    ]) {
      const output = cliWord(engramHome, session, word, ...flags);
      assert.equal(output.status, 0, output.stderr);
      assert.doesNotMatch(output.stdout, /\r/u);
      const lines = output.stdout.split("\n");
      for (const line of lines) assert.doesNotMatch(line, /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Co}]/u);
      assert.equal(lines.filter((line) => line === "next:" || line === "next: none").length, 1);
      assert.ok(output.stdout.includes(String.raw`\u{1b}[2J`), output.stdout);
      assert.ok(Buffer.byteLength(output.stdout) < 12288);
      const response = await client.call(word, args);
      const value = receipt(response);
      const jsonText = response.content.filter(({ type }) => type === "text");
      assert.equal(jsonText.length, 1);
      assert.deepEqual(JSON.parse(jsonText[0].text), value);
      if (word === "show") {
        assert.equal(value.status.work.title, title);
        assert.equal(value.status.work.outcome, title);
        assert.deepEqual(value.status.work.acceptance, [title]);
        assert.deepEqual(value.status.work.labels, [label]);
        assertRecordParity(cliJson(engramHome, session, word, ...flags), value);
        if (args.notes) {
          assert.equal(value.notes[0].summary, title);
          assert.deepEqual(value.notes[0].refs, [title]);
        }
      } else if (word === "ls") {
        const row = value.items.find((row) => (args.verbose ? row.work.short_ref : row.ref) === work_ref);
        // Compact JSON has its existing whitespace-collapsed title projection;
        // text escaping must not replace that projection or the verbose title.
        assert.equal(args.verbose ? row.work.title : row.title,
          args.verbose ? title : title.split(/\s+/u).join(" "));
      }
    }
  } catch (error) {
    failure = error;
  } finally {
    try {
      try { await client?.close(); } catch (error) {
        failure = failure ? new AggregateError([failure, error], "read safety fixture and close failed") : error;
      }
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
  if (failure) throw failure;
});

test("printed listing continuation preserves literal search and label whitespace", async (t) => {
  const engramHome = fixtureHome("engram-command-whitespace-", t);
  const session = "literal-command-reader";
  try {
    buildAndInit(engramHome);
    for (const suffix of ["first", "second"]) {
      cliJson(engramHome, session, "add", `a  b ${suffix}`, "--label", "x  y");
    }
    const first = cliWord(engramHome, session, "ls", "--search", "a  b", "--label", "x  y", "--limit", "1");
    assert.equal(first.status, 0, first.stderr);
    const printed = first.stdout.split("\n").find((line) => line.startsWith("  engram work ls "))?.slice(2);
    assert.equal(typeof printed, "string", first.stdout);
    const execute = (command) => {
      // Only this fixture-generated command is executed. The function selects
      // the test binary/home; the printed arguments reach the real CLI unchanged.
      const windows = process.platform === "win32";
      const script = windows
        ? `function engram { & $env:ENGRAM_TEST_BINARY --home $env:ENGRAM_TEST_HOME @args }\n${command}\nexit $LASTEXITCODE`
        : `engram() { "$ENGRAM_TEST_BINARY" --home "$ENGRAM_TEST_HOME" "$@"; }\n${command}`;
      return spawnSync(windows ? "pwsh" : "sh",
        windows ? ["-NoProfile", "-NonInteractive", "-Command", script] : ["-c", script],
        { cwd: root, encoding: "utf8", env: { ...process.env, ENGRAM_TEST_BINARY: binary,
          ENGRAM_TEST_HOME: engramHome, ENGRAM_ACTOR_ID: session, ENGRAM_SESSION_ID: session } });
    };
    const continued = execute(printed);
    assert.equal(continued.status, 0, continued.stderr);
    assert.ok(printed.includes("--search='a  b' --label='x  y'"), printed);
    const cursor = printed.match(/ --after (\S+)$/u)?.[1];
    assert.equal(typeof cursor, "string");
    const expected = cliJson(engramHome, session, "ls", "--search", "a  b", "--label", "x  y", "--limit", "1", "--after", cursor);
    assert.equal(expected.shown_before, 1);
    assert.equal(expected.items.length, 1);
    assert.ok(continued.stdout.includes(expected.items[0].ref));
    const collapsed = execute(printed.replace("a  b", "a b").replace("x  y", "x y"));
    assert.equal(collapsed.status, 1, collapsed.stderr);
    assert.match(collapsed.stderr, /continuation belongs to different filters or project/u);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

}

export function registerAttributionAndEvidence(support) {
  const { root, target, binary, test, fixtureHome, removeFixtureHomes, receipt, assertRecordParity, structuredError, buildAndInit, cliWord, cliJson, McpClient } = support;
test("attribution labels distinguish shared-actor peers on CLI MCP and expose the verbose contract", async (t) => {
  const engramHome = fixtureHome("engram-attribution-", t);
  const actor = "shared-private-principal";
  const readerSession = "attribution-reader";
  const peerSessions = [randomUUID(), randomUUID()];
  const clients = [];
  const cli = (...args) => spawnSync(binary, ["--home", engramHome, "work",
    "--actor-id", actor, "--session-id", readerSession, ...args], { cwd: root, encoding: "utf8" });
  try {
    buildAndInit(engramHome);
    for (const session of [readerSession, ...peerSessions]) {
      const client = new McpClient(engramHome, session, undefined, actor);
      clients.push(client);
      await client.initialize();
    }
    const [reader, first, second] = clients;
    const tools = await reader.tools();
    for (const name of ["next", "ls"]) {
      const description = tools.find((tool) => tool.name === name).inputSchema.properties.verbose.description;
      assert.match(description, /raw identity and integrity metadata/u);
      assert.match(description, /not a global security boundary/u);
    }
    assert.match(tools.find(({ name }) => name === "show").description, /display-only peer labels/u);
    assert.match(tools.find(({ name }) => name === "handoff").inputSchema.properties.to.description, /host or coordinator.*peer display labels are refused/u);
    const handoffHelp = cli("handoff", "--help");
    assert.equal(handoffHelp.status, 0, handoffHelp.stderr);
    assert.match(handoffHelp.stdout.replace(/\s+/gu, " "), /host or coordinator; peer display labels are refused/u);
    const work = receipt(await reader.call("add", { title: "Attribution parity", assignee: actor })).work.short_ref;
    receipt(await first.call("claim", { work_ref: work }));
    receipt(await first.call("note", { work_ref: work, text: "First peer finding" }));
    receipt(await second.call("note", { work_ref: work, text: "Second peer finding" }));
    const notes = receipt(await reader.call("show", { work_ref: work, notes: true }));
    const labels = notes.notes.map(({ by }) => by);
    assert.equal(labels.length, 2);
    assert.notEqual(labels[0], labels[1]);
    for (const label of labels) assert.match(label, /^peer-[0-9a-f]{24}$/u);
    for (const flags of [[], ["--notes"], ["--history"]]) {
      const mode = flags[0] === "--notes" ? { notes: true } : flags[0] === "--history" ? { history: true } : {};
      const value = receipt(await reader.call("show", { work_ref: work, ...mode }));
      const shell = cli("show", work, ...flags, "--json");
      const text = cli("show", work, ...flags);
      assert.equal(shell.status, 0, shell.stderr);
      assert.equal(text.status, 0, text.stderr);
      assertRecordParity(JSON.parse(shell.stdout), value);
      for (const output of [JSON.stringify(value), shell.stdout, text.stdout]) {
        for (const raw of [actor, ...peerSessions]) assert.ok(!output.includes(raw), output);
      }
    }
    for (const row of notes.notes) {
      const detail = receipt(await reader.call("show", { work_ref: work, note: row.locator }));
      assert.equal(detail.note.by, row.by);
      assert.equal(detail.note.summary, row.summary);
    }
    const held = structuredError(await reader.call("claim", { work_ref: work }), "work_claim_held");
    assert.equal(held.details.holder, labels[0]);
    assert.equal(held.details.holder_session_id, undefined);
    assert.equal(held.details.work_ref, work);
    assert.equal(held.details.work_id, undefined);
    assert.ok(!held.message.includes(String(held.details.expires_at_ms)));
    assert.match(held.message, /until (?:\d{4}-\d{2}-\d{2} )?\d{2}:\d{2} UTC$/u);
    assert.equal(held.reminders[0], `held by ${labels[0]} until ${held.message.split(" until ")[1]}`);
    const shellError = cli("claim", work, "--json");
    assert.notEqual(shellError.status, 0);
    assert.deepEqual(JSON.parse(shellError.stderr).error, held);
    for (const raw of [actor, ...peerSessions]) assert.ok(!JSON.stringify(held).includes(raw));
    const compact = receipt(await reader.call("next", { peek: true }));
    const verbose = receipt(await reader.call("next", { peek: true, verbose: true }));
    assert.ok(!JSON.stringify(compact).includes(peerSessions[0]));
    assert.equal(verbose.session.session_id, readerSession);
    assert.ok(JSON.stringify(verbose).includes(actor));
    receipt(await first.call("update", { work_ref: work, action: "release" }));
    const released = receipt(await reader.call("show", { work_ref: work, history: true }));
    assert.ok(!JSON.stringify(released).includes(actor));
    receipt(await reader.call("claim", { work_ref: work }));
    receipt(await reader.call("note", { work_ref: work, text: "Ready for real target" }));
    const before = receipt(await reader.call("show", { work_ref: work }));
    const targets = [labels[0], before.status.work.assigned_to];
    for (const target of targets) {
      assert.match(target, /^peer-(?:actor-)?[0-9a-f]{24}$/u);
      for (const surface of ["cli", "mcp"]) {
        let error;
        if (surface === "cli") {
          const result = cli("handoff", work, "--to", target, "--json");
          assert.notEqual(result.status, 0);
          error = JSON.parse(result.stderr).error;
        } else {
          error = structuredError(await reader.call("handoff", { work_ref: work, action: "offer", to: target }), "work_invalid");
        }
        assert.match(error.message, /peer display label is not a handoff target/u);
        assert.match(error.reminders[0], /host or coordinator/u);
        assert.deepEqual(error.next, ["engram work next --peek"]);
        assert.deepEqual(receipt(await reader.call("show", { work_ref: work })), before);
      }
    }
    receipt(await reader.call("handoff", { work_ref: work, action: "offer", to: peerSessions[0] }));
    receipt(await first.call("handoff", { work_ref: work, action: "accept" }));
    assert.equal(receipt(await first.call("show", { work_ref: work })).holder, "you");
  } finally {
    try { await closeFixtureClients(...clients); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("done criterion evidence disclosure agrees with frozen show and replay on CLI MCP", async (t) => {
  const engramHome = fixtureHome("engram-criterion-evidence-", t);
  const session = "criterion-reader";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const doneTool = (await client.tools()).find(({ name }) => name === "done");
    assert.match(doneTool.description, /no evidence linked to this criterion/);
    assert.match(doneTool.description, /what is still owed and the command that resolves it/);
    assert.match(doneTool.inputSchema.properties.note.description, /does not link evidence/);
    assert.deepEqual(Object.keys(doneTool.inputSchema.properties).sort(), ["landing", "link_basis", "links", "note", "source_fingerprint", "summary", "work_ref"]);
    assert.match(doneTool.inputSchema.properties.link_basis.description, /Required with links/);
    const help = cliWord(engramHome, session, "done", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout.replace(/\s+/g, " "), /no evidence linked to this criterion/);
    assert.match(help.stdout.replace(/\s+/g, " "), /does not link evidence to individual criteria/);
    for (const surface of ["cli", "mcp"]) {
      const ref = cliJson(engramHome, session, "add", `Criterion disclosure ${surface}`,
        "--accept", "same opening first tail", "--accept", "same opening second tail").work.short_ref;
      cliJson(engramHome, session, "claim", ref);
      cliJson(engramHome, session, "note", ref, "both criteria have work-level discussion");
      cliJson(engramHome, session, "gate", "criterion-check", "--work-ref", ref);
      const complete = async () => surface === "cli"
        ? cliJson(engramHome, session, "done", ref, "Both delivered", "--note", "Shared assertion")
        : receipt(await client.call("done", { work_ref: ref, summary: "Both delivered", note: "Shared assertion" }));
      const first = await complete();
      const expected = {
        criteria_count: 2, unlinked_count: 2, unlinked_positions: [1, 2],
        unlinked_label: "no evidence linked to this criterion", omitted_count: 0,
      };
      assert.equal(first.work.lifecycle, "completed");
      assert.equal(first.acceptance_criteria_asserted, 2);
      assert.equal(first.acceptance_criteria_changed, false);
      assert.deepEqual(first.acceptance_evidence, expected);
      cliJson(engramHome, session, "note", ref, "Late evidence does not rewrite the seal");
      const replay = await complete();
      assert.equal(replay.seal, first.seal);
      assert.deepEqual(replay.acceptance_evidence, expected);
      assert.deepEqual(cliJson(engramHome, session, "show", ref).acceptance_evidence, expected);
      assert.deepEqual(receipt(await client.call("show", { work_ref: ref })).acceptance_evidence, expected);
      for (const command of [["show", ref], ["done", ref, "Both delivered", "--note", "Shared assertion"]]) {
        const text = cliWord(engramHome, session, ...command);
        assert.equal(text.status, 0, text.stderr);
        assert.match(text.stdout, /criterion 1: no evidence linked to this criterion/);
        assert.match(text.stdout, /criterion 2: no evidence linked to this criterion/);
        assert.doesNotMatch(text.stdout, /no evidence exists/);
      }
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}
