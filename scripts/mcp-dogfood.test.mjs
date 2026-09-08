#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";

import { fixtureHome as ownedFixtureHome, removeFixtureHomes as cleanupFixtureHomes, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { existsSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawn, spawnSync } from "node:child_process";
import nodeTest, { after } from "node:test";

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
const SOFT_TIMING_MS = 2000;
const fixtureTimings = new Map();
const testTimings = new WeakMap();

function walBytes(home) {
  try {
    return readdirSync(join(home, "projects"), { withFileTypes: true })
      .filter((entry) => entry.isDirectory())
      .reduce((total, entry) => {
        try { return total + statSync(join(home, "projects", entry.name, "engram.db-wal")).size; }
        catch (error) { if (error.code === "ENOENT") return total; throw error; }
      }, 0);
  } catch { return null; }
}

function timingLine(kind, name, elapsed, bytes) {
  if (elapsed < SOFT_TIMING_MS) return undefined;
  return `MCP timing: ${kind}=${JSON.stringify(name)} elapsed_ms=${elapsed.toFixed(1)} soft_threshold_ms=${SOFT_TIMING_MS} wal_bytes=${bytes ?? "unavailable"} (sampled floor; close may truncate WAL)`;
}

function test(name, body) {
  return nodeTest(name, async (context) => {
    const timing = { started: performance.now(), homes: new Map() };
    testTimings.set(context, timing);
    try { return await body(context); }
    finally {
      const sizes = [...timing.homes.values()];
      const bytes = sizes.length && sizes.every((size) => size !== null)
        ? sizes.reduce((total, size) => total + size, 0) : null;
      const line = timingLine("test", name, performance.now() - timing.started, bytes);
      if (line) console.error(`${line} wal_sample=maximum_observed`);
      for (const home of timing.homes.keys()) fixtureTimings.delete(home);
    }
  });
}

function fixtureHome(prefix, context) {
  const home = ownedFixtureHome(prefix, context);
  const timing = testTimings.get(context);
  if (timing) { timing.homes.set(home, null); fixtureTimings.set(home, timing); }
  return home;
}

function removeFixtureHomes(...homes) {
  for (const home of homes) recordWalSample(home);
  cleanupFixtureHomes(...homes);
}

function recordWalSample(home) {
  const bytes = walBytes(home);
  const timing = fixtureTimings.get(home);
  if (timing && bytes !== null) timing.homes.set(home, Math.max(timing.homes.get(home) ?? 0, bytes));
  return bytes;
}

test("slow MCP diagnostics preserve the soft threshold and unknown WAL state", () => {
  assert.equal(timingLine("call", "note", SOFT_TIMING_MS - 1, 42), undefined);
  assert.equal(timingLine("call", "note", SOFT_TIMING_MS, 42), 'MCP timing: call="note" elapsed_ms=2000.0 soft_threshold_ms=2000 wal_bytes=42 (sampled floor; close may truncate WAL)');
  assert.match(timingLine("test", "fixture", SOFT_TIMING_MS, null), /wal_bytes=unavailable /);
});

function shortRef(workId) {
  assert.match(workId, /^[0-9a-f-]{36}$/u);
  return `w-${workId.replaceAll("-", "").slice(20)}`;
}

class McpClient {
  constructor(engramHome, sessionId, actorContext, actorId = sessionId) {
    this.engramHome = engramHome;
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
    let result;
    let elapsed;
    try {
      result = await this.request("tools/call", { name, arguments: arguments_ });
    } finally {
      elapsed = performance.now() - started;
      if (elapsed >= SOFT_TIMING_MS) console.error(timingLine("call", name, elapsed, recordWalSample(this.engramHome)));
    }
    // Catch the former 14s pathology; precise bounds live in Rust decode/statement-count regressions.
    assert.ok(elapsed < 10000, `${name} took ${elapsed.toFixed(1)}ms; sanity limit is 10000ms`);
    return result;
  }

  async close() {
    // Closing the last SQLite owner may checkpoint/remove the WAL. Capture
    // it before EOF as well as on slow calls and before fixture cleanup.
    recordWalSample(this.engramHome);
    const started = performance.now();
    if (!this.child.stdin.destroyed) this.child.stdin.end();
    const waitForClose = async (milliseconds) => {
      let timer;
      try {
        return await Promise.race([
          this.closed,
          new Promise(resolvePromise => { timer = setTimeout(resolvePromise, milliseconds); }),
        ]);
      } finally {
        clearTimeout(timer);
      }
    };
    // Locked rmcp 3.1.4 may spend 5 s draining responses after stdin EOF.
    // A watchdog at that bound races legitimate drain completion; leave headroom for
    // store close and runtime shutdown under host I/O contention.
    const closed = await waitForClose(10000);
    if (!closed) {
      // The watchdog remains a failure, even if diagnostic observation
      // later sees a clean exit. Never convert this window into a passing retry.
      const exceededAt = performance.now();
      const diagnostic = `pid=${this.child.pid} exitCode=${this.child.exitCode} signalCode=${this.child.signalCode} elapsed=${(exceededAt - started).toFixed(1)}ms stdinFinished=${this.child.stdin.writableFinished} stdinDestroyed=${this.child.stdin.destroyed} pending=${this.pending.size}`;
      const sampledAt = performance.now();
      const host = process.platform === "win32"
        ? spawnSync("pwsh", ["-NoProfile", "-Command", "Get-Process -Name engram,cargo,rustc,MsMpEng -ErrorAction SilentlyContinue | Select-Object ProcessName,Id,CPU,WorkingSet64 | ConvertTo-Json -Compress"], { encoding: "utf8", timeout: 2000, maxBuffer: 16384, windowsHide: true })
        : spawnSync("ps", ["-eo", "pid,comm,time,rss"], { encoding: "utf8", timeout: 2000, maxBuffer: 16384 });
      const hostText = process.platform === "win32" ? host.stdout : host.stdout?.split("\n").filter(line => /engram|cargo|rustc/u.test(line)).join("\n");
      const sample = `sampleMs=${(performance.now() - sampledAt).toFixed(1)} status=${host.status} processes=${hostText?.trim()} error=${host.error?.message ?? host.stderr?.trim() ?? ""}`;
      const eventual = await waitForClose(Math.max(0, 15000 - (performance.now() - exceededAt)));
      const observed = `observedElapsed=${(performance.now() - started).toFixed(1)}ms eventual=${JSON.stringify(eventual ?? null)}`;
      if (!eventual) {
        this.child.kill();
        await waitForClose(1000);
      }
      throw new Error(`MCP server did not close (${diagnostic}); ${observed}; ${sample}; stderr=${this.stderr}`);
    }
    assert.equal(closed.signal, null, `MCP server terminated by ${closed.signal}`);
    assert.equal(closed.code, 0, this.stderr);
  }
}

test("MCP close watchdog leaves room beyond the rmcp drain bound", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let ended = 0;
  const client = {
    child: { stdin: { destroyed: false, end() { ended++; } } },
    closed: new Promise(resolvePromise => {
      setTimeout(() => resolvePromise({ code: 0, signal: null }), 6000);
    }),
    pending: new Map(),
    stderr: "",
  };
  const checked = assert.doesNotReject(McpClient.prototype.close.call(client));
  t.mock.timers.tick(5001);
  await Promise.resolve();
  t.mock.timers.tick(999);
  await checked;
  assert.equal(ended, 1);
});

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
      const history = receipt(await client.call("show", { work_ref: child, history: true }));
      const parentHistory = receipt(await client.call("show", { work_ref: parent, history: true }));
      // Discard the original response and retry from new processes with the
      // same session and intent. Neither atomic effect may be appended twice.
      // Each surface retains its original source-skill attribution, which
      // participates in canonical intent. Cross-surface attribution differs.
      if (surface === "cli") {
        const shellRetry = cliJson(engramHome, session, "update", child, "--reject", "Evidence disproves finding");
        assert.deepEqual(shellRetry.receipt.result, rejected.receipt.result);
      }
      await client.close();
      client = new McpClient(engramHome, session);
      await client.initialize();
      if (surface === "mcp") {
        const mcpRetry = receipt(await client.call("update", { work_ref: child, action: "reject", reason: "Evidence disproves finding" }));
        assert.deepEqual(mcpRetry.receipt.result, rejected.receipt.result);
      }
      assertRecordParity(receipt(await client.call("show", { work_ref: child, history: true })), history);
      assertRecordParity(receipt(await client.call("show", { work_ref: parent, history: true })), parentHistory);
      const changedIntent = await client.call("update", { work_ref: child, action: "reject", reason: "Different rejection intent" });
      structuredError(changedIntent, "work_reject_refused");
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
        assert.ok(Buffer.byteLength(JSON.stringify(value, null, 2)) < 12 * 1024);
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
      assert.ok(Buffer.byteLength(text) < 12 * 1024);
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

test("hygiene correction pending rejection gives conditional recovery on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-pending-reject-", t);
  const session = "reject-reader";
  let client;
  try {
    buildAndInit(engramHome);
    const parent = cliJson(engramHome, session, "add", "Parent").work.short_ref;
    client = new McpClient(engramHome, session);
    await client.initialize();
    for (const surface of ["cli", "mcp"]) {
      for (const drift of ["child", "claim"]) {
        const child = cliJson(engramHome, session, "add", `Pending ${surface} ${drift}`, "--under", parent).work.short_ref;
        cliJson(engramHome, "peer-holder", "claim", child);
        // Account for the holder's contribution so the later release is admitted.
        cliJson(engramHome, "peer-holder", "note", child, "Peer inspection is complete");
        const args = { work_ref: child, action: "reject", reason: "Evidence refutes finding" };
        if (surface === "cli") {
          assert.notEqual(cliWord(engramHome, session, "update", child, "--reject", args.reason).status, 0);
        } else {
          assert.equal((await client.call("update", args)).isError, true);
        }
        const originalChild = receipt(await client.call("show", { work_ref: child })).status.work;
        if (drift === "child") {
          cliJson(engramHome, "peer-holder", "update", child, "--title", `Revised pending ${surface}`);
        } else {
          cliJson(engramHome, "peer-holder", "update", child, "--release");
        }
        const before = receipt(await client.call("show", { work_ref: child }));
        if (drift === "claim") {
          assert.ok(originalChild);
          assert.deepEqual(before.status.work, originalChild);
        }
        let error;
        if (surface === "cli") {
          const text = cliWord(engramHome, session, "update", child, "--reject", args.reason);
          assert.notEqual(text.status, 0);
          if (drift === "child") {
            assert.ok(text.stderr.includes(`update ${child} --cancel`));
            assert.ok(text.stderr.includes(`update ${parent} --waive ${child}`));
          } else {
            assert.match(text.stderr, /recorded claim or execution basis changed; the child is unchanged/);
            assert.doesNotMatch(text.stderr, /--cancel|--waive/);
          }
          assert.doesNotMatch(text.stderr, /its recorded rejection|auto:|idempotency/);
          const json = cliWord(engramHome, session, "update", child, "--reject", args.reason, "--json");
          assert.notEqual(json.status, 0);
          error = JSON.parse(json.stderr).error;
        } else {
          error = structuredError(await client.call("update", args), "work_reject_refused");
        }
        assert.equal(error.code, "work_reject_refused");
        if (drift === "child") {
          assert.equal(error.details.reason, "the child changed since the original rejection attempt");
          assert.ok(error.details.remedy.includes(`update ${child} --cancel`));
          assert.ok(error.details.remedy.includes(`update ${parent} --waive ${child}`));
          assert.ok(error.details.remedy.includes("if"));
        } else {
          assert.equal(error.details.reason, "the recorded claim or execution basis changed; the child is unchanged");
          assert.match(error.details.remedy, /different reason text or an explicit key/);
          assert.doesNotMatch(error.details.remedy, /--cancel|--waive/);
        }
        assert.doesNotMatch(error.message, /its recorded rejection|auto:|idempotency/);
        assert.deepEqual(error.next, [`engram work show ${child}`]);
        assert.deepEqual(receipt(await client.call("show", { work_ref: child })), before);
        if (drift === "claim") {
          const reason = "Reassessed evidence refutes finding";
          const fresh = surface === "cli"
            ? cliJson(engramHome, session, "update", child, "--reject", reason)
            : receipt(await client.call("update", { ...args, reason }));
          assert.equal(fresh.receipt.result.required_child_waived, true);
        }
      }
    }
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

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
    assert.deepEqual(Object.keys(doneTool.inputSchema.properties).sort(), ["link_basis", "links", "note", "summary", "work_ref"]);
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

test("criterion links reuse existing records and fence the authors read on CLI MCP", async (t) => {
  const engramHome = fixtureHome("engram-criterion-links-", t);
  const session = "criterion-link-author";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    for (const surface of ["cli", "mcp"]) {
      const ref = cliJson(engramHome, session, "add", `Explicit links ${surface}`,
        "--accept", "First outcome", "--accept", "Second outcome").work.short_ref;
      cliJson(engramHome, session, "claim", ref);
      cliJson(engramHome, session, "note", ref, "Existing artifact establishes the first outcome");
      const shown = cliJson(engramHome, session, "show", ref);
      const records = receipt(await client.call("show", {work_ref: ref, notes: true, gates: true}));
      const locator = records.notes[0].locator;
      const args = {work_ref: ref, summary: "Delivered", link_basis: shown.acceptance_basis,
        links: [{criterion: 1, locator}]};
      for (const malformed of ["missing-separator", "word=12345678"]) {
        const output = cliWord(engramHome, session, "done", ref, "--link", malformed,
          "--link-basis", String(args.link_basis), "--json");
        assert.notEqual(output.status, 0);
        const error = JSON.parse(output.stderr).error;
        assert.equal(error.code, "work_criterion_link_invalid");
        assert.equal(error.details.criterion, undefined);
        assert.deepEqual(error.next, [`engram work show ${ref}`, `engram work show ${ref} --notes --gates`]);
        assert.doesNotMatch(output.stderr, /criterion 0|linked-completion:/);
      }
      for (const shape of [
        {work_ref: ref, links: args.links},
        {...args, links: Array.from({length: 65}, () => args.links[0])},
        {...args, links: [{criterion: 0, locator}]},
      ]) {
        const error = structuredError(await client.call("done", shape), "work_criterion_link_invalid");
        assert.equal(error.details.criterion, undefined);
        assert.doesNotMatch(JSON.stringify(error), /criterion 0|linked-completion:/);
      }
      const complete = () => surface === "cli"
        ? Promise.resolve(cliJson(engramHome, session, "done", ref, "Delivered", "--link", `1=${locator}`, "--link-basis", String(args.link_basis)))
        : client.call("done", args).then(receipt);
      const first = await complete();
      assert.doesNotMatch(JSON.stringify(first), /linked-completion:/);
      assert.deepEqual(first.acceptance_evidence.unlinked_positions, [2]);
      assert.equal(first.acceptance_evidence.links[0].locator, locator);
      assert.match(first.acceptance_evidence.links[0].preview, /Existing artifact/);
      assert.equal(first.acceptance_evidence.link_label, "author-linked evidence; not verification");
      assert.deepEqual((await complete()).acceptance_evidence, first.acceptance_evidence);
      const otherAuthor = cliWord(engramHome, "different-link-session", "done", ref, "Delivered",
        "--link", `1=${locator}`, "--link-basis", String(args.link_basis), "--json");
      assert.notEqual(otherAuthor.status, 0);
      const otherError = JSON.parse(otherAuthor.stderr).error;
      assert.equal(otherError.code, "work_criterion_link_invalid");
      assert.match(otherError.details.reason, /completion is frozen/);
      assert.doesNotMatch(otherAuthor.stderr, /linked-completion:/);
      assert.deepEqual(receipt(await client.call("show", {work_ref: ref})).acceptance_evidence, first.acceptance_evidence);
      const text = cliWord(engramHome, session, "show", ref);
      assert.equal(text.status, 0, text.stderr);
      assert.match(text.stdout, /criterion 2: no evidence linked to this criterion/);
      assert.match(text.stdout, /author-linked evidence; not verification/);

      const stale = cliJson(engramHome, session, "add", `Stale basis ${surface}`).work.short_ref;
      cliJson(engramHome, session, "claim", stale);
      cliJson(engramHome, session, "note", stale, "Real current-run evidence");
      const prior = cliJson(engramHome, session, "show", stale, "--notes");
      cliJson(engramHome, session, "update", stale, "--accept", "Revised criterion");
      let error;
      if (surface === "cli") {
        const refused = cliWord(engramHome, session, "done", stale, "No capture should occur", "--link", `1=${prior.notes[0].locator}`, "--link-basis", String(prior.acceptance_basis), "--json");
        assert.notEqual(refused.status, 0);
        error = JSON.parse(refused.stderr).error;
      } else {
        error = structuredError(await client.call("done", {work_ref: stale, summary: "No capture should occur",
          links: [{criterion: 1, locator: prior.notes[0].locator}], link_basis: prior.acceptance_basis}), "work_criterion_link_invalid");
      }
      assert.equal(error.code, "work_criterion_link_invalid");
      assert.match(error.details.reason, /basis changed/);
      assert.deepEqual(error.next, [`engram work show ${stale}`, `engram work show ${stale} --notes --gates`]);
      assert.equal(cliJson(engramHome, session, "show", stale).status.work.lifecycle, "open");
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

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
      assert.ok(Buffer.byteLength(JSON.stringify(value, null, 2)) < 12 * 1024);
    };
    for (const verbose of [false, true, false]) {
      const flags = ["--peek", ...(verbose ? ["--verbose"] : [])];
      assertPeek(cliJson(engramHome, session, "next", ...flags), true);
      assertPeek(receipt(await client.call("next", { peek: true, verbose })), true);
      const text = cliWord(engramHome, session, "next", ...flags);
      assert.equal(text.status, 0, text.stderr);
      assert.match(text.stdout, /delivery: not advanced/);
      assert.match(text.stdout, /memory detail: engram work memories/);
      assert.match(text.stdout, /not whether notes were read or applied/);
      assert.ok(text.stdout.includes(peer));
      assert.doesNotMatch(text.stdout, /more arrive with your next call/);
      assert.ok(Buffer.byteLength(text.stdout) < 12 * 1024);
      assert.deepEqual(cliJson(engramHome, session, "show", held), before);
    }
    receipt(await client.call("memories", { query: "orientation", full: true }));
    assertPeek(receipt(await client.call("next", { peek: true })), true);
    receipt(await client.call("next", {}));
    assertPeek(cliJson(engramHome, session, "next", "--peek"), false);
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("peek cold CLI and MCP refuse a missing store without creating it", async (t) => {
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
    for (const error of [missingRevision, JSON.parse(cliMissing.stderr).error]) {
      assert.equal(error.code, "memory_revision_not_found");
      assert.deepEqual(error.details, { key, revision: 4, current_revision: 3, remedy: `read memories ${key} --full for history navigation` });
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
    assert.match(exists.details.remedy, /--revise/);
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
    for (const error of [mcp, JSON.parse(cli.stderr).error]) {
      assert.deepEqual(error.next, ["engram work next"]);
      assert.deepEqual(error.reminders, ["no item is selected for this gate; use gate NAME --work-ref REF"]);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("targeted reads do not steer later bare writes on CLI or MCP", async (t) => {
  const engramHome = fixtureHome("engram-read-target-", t);
  const session = "read-target-session";
  let client;
  try {
    buildAndInit(engramHome);
    const other = cliJson(engramHome, "peer", "add", "Read without selecting").work.short_ref;
    cliJson(engramHome, "peer", "note", other, "Committed note for reading");
    const locator = cliJson(engramHome, "peer", "show", other, "--notes").notes[0].locator;
    const held = cliJson(engramHome, session, "add", "Keep execution here").work.short_ref;
    cliJson(engramHome, session, "claim", held);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const modes = [
      [[], {}],
      [["--notes"], { notes: true }],
      [["--notes", "--gates"], { notes: true, gates: true }],
      [["--history"], { history: true }],
      [["--note", locator], { note: locator }],
    ];
    for (const surface of ["cli", "mcp"]) {
      for (const [flags, args] of modes) {
        const shown = surface === "cli"
          ? cliJson(engramHome, session, "show", other, ...flags)
          : receipt(await client.call("show", { work_ref: other, ...args }));
        assert.equal(args.note ? shown.work_ref : shown.status.work.short_ref, other);
        assert.ok(shown.next.length > 0);
        assert.ok(shown.next.every((command) => command.includes(other)));
        const text = `Bare note remains on held item ${surface} ${flags.join(" ")}`;
        const written = surface === "cli"
          ? cliJson(engramHome, session, "note", text)
          : receipt(await client.call("note", { text }));
        assert.equal(written.work.short_ref, held);
      }
      const text = `Explicit observation on read item ${surface}`;
      const written = surface === "cli"
        ? cliJson(engramHome, session, "note", other, text)
        : receipt(await client.call("note", { work_ref: other, text }));
      assert.equal(written.work.short_ref, other);
      // An explicit mutation still selects its target; restore execution focus
      // by renewing the existing claim before the next surface's read matrix.
      cliJson(engramHome, session, "claim", held);
    }
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
      assert.ok(Buffer.byteLength(result.stdout) <= 12 * 1024);
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
    const afterRead = receipt(await client.call("next", {}));
    assert.equal(afterRead.focus.ref, parent);
    assert.equal(afterRead.focus.state, "completed");
    assert.deepEqual(afterRead.reminders, []);
    assert.ok(!afterRead.next.includes(command));
    cliJson(engramHome, "detacher", "core", "focus", child);
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

test("file intake notifies ordinary CLI and MCP reads without steering local work", async (t) => {
  const engramHome = fixtureHome("engram-source-intake-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const session = "source-reader";
    client = new McpClient(engramHome, session);
    await client.initialize();
    const held = receipt(await client.call("add", { title: "Existing local work" })).work.short_ref;
    receipt(await client.call("claim", { work_ref: held }));
    const file = join(engramHome, "intake.json");
    const input = {
      snapshot: {
        schema_version: 1, adapter_kind: "planner", canonical_ref: "plan/item-1",
        projected: { title: "Outside title", body: "Outside context", status: "closed", owner: "outside-owner" },
        captured_at: new Date().toISOString(), source_revision: "1", fingerprint: "revision-1",
        canonical_url: null, payload_hash: createHash("sha256").update("test source payload").digest("hex"),
        raw: { planner_context: ["untrusted context"] },
      },
      draft: { title: "Authored local title", outcome: "Authored local outcome" },
    };
    let importingSession = session;
    const invoke = (...args) => spawnSync(binary, ["--home", engramHome, "import",
      "--actor-id", importingSession, "--session-id", importingSession, ...args], { cwd: root, encoding: "utf8" });
    const json = (...args) => {
      const result = invoke(...args);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    writeFileSync(file, JSON.stringify(input));
    const preview = json("preview", file);
    assert.equal(preview.effect, "create");
    assert.deepEqual(preview.draft.acceptance, []);
    assert.equal(json("lookup", "planner", "plan/item-1"), null);
    const imported = json("apply", file, "--preview", preview.preview_token);
    assert.deepEqual(json("apply", file, "--preview", preview.preview_token), imported);
    const work_ref = imported.work_ref;
    const before = receipt(await client.call("show", { work_ref }));
    assert.equal(before.status.work.title, input.draft.title);
    assert.equal(before.status.work.outcome, input.draft.outcome);
    assert.deepEqual(before.status.work.acceptance, []);
    assert.equal(before.source.notice_count, 0);
    assert.equal(before.source.local_work_unchanged_by_notices, undefined);
    assert.doesNotMatch(cliText(engramHome, session, "show", work_ref), /change notices|local work not changed by notices/u);
    importingSession = "source-notifier";
    for (const revision of [2, 3]) {
      delete input.draft;
      input.snapshot.source_revision = String(revision);
      input.snapshot.projected.body = `Outside changed context ${revision}`;
      writeFileSync(file, JSON.stringify(input));
      const refresh = json("preview", file);
      assert.equal(refresh.effect, "notify");
      const result = json("apply", file, "--preview", refresh.preview_token);
      assert.equal(result.work_revision, imported.work_revision);
      assert.equal(result.cited_snapshot, imported.snapshot);
      assert.notEqual(result.snapshot, result.cited_snapshot);
    }
    const detail = json("lookup", "planner", "plan/item-1");
    assert.equal(detail.notice_count, 2);
    assert.equal(detail.notices_omitted, 1);
    assert.equal(detail.cited_source.projected.body, "Outside context");
    assert.equal(detail.latest_proposed_source.projected.body, "Outside changed context 3");
    assert.equal(detail.latest_notice.actor.session_id, importingSession);
    assert.equal(json("lookup", "Planner", "plan/item-1"), null);
    const after = receipt(await client.call("show", { work_ref }));
    const { source: beforeSource, ...beforeLocal } = before;
    const { source: afterSource, ...afterLocal } = after;
    assert.deepEqual(afterLocal, beforeLocal);
    assert.equal(afterSource.notice_count, 2);
    assert.equal(afterSource.notices_omitted, 1);
    assert.equal(afterSource.local_work_unchanged_by_notices, undefined);
    assert.match(afterSource.detail, /engram import lookup -- 'planner' 'plan\/item-1'/u);
    assert.deepEqual(cliJson(engramHome, session, "show", work_ref), after);
    const peerChanges = receipt(await client.call("next", { peek: true })).changes
      .filter((line) => line.includes("external source changed"));
    assert.equal(peerChanges.length, 2);
    for (const line of peerChanges) {
      assert.match(line, /by peer-[0-9a-f]{24}/u);
      assert.ok(line.includes(work_ref));
      assert.ok(!line.includes(importingSession));
    }
    const text = cliText(engramHome, session, "show", work_ref);
    assert.ok(text.includes(`source detail: ${afterSource.detail}`));
    assert.match(text, /2 change notices \(1 older not shown\)/u);
    assert.match(text, /latest source notice: \d{2}:\d{2} UTC/u);
    assert.doesNotMatch(text, /latest source notice: .*\.\d/u);
    assert.ok(Buffer.byteLength(text) < 12288);
    assert.ok(Buffer.byteLength(JSON.stringify(after, null, 2)) < 12288);
    for (const hidden of [imported.snapshot, "outside-owner", "Outside changed context", session]) {
      assert.ok(!JSON.stringify(after).includes(hidden));
      assert.ok(!text.includes(hidden));
    }
    assert.equal(json("preview", file).effect, "already_known");
    const note = receipt(await client.call("note", { text: "Bare write still targets held local work" }));
    assert.equal(note.work.short_ref, held);
    input.draft = { title: "Must not overwrite", outcome: "Must refuse" };
    writeFileSync(file, JSON.stringify(input));
    const refused = invoke("preview", file);
    assert.notEqual(refused.status, 0);
    assert.match(refused.stderr, /source refresh takes no local draft/u);
    assert.deepEqual(receipt(await client.call("show", { work_ref })).source, afterSource);
    input.snapshot.canonical_ref = "--help";
    writeFileSync(file, JSON.stringify(input));
    const dashPreview = json("preview", file);
    const dashItem = json("apply", file, "--preview", dashPreview.preview_token);
    const dashShow = receipt(await client.call("show", { work_ref: dashItem.work_ref }));
    assert.equal(dashShow.source.detail, "engram import lookup -- 'planner' '--help'");
    assert.ok(cliText(engramHome, session, "show", dashItem.work_ref).includes(`source detail: ${dashShow.source.detail}`));
    const dashLookup = json("lookup", "--", "planner", "--help");
    assert.equal(dashLookup.work_ref, dashItem.work_ref);
    assert.equal(dashLookup.source_key.canonical_ref, "--help");
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("import file reader refuses input above one MiB before persistence", (t) => {
  const engramHome = fixtureHome("engram-import-input-bound-", t);
  try {
    buildAndInit(engramHome);
    const file = join(engramHome, "oversized.json");
    const before = cliJson(engramHome, "bound-reader", "ls", "--all");
    writeFileSync(file, Buffer.alloc(1024 * 1024 + 1, 32));
    const result = spawnSync(binary, ["--home", engramHome, "import", "--actor-id", "bound-reader",
      "--session-id", "bound-reader", "preview", file], { cwd: root, encoding: "utf8" });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /import input exceeds the 1048576-byte limit/u);
    assert.deepEqual(cliJson(engramHome, "bound-reader", "ls", "--all"), before);
  } finally { removeFixtureHomes(engramHome); }
});

test("printed shell session preserves import and observation replay identity", (t) => {
  const engramHome = fixtureHome("engram-printed-session-replay-", t);
  try {
    buildAndInit(engramHome);
    const environment = { ...process.env };
    delete environment.ENGRAM_SESSION_ID;
    delete environment.ENGRAM_ACTOR_CONTEXT;
    const invoke = (family, session, ...args) => {
      const result = spawnSync(binary, ["--home", engramHome, family,
        "--actor-id", "shell-author", ...(session ? ["--session-id", session] : []), ...args],
      { cwd: root, encoding: "utf8", env: environment });
      assert.equal(result.status, 0, result.stderr);
      return { value: JSON.parse(result.stdout), stderr: result.stderr };
    };
    const replay = (family, ...args) => {
      const first = invoke(family, null, ...args);
      const session = /--session-id (local-process-v1-[^\s]+)/u.exec(first.stderr)?.[1];
      assert.ok(session, first.stderr);
      const retry = invoke(family, session, ...args);
      if (family === "work") {
        const { effective_session_id, ...originalReceipt } = first.value;
        assert.equal(effective_session_id, session);
        assert.equal(retry.value.effective_session_id, undefined);
        assert.deepEqual(retry.value, originalReceipt);
      } else {
        assert.deepEqual(retry.value, first.value);
      }
      return { value: first.value, session };
    };
    const file = join(engramHome, "intake.json");
    const input = {
      snapshot: {
        schema_version: 1, adapter_kind: "planner", canonical_ref: "printed-session",
        projected: { title: "External", body: "Context", status: null, owner: null },
        captured_at: new Date().toISOString(), source_revision: "1", fingerprint: "first",
        canonical_url: null, payload_hash: createHash("sha256").update("source").digest("hex"), raw: {},
      },
      draft: { title: "Local item", outcome: "Authored outcome" },
    };
    writeFileSync(file, JSON.stringify(input));
    let preview = invoke("import", "previewer", "preview", file).value;
    const created = replay("import", "apply", file, "--preview", preview.preview_token);
    delete input.draft;
    input.snapshot.source_revision = "2";
    writeFileSync(file, JSON.stringify(input));
    preview = invoke("import", "previewer", "preview", file).value;
    const notified = replay("import", "apply", file, "--preview", preview.preview_token);
    assert.equal(notified.value.effect, "notify");
    const detail = invoke("import", "reader", "lookup", "planner", "printed-session").value;
    assert.equal(detail.notice_count, 1);
    assert.equal(detail.latest_notice.actor.session_id, notified.session);
    assert.ok(detail.latest_notice.actor.provenance_chain.some((link) =>
      link.source === "defaulted:process_session" && link.reference === "session_id"));
    const observation = replay("work", "note", created.value.work_ref, "One non-holder observation", "--json");
    const notes = invoke("work", "reader", "show", created.value.work_ref, "--notes", "--json").value;
    assert.equal(notes.notes.filter((note) => note.summary.includes("One non-holder observation")).length, 1);
    assert.equal(observation.value.work.short_ref, created.value.work_ref);
  } finally {
    removeFixtureHomes(engramHome);
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
    // Derive both dates from the recorded first claim, not the wall clock
    // after the subprocess. A five-minute lease can cross midnight UTC.
    const expiry = new Date(cliJson(engramHome, actor, "show", workRef).held_until);
    const claimInstant = new Date(expiry.getTime() - 300_000).toISOString();
    const expires = expiry.toISOString();
    const expiryClock = `${expires.slice(0, 10) === claimInstant.slice(0, 10) ? "" : `${expires.slice(0, 10)} `}${expires.slice(11, 16)} UTC`;
    assert.equal(claimed.split("\n")[0], `claimed ${workRef} "Dogfood work CLI" (held by you until ${expiryClock}) [open; revision 1]`);
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
    assert.match(shown, /acceptance:\n  acceptance basis: \d+ \(pass --link-basis with --link\)\n  1\. CLI completion is sealed\n/u);
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
    assert.match(done, /^done w-[0-9a-f]{12} "Dogfood work CLI" \[completed; revision \d+\]\nasserted 1 acceptance criterion satisfied; completion changed no criterion\nfull detail: engram work show 'w-[0-9a-f]{12}'\ncriterion evidence: 1 of 1 criteria unlinked \(1 shown\)\n  criterion 1: no evidence linked to this criterion\nreminders: none\nnext:\n/u);
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
    assert.equal(held.details.holder_session_id, undefined);
    const holderLabel = receipt(await b.call("show", { work_ref: workRef })).holder;
    assert.match(holderLabel, /^peer-[0-9a-f]{24}$/u);
    assert.equal(held.details.holder, holderLabel);
    assert.ok(held.reminders[0].startsWith(`held by ${holderLabel} until `));
    assert.ok(!JSON.stringify(held).includes(sessionA));
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
