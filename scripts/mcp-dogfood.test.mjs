#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";

import { fixtureHome as ownedFixtureHome, removeFixtureHomes as cleanupFixtureHomes, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
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
  "evaluate",
  "note",
  "done",
  "search",
  "handoff",
  "remember",
  "memories",
  "forget",
];
// A full record id: minted ids are 32 hex digits, earlier ids are 64.
const HASH = /\b(?:[0-9a-f]{32}|[0-9a-f]{64})\b/u;
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

// The server's opt-in phase trace: one stderr JSON record per tool call,
// correlated by the call's numeric request id. A bounded number are kept.
const PHASE_TRACE_ENV = "ENGRAM_MCP_PHASE_TRACE";
const MAX_PHASE_RECORDS = 256;

// The record states that time a whole call, its response's send included.
const PHASE_TIMED_STATES = new Set(["complete", "send_failed"]);

/**
 * The line printed beside a slow call: its phase record, or why there is
 * none. A record that did not time the whole call, such as an evicted,
 * incomplete or cancelled one, reads as unavailable, so it never passes for
 * a fast call; its fields follow for diagnosis.
 */
function phaseTraceLine(id, record, why = "no record received") {
  if (record === undefined) return `MCP phase trace: id=${id} unavailable (${why})`;
  if (!PHASE_TIMED_STATES.has(record.state)) {
    return `MCP phase trace: id=${id} unavailable (record ${record.state}) ${JSON.stringify(record)}`;
  }
  return `MCP phase trace: id=${id} ${JSON.stringify(record)}`;
}

/** Prints a slow call's phase line and keeps it on the client for tests. */
function phaseNotice(client, line) {
  client.phaseNotices?.push(line);
  console.error(line);
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
  constructor(engramHome, sessionId, actorContext, actorId = sessionId, extraArgs = [], { phaseTrace = true, softTimingMs = SOFT_TIMING_MS } = {}) {
    this.engramHome = engramHome;
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = "";
    this.buffer = "";
    this.stderrBuffer = "";
    this.phaseTrace = phaseTrace;
    this.phaseRecords = new Map();
    this.phaseLines = 0;
    // Slow calls whose record had not arrived when they settled.
    this.awaitingPhase = new Set();
    // The calls at or above this many milliseconds get a phase line.
    this.softTimingMs = softTimingMs;
    this.phaseNotices = [];
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
      ...extraArgs,
    ];
    this.args = [...args];
    const environment = { ...process.env };
    if (actorContext === undefined) delete environment.ENGRAM_ACTOR_CONTEXT;
    else environment.ENGRAM_ACTOR_CONTEXT = actorContext;
    if (phaseTrace) environment[PHASE_TRACE_ENV] = "1";
    else delete environment[PHASE_TRACE_ENV];
    this.child = spawn(binary, args, {
      cwd: root,
      env: environment,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.child.stderr.on("data", (chunk) => this.receiveStderr(chunk));
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
    this.closed = new Promise((resolvePromise) => {
      this.child.once("close", (code, signal) => {
        this.flushStderr();
        resolvePromise({ code, signal });
      });
    });
    this.child.on("exit", (code, signal) => this.serverExited(code, signal));
  }

  // The phase line beside a slow call's soft-threshold line. The record
  // follows the response on another pipe: when it is not here yet, the call
  // reads as unavailable now and its record is printed when it arrives,
  // never waited for. Called through the prototype by the notice test.
  slowCallNotice(id) {
    if (!this.phaseTrace) {
      phaseNotice(this, phaseTraceLine(id, undefined, "trace off"));
    } else if (this.phaseRecords.has(id)) {
      phaseNotice(this, phaseTraceLine(id, this.phaseRecords.get(id)));
    } else {
      phaseNotice(this, phaseTraceLine(id, undefined, "record not yet received"));
      this.awaitingPhase.add(id);
    }
  }

  // Stderr may still be draining at exit, so a partial line stays buffered
  // until close; the message only shows it. Called through the prototype by
  // the stderr test, as the methods below are.
  serverExited(code, signal) {
    const error = new Error(
      `MCP server exited code=${code} signal=${signal}: ${this.stderr}${this.stderrBuffer}`,
    );
    for (const { reject } of this.pending.values()) reject(error);
    this.pending.clear();
  }

  // A last stderr fragment with no newline, such as a partial message at
  // exit, still reaches the diagnostics once stderr has closed.
  flushStderr() {
    if (this.stderrBuffer === "") return;
    this.stderr += this.stderrBuffer;
    this.stderrBuffer = "";
  }

  // Phase records are kept apart from other stderr, so error messages stay
  // readable, and every one is written to the test log; a record split
  // across chunks is reassembled by line.
  receiveStderr(chunk) {
    this.stderrBuffer += chunk.toString("utf8");
    for (;;) {
      const newline = this.stderrBuffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.stderrBuffer.slice(0, newline + 1);
      this.stderrBuffer = this.stderrBuffer.slice(newline + 1);
      let record;
      if (line.startsWith('{"') && line.includes('"engram_mcp_phase_trace"')) {
        try { record = JSON.parse(line); } catch { record = undefined; }
      }
      if (record === undefined) {
        this.stderr += line;
        continue;
      }
      this.phaseLines++;
      console.error(`MCP phase record: ${line.trimEnd()}`);
      if (record.id === null) continue;
      this.phaseRecords.set(record.id, record);
      if (this.phaseRecords.size > MAX_PHASE_RECORDS) {
        this.phaseRecords.delete(this.phaseRecords.keys().next().value);
      }
      if (this.awaitingPhase.delete(record.id)) phaseNotice(this, phaseTraceLine(record.id, record));
    }
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
    const id = this.nextId;
    let result;
    let elapsed;
    try {
      result = await this.request("tools/call", { name, arguments: arguments_ });
    } finally {
      elapsed = performance.now() - started;
      if (elapsed >= this.softTimingMs) {
        const timing = timingLine("call", name, elapsed, recordWalSample(this.engramHome));
        if (timing !== undefined) console.error(timing);
        this.slowCallNotice(id);
      }
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
    try {
      // Called through the prototype so the close tests' plain stand-ins work.
      await McpClient.prototype.closeChecked.call(this, started, waitForClose);
    } finally {
      // Slow calls whose record never came read as unavailable, on a failed
      // close as much as on a clean one.
      for (const id of this.awaitingPhase) {
        phaseNotice(this, phaseTraceLine(id, this.phaseRecords.get(id), "no record before close"));
      }
      this.awaitingPhase.clear();
    }
  }

  async closeChecked(started, waitForClose) {
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
    awaitingPhase: new Set(),
    phaseRecords: new Map(),
    phaseNotices: [],
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

test("a slow call prints its phase record, or says why there is none", () => {
  const record = { engram_mcp_phase_trace: 1, id: 7, state: "complete" };
  assert.equal(phaseTraceLine(7, record), `MCP phase trace: id=7 ${JSON.stringify(record)}`);
  assert.equal(phaseTraceLine(7, undefined), "MCP phase trace: id=7 unavailable (no record received)");
  assert.equal(phaseTraceLine(7, undefined, "trace off"), "MCP phase trace: id=7 unavailable (trace off)");
  const failed = { engram_mcp_phase_trace: 1, id: 7, state: "send_failed" };
  assert.equal(phaseTraceLine(7, failed), `MCP phase trace: id=7 ${JSON.stringify(failed)}`);
  // A record that did not time the whole call never passes for a fast one.
  for (const state of ["evicted", "incomplete", "cancelled"]) {
    const partial = { engram_mcp_phase_trace: 1, id: 7, state, handler_total_ms: 1 };
    assert.equal(phaseTraceLine(7, partial), `MCP phase trace: id=7 unavailable (record ${state}) ${JSON.stringify(partial)}`);
  }
});

test("a slow call's phase line names its record when it arrives, or why it never did", async (t) => {
  // A record that never arrived before close reads as unavailable.
  const closing = {
    child: { stdin: { destroyed: true, end() {} } },
    closed: Promise.resolve({ code: 0, signal: null }),
    pending: new Map(),
    stderr: "",
    awaitingPhase: new Set([42]),
    phaseRecords: new Map(),
    phaseNotices: [],
  };
  await McpClient.prototype.close.call(closing);
  assert.deepEqual(closing.phaseNotices, ["MCP phase trace: id=42 unavailable (no record before close)"]);
  // A failed close still says so, and keeps its own failure.
  const failing = { ...closing, closed: Promise.resolve({ code: 3, signal: null }), awaitingPhase: new Set([43]), phaseNotices: [] };
  await assert.rejects(McpClient.prototype.close.call(failing), /3/u);
  assert.deepEqual(failing.phaseNotices, ["MCP phase trace: id=43 unavailable (no record before close)"]);

  // At the threshold, a record not here yet reads unavailable at once and is
  // printed when it arrives; an evicted record already here reads
  // unavailable, never as a fast call.
  const waiting = {
    phaseTrace: true,
    pending: new Map(),
    stderr: "",
    stderrBuffer: "",
    phaseLines: 0,
    phaseRecords: new Map([[46, { engram_mcp_phase_trace: 1, id: 46, state: "evicted", handler_total_ms: 3 }]]),
    awaitingPhase: new Set(),
    phaseNotices: [],
  };
  McpClient.prototype.slowCallNotice.call(waiting, 45);
  McpClient.prototype.slowCallNotice.call(waiting, 46);
  const late = JSON.stringify({ engram_mcp_phase_trace: 1, id: 45, state: "complete" });
  McpClient.prototype.receiveStderr.call(waiting, Buffer.from(`${late}\n`));
  assert.deepEqual(waiting.phaseNotices, [
    "MCP phase trace: id=45 unavailable (record not yet received)",
    `MCP phase trace: id=46 unavailable (record evicted) ${JSON.stringify(waiting.phaseRecords.get(46))}`,
    `MCP phase trace: id=45 ${late}`,
  ]);
  assert.equal(waiting.awaitingPhase.size, 0);

  // A record split across stderr chunks, with the server's exit between
  // them, is still read whole once stderr closes, and the slow call it
  // answers names it rather than reading unavailable.
  const split = {
    pending: new Map(),
    stderr: "",
    stderrBuffer: "",
    phaseLines: 0,
    phaseRecords: new Map(),
    awaitingPhase: new Set([44]),
    phaseNotices: [],
  };
  const record = JSON.stringify({ engram_mcp_phase_trace: 1, id: 44, state: "complete" });
  McpClient.prototype.receiveStderr.call(split, Buffer.from(`warning\n${record.slice(0, 20)}`));
  McpClient.prototype.serverExited.call(split, 0, null);
  McpClient.prototype.receiveStderr.call(split, Buffer.from(`${record.slice(20)}\n`));
  McpClient.prototype.flushStderr.call(split);
  assert.equal(split.stderr, "warning\n");
  assert.equal(split.phaseLines, 1);
  assert.deepEqual(split.phaseNotices, [`MCP phase trace: id=44 ${record}`]);

  const engramHome = fixtureHome("engram-mcp-phase-notices-", t);
  const clients = [];
  let failure;
  try {
    buildAndInit(engramHome);
    // Every call counts as slow here, so every call gets a phase line.
    const traced = new McpClient(engramHome, "notice-on", undefined, "notice-on", [], { phaseTrace: true, softTimingMs: 0 });
    clients.push(traced);
    await traced.initialize();
    const first = traced.nextId;
    const work_ref = receipt(await traced.call("add", { title: "Noticed work", acceptance: ["noticed"] })).work.short_ref;
    await traced.call("show", { work_ref });
    await traced.call("show", { work_ref });
    await traced.close();
    // Each slow call's lines end with its own record, whether the record
    // came before the response settled or after it; a record that came after
    // is preceded by the call reading unavailable at its threshold. No other
    // line appears: not one for an id that was never called, and not a
    // second record for the same id.
    let expected = 0;
    for (let index = 0; index < 3; index++) {
      const prefix = `MCP phase trace: id=${first + index} `;
      const lines = traced.phaseNotices.filter((line) => line.startsWith(prefix));
      assert.ok(lines.length === 1 || lines.length === 2, traced.phaseNotices.join("\n"));
      if (lines.length === 2) assert.equal(lines[0], `${prefix}unavailable (record not yet received)`);
      const record = JSON.parse(lines.at(-1).slice(prefix.length));
      assert.equal(record.id, first + index);
      assert.equal(record.state, "complete");
      expected += lines.length;
    }
    assert.equal(traced.phaseNotices.length, expected, traced.phaseNotices.join("\n"));

    const untraced = new McpClient(engramHome, "notice-off", undefined, "notice-off", [], { phaseTrace: false, softTimingMs: 0 });
    clients.push(untraced);
    await untraced.initialize();
    const id = untraced.nextId;
    await untraced.call("show", { work_ref });
    await untraced.close();
    assert.deepEqual(untraced.phaseNotices, [`MCP phase trace: id=${id} unavailable (trace off)`]);
  } catch (error) {
    failure = error;
  } finally {
    for (const client of clients) {
      try { await client.close(); } catch { /* already closed */ }
    }
    removeFixtureHomes(engramHome);
  }
  if (failure) throw failure;
});

test("the opt-in phase trace correlates every call and an unset server writes none", async (t) => {
  const engramHome = fixtureHome("engram-mcp-phase-trace-", t);
  const clients = [];
  let failure;
  // The same calls on each server: one write, one claim, then reads.
  const workload = async (client) => {
    await client.initialize();
    const first = client.nextId;
    const added = await client.call("add", { title: "Traced work", acceptance: ["traced"] });
    const work_ref = receipt(added).work.short_ref;
    await client.call("claim", { work_ref });
    const elapsed = [];
    for (let index = 0; index < 10; index++) {
      const started = performance.now();
      await client.call("show", { work_ref });
      elapsed.push(performance.now() - started);
    }
    return { first, last: client.nextId - 1, elapsed };
  };
  const summary = (values) => {
    const sorted = [...values].sort((left, right) => left - right);
    return `median=${sorted[Math.floor(sorted.length / 2)].toFixed(2)}ms max=${sorted.at(-1).toFixed(2)}ms`;
  };
  try {
    buildAndInit(engramHome);
    const traced = new McpClient(engramHome, "trace-on", undefined, "trace-on", [], { phaseTrace: true });
    clients.push(traced);
    const on = await workload(traced);
    await traced.close();
    // Every tool call has one complete record, correlated by its id; the
    // handshake requests have none.
    assert.equal(traced.phaseLines, on.last - on.first + 1);
    for (let id = on.first; id <= on.last; id++) {
      const record = traced.phaseRecords.get(id);
      assert.ok(record, `no phase record for call ${id}`);
      assert.equal(record.correlation, "numeric");
      assert.equal(record.state, "complete");
      assert.equal(typeof record.handler_total_ms, "number");
      assert.equal(typeof record.wire_encode_send_inclusive_ms, "number");
      const line = JSON.stringify(record);
      assert.ok(Buffer.byteLength(line) <= 4096, line);
      assert.ok(!line.includes(engramHome) && !line.includes("Traced work"), line);
      assert.doesNotMatch(line, /\b(?:SELECT|INSERT|UPDATE|DELETE|BEGIN|COMMIT)\b/u);
    }
    const write = traced.phaseRecords.get(on.first);
    assert.equal(write.tool, "add");
    assert.ok(write.store_open_total.count >= 1, JSON.stringify(write));
    assert.ok(write.begin_immediate.count >= 1, JSON.stringify(write));
    assert.ok(write.commit.count >= 1, JSON.stringify(write));
    assert.equal(traced.phaseRecords.get(on.last).tool, "show");
    assert.doesNotMatch(traced.stderr, /engram_mcp_phase_trace/u);

    const untraced = new McpClient(engramHome, "trace-off", undefined, "trace-off", [], { phaseTrace: false });
    clients.push(untraced);
    const off = await workload(untraced);
    // The same reads again on two open servers, one traced and one not,
    // interleaved so neither runs on a colder store or cache than the other.
    const second = new McpClient(engramHome, "trace-on-again", undefined, "trace-on-again", [], { phaseTrace: true });
    clients.push(second);
    await second.initialize();
    const work_ref = receipt(await second.call("add", { title: "Compared work", acceptance: ["compared"] })).work.short_ref;
    const interleaved = { on: [], off: [] };
    for (let index = 0; index < 30; index++) {
      for (const [side, client] of index % 2 === 0 ? [["on", second], ["off", untraced]] : [["off", untraced], ["on", second]]) {
        const started = performance.now();
        await client.call("show", { work_ref });
        interleaved[side].push(performance.now() - started);
      }
    }
    await second.close();
    await untraced.close();
    assert.equal(untraced.phaseLines, 0);
    assert.doesNotMatch(untraced.stderr, /engram_mcp_phase_trace/u);
    // A comparison report, not a benchmark. That an unset server adds no
    // work rests on its structure, not on these numbers: it installs no
    // wrapper, profile callback, transport or writer thread, and its hooks
    // only find no accumulator.
    t.diagnostic(`show over MCP, first runs: trace on ${summary(on.elapsed)}, trace off ${summary(off.elapsed)}; interleaved: trace on ${summary(interleaved.on)}, trace off ${summary(interleaved.off)}`);
  } catch (error) {
    failure = error;
  } finally {
    for (const client of clients) {
      try { await client.close(); } catch { /* already closed */ }
    }
    removeFixtureHomes(engramHome);
  }
  if (failure) throw failure;
});

test("a client's own stall is not attributed to the server or its send", async (t) => {
  const engramHome = fixtureHome("engram-mcp-phase-blocked-", t);
  let client;
  let failure;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "blocked-client", undefined, "blocked-client", [], { phaseTrace: true });
    await client.initialize();
    const work_ref = receipt(await client.call("add", { title: "Stalled reader", acceptance: ["read"] })).work.short_ref;
    const id = client.nextId;
    const started = performance.now();
    const answered = client.request("tools/call", { name: "show", arguments: { work_ref } });
    // The client's event loop stalls. The request may only leave when the
    // stall ends, so the server's own phases stay short however long the
    // client waited.
    const stall = 1500;
    const until = performance.now() + stall;
    while (performance.now() < until) { /* busy client */ }
    await answered;
    const elapsed = performance.now() - started;
    await client.close();
    const record = client.phaseRecords.get(id);
    assert.ok(record, `no phase record for call ${id}`);
    assert.ok(elapsed >= stall, `client elapsed ${elapsed}`);
    assert.ok(record.handler_total_ms < stall / 2, JSON.stringify(record));
    assert.ok(record.wire_encode_send_inclusive_ms < stall / 2, JSON.stringify(record));
    t.diagnostic(`client elapsed ${elapsed.toFixed(1)}ms; server handler ${record.handler_total_ms.toFixed(2)}ms; send ${record.wire_encode_send_inclusive_ms.toFixed(2)}ms`);
  } catch (error) {
    failure = error;
  } finally {
    try { await client?.close(); } catch { /* already closed */ }
    removeFixtureHomes(engramHome);
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
        assert.ok(Buffer.byteLength(text.stdout) < 12288);
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
      const refusedBytes = Buffer.byteLength(refused.stdout);
      if (json) {
        assert.ok(refusedBytes <= 12288);
      } else {
        assert.ok(refusedBytes < 12288);
      }
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
    // Like every tool error: the reason as the reminder, and no command.
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

test("a holder that did no work releases with a reason recorded as its waiver on CLI and MCP", async (t) => {
  const engramHome = fixtureHome("engram-release-waiver-", t);
  const session = "release-waiver-holder";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const tools = await client.tools();
    assert.match(tools.find(({ name }) => name === "update").inputSchema.properties.reason.description.replace(/\s+/gu, " "), /attributed waiver/u);
    const help = cliWord(engramHome, session, "update", "--help");
    assert.equal(help.status, 0, help.stderr);
    assert.match(help.stdout.replace(/\s+/gu, " "), /recorded as the attributed waiver/u);
    for (const surface of ["mcp", "cli"]) {
      const work = receipt(await client.call("add", { title: `Redirected on ${surface}` })).work.short_ref;
      receipt(await client.call("claim", { work_ref: work }));
      // Without a reason the release names the missing input and the command
      // that supplies it; the claim stays held.
      let error;
      if (surface === "cli") {
        const text = cliWord(engramHome, session, "update", work, "--release");
        assert.notEqual(text.status, 0);
        assert.ok(text.stderr.includes(`engram work update ${work} --release --reason`), text.stderr);
        const json = cliWord(engramHome, session, "update", work, "--release", "--json");
        assert.notEqual(json.status, 0);
        error = JSON.parse(json.stderr).error;
      } else {
        error = structuredError(await client.call("update", { work_ref: work, action: "release" }), "work_release_waiver_required");
      }
      assert.equal(error.code, "work_release_waiver_required");
      assert.match(error.details.remedy, /nonblank reason/u);
      assert.ok(error.reminders[0].includes("attributed waiver"), JSON.stringify(error));
      assert.deepEqual(error.next, [`engram work update ${work} --release --reason "…"`]);
      const held = receipt(await client.call("show", { work_ref: work }));
      assert.equal(held.status.work.short_ref, work);
      // With a reason the release succeeds and says the waiver was recorded.
      const reason = "redirected before any work";
      const released = surface === "cli"
        ? cliJson(engramHome, session, "update", work, "--release", "--reason", reason)
        : receipt(await client.call("update", { work_ref: work, action: "release", reason }));
      assert.equal(released.receipt.result.waiver_recorded, true, JSON.stringify(released));
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

test("read-only MCP lists the read words and refuses every writing call as a tool error", async (t) => {
  const engramHome = fixtureHome("engram-read-only-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const ref = cliJson(engramHome, "writer", "add", "Read me").work.short_ref;
    cliJson(engramHome, "writer", "claim", ref);
    cliJson(engramHome, "writer", "remember", "a rule to read", "--key", "read-rule");
    client = new McpClient(engramHome, "reader", undefined, "reader", ["--read-only"]);
    await client.initialize();
    assert.deepEqual([...(await client.toolNames())].sort(), ["ls", "memories", "next", "search", "show"]);
    for (const [name, arguments_] of [
      ["next", { peek: true }],
      ["ls", {}],
      ["search", { query: "Read" }],
      ["show", { work_ref: ref }],
      ["memories", {}],
      ["memories", { query: "read-rule", full: true }],
    ]) {
      structured(await client.call(name, arguments_));
    }
    const refused = async (name, arguments_, restriction) => {
      const error = structuredError(await client.call(name, arguments_), "mcp_read_only_refused");
      assert.equal(error.details.mode, "read_only");
      assert.equal(error.details.tool, name);
      assert.equal(error.details.restriction, restriction);
      assert.match(error.message, /^MCP read-only mode refused /u);
      // Like every tool error: reminders in words, and the read to make instead.
      assert.equal(error.reminders.length, 1);
      assert.match(error.reminders[0], /^this connection is read-only: /u);
      const instead = {
        tool_not_admitted: ["engram work next --peek"],
        next_without_peek: ["engram work next --peek"],
        memories_with_context_generation: ["engram work memories"],
        argument_not_admitted: [],
      };
      assert.deepEqual(error.next, instead[restriction]);
    };
    await refused("next", {}, "next_without_peek");
    await refused("next", { peek: false }, "next_without_peek");
    await refused("next", { peek: "true" }, "argument_not_admitted");
    await refused("memories", { context_generation: null }, "memories_with_context_generation");
    await refused("memories", { context_generation: "fresh" }, "memories_with_context_generation");
    await refused("ls", { write: true }, "argument_not_admitted");
    for (const name of ["add", "claim", "update", "gate", "evaluate", "remember", "forget", "note", "done", "handoff", "drop_everything"]) {
      await refused(name, { title: "x" }, "tool_not_admitted");
    }
    // Nothing the read-only connection did reached the store.
    const shown = cliJson(engramHome, "writer", "show", ref, "--notes");
    assert.equal(shown.notes.length, 0, JSON.stringify(shown));
    assert.equal(cliJson(engramHome, "writer", "memories").memories.length, 1);
  } finally {
    await client?.close();
    removeFixtureHomes(engramHome);
  }
});

test("done records a typed landing on CLI and MCP, and show reads it back", async (t) => {
  const engramHome = fixtureHome("engram-landing-", t);
  const session = "landing-author";
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, session);
    await client.initialize();
    const doneTool = (await client.tools()).find(({ name }) => name === "done");
    assert.match(doneTool.inputSchema.properties.landing.description, /asserted provenance/);
    const build = createHash("sha256").update("installed build").digest("hex");
    for (const surface of ["cli", "mcp"]) {
      const ref = cliJson(engramHome, session, "add", `Landing ${surface}`).work.short_ref;
      cliJson(engramHome, session, "claim", ref);
      const commit = createHash("sha1").update(`landed ${surface}`).digest("hex");
      const landing = { commit, remote: "origin", branch: "master", pushed_at: "2026-09-29T05:00:00Z", installed_build: build };
      const done = surface === "cli"
        ? cliJson(engramHome, session, "done", ref, "Landed", "--landed", commit, "--remote", "origin",
          "--branch", "master", "--pushed-at", landing.pushed_at, "--installed-build", build)
        : receipt(await client.call("done", { work_ref: ref, summary: "Landed", landing }));
      assert.equal(done.work.lifecycle, "completed");
      assert.equal(done.landing.commit, commit);
      for (const shown of [cliJson(engramHome, session, "show", ref), receipt(await client.call("show", { work_ref: ref }))]) {
        assert.equal(shown.landing.commit, commit);
        assert.equal(shown.landing.remote, "origin");
        assert.equal(shown.landing.branch, "master");
        assert.equal(shown.landing.installed_build, build);
        assert.equal(shown.landing.installed_build_assurance, "asserted, unchecked");
        assertTerseShow(shown);
      }
      const text = cliWord(engramHome, session, "show", ref);
      assert.equal(text.status, 0, text.stderr);
      assert.match(text.stdout, new RegExp(`landing: ${commit} on origin/master, pushed `));
    }
    // A partial landing is refused before anything is recorded, and an item
    // completed without a landing says none was recorded.
    const ref = cliJson(engramHome, session, "add", "No landing").work.short_ref;
    cliJson(engramHome, session, "claim", ref);
    const partial = cliWord(engramHome, session, "done", ref, "--landed", "0".repeat(40), "--json");
    assert.notEqual(partial.status, 0);
    assert.equal(cliJson(engramHome, session, "show", ref).status.work.lifecycle, "open");
    const done = cliJson(engramHome, session, "done", ref, "Delivered without landing");
    assert.equal(done.landing, undefined);
    const shown = cliJson(engramHome, session, "show", ref);
    assert.equal(shown.landing, "no landing recorded");
    assertTerseShow(shown);
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

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
    assert.equal(cliJson(engramHome, session, "show", ref).blockers_total, 3);
    // A selector that names no blocker is refused on both surfaces.
    const malformed = cliWord(engramHome, session, "update", ref, "--unblock", "--blocker", "b1-!!", "--json");
    assert.notEqual(malformed.status, 0);
    assert.match(malformed.stdout + malformed.stderr, /not a blocker selector/u);
    const refused = await client.call("update", { work_ref: ref, action: "unblock", blocker: `${shown.blockers[0].blocker}=` });
    assert.match(JSON.stringify(refused), /not a blocker selector/u);
    const misplaced = await client.call("update", { work_ref: ref, action: "cancel", reason: "no", blocker: shown.blockers[0].blocker });
    assert.match(JSON.stringify(misplaced), /requires action unblock/u);
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
      `cleared blocker ${second.blocker} (manual) "Await the release": "Wait on three things"`,
      `cleared blocker ${third.blocker} (manual) "Await the vendor": "Wait on three things"`,
    ].sort());
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("doctor checks recorded landings against a local repository only on request", (t) => {
  const engramHome = fixtureHome("engram-landing-doctor-", t);
  const session = "landing-doctor";
  try {
    buildAndInit(engramHome);
    const repository = join(engramHome, "landed");
    mkdirSync(repository);
    const emptyConfig = join(engramHome, "empty.gitconfig");
    writeFileSync(emptyConfig, "");
    // The scratch repository ignores the developer's git configuration.
    const git = (...args) => {
      const run = spawnSync("git", ["-c", "user.email=landing@test", "-c", "user.name=landing",
        "-c", "commit.gpgsign=false", "-c", "core.hooksPath=", "-c", "init.defaultBranch=master", ...args], {
        cwd: repository,
        encoding: "utf8",
        env: { ...process.env, GIT_TERMINAL_PROMPT: "0", GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: emptyConfig },
      });
      assert.equal(run.status, 0, run.stderr);
      return run.stdout.trim();
    };
    git("init", "-q", ".");
    git("commit", "-q", "--allow-empty", "-m", "landed");
    const landed = git("rev-parse", "HEAD");
    git("update-ref", "refs/remotes/origin/master", landed);
    const doneReceipts = new Map();
    const land = (title, commit, installedBuild) => {
      const ref = cliJson(engramHome, session, "add", title).work.short_ref;
      cliJson(engramHome, session, "claim", ref);
      doneReceipts.set(ref, cliJson(engramHome, session, "done", ref, "Landed", "--landed", commit,
        "--remote", "origin", "--branch", "master", "--pushed-at", "2026-09-29T05:00:00Z",
        ...(installedBuild ? ["--installed-build", installedBuild] : [])));
      return ref;
    };
    const doctor = (...args) => spawnSync(binary, ["--home", engramHome, ...args], { cwd: root, encoding: "utf8" });
    const statuses = (report) => report.landings.map(({ work_ref, status }) => [work_ref, status]);
    const verifiedRef = land("Verified landing", landed);

    // The project file's directory is the default repository.
    const projectFile = join(repository, ".engram-project");
    writeFileSync(projectFile, readFileSync(join(root, ".engram-project")));
    const byDefault = doctor("--project-file", projectFile, "doctor", "--check-landings", "--json");
    assert.equal(byDefault.status, 0, byDefault.stderr);
    const report = JSON.parse(byDefault.stdout);
    assert.equal(report.mode, "landing_check");
    assert.equal(report.repository_problem, null);
    assert.equal(report.recorded, 1);
    assert.deepEqual(statuses(report), [[verifiedRef, "verified"]]);

    // A git.exe in the directory the doctor runs from, here the repository
    // under check, is never run in place of git: this one is no program, so
    // running it would fail every git call and leave nothing verified.
    const planted = join(repository, "git.exe");
    writeFileSync(planted, "not a program\n");
    const fromInside = spawnSync(binary, ["--home", engramHome, "--project-file", projectFile, "doctor",
      "--check-landings", "--json"], { cwd: repository, encoding: "utf8" });
    rmSync(planted);
    assert.equal(fromInside.status, 0, fromInside.stderr);
    assert.deepEqual(statuses(JSON.parse(fromInside.stdout)), [[verifiedRef, "verified"]]);

    // An absent commit is named, and the check fails.
    const absentRef = land("Absent landing", "f".repeat(40));
    const checked = doctor("doctor", "--check-landings", "--repo", repository, "--json");
    assert.notEqual(checked.status, 0);
    const found = JSON.parse(checked.stdout);
    assert.deepEqual(statuses(found), [[verifiedRef, "verified"], [absentRef, "commit_absent"]]);
    assert.match(found.landings[1].finding, /absent from this repository/);
    const text = doctor("doctor", "--check-landings", "--repo", repository);
    assert.notEqual(text.status, 0);
    assert.match(text.stdout, new RegExp(`landing ${absentRef}: commit f{40} absent from this repository`));

    // An installed build is the agent's word, shown in full beside
    // "asserted, unchecked" and kept apart from Git's answer: a build that is
    // this doctor's own reads the same as one with its prefix and another
    // build's remainder, and neither changes the verdict on the commit.
    const own = JSON.parse(doctor("doctor", "--json").stdout).build_fingerprint;
    assert.match(own, /^[0-9a-f]{64}$/u);
    const otherBuild = createHash("sha256").update(`not ${own}`).digest("hex");
    const wrongRemainder = `${own.slice(0, 12)}${otherBuild.slice(12)}`;
    const builds = [["Own build", own], ["Wrong remainder", wrongRemainder]];
    const builtRefs = builds.map(([title, build]) => [land(title, landed, build), build]);
    const withBuilds = doctor("doctor", "--check-landings", "--repo", repository, "--json");
    const buildReport = JSON.parse(withBuilds.stdout);
    for (const [ref, build] of builtRefs) {
      const entry = buildReport.landings.find(({ work_ref }) => work_ref === ref);
      assert.deepEqual([entry.status, entry.installed_build, entry.installed_build_assurance],
        ["verified", build, "asserted, unchecked"]);
      const shown = cliJson(engramHome, session, "show", ref);
      assert.deepEqual([shown.landing.installed_build, shown.landing.installed_build_assurance],
        [build, "asserted, unchecked"]);
    }
    const bare = buildReport.landings.find(({ work_ref }) => work_ref === verifiedRef);
    assert.deepEqual([bare.installed_build, bare.installed_build_assurance], [null, "no installed build recorded"]);
    // The documented difference: the check's report writes an absent build as
    // null, while show keeps the seal's shape and omits the member.
    assert.ok(Object.hasOwn(bare, "installed_build"));
    const bareShown = cliJson(engramHome, session, "show", verifiedRef).landing;
    assert.ok(!Object.hasOwn(bareShown, "installed_build"), JSON.stringify(bareShown));
    assert.equal(bareShown.installed_build_assurance, "no installed build recorded");
    const bareDone = doneReceipts.get(verifiedRef).landing;
    assert.ok(!Object.hasOwn(bareDone, "installed_build"), JSON.stringify(bareDone));
    assert.equal(bareDone.installed_build_assurance, "no installed build recorded");
    const buildText = doctor("doctor", "--check-landings", "--repo", repository).stdout;
    for (const [, build] of builtRefs) {
      assert.ok(buildText.split(/\r?\n/u).includes(`  installed build: ${build} (asserted, unchecked)`), buildText);
    }
    assert.match(buildText, /^  installed build: no installed build recorded$/mu);
    assert.doesNotMatch(buildText, /matches|differs/u);

    // A directory that is not a repository is refused, with git's exit code.
    const plain = join(engramHome, "plain");
    mkdirSync(plain);
    const refused = doctor("doctor", "--check-landings", "--repo", plain, "--json");
    assert.notEqual(refused.status, 0);
    const refusedReport = JSON.parse(refused.stdout);
    assert.match(
      refusedReport.repository_problem,
      /^git could not open .+ as a repository \(git exited with 128\)$/,
    );
    // Every landing is still listed, unchecked by Git, with its installed
    // build in full and what it is worth: the build does not depend on Git.
    const everyRef = [verifiedRef, absentRef, ...builtRefs.map(([ref]) => ref)];
    assert.deepEqual(statuses(refusedReport).map(([ref]) => ref).sort(), [...everyRef].sort());
    for (const entry of refusedReport.landings) {
      assert.equal(entry.status, "unverifiable");
      assert.match(entry.finding, /could not be checked: the repository could not be read$/u);
      const build = builtRefs.find(([ref]) => ref === entry.work_ref)?.[1] ?? null;
      assert.deepEqual([entry.installed_build, entry.installed_build_assurance],
        [build, build ? "asserted, unchecked" : "no installed build recorded"]);
    }
    const refusedText = doctor("doctor", "--check-landings", "--repo", plain);
    assert.notEqual(refusedText.status, 0);
    const refusedLines = refusedText.stdout.split(/\r?\n/u);
    for (const [, build] of builtRefs) {
      assert.ok(refusedLines.includes(`  installed build: ${build} (asserted, unchecked)`), refusedText.stdout);
    }
    assert.equal(refusedLines.filter((line) => line === "  installed build: no installed build recorded").length, 2,
      refusedText.stdout);

    // A directory that does not exist is no different: every landing is still
    // listed with its installed build in full and its assurance words, in
    // JSON and text, with Git's answer unavailable.
    const missing = join(engramHome, "no-such-directory");
    const missingJson = doctor("doctor", "--check-landings", "--repo", missing, "--json");
    assert.notEqual(missingJson.status, 0);
    const missingReport = JSON.parse(missingJson.stdout);
    assert.ok(missingReport.repository_problem, missingJson.stdout);
    assert.deepEqual(statuses(missingReport).map(([ref]) => ref).sort(), [...everyRef].sort());
    for (const entry of missingReport.landings) {
      assert.equal(entry.status, "unverifiable");
      const build = builtRefs.find(([ref]) => ref === entry.work_ref)?.[1] ?? null;
      assert.deepEqual([entry.installed_build, entry.installed_build_assurance],
        [build, build ? "asserted, unchecked" : "no installed build recorded"]);
    }
    const missingText = doctor("doctor", "--check-landings", "--repo", missing);
    assert.notEqual(missingText.status, 0);
    const missingLines = missingText.stdout.split(/\r?\n/u);
    for (const [, build] of builtRefs) {
      assert.ok(missingLines.includes(`  installed build: ${build} (asserted, unchecked)`), missingText.stdout);
    }
    assert.equal(missingLines.filter((line) => line === "  installed build: no installed build recorded").length, 2,
      missingText.stdout);

    // Without the flag, the doctor's audit is unchanged and runs no check.
    const audit = doctor("doctor", "--json");
    assert.equal(audit.status, 0, audit.stderr);
    assert.equal(JSON.parse(audit.stdout).mode, undefined);
  } finally {
    removeFixtureHomes(engramHome);
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
      assert.ok(Buffer.byteLength(JSON.stringify(value)) < 12288);
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
    // A generation is a plain token on both words and both routes.
    for (const word of ["next", "memories"]) {
      const refused = structuredError(
        await client.call(word, { context_generation: "two words" }),
        "memory_invalid",
      );
      assert.match(refused.details.remedy, /1 to 256 ASCII letters, digits, dots, underscores or dashes/);
      const cliRefused = cliWord(engramHome, session, word, "--context-generation", "two words", "--json");
      assert.notEqual(cliRefused.status, 0);
      assert.match(JSON.parse(cliRefused.stderr).error.message, /context_generation must be 1 to 256 ASCII/);
    }
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
      // An observation on an item this session does not hold leaves its
      // focus: a following bare note still lands on the held item.
      const bare = `Bare note after the observation ${surface}`;
      const after = surface === "cli"
        ? cliJson(engramHome, session, "note", bare)
        : receipt(await client.call("note", { text: bare }));
      assert.equal(after.work.short_ref, held);
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
    assert.match(identity.build.source_revision, /^(?:unavailable|[0-9a-f]{40}(?:\+dirty)?)$/u);
    const [commit, marker] = identity.build.source_revision.split("+");
    const revision = commit === "unavailable" ? commit : `${commit.slice(0, 12)}${marker ? `+${marker}` : ""}`;
    assert.equal(version.stdout.trim(), `engram ${identity.build.package_version} build ${identity.build_fingerprint.slice(0, 12)} (exe ${identity.build.executable_sha256.slice(0, 12)}, schema ${identity.build.schema_reference.slice(0, 12)}, rev ${revision})`);
    client = new McpClient(engramHome, "build-reader");
    await client.initialize();
    for (const verbose of [false, true, false]) {
      const next = receipt(await client.call("next", { verbose }));
      assert.equal(next.build_fingerprint, identity.build_fingerprint);
      assert.equal(JSON.stringify(next).match(/"build_fingerprint"/gu).length, 1);
      assert.ok(Buffer.byteLength(JSON.stringify(next)) < 12288);
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
    // MCP names the field the caller passes, not the CLI flag.
    assert.ok(added.reminders.includes("acceptance defaulted to the title being done; set acceptance"));
    assert.ok(added.reminders.every((line) => !line.includes("--accept")));
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
      assert.ok(Buffer.byteLength(JSON.stringify(bounded)) < 12288);
      for (const content of result.content.filter(({ type }) => type === "text")) {
        assert.ok(Buffer.byteLength(content.text) < 12288);
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
    assert.ok(Buffer.byteLength(JSON.stringify(full)) < 12288);
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

test("every MCP word refuses an unknown argument by name and lists the accepted ones", async (t) => {
  const engramHome = fixtureHome("engram-mcp-unknown-argument-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "unknown-argument");
    await client.initialize();
    const argumentText = (result) => {
      assert.equal(result.isError, true, JSON.stringify(result));
      return result.content.filter(({ type }) => type === "text").map(({ text }) => text).join("\n");
    };
    const tools = await client.tools();
    assert.equal(tools.length, 15);
    for (const tool of tools) {
      assert.equal(tool.inputSchema.additionalProperties, false, tool.name);
      const text = argumentText(await client.call(tool.name, { unknown_argument: true }));
      assert.match(text, /unknown field `unknown_argument`, expected /u, `${tool.name}: ${text}`);
      for (const field of Object.keys(tool.inputSchema.properties)) {
        assert.ok(text.includes(`\`${field}\``), `${tool.name} names ${field}: ${text}`);
      }
    }
    // A misspelled criterion field is refused whole, not dropped in favor
    // of a defaulted criterion.
    const before = receipt(await client.call("ls", { all: true })).total;
    const text = argumentText(await client.call("add", { title: "Misspelled criteria", accept: ["Criterion"] }));
    assert.match(text, /unknown field `accept`, expected one of /u, text);
    assert.ok(text.includes("`acceptance`"), text);
    assert.equal(receipt(await client.call("ls", { all: true })).total, before);
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

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
    assert.deepEqual(shown.status.work.acceptance, ["B", "A"]);
    assert.equal(shown.status.work.title, "Searchable first");
    assert.ok(shown.history.items.some(({ kind, summary }) => kind === "revised" && summary.startsWith("acceptance:")));
    for (const acceptance of [[], [""], ["good", " "]]) {
      structuredError(await client.call("update", { work_ref: first.short_ref, action: "revise", acceptance }), "work_invalid");
    }
    receipt(await client.call("update", { work_ref: first.short_ref, action: "revise", title: "Searchable renamed" }));
    shown = receipt(await client.call("show", { work_ref: first.short_ref }));
    assert.deepEqual(shown.status.work.acceptance, ["B", "A"]);
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
    // Match McpClient's default attribution: host context must not alter this fixture.
    const importEnvironment = { ...process.env };
    delete importEnvironment.ENGRAM_ACTOR_CONTEXT;
    const invoke = (...args) => spawnSync(binary, ["--home", engramHome, "import",
      "--actor-id", importingSession, "--session-id", importingSession, ...args],
      { cwd: root, encoding: "utf8", env: importEnvironment });
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
    const peerOrientation = receipt(await client.call("next", { peek: true }));
    const peerChanges = peerOrientation.changes
      .filter((line) => line.includes("external source changed"));
    assert.equal(peerChanges.length, 2, JSON.stringify(peerOrientation));
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
    assert.ok(Buffer.byteLength(JSON.stringify(after)) < 12288);
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

test("evaluated acceptance policy over the real transports: locators, source freshness, provenance", async (t) => {
  const engramHome = fixtureHome("engram-evaluated-policy-", t);
  const holder = "evaluated-holder";
  const peer = "evaluated-peer";
  let client;
  try {
    buildAndInit(engramHome);
    const policy = spawnSync(binary, [
      "--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session,independent-session", "--mechanical-basis", "asserted",
      "--require-source-freshness", "--authorized-by", "dogfood-operator",
      "--idempotency-key", "dogfood-enable-evaluation",
    ], { cwd: root, encoding: "utf8" });
    assert.equal(policy.status, 0, policy.stderr);
    client = new McpClient(engramHome, holder);
    await client.initialize();
    const isGate = (row) => String(row.family).toLowerCase() === "gates";

    // MCP: same-session evaluation citing the printed gate locator; an
    // observation locator refuses; done without, with a changed, and with
    // the matching fingerprint.
    const ref = cliJson(engramHome, holder, "add", "Evaluated over MCP", "--accept", "the build passes").work.short_ref;
    // A supplied evaluation_mode reaches the store only through the
    // evaluation_mode action; any other action refuses it before effects.
    const beforeMisroute = receipt(await client.call("show", { work_ref: ref }));
    const peekBefore = receipt(await client.call("next", { peek: true }));
    assert.equal(typeof peekBefore.read_cut.project_position, "number");
    for (const misrouted of ["same_session", ""]) {
      const wrong = structuredError(
        await client.call("update", { work_ref: ref, action: "revise", title: "Misrouted mode", evaluation_mode: misrouted }),
        "invalid_argument",
      );
      assert.equal(wrong.details.field, "evaluation_mode");
      // No effect: the item, the project feed, and the focus are unchanged.
      assert.deepEqual(receipt(await client.call("show", { work_ref: ref })), beforeMisroute);
      const peekAfter = receipt(await client.call("next", { peek: true }));
      assert.equal(peekAfter.read_cut.project_position, peekBefore.read_cut.project_position, JSON.stringify(misrouted));
      assert.deepEqual(peekAfter.focus, peekBefore.focus, JSON.stringify(misrouted));
    }
    receipt(await client.call("update", { work_ref: ref, action: "evaluation_mode", evaluation_mode: "same_session" }));
    assert.match(cliWord(engramHome, holder, "show", ref).stdout, /evaluation mode: same_session/u);
    receipt(await client.call("update", { work_ref: ref, action: "evaluation_mode" }));
    assert.doesNotMatch(cliWord(engramHome, holder, "show", ref).stdout, /evaluation mode:/u);
    // A peer marks the task for same-session evaluation before the holder
    // takes it; a mark the holder set itself would not waive independence.
    cliJson(engramHome, peer, "update", ref, "--evaluation-mode", "same_session");
    cliJson(engramHome, holder, "claim", ref);
    cliJson(engramHome, holder, "gate", "cargo-test", "--work-ref", ref);
    cliJson(engramHome, peer, "note", ref, "peer observation without holding the run");
    // A refused evaluated completion carries a recovery command the CLI
    // parses and runs read-only: navigation to the criteria and evidence,
    // never an evaluation template with a pre-filled verdict.
    const runRecovery = (refusalJson, expectedCode) => {
      assert.equal(refusalJson.code, expectedCode);
      const command = refusalJson.next[0];
      const parts = command.split(" ");
      assert.deepEqual(parts.slice(0, 2), ["engram", "work"], command);
      const run = cliWord(engramHome, holder, ...parts.slice(2));
      assert.equal(run.status, 0, `${command}\n${run.stderr}`);
      assert.equal(parts[2], "show", command);
      assert.ok(parts.includes("--notes"), command);
      assert.doesNotMatch(command, /=pass/u);
    };
    const missingRefusal = cliWord(engramHome, holder, "done", ref, "Delivered", "--json");
    assert.equal(missingRefusal.status, 2, missingRefusal.stderr);
    const revisionBefore = cliJson(engramHome, holder, "show", ref).status.work.revision;
    runRecovery(JSON.parse(missingRefusal.stdout), "missing_acceptance_evaluation");
    assert.equal(cliJson(engramHome, holder, "show", ref).status.work.revision, revisionBefore);
    const shown = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(typeof shown.evidence_basis, "number");
    const records = receipt(await client.call("show", { work_ref: ref, notes: true, gates: true }));
    const gate = records.notes.find(isGate);
    const observation = records.notes.find((row) => row.non_holder === true);
    assert.ok(gate && observation, JSON.stringify(records.notes));
    const verdicts = (evidence) => [{ criterion: 1, verdict: "pass", basis: "asserted", rationale: "the gate passed", evidence }];
    const base = { work_ref: ref, mode: "same_session", acceptance_basis: shown.acceptance_basis, evidence_basis: shown.evidence_basis };
    const refused = structuredError(await client.call("evaluate", { ...base, verdicts: verdicts([observation.locator]) }), "acceptance_evaluation_refused");
    assert.match(JSON.stringify(refused), /observation/);
    assert.equal(refused.details.cause.kind, "citation");
    assert.equal(refused.details.cause.mismatch, "not_on_run");
    assert.equal(refused.details.cause.citation, observation.locator);
    assert.equal(refused.details.cause.evaluated_cut, base.evidence_basis);
    assert.equal(refused.details.cause.remedy, "read_run_evidence");
    assert.ok(refused.next.some((command) => command.includes(ref)));
    const pinned = structuredError(await client.call("evaluate", { ...base, mode: "independent_session", verdicts: verdicts([gate.locator]) }), "acceptance_evaluation_refused");
    assert.equal(pinned.details.cause.kind, "eligibility");
    assert.equal(pinned.details.cause.mismatch, "task_pin_mismatch");
    assert.equal(pinned.details.cause.task_mark, "same_session");
    const cliPinned = cliWord(engramHome, holder, "evaluate", ref,
      "--mode", "independent-session", "--acceptance-basis", String(base.acceptance_basis),
      "--evidence-basis", String(base.evidence_basis), "--verdict", "1=pass:asserted",
      "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`, "--json");
    assert.equal(cliPinned.status, 1, cliPinned.stderr);
    const pinError = JSON.parse(cliPinned.stderr).error;
    assert.equal(pinError.code, pinned.code);
    assert.equal(pinError.message, pinned.message);
    assert.deepEqual(pinError.details, pinned.details);
    const cliCitation = cliWord(engramHome, holder, "evaluate", ref,
      "--mode", "same-session", "--acceptance-basis", String(base.acceptance_basis),
      "--evidence-basis", String(base.evidence_basis), "--verdict", "1=pass:asserted",
      "--rationale", "1=the gate passed", "--evidence", `1=${observation.locator}`, "--json");
    assert.equal(cliCitation.status, 1, cliCitation.stderr);
    assert.deepEqual(JSON.parse(cliCitation.stderr).error.details, refused.details);
    const evaluated = receipt(await client.call("evaluate", { ...base, source_fingerprint: "sha256:tree-a", verdicts: verdicts([gate.locator]) }));
    assert.equal(evaluated.evaluation.passed, 1);
    assert.equal(evaluated.evaluation.verdicts_total, 1);
    const unmeasured = receipt(await client.call("done", { work_ref: ref, summary: "Delivered" }));
    assert.equal(unmeasured.code, "acceptance_evaluation_stale");
    assert.deepEqual(unmeasured.recovery.cause, { kind: "acceptance_evaluation_stale", reason: "source" });
    assert.equal(unmeasured.recovery.source.mismatch, "completion_measurement_missing");
    assert.equal(unmeasured.recovery.source.remedy, "measure_source_and_retry");
    assert.equal(unmeasured.recovery.source.evaluation, evaluated.evaluation.hash);
    assert.match(unmeasured.remedy, /fresh source measurement/u);
    const unmeasuredShow = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(unmeasuredShow.acceptance_evaluation.source_recovery, undefined);
    assert.ok(unmeasured.reminders.some((line) => line.includes("--source-fingerprint")), JSON.stringify(unmeasured.reminders));
    const changed = receipt(await client.call("done", { work_ref: ref, summary: "Delivered", source_fingerprint: "sha256:tree-b" }));
    assert.equal(changed.code, "acceptance_evaluation_stale");
    assert.equal(changed.recovery.source.mismatch, "completion_fingerprint_mismatch");
    assert.equal(changed.recovery.source.remedy, "evaluate_current_source");
    assert.equal(changed.recovery.source.expected_fingerprint, "sha256:tree-a");
    assert.equal(changed.recovery.source.presented_fingerprint, "sha256:tree-b");
    assert.match(changed.remedy, /new acceptance evaluation/u);
    assert.match(changed.remedy, /copying.*insufficient/u);
    assert.ok(changed.reminders.some((line) => line.includes("(source)")), JSON.stringify(changed.reminders));
    assert.equal(receipt(await client.call("show", { work_ref: ref })).status.work.lifecycle, "open");
    const sealed = receipt(await client.call("done", { work_ref: ref, summary: "Delivered", source_fingerprint: "sha256:tree-a" }));
    assert.equal(sealed.work.lifecycle, "completed");
    assert.equal(sealed.acceptance.provenance, "evaluated");
    assert.equal(sealed.acceptance.mode, "same_session");
    assert.equal(sealed.acceptance.evaluation, evaluated.evaluation.hash);
    // The evaluator label is the display identity: the caller's own session
    // is "you"; any other session is an opaque peer label, never its id.
    assert.equal(sealed.acceptance.evaluator, "you");
    const completedShow = receipt(await client.call("show", { work_ref: ref }));
    assert.equal(completedShow.acceptance.provenance, "evaluated");
    assert.equal(completedShow.acceptance.evaluation, evaluated.evaluation.hash);

    // CLI: an independent peer evaluates over the CLI; the holder's done
    // refuses without a fingerprint and seals with --source-fingerprint.
    const cliRef = cliJson(engramHome, holder, "add", "Evaluated over CLI", "--accept", "the build passes").work.short_ref;
    cliJson(engramHome, holder, "claim", cliRef);
    cliJson(engramHome, holder, "gate", "cargo-test", "--work-ref", cliRef);
    const cliShown = cliJson(engramHome, holder, "show", cliRef);
    const cliGate = cliJson(engramHome, peer, "show", cliRef, "--notes", "--gates").notes.find(isGate);
    assert.ok(cliGate);
    const cliEvaluated = cliJson(engramHome, peer, "evaluate", cliRef,
      "--mode", "independent-session",
      "--acceptance-basis", String(cliShown.acceptance_basis),
      "--evidence-basis", String(cliShown.evidence_basis),
      "--verdict", "1=pass:asserted", "--rationale", "1=the gate passed",
      "--evidence", `1=${cliGate.locator}`, "--source-fingerprint", "sha256:tree-c");
    assert.equal(cliEvaluated.evaluation.passed, 1);
    const cliRefused = cliWord(engramHome, holder, "done", cliRef, "Delivered", "--json");
    assert.equal(cliRefused.status, 2, cliRefused.stderr);
    assert.equal(JSON.parse(cliRefused.stdout).code, "acceptance_evaluation_stale");
    assert.equal(JSON.parse(cliRefused.stdout).recovery.source.mismatch, "completion_measurement_missing");
    assert.equal(JSON.parse(cliRefused.stdout).recovery.source.remedy, "measure_source_and_retry");
    runRecovery(JSON.parse(cliRefused.stdout), "acceptance_evaluation_stale");
    const cliSealed = cliJson(engramHome, holder, "done", cliRef, "Delivered", "--source-fingerprint", "sha256:tree-c");
    assert.equal(cliSealed.work.lifecycle, "completed");
    assert.equal(cliSealed.acceptance.provenance, "evaluated");
    assert.equal(cliSealed.acceptance.mode, "independent_session");
    assert.match(cliSealed.acceptance.evaluator, /^peer-[0-9a-f]+$/u);
    assert.doesNotMatch(cliSealed.acceptance.evaluator, /evaluated-peer/u);
    const cliText = cliWord(engramHome, holder, "show", cliRef);
    assert.equal(cliText.status, 0, cliText.stderr);
    assert.match(cliText.stdout, /acceptance: evaluated \(independent_session, asserted\) by peer-[0-9a-f]+/u);
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("open obligations guide evaluation timing across MCP, host observations, and completion", async (t) => {
  const engramHome = fixtureHome("engram-evaluation-obligations-", t);
  const projectId = readFileSync(join(root, ".engram-project"), "utf8").trim();
  const libraryFile = { kind: "path", project_id: projectId, segments: ["src", "lib.rs"], coverage: "exact" };
  const fingerprint = (value) => createHash("sha256").update(value).digest("hex");
  const isGate = (row) => String(row.family).toLowerCase() === "gates";
  const clients = [];
  const controlClients = [];
  const run = (args) => {
    const result = spawnSync(binary, ["--home", engramHome, ...args], { cwd: root, encoding: "utf8" });
    assert.equal(result.status, 0, result.stderr);
    return JSON.parse(result.stdout);
  };
  const core = (session, operation, input) => run([
    "work", "--actor-id", session, "--session-id", session,
    "core", operation, "--input", JSON.stringify(input),
  ]);
  const focus = (session, ref) => run([
    "work", "--actor-id", session, "--session-id", session, "core", "focus", ref,
  ]);
  class ControlClient {
    constructor(session) {
      this.pending = [];
      this.buffer = "";
      this.stderr = "";
      this.child = spawn(binary, [
        "--home", engramHome, "control", "--actor-id", session,
        "--session-id", session, "--source-skill", "engram-dogfood",
      ], { cwd: root, stdio: ["pipe", "pipe", "pipe"] });
      this.child.stdout.on("data", (chunk) => {
        this.buffer += chunk.toString("utf8");
        for (;;) {
          const newline = this.buffer.indexOf("\n");
          if (newline < 0) break;
          const line = this.buffer.slice(0, newline).trim();
          this.buffer = this.buffer.slice(newline + 1);
          if (line) this.pending.shift()?.(JSON.parse(line));
        }
      });
      this.child.stderr.on("data", (chunk) => { this.stderr += chunk.toString("utf8"); });
    }
    request(payload) {
      return new Promise((resolvePromise) => {
        this.pending.push(resolvePromise);
        this.child.stdin.write(`${JSON.stringify(payload)}\n`);
      }).then((response) => {
        assert.equal(response.status, "ok", `${JSON.stringify(response)} ${this.stderr}`);
        return response.result;
      });
    }
    close() {
      return new Promise((resolvePromise) => {
        this.child.once("close", resolvePromise);
        this.child.stdin.end();
      });
    }
  }
  try {
    buildAndInit(engramHome);
    run([
      "control-policy", "set-acceptance-evaluation", "--modes", "same-session",
      "--mechanical-basis", "asserted", "--authorized-by", "dogfood-operator",
      "--idempotency-key", "evaluation-obligation-policy",
    ]);
    const selectRule = (ruleId, key) => run([
      "control-policy", "set-obligation-rule-set", "--input", JSON.stringify({
        schema_version: 1,
        rules: [{
          rule: { rule_id: ruleId, rule_version: 1 },
          trigger: "source_changed",
          requirement: { check_kind: "test" },
        }],
      }), "--authorized-by", "dogfood-operator", "--idempotency-key", key,
    ]);
    const createChangedItem = async (suffix, sourceChanges = 1) => {
      const session = `obligations-${suffix}`;
      const proposed = core(session, "propose", {
        kind: "root", title: `Evaluate ${suffix}`, outcome: "The evidence is judged",
        acceptance: ["A test gate passes"], work_kind: "task", idempotency_key: `add-${suffix}`,
      });
      const ref = proposed.work.short_ref;
      const claimed = core(session, "update", { kind: "claim", ttl_seconds: 300, idempotency_key: `claim-${suffix}` });
      assert.ok(claimed.receipt.control_binding);
      const control = new ControlClient(session);
      controlClients.push(control);
      const bound = await control.request({
        operation: "session_bind", external_ref: `local-work:${suffix}`,
        title: suffix, assurance: "turn_gated", mediated_effects: ["observe", "mutate_local"],
        work_binding: claimed.receipt.control_binding, capability_map_revision: 1,
        idempotency_key: `bind-${suffix}`,
      });
      for (let index = 0; index < sourceChanges; index += 1) {
        const turn = await control.request({
          operation: "turn_evaluate", routing_token: bound.routing_token,
          idempotency_key: `turn-${suffix}-${index}`,
          intent_fingerprint: fingerprint(`turn-${suffix}-${index}`),
          purpose: "ordinary", requested_effects: ["mutate_local"], resource_intents: [libraryFile],
        });
        assert.equal(turn.decision, "grant", JSON.stringify(turn));
        const grant = turn.grant.grant_id;
        assert.equal((await control.request({
          operation: "turn_begin", routing_token: bound.routing_token, grant_id: grant,
          delivery_tokens: [], idempotency_key: `begin-${suffix}-${index}`,
        })).decision, "begin");
        const checkpoint = await control.request({
          operation: "turn_checkpoint", routing_token: bound.routing_token, grant_id: grant,
          next_intent: "continue", observations: [{
            observation_id: `change-${suffix}-${index}`,
            action_fingerprint: fingerprint(`change-${suffix}-${index}`),
            effect: "mutate_local", outcome: "succeeded", source_changed: true,
            source_basis: {
              workspace_id: `workspace-${suffix}`,
              source_revision: `revision-${suffix}-${index}`,
            },
            observed_at: "2026-09-29T14:00:00Z",
          }], idempotency_key: `checkpoint-${suffix}-${index}`,
        });
        assert.equal(checkpoint.decision, "checkpointed", JSON.stringify(checkpoint));
      }
      cliJson(engramHome, session, "gate", "cargo-test", "--work-ref", ref);
      const mcp = new McpClient(engramHome, session);
      clients.push(mcp);
      await mcp.initialize();
      const shown = receipt(await mcp.call("show", { work_ref: ref }));
      assertTerseShow(shown);
      assert.equal(shown.evaluation_obligations.open_total, sourceChanges);
      assert.equal(shown.evaluation_obligations.omitted_open,
        sourceChanges - shown.evaluation_obligations.items.length);
      assert.equal(shown.evaluation_obligations.read_cut, shown.evidence_basis);
      const next = receipt(await mcp.call("next", { peek: true }));
      assert.match(JSON.stringify(next.evaluation_obligations), /before evaluation/u);
      const notes = receipt(await mcp.call("show", { work_ref: ref, notes: true, gates: true }));
      const gate = notes.notes.find(isGate);
      assert.ok(gate);
      const verdicts = [{ criterion: 1, verdict: "pass", basis: "asserted", rationale: "the test gate passed", evidence: [gate.locator] }];
      const evaluate = async () => {
        const current = receipt(await mcp.call("show", { work_ref: ref }));
        return receipt(await mcp.call("evaluate", {
          work_ref: ref, mode: "same_session", acceptance_basis: current.acceptance_basis,
          evidence_basis: current.evidence_basis, verdicts,
        }));
      };
      return { session, ref, mcp, shown, next, evaluate };
    };
    const waive = (item, key) => {
      const open = focus(item.session, item.ref).obligation_page.items.find((row) => row.state === "open");
      assert.ok(open);
      return run([
        "authority", "waive-obligation", "--obligation-id", open.obligation_id,
        "--expected-definition", open.definition, "--waived-by", "dogfood-human",
        "--reason", "reviewed this exact test obligation", "--idempotency-key", key,
      ]);
    };

    selectRule("source_mutation_requires_operator_test", "operator-test-rule");
    const late = await createChangedItem("late-waiver");
    assert.equal(late.shown.evaluation_obligations.items[0].action_required_before_evaluation, true);
    assert.equal(late.next.next.some((command) => command.startsWith("engram work done ")), false);
    const first = await late.evaluate();
    assert.equal(first.evaluation_obligations.open_total, 1);
    assert.equal(first.evaluation_obligations.items.length, 1);
    assert.equal(first.evaluation_obligations.read_cut, undefined);
    assert.match(JSON.stringify(first.reminders), /open obligations: 1 total, 0 not shown/u);
    assert.equal(first.next.some((command) => command.startsWith("engram work done ")), false);
    assert.match(JSON.stringify(first.reminders), /resolve obligations needing action/u);
    assert.equal(first.evaluation.passed, 1);
    waive(late, "waive-after-evaluation");
    const stale = receipt(await late.mcp.call("done", { work_ref: late.ref, summary: "Delivered" }));
    assert.equal(stale.code, "acceptance_evaluation_stale");
    await late.evaluate();
    assert.equal(receipt(await late.mcp.call("done", { work_ref: late.ref, summary: "Delivered" })).work.lifecycle, "completed");

    const early = await createChangedItem("early-waiver");
    waive(early, "waive-before-evaluation");
    assert.equal((await early.evaluate()).evaluation.passed, 1);
    assert.equal(receipt(await early.mcp.call("done", { work_ref: early.ref, summary: "Delivered" })).work.lifecycle, "completed");

    selectRule("source_mutation_requires_test", "stock-test-rule");
    const stock = await createChangedItem("stock-waiver", 12);
    assert.ok(stock.shown.evaluation_obligations.omitted_open > 0);
    assert.equal(stock.shown.evaluation_obligations.action_required_total, 0);
    const verboseStock = receipt(await stock.mcp.call("next", { peek: true, verbose: true }));
    assert.equal(verboseStock.evaluation_obligations.open_total, 12);
    assert.equal(verboseStock.evaluation_obligations.action_required_total, 0);
    assert.equal(verboseStock.evaluation_obligations.omitted_open,
      12 - verboseStock.evaluation_obligations.items.length);
    assert.equal(stock.shown.evaluation_obligations.items[0].action_required_before_evaluation, false);
    assert.match(stock.shown.evaluation_obligations.items[0].remedy, /no action before evaluation/u);
    await stock.evaluate();
    const candidate = cliJson(engramHome, "ready-candidate-author", "add", "Ready candidate", "--accept", "A criterion");
    const stockNext = receipt(await stock.mcp.call("next", { peek: true }));
    assert.ok(stockNext.ready.some((item) => item.ref === candidate.work.short_ref));
    assert.ok(stockNext.next.some((command) => command.startsWith(`engram work done ${stock.ref} `)));
    assert.doesNotMatch(JSON.stringify(stockNext.reminders), /request acceptance evaluation/u);
    assert.equal(receipt(await stock.mcp.call("done", { work_ref: stock.ref, summary: "Delivered" })).work.lifecycle, "completed");
  } finally {
    await Promise.all(clients.map((client) => client.close()));
    await Promise.all(controlClients.map((client) => client.close()));
    removeFixtureHomes(engramHome);
  }
});

test("a carried failure over the real transports: shown, refused until another evaluator names it, then recorded", async (t) => {
  const engramHome = fixtureHome("engram-carried-failure-", t);
  const holder = "carried-holder";
  const reviewer = "carried-reviewer";
  let client;
  try {
    buildAndInit(engramHome);
    const policy = spawnSync(binary, [
      "--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session,independent-session", "--mechanical-basis", "asserted",
      "--authorized-by", "dogfood-operator", "--idempotency-key", "dogfood-carried-failure",
    ], { cwd: root, encoding: "utf8" });
    assert.equal(policy.status, 0, policy.stderr);
    const ref = cliJson(engramHome, holder, "add", "Carried item", "--accept", "the report lists every store").work.short_ref;
    cliJson(engramHome, holder, "claim", ref);
    cliJson(engramHome, holder, "gate", "cargo-test");
    const shown = cliJson(engramHome, holder, "show", ref);
    const bases = (read) => ["--acceptance-basis", String(read.acceptance_basis), "--evidence-basis", String(read.evidence_basis)];
    // A session that never held the run records the failure.
    const failed = cliJson(engramHome, "carried-judge", "evaluate", ref, "--mode", "independent-session", ...bases(shown),
      "--verdict", "1=fail:judgment", "--rationale", "1=it lists one store");
    const failedId = failed.evaluation.hash;
    // The executor rewords the criterion the evaluation failed.
    cliJson(engramHome, holder, "update", ref, "--accept", "the report lists some stores");
    const carried = cliJson(engramHome, holder, "show", ref);
    assert.deepEqual(carried.acceptance_evaluation.carried_failure, {
      evaluation: failedId,
      revised_by: "executor",
      judged_revision: failed.evaluation.work_revision,
      failing: 1,
      supersedes_required: true,
    });
    const gate = cliJson(engramHome, holder, "show", ref, "--notes", "--gates").notes
      .find((row) => String(row.family).toLowerCase() === "gates");
    assert.ok(gate);

    client = new McpClient(engramHome, holder);
    await client.initialize();
    const request = {
      work_ref: ref, mode: "same_session",
      acceptance_basis: carried.acceptance_basis, evidence_basis: carried.evidence_basis,
      verdicts: [{ criterion: 1, verdict: "pass", basis: "asserted", rationale: "the gate passed", evidence: [gate.locator] }],
    };
    const refused = structuredError(await client.call("evaluate", request), "acceptance_evaluation_refused");
    assert.equal(refused.details.reason, "carried_failure_unacknowledged");
    assert.equal(refused.details.failed_evaluation, failedId);
    assert.match(refused.details.remedy, /--supersedes RECORD_ID/u);
    // The executor naming its own failure is not someone else accepting
    // the revision.
    const selfNamed = structuredError(await client.call("evaluate", { ...request, supersedes: failedId }), "acceptance_evaluation_refused");
    assert.equal(selfNamed.details.reason, "carried_failure_self_acknowledged");
    assert.equal(selfNamed.details.failed_evaluation, failedId);
    assert.match(selfNamed.details.remedy, /independent_session/u);
    // A reviewer that never held the run names it over the CLI.
    const recorded = cliJson(engramHome, reviewer, "evaluate", ref, "--mode", "independent-session", ...bases(carried),
      "--verdict", "1=pass:asserted", "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`,
      "--supersedes", failedId);
    assert.equal(recorded.evaluation.supersedes, failedId);

    // Once a newer evaluation ends the carry, the CLI flag names nothing.
    const after = cliJson(engramHome, holder, "show", ref);
    assert.equal(after.acceptance_evaluation.carried_failure, undefined);
    assert.equal(after.acceptance_evaluation.supersedes, failedId);
    const nothing = cliWord(engramHome, holder, "evaluate", ref, "--mode", "same-session", ...bases(after),
      "--verdict", "1=pass:asserted", "--rationale", "1=the gate passed", "--evidence", `1=${gate.locator}`,
      "--supersedes", failedId, "--json");
    assert.notEqual(nothing.status, 0);
    assert.equal(JSON.parse(nothing.stderr || nothing.stdout).error.details.reason, "nothing_to_supersede");
    assert.equal(cliJson(engramHome, holder, "done", ref, "Delivered").work.lifecycle, "completed");
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

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
    assert.match(done, /^done w-[0-9a-f]{12} "Dogfood work CLI" \[completed; revision \d+\]\nasserted 1 acceptance criterion satisfied; completion changed no criterion\nacceptance: self-asserted\nfull detail: engram work show 'w-[0-9a-f]{12}'\ncriterion evidence: 1 of 1 criteria unlinked \(1 shown\)\n  criterion 1: no evidence linked to this criterion\nreminders: none\nnext:\n/u);
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
    // Both sessions receive the same fifteen-tool MCP surface with only
    // project and asserted actor/session bindings.
    a = new McpClient(engramHome, sessionA);
    b = new McpClient(engramHome, sessionB);
    await Promise.all([a.initialize(), b.initialize()]);
    assert.match(
      a.instructions,
      /Fourteen words: next, ls, show, add, claim, update, gate, evaluate, note, done, handoff, remember, memories, forget \(plus search\)/u,
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
    assert.match(blockedShow.blockers[0].blocker, /^b1-[A-Za-z0-9_-]+$/u);
    assert.ok(
      blockedShow.next.includes(`engram work update ${workRef} --unblock --blocker ${blockedShow.blockers[0].blocker}`),
      JSON.stringify(blockedShow.next),
    );
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

test("evaluation history over MCP: a failing record then a passing one from one session", async (t) => {
  const engramHome = fixtureHome("engram-evaluation-history-", t);
  const holder = "history-holder";
  let client;
  try {
    buildAndInit(engramHome);
    const policy = spawnSync(binary, [
      "--home", engramHome, "control-policy", "set-acceptance-evaluation",
      "--modes", "same-session", "--mechanical-basis", "asserted",
      "--authorized-by", "dogfood-operator",
      "--idempotency-key", "dogfood-history-evaluation",
    ], { cwd: root, encoding: "utf8" });
    assert.equal(policy.status, 0, policy.stderr);
    client = new McpClient(engramHome, holder);
    await client.initialize();
    const ref = cliJson(engramHome, holder, "add", "Evaluated twice", "--accept", "the build passes").work.short_ref;
    cliJson(engramHome, holder, "claim", ref);
    cliJson(engramHome, holder, "gate", "cargo-test", "--work-ref", ref);
    const records = receipt(await client.call("show", { work_ref: ref, notes: true, gates: true }));
    const gate = records.notes.find((row) => String(row.family).toLowerCase() === "gates");
    assert.ok(gate, JSON.stringify(records.notes));
    const evaluate = async (verdict) => {
      const shown = receipt(await client.call("show", { work_ref: ref }));
      const evaluated = receipt(await client.call("evaluate", {
        work_ref: ref,
        mode: "same_session",
        acceptance_basis: shown.acceptance_basis,
        evidence_basis: shown.evidence_basis,
        verdicts: [{ criterion: 1, verdict, basis: "asserted", rationale: `the gate says ${verdict}`, evidence: [gate.locator] }],
      }));
      return evaluated.evaluation.hash;
    };
    const failed = await evaluate("fail");
    // The failure stands until new evidence: a correction precedes the pass.
    receipt(await client.call("note", { work_ref: ref, text: "correction: the build is fixed" }));
    const passed = await evaluate("pass");

    // The window lists both records of the run in run-feed order, from the
    // same evaluator session; only the newer one is the newest.
    const window = receipt(await client.call("show", { work_ref: ref, evaluations: true }));
    assert.equal(window.evaluations_window.total, 2);
    assert.equal(window.evaluations_window.shown, 2);
    assert.equal(window.evaluations_window.omitted, 0);
    assert.equal(window.evaluations_window.after, null);
    const [older, newer] = window.evaluations;
    assert.deepEqual([older.evaluation, newer.evaluation], [failed, passed]);
    assert.ok(older.run_position < newer.run_position, JSON.stringify(window.evaluations));
    assert.equal(older.evaluator_session, "you");
    assert.equal(newer.evaluator_session, older.evaluator_session);
    assert.equal(older.verdicts[0].verdict, "fail");
    assert.equal(newer.verdicts[0].verdict, "pass");
    assert.equal(older.newest, false);
    assert.equal(newer.newest, true);
    assert.equal(older.stale, null, "an older record is not stale merely because a newer one exists");
    assert.equal(newer.stale, null);

    // The CLI gives the same rows; the detail reads the older record complete.
    const viaCli = cliJson(engramHome, holder, "show", ref, "--evaluations");
    assert.deepEqual(viaCli.evaluations.map((row) => row.evaluation), [failed, passed]);
    const detail = receipt(await client.call("show", { work_ref: ref, evaluation: failed }));
    assert.equal(detail.evaluation.evaluation, failed);
    assert.equal(detail.evaluation.verdicts[0].verdict, "fail");
    assert.equal(detail.evaluation.verdicts[0].rationale, "the gate says fail");
    assert.deepEqual(detail.evaluation.verdicts[0].citations, [gate.locator]);

    // Ordinary show and done keep reading only the newest record.
    const sealed = receipt(await client.call("done", { work_ref: ref, summary: "Delivered" }));
    assert.equal(sealed.work.lifecycle, "completed");
    assert.equal(sealed.acceptance.evaluation, passed);
    const afterDone = receipt(await client.call("show", { work_ref: ref, evaluations: true }));
    assert.deepEqual(afterDone.evaluations.map((row) => row.evaluation), [failed, passed]);
    // Completion revised the item: the ended run's records are listed but not
    // judged, so the record the seal consumed is never shown as stale.
    assert.equal(afterDone.evaluations_window.stale_judged, false);
    for (const row of afterDone.evaluations) {
      assert.equal(row.stale, null, JSON.stringify(row));
      assert.equal(row.stale_judged, false);
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});
