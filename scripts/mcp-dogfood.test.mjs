#!/usr/bin/env node

import assert from "node:assert/strict";

import { fixtureHome, removeFixtureHomes, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import test, { after } from "node:test";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

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

function shortRef(workId) {
  assert.match(workId, /^[0-9a-f-]{36}$/u);
  return `w-${workId.replaceAll("-", "").slice(20)}`;
}

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
      }, 15000);
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
    // Catch the former 14s pathology; precise bounds live in Rust decode/statement-count regressions.
    assert.ok(elapsed < 10000, `${name} took ${elapsed.toFixed(1)}ms; sanity limit is 10000ms`);
    return result;
  }

  async close() {
    const started = performance.now();
    if (!this.child.stdin.destroyed) this.child.stdin.end();
    let timer;
    try {
      const { code, signal } = await Promise.race([
        this.closed,
        new Promise((_, reject) => {
          timer = setTimeout(() => {
            const diagnostic = `exitCode=${this.child.exitCode} signalCode=${this.child.signalCode} elapsed=${(performance.now() - started).toFixed(1)}ms`;
            this.child.kill();
            reject(new Error(`MCP server did not close (${diagnostic}): ${this.stderr}`));
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

function assertRecordParity(actual, expected) {
  const copy = structuredClone(actual);
  const actualWindow = copy.notes_window ?? copy.history?.window;
  const expectedWindow = expected.notes_window ?? expected.history?.window;
  if (expectedWindow) {
    assert.ok(actualWindow);
    // Independent reads reflect their own observation instant. Everything
    // else (including membership, cut position/expiry and output) stays exact.
    assert.ok(Number.isFinite(Date.parse(expectedWindow.read_cut.observed_at)));
    assert.ok(Date.parse(actualWindow.read_cut.observed_at) >= Date.parse(expectedWindow.read_cut.observed_at));
    if (expectedWindow.after) {
      const decode = (token) => {
        assert.match(token, /^s1-[0-9a-f]+$/u);
        return JSON.parse(Buffer.from(token.slice(3), "hex").toString("utf8"));
      };
      const actualCursor = decode(actualWindow.after);
      const expectedCursor = decode(expectedWindow.after);
      assert.deepEqual(actualCursor.cut, actualWindow.read_cut);
      assert.deepEqual(expectedCursor.cut, expectedWindow.read_cut);
      actualCursor.cut.observed_at = expectedCursor.cut.observed_at;
      assert.deepEqual(actualCursor, expectedCursor);
      copy.next = copy.next.map((command) => command.replace(actualWindow.after, expectedWindow.after));
      actualWindow.after = expectedWindow.after;
    }
    actualWindow.read_cut.observed_at = expectedWindow.read_cut.observed_at;
  }
  assert.deepEqual(copy, expected);
}

function structuredError(result, code) {
  assert.equal(result.isError, true, JSON.stringify(result));
  assert.equal(result.structuredContent.error.code, code);
  return result.structuredContent.error;
}

async function wait(milliseconds) {
  await new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

test("compact mutation wire carries one item and shrinks the full-context fixture", async (t) => {
  const engramHome = fixtureHome("engram-mcp-economy-", t);
  const session = "economy-agent";
  let client;
  let failure;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const title = "Fixed unique receipt title";
    const outcome = "Durable full outcome. ".repeat(40).trim();
    const acceptance = ["Durable full acceptance. ".repeat(20).trim()];
    const added = await client.call("add", { title, outcome, acceptance });
    const work_ref = receipt(added).work.short_ref;
    let compactTotal = 0;
    let fullTotal = 0;
    const measure = (operation, response) => {
      const value = receipt(response);
      assert.equal(value.operation, operation);
      assert.deepEqual(Object.keys(value.work).sort(), ["lifecycle", "revision", "short_ref", "title"]);
      assert.equal(value.work.short_ref, work_ref);
      assert.equal(value.work.title, title);
      const encoded = JSON.stringify(value);
      assert.equal(encoded.split(title).length - 1, 1);
      assert.equal(encoded.match(/"title":/gu)?.length, 1);
      for (const field of ["focus", "status", "history", "parent", "receipt", "control_binding", "allowed_next"]) {
        assert.equal(value[field], undefined, field);
      }
      assert.equal(encoded.match(/"full_detail":/gu)?.length, 1);
      assert.match(value.full_detail, /^engram work show 'w-[0-9a-f]{12}'/u);
      assert.ok(!value.next.includes(value.full_detail));
      const text = response.content.filter(({ type }) => type === "text");
      assert.equal(text.length, 1);
      assert.deepEqual(JSON.parse(text[0].text), value);
      const focus = cliWord(engramHome, session, "core", "focus", work_ref);
      assert.equal(focus.status, 0, focus.stderr);
      const core = JSON.parse(focus.stdout);
      assert.equal(core.status.work.short_ref, work_ref);
      assert.equal(core.status.work.revision, value.work.revision);
      assert.equal(core.status.work.lifecycle, value.work.lifecycle);
      // Independent item-read lower bound, not a reconstruction using the
      // compact payload. Rust separately measures actual core result + focus.
      const full = cliJson(engramHome, session, "show", work_ref);
      assert.equal(full.status.work.short_ref, work_ref);
      // Terse show omits revision; the separate core read above pins it.
      assert.equal(full.status.work.title, value.work.title);
      assert.equal(full.status.work.lifecycle, value.work.lifecycle);
      const fullWire = { structuredContent: full, content: [{ type: "text", text: JSON.stringify(full) }], isError: false };
      const compactBytes = Buffer.byteLength(JSON.stringify(response));
      const fullBytes = Buffer.byteLength(JSON.stringify(fullWire));
      assert.ok(compactBytes < fullBytes, `${operation}: ${compactBytes} < ${fullBytes}`);
      assert.ok(Buffer.byteLength(text[0].text) < 12288);
      compactTotal += compactBytes;
      fullTotal += fullBytes;
      t.diagnostic(`${operation}: independent show lower-bound wire=${fullBytes}, compact wire=${compactBytes}, structured=${Buffer.byteLength(encoded)}, MCP text=${Buffer.byteLength(text[0].text)}`);
      return value;
    };
    measure("add", added);
    const claimed = measure("claim", await client.call("claim", { work_ref, ttl_seconds: 7200 }));
    assert.equal(claimed.claim.holder, "you");
    assert.ok(Date.parse(claimed.claim.held_until));
    assert.doesNotMatch(JSON.stringify(claimed), /fence|control_binding/u);
    const gated = measure("gate", await client.call("gate", { work_ref, name: "fixed-gate", failed: ["fixed::case"], evidence_ref: "test:fixed" }));
    assert.deepEqual(gated.gate, { name: "fixed-gate", passed: false, failed_count: 1, referenced: true });
    const body = "Durable full note body. ".repeat(30).trim();
    measure("note", await client.call("note", { work_ref, text: body }));
    const notes = receipt(await client.call("show", { work_ref, notes: true }));
    assert.ok(notes.notes.some(({ summary }) => summary === body));
    const gates = receipt(await client.call("show", { work_ref, notes: true, gates: true }));
    assert.ok(gates.notes.some(({ summary }) => summary.includes("fixed::case")));
    assert.deepEqual(cliJson(engramHome, session, "show", work_ref).status, receipt(await client.call("show", { work_ref })).status);
    const done = measure("done", await client.call("done", { work_ref, summary: "Fixed delivery" }));
    assert.equal(done.work.lifecycle, "completed");
    assert.match(done.seal, HASH);
    assert.equal(done.claim, undefined);
    assert.ok(compactTotal < fullTotal);
    t.diagnostic(`five-operation aggregate: independent show lower-bound wire=${fullTotal}, compact wire=${compactTotal}`);
  } catch (error) {
    failure = error;
  } finally {
    try {
      try { await client?.close(); } catch (error) {
        failure = failure ? new AggregateError([failure, error], "economy fixture and close failed") : error;
      }
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
  if (failure) throw failure;
});

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
      assert.ok(Buffer.byteLength(text) <= 12288);
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
    assert.ok(peerNote.includes("held by another session until "));
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
      assert.ok(Buffer.byteLength(output.stdout) <= 12288);
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

test("required successor resolution agrees across CLI, MCP, listing and done", async (t) => {
  const engramHome = fixtureHome("engram-mcp-successor-", t);
  let client;
  let phase = "initialize";
  let failure;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "successor-reader");
    await client.initialize();
    const parent = receipt(await client.call("add", { title: "Parent" })).work.short_ref;
    const child = receipt(await client.call("add", { title: "Original required", under: parent })).work.short_ref;
    const successor = receipt(await client.call("add", { title: "Required successor", under: parent })).work.short_ref;
    receipt(await client.call("update", { work_ref: child, action: "supersede", replacement: successor, reason: "This sibling owns delivery" }));
    const check = async (resolved) => {
      for (const notes of [false, true]) {
        phase = `show resolved=${resolved} notes=${notes}`;
        const value = receipt(await client.call("show", { work_ref: child, notes }));
        assert.equal(value.status.work.child_resolution.ref, successor);
        assert.equal(value.status.work.child_resolution.disposition, resolved ? "resolved_by_successor" : "owed");
        const flags = ["show", child, ...(notes ? ["--notes"] : [])];
        assertRecordParity(cliJson(engramHome, "successor-reader", ...flags), value);
        const text = spawnSync(binary, ["--home", engramHome, "work", "--actor-id", "successor-reader", "--session-id", "successor-reader", ...flags], { cwd: root, encoding: "utf8" });
        assert.equal(text.status, 0, text.stderr);
        assert.ok(text.stdout.includes(resolved ? `resolved by successor ${successor} (completed)` : `successor ${successor} (open)`));
        assert.ok(Buffer.byteLength(text.stdout) <= 12288);
        assert.doesNotMatch(JSON.stringify(value), HASH);
      }
      for (const verbose of [false, true]) {
        phase = `listing resolved=${resolved} verbose=${verbose}`;
        const listing = receipt(await client.call("ls", { under: parent, required: true, all: true, verbose }));
        assert.deepEqual(cliJson(engramHome, "successor-reader", "ls", "--under", parent, "--required", "--all", ...(verbose ? ["--verbose"] : [])), listing);
        const item = listing.items.map((row) => verbose ? row.work : row).find((row) => (verbose ? row.short_ref : row.ref) === child);
        assert.equal(item.child_resolution.disposition, resolved ? "resolved_by_successor" : "owed");
      }
      const focus = receipt(await client.call("show", { work_ref: parent }));
      assert.equal(focus.child_obligations.required_owed.count, resolved ? 0 : 2);
      assert.equal(focus.children.find((row) => row.short_ref === child).child_resolution.disposition, resolved ? "resolved_by_successor" : "owed");
    };
    await check(false);
    phase = "parent refusal";
    receipt(await client.call("claim", { work_ref: parent }));
    const owed = receipt(await client.call("done", { work_ref: parent, summary: "Not ready yet" }));
    assert.equal(owed.code, "required_child_unsealed");
    assert.equal(owed.recovery.cause.kind, "required_child_unsealed");
    assert.equal(owed.recovery.item.ref, child);
    assert.equal(owed.recovery.item.state, "superseded");
    assert.deepEqual(owed.next, [`engram work update ${parent} --waive ${child} --reason "account for disposed required child"`]);
    assert.equal(owed.seal, undefined);
    const shownResolution = receipt(await client.call("show", { work_ref: child })).status.work.child_resolution;
    assert.deepEqual(owed.recovery.item.child_resolution, shownResolution);
    const successorLine = `successor ${successor} (${shownResolution.lifecycle}): ${shownResolution.reason}`;
    assert.ok(owed.reminders.some((line) => line.includes(successorLine)));
    for (const json of [false, true]) {
      phase = `CLI parent refusal json=${json}`;
      const refused = cliWord(engramHome, "successor-reader", "done", parent, "Still owed", ...(json ? ["--json"] : []));
      assert.equal(refused.status, 2, refused.stderr);
      assert.ok(Buffer.byteLength(refused.stdout) <= 12288);
      if (json) {
        const cliRefusal = JSON.parse(refused.stdout);
        // Repeating completion capture renews the held claim. Check its
        // current authority, then compare every non-time receipt field exactly.
        assert.ok(Date.parse(cliRefusal.claim.held_until) >= Date.parse(owed.claim.held_until));
        assert.equal(cliRefusal.claim.held_until, cliJson(engramHome, "successor-reader", "show", parent).held_until);
        cliRefusal.claim.held_until = owed.claim.held_until;
        assert.deepEqual(cliRefusal, owed);
      }
      else assert.ok(refused.stdout.includes(successorLine));
    }
    phase = "successor completion";
    receipt(await client.call("claim", { work_ref: successor }));
    assert.match(receipt(await client.call("done", { work_ref: successor, summary: "Replacement delivered" })).seal, HASH);
    assert.equal(receipt(await client.call("show", { work_ref: successor })).status.work.lifecycle, "completed");
    await check(true);
    phase = "parent completion";
    assert.match(receipt(await client.call("done", { work_ref: parent, summary: "Delivered without waiver" })).seal, HASH);
    assert.equal(receipt(await client.call("show", { work_ref: parent })).status.work.lifecycle, "completed");
  } catch (error) {
    failure = new Error(`successor test failed during ${phase}`, { cause: error });
  } finally {
    try {
      if (client) {
        try { await client.close(); }
        catch (error) { failure = failure ? new AggregateError([failure, error], "successor test and cleanup failed") : error; }
      }
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
  if (failure) throw failure;
});

test("required child rejection and acceptance assertion agree through CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-rejection-", t);
  const session = "rejection-agent";
  let client;
  try {
    buildAndInit(engramHome);
    const parent = cliJson(engramHome, session, "add", "Accepted delivery", "--accept", "first delivered", "--accept", "second delivered").work.short_ref;
    client = new McpClient(engramHome, session);
    await client.initialize();
    for (const surface of ["cli", "mcp"]) {
      const child = receipt(await client.call("add", { title: `Rejected ${surface}`, under: parent })).work.short_ref;
      receipt(await client.call("note", { work_ref: child, text: "Evidence refutes the finding" }));
      const rejected = surface === "cli"
        ? cliJson(engramHome, session, "update", child, "--reject", "Evidence disproves finding")
        : receipt(await client.call("update", { work_ref: child, action: "reject", reason: "Evidence disproves finding" }));
      assert.equal(rejected.operation, "reject");
      assert.equal(rejected.receipt.result.lifecycle, "cancelled");
      assert.equal(rejected.receipt.result.parent_ref, parent);
      assert.equal(rejected.receipt.result.required_child_waived, true);
      const shown = receipt(await client.call("show", { work_ref: child }));
      assert.equal(shown.status.work.lifecycle, "cancelled");
      assert.deepEqual(shown, cliJson(engramHome, session, "show", child));
    }
    const optional = cliJson(engramHome, session, "add", "Optional rejection refused", "--under", parent, "--optional").work.short_ref;
    const refused = await client.call("update", { work_ref: optional, action: "reject", reason: "not a required barrier" });
    assert.equal(refused.isError, true);
    const error = structuredError(refused, "work_reject_refused");
    assert.equal(error.details.child_ref, optional);
    assert.equal(error.details.parent_ref, parent);
    assert.ok(error.details.remedy.includes(`update ${optional} --cancel`));
    assert.ok(error.details.remedy.includes(`update ${parent} --waive ${optional}`));
    assert.equal(receipt(await client.call("show", { work_ref: optional })).status.work.lifecycle, "open");
    cliJson(engramHome, session, "claim", parent);
    const completed = receipt(await client.call("done", { work_ref: parent, summary: "Both delivered; findings rejected on evidence" }));
    assert.equal(completed.acceptance_criteria_asserted, 2);
    assert.equal(completed.acceptance_criteria_changed, false);
    const replay = cliJson(engramHome, session, "done", parent, "Both delivered; findings rejected on evidence");
    assert.equal(replay.acceptance_criteria_asserted, 2);
    assert.equal(replay.acceptance_criteria_changed, false);
    assert.equal(replay.seal, completed.seal);
    assert.equal(receipt(await client.call("show", { work_ref: parent })).child_obligations.required_owed.count, 0);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
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
        const shellRow = shell.assigned.find(row => row.ref === reference);
        assert.equal(shellRow.external_ref, external);
        assert.equal(shellRow.current_status.body_or_first_line, body);
        client = new McpClient(engramHome, session, undefined, actor);
        await client.initialize();
        const resumed = receipt(await client.call("next"));
        const row = resumed.assigned.find(row => row.ref === reference);
        assert.deepEqual(row.current_status, shellRow.current_status);
        assert.equal(row.current_status.by, replacement ? "another session" : "you");
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
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

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

test("decomposition retry survives parent reread and process restart without duplicate notes", async (t) => {
  const engramHome = fixtureHome("engram-decomposition-retry-", t);
  const session = "decomposition-reader";
  let client;
  let failure;
  try {
    buildAndInit(engramHome);
    // Separate CLI processes exercise both the printed session binding and
    // the child word's automatic rebind from child focus back to its parent.
    const parent = cliJson(engramHome, session, "add", "CLI parent").work.short_ref;
    const first = cliJson(engramHome, session, "add", "CLI child", "--under", parent, "--note", "Initial CLI note");
    const after = cliJson(engramHome, session, "show", parent);
    const replay = cliJson(engramHome, session, "add", "CLI child", "--under", parent, "--note", "Initial CLI note");
    assert.deepEqual(replay, first);
    assert.deepEqual(cliJson(engramHome, session, "show", parent), after);
    assert.deepEqual(cliJson(engramHome, session, "show", first.work.short_ref, "--notes").notes.map(({ summary }) => summary), ["Initial CLI note"]);
    assert.equal(cliJson(engramHome, session, "ls", "--under", parent).total, 1);

    client = new McpClient(engramHome, session);
    await client.initialize();
    const mcpParent = receipt(await client.call("add", { title: "MCP parent" })).work.short_ref;
    const input = { title: "MCP child", under: mcpParent, notes: ["Initial MCP note"] };
    const created = receipt(await client.call("add", input));
    const parentAfter = receipt(await client.call("show", { work_ref: mcpParent }));
    await client.close();
    client = undefined;
    client = new McpClient(engramHome, session);
    await client.initialize();
    assert.deepEqual(receipt(await client.call("add", input)), created);
    assert.deepEqual(receipt(await client.call("show", { work_ref: mcpParent })), parentAfter);
    assert.deepEqual(receipt(await client.call("show", { work_ref: created.work.short_ref, notes: true })).notes.map(({ summary }) => summary), ["Initial MCP note"]);
    assert.equal(receipt(await client.call("ls", { under: mcpParent })).total, 1);
    const changed = receipt(await client.call("add", { ...input, title: "Different MCP intent" }));
    assert.notEqual(changed.work.short_ref, created.work.short_ref);
    assert.equal(receipt(await client.call("ls", { under: mcpParent })).total, 2);
    receipt(await client.call("update", { work_ref: mcpParent, action: "revise", title: "Changed MCP parent" }));
    const refusal = await client.call("add", input);
    const error = structuredError(refusal, "work_decomposition_retry_conflict");
    assert.equal(error.details.parent_ref, mcpParent);
    assert.match(error.message, /parent planning state changed/u);
    assert.deepEqual(error.next, [`engram work show ${mcpParent}`]);
    assert.match(error.details.remedy, /different child intent/u);
    for (const content of refusal.content.filter(({ type }) => type === "text")) {
      assert.deepEqual(JSON.parse(content.text), refusal.structuredContent);
      assert.doesNotMatch(content.text, HASH);
      assert.doesNotMatch(content.text, /idempotency|auto:/u);
    }
    cliJson(engramHome, session, "update", parent, "--title", "Changed CLI parent");
    for (const json of [false, true]) {
      const refused = cliWord(engramHome, session, "add", "CLI child", "--under", parent, "--note", "Initial CLI note", ...(json ? ["--json"] : []));
      assert.equal(refused.status, 1, refused.stderr);
      const output = refused.stdout + refused.stderr;
      assert.doesNotMatch(output, HASH);
      assert.doesNotMatch(output, /idempotency|auto:/u);
      assert.ok(output.includes(`engram work show ${parent}`));
      assert.match(output, /parent planning state changed/u);
      if (json) {
        assert.equal(refused.stdout, "");
        assert.equal(JSON.parse(refused.stderr).error.code, error.code);
      }
    }
  } catch (error) {
    failure = error;
  } finally {
    try {
      if (client) {
        try { await client.close(); }
        catch (error) { failure = failure ? new AggregateError([failure, error], "decomposition retry and cleanup failed") : error; }
      }
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
  if (failure) throw failure;
});

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
    await client.call("add", { title: "Advance cut" });
    const stale = structuredError(await client.call("ls", { ...args, after: first.after }), "work_catalog_cursor_invalid");
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
    assert.equal(preview.note_session_id, session);
    assert.equal(next.context_generation, "termal-after");
    assert.ok(next.read_cut.project_position > olderCut.project_position);
    assert.ok(Date.parse(next.read_cut.observed_at) >= Date.parse(olderCut.observed_at));
    assert.equal(stale.participated.find((row) => row.ref === work).note, "Earlier shell observation");
    assert.equal(stale.context_generation, "termal-before");
    const text = cliText(engramHome, session, "next", "--context-generation", "termal-after");
    assert.ok(text.includes(`[note session ${session}] — Newer MCP observation`));
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
    assert.equal(earlier.actor_session_id, session);
    assert.equal(newer.actor_session_id, session);
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
    assert.equal(ownPeerNote.actor_session_id, "other-own-session");
    assert.ok(ownPeerNote.feed_position > newer.feed_position);
    const ownPeerNext = receipt(await ownPeer.call("next"));
    assert.equal(ownPeerNext.participated.find((row) => row.ref === work).note_session_id, "other-own-session");
    cliJson(engramHome, "peer-session", "note", work, "Still newer peer observation");
    const afterPeer = cliJson(engramHome, session, "next");
    assert.equal(afterPeer.participated.find((row) => row.ref === work).note, "Newer MCP observation");
    assert.equal(afterPeer.participated.find((row) => row.ref === work).note_session_id, session);
    const peer = cliJson(engramHome, "peer-session", "next");
    assert.equal(peer.participated.find((row) => row.ref === work).note_session_id, "peer-session");
    assert.equal(peer.participated.find((row) => row.ref === work).note, "Still newer peer observation");
    const peerNote = cliJson(engramHome, session, "show", work, "--notes").notes.at(-1);
    assert.equal(Object.hasOwn(peerNote, "actor_session_id"), false);
    assert.equal(cliJson(engramHome, "peer-session", "show", work, "--notes").notes.at(-1).actor_session_id, "peer-session");
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
    assert.ok(value.assigned.some((row) => row.holder === "another session"));
    assert.deepEqual(value.participated.map((row) => row.ref), participated.toReversed().slice(0, 5));
    assert.equal(value.participated_omitted, 1);
    assert.equal(value.participated[0].note, "Own finding 5");
    assert.ok(value.participated.every((row) => row.holder === "another session"));
    const cli = cliJson(engramHome, "coordinator", "next");
    for (const key of ["assigned", "participated", "participated_omitted"]) assert.deepEqual(cli[key], value[key]);
    const text = cliText(engramHome, "coordinator", "next");
    const headings = ["held by you (0 shown):", "assigned (2 shown):", "participated (5 shown):", "ready (1 shown):"];
    const positions = headings.map((heading) => text.indexOf(heading));
    assert.ok(positions.every((position) => position >= 0), text);
    assert.deepEqual(positions, positions.toSorted((a, b) => a - b));
    assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12 * 1024);
    assert.ok(Buffer.byteLength(text) < 12 * 1024);
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
      assert.ok(Buffer.byteLength(cli.stdout) <= 12 * 1024);
      const text = spawnSync(binary, [...context, ...args], { cwd: root, encoding: "utf8" });
      assert.equal(text.status, 0, text.stderr);
      assert.ok(Buffer.byteLength(text.stdout) <= 12 * 1024);
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
      assert.ok(Buffer.byteLength(JSON.stringify(page, null, 2)) < 12288);
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
      assert.ok(Buffer.byteLength(text.stdout) <= 12288);
      assert.equal(text.stdout.match(/gate evidence:/gu).length, 1);
      for (const [family, total] of Object.entries({ notes: 1, observations: 1, gates: 9 })) {
        const shown = value.notes.filter((row) => row.family === family).length;
        assert.deepEqual(value.notes_window.families[family], { total, shown, omitted: total - shown });
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
      assert.ok(rows.some(({ by }) => by === "another actor (host-context-1)"));
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
      assert.ok(Buffer.byteLength(result.stdout) <= 12 * 1024);
      return result.stdout;
    };
    assert.equal(JSON.parse(cli(["show", work, "--json"])).status.work.acceptance[0], criterion);
    assert.ok(cli(["show", work]).includes(`  - ${criterion}\n`));
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

test("detach exposes the same remedy and independent root through MCP", async (t) => {
  const engramHome = fixtureHome("engram-mcp-detach-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "detacher");
    await client.initialize();
    const parent = receipt(await client.call("add", { title: "Parent" })).work.short_ref;
    const child = receipt(await client.call("add", { title: "Follow-up", under: parent, optional: true, notes: ["Original observation"] })).work.short_ref;
    receipt(await client.call("claim", { work_ref: parent }));
    const completed = await client.call("done", { work_ref: parent, summary: "Parent delivered" });
    const completedValue = receipt(completed);
    const history = receipt(await client.call("show", { work_ref: parent })).history;
    const command = `engram work update ${child} --detach "Continue as independent work"`;
    assert.deepEqual(completedValue.child_obligations.open_optional, {
      count: 1,
      items: [{ ref: child, title: "Follow-up", remedy: command }],
      omitted: 0,
      navigation: `engram work show ${parent}`,
    });
    assert.deepEqual(JSON.parse(completed.content[0].text), completedValue);
    assert.equal(receipt(await client.call("show", { work_ref: child })).next[0], command);
    assert.equal(receipt(await client.call("next", {})).next[0], command);
    const blocked = await client.call("ls", { blocked: true });
    const blockedValue = receipt(blocked);
    assert.equal(blockedValue.total, 1);
    assert.equal(blockedValue.items[0].blocked_reason, "parent completed");
    assert.equal(blockedValue.items[0].remedy, command);
    // MCP text is the JSON fallback, not the CLI's terminal rendering.
    assert.deepEqual(JSON.parse(blocked.content[0].text), blockedValue);
    structuredError(await client.call("update", { work_ref: child, action: "detach" }), "work_invalid");
    const result = receipt(await client.call("update", { work_ref: child, action: "detach", reason: "Independent follow-up" }));
    const successor = result.receipt.work_ref;
    assert.notEqual(successor, child);
    assert.equal(result.next[0], `engram work claim ${successor}`);
    const successorView = receipt(await client.call("show", { work_ref: successor, notes: true }));
    assert.deepEqual(successorView.detached_from, { ref: child, reason: "Independent follow-up" });
    assert.ok(successorView.next.includes(`engram work show ${child}`));
    assert.deepEqual(successorView.notes, []);
    assert.equal(receipt(await client.call("show", { work_ref: child, notes: true })).notes[0].summary, "Original observation");
    assert.deepEqual(receipt(await client.call("show", { work_ref: parent })).history, history);
    assert.equal(receipt(await client.call("show", { work_ref: child })).status.work.superseded_by, successor);
    structuredError(await client.call("update", { work_ref: child, action: "detach", reason: "Independent follow-up" }), "work_detach_refused");
    assert.equal(receipt(await client.call("ls", { all: true })).total, 3);
    receipt(await client.call("claim", { work_ref: successor }));
    receipt(await client.call("done", { work_ref: successor, summary: "Follow-up delivered" }));
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("running build identity agrees across version, CLI next, doctor and retained MCP next", async (t) => {
  const engramHome = fixtureHome("engram-build-identity-", t);
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
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("Phoenix atomic initial notes and peer child proposals through MCP", async (t) => {
  const engramHome = fixtureHome("engram-mcp-creation-", t);
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
    try {
      await closeFixtureClients(peer, holder);
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("Phoenix full notes, defaulted acceptance and terminal-parent remedy through MCP", async (t) => {
  const engramHome = fixtureHome("engram-phoenix-notes-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "notes-reader");
    await client.initialize();
    const showTool = (await client.tools()).find(({ name }) => name === "show");
    assert.ok(showTool.inputSchema.properties.notes);
    const added = receipt(await client.call("add", { title: "Full MCP notes" }));
    assert.ok(added.reminders.includes("acceptance defaulted to the title being done; set --accept"));
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
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("Phoenix planning revisions and exact list counts through MCP", async (t) => {
  const engramHome = fixtureHome("engram-phoenix-planning-", t);
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
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
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

test("CLI words translate the same ambient lifecycle service", (t) => {
  const engramHome = fixtureHome("engram-work-cli-", t);
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
    assert.match(added, /^added w-[0-9a-f]{12} "Dogfood work CLI" \[open; revision \d+\]\n/u);
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
    assert.doesNotMatch(shown, new RegExp(`^\\s*engram work show ${workRef}\\s*$`, "mu"));
    assert.match(shown, new RegExp(`^  engram work show ${workRef} --history$`, "mu"));

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
    assert.match(notedJson.evidence, HASH);
    assert.ok(Array.isArray(notedJson.next));
    assert.equal(notedJson.full_detail, `engram work show '${workRef}' --notes`);

    const done = cliText(engramHome, actor, "done");
    assert.match(done, /^done w-[0-9a-f]{12} "Dogfood work CLI" \[completed; revision \d+\]\nasserted 1 acceptance criterion satisfied; completion changed no criterion\nfull detail: engram work show 'w-[0-9a-f]{12}'\nreminders: none\nnext:\n/u);
    assert.match(done, /\s+engram work next/u);
    const doneJson = cliJson(engramHome, actor, "done");
    assert.match(doneJson.seal, HASH);
    assert.equal(doneJson.acceptance_criteria_asserted, 1);
    assert.equal(doneJson.acceptance_criteria_changed, false);
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
    removeFixtureHomes(engramHome);
  }
});

test("two MCP sessions complete ambient work through a fenced handoff", async (t) => {
  const engramHome = fixtureHome("engram-work-dogfood-", t);
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
      shortRef(supersedeReceipt.receipt.result.superseded_by),
      mcpReplacement.short_ref,
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
    assert.equal(shortRef(next.session.focused_work_id), added.work.short_ref);
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
    const keylessDetail = receipt(await a.call("show", { work_ref: keyless.work.short_ref }));
    assert.equal(keylessDetail.status.work.outcome, "Keyless root");
    assert.deepEqual(keylessDetail.status.work.acceptance, ["Keyless root is done"]);
    const keylessReplay = receipt(await a.call("add", { title: "Keyless root" }));
    assert.equal(keylessReplay.work.short_ref, keyless.work.short_ref);
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
    assert.equal(claimed.work.short_ref, added.work.short_ref);
    assert.equal(claimed.claim.holder, "you");
    assert.equal(typeof claimed.claim.held_until, "string");
    assert.equal(claimed.control_binding, undefined);
    const liveCore = cliWord(engramHome, sessionA, "core", "focus", workRef);
    assert.equal(liveCore.status, 0, liveCore.stderr);
    assert.ok(JSON.parse(liveCore.stdout).control_binding);
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
    assert.equal(replayedClaim.work.short_ref, added.work.short_ref);
    assert.equal("focus" in replayedClaim, false);
    assert.equal(typeof replayedClaim.obligations.open, "number");
    assert.equal(typeof replayedClaim.obligations.omitted, "number");
    assert.ok(Array.isArray(replayedClaim.next));
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
    assert.match(noted.evidence, HASH);
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
    assert.equal(observation.checkpoint, undefined);
    assert.match(observation.evidence, HASH);
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
    assert.equal(seal.work.short_ref, added.work.short_ref);
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
    assert.equal(sameSessionLate.checkpoint, undefined);
    assert.match(sameSessionLate.evidence, HASH);
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
    assert.equal(lateNote.checkpoint, undefined);
    assert.match(lateNote.evidence, HASH);
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
    assert.deepEqual(afterLateFindings.next, [
      `engram work note ${workRef} "…"`,
      `engram work show ${workRef} --history`,
    ]);
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
      verboseCompletedCatalog.items[0].work.short_ref,
      added.work.short_ref,
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
    assert.equal(shortRef(cancelled.receipt.work_id), disposable.short_ref);
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
    assert.equal(compactSeal.work.short_ref, compact.short_ref);
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
    assert.equal(singleChild.parent_ref, keyless.work.short_ref);
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
    assert.equal(optionalChild.child_requirement, "optional");
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
    assert.equal(shortRef(waiver.receipt.work_id), waiverParent.short_ref);
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
    try {
      await closeFixtureClients(a, b);
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

test("MCP actor context stays attribution-only across words and handoff", async (t) => {
  const engramHome = fixtureHome("engram-mcp-actor-context-", t);
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
    try {
      await closeFixtureClients(author, assignee, recipient);
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});
