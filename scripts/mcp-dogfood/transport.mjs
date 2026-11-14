import assert from "node:assert/strict";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { after } from "node:test";
import { EventEmitter } from "node:events";
import { PassThrough } from "node:stream";
import { createMcpClient } from "./client.mjs";
import { CLOSE_BUDGET_MS, HostLoadSamples, McpCloseObservation } from "../mcp-close.mjs";

export function registerTimingDiagnostics(support) {
  const { SOFT_TIMING_MS, timingLine, test } = support;
test("MCP client preserves UTF-8 split across stdout and stderr chunks", async (t) => {
  const child = new EventEmitter();
  child.stdin = new PassThrough();
  child.stdout = new PassThrough();
  child.stderr = new PassThrough();
  const Client = createMcpClient({ ...support, spawnImpl: () => child });
  const client = new Client("unused", "split-utf8", undefined, "split-utf8", [], { phaseTrace: false });
  t.after(() => {
    for (const stream of [child.stdin, child.stdout, child.stderr]) stream.destroy();
    child.emit("close", 0, null);
  });
  const value = { text: "żółć 🦀" };
  const result = { content: [{ type: "text", text: JSON.stringify(value) }], structuredContent: value };
  const response = client.request("split-response");
  const frame = Buffer.from(`${JSON.stringify({ jsonrpc: "2.0", id: 1, result })}\n`);
  // Let both pipes enter flowing mode so each write reaches the real data
  // listener separately rather than being coalesced before the reader starts.
  await new Promise((resolvePromise) => setImmediate(resolvePromise));
  for (const byte of frame) child.stdout.write(Buffer.from([byte]));
  assert.deepEqual(await response, result);
  const diagnostic = "żółć 🦀\nfinal ż";
  for (const byte of Buffer.from(diagnostic)) child.stderr.write(Buffer.from([byte]));
  client.flushStderr();
  assert.equal(client.stderr, diagnostic);
});

test("slow MCP diagnostics preserve the soft threshold and unknown WAL state", () => {
  assert.equal(timingLine("call", "note", SOFT_TIMING_MS - 1, 42), undefined);
  assert.equal(timingLine("call", "note", SOFT_TIMING_MS, 42), 'MCP timing: call="note" elapsed_ms=2000.0 soft_threshold_ms=2000 wal_bytes=42 (sampled floor; close may truncate WAL)');
  assert.match(timingLine("test", "fixture", SOFT_TIMING_MS, null), /wal_bytes=unavailable /);
});

}

export function registerCloseDiagnostics(support) {
  const { test, fixtureHome, removeFixtureHomes, receipt, buildAndInit, McpClient } = support;
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

test("MCP close deadline and diagnostic classifications", async (t) => {
  assert.equal(CLOSE_BUDGET_MS, 60000);
  async function fixture(context) {
    context.mock.timers.enable({ apis: ["setTimeout"] });
    let milliseconds = 0;
    let kills = 0;
    const observation = new McpCloseObservation({ now: () => milliseconds });
    const child = {
      pid: 123,
      stdin: { writableFinished: true, destroyed: true },
      kill() { kills++; observation.exited(null, "SIGTERM"); observation.didClose(null, "SIGTERM"); },
    };
    return {
      observation, child,
      kills: () => kills,
      async advance(amount, clockAmount = amount) {
        milliseconds += clockAmount;
        context.mock.timers.tick(amount);
        for (let i = 0; i < 12; i++) await Promise.resolve();
      },
      check(collect, options = {}) {
        return observation.check({ started: 0, child, collect, ...options }).then(() => ({ passed: true }), error => ({ error }));
      },
    };
  }

  await t.test("six-second clean close passes", async (context) => {
    const run = await fixture(context);
    const result = run.check();
    await run.advance(6000);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    assert.deepEqual(await result, { passed: true });
    assert.equal(run.kills(), 0);
  });
  await t.test("late clean close fails and reports measured host load", async (context) => {
    const run = await fixture(context);
    const load = { state: "measured", before_eof: true, intervals: [{ from_eof_ms: -1000, to_eof_ms: 0, host_cpu_busy_fraction: 0.97 }] };
    const result = run.check(async () => load);
    await run.advance(CLOSE_BUDGET_MS);
    await run.advance(1000);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    const { error } = await result;
    assert.equal(error.closeDiagnostic.classification, "late_clean_exit");
    assert.equal(error.closeDiagnostic.close_observed_ms, CLOSE_BUDGET_MS + 1000);
    assert.deepEqual(error.closeDiagnostic.host_load, load);
    assert.equal(run.kills(), 0);
  });
  await t.test("an early timer cannot reject a close before the monotonic deadline", async (context) => {
    const run = await fixture(context);
    const result = run.check();
    // The timer clock reaches its deadline while performance.now trails it.
    await run.advance(CLOSE_BUDGET_MS, CLOSE_BUDGET_MS - 2);
    await run.advance(1);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    assert.deepEqual(await result, { passed: true });
    assert.equal(run.kills(), 0);
  });
  await t.test("overdue resolved close cannot win against a delayed timer", async (context) => {
    const run = await fixture(context);
    // Observe before check starts: both its close promise and overdue timer
    // are ready. The timestamp, rather than race ordering, decides the pass.
    await run.advance(CLOSE_BUDGET_MS + 1);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    const { error } = await run.check();
    assert.equal(error.closeDiagnostic.classification, "late_clean_exit");
    assert.equal(error.closeDiagnostic.budget_ms, CLOSE_BUDGET_MS);
  });
  await t.test("timely exit with delayed stdio close fails separately", async (context) => {
    const run = await fixture(context);
    const result = run.check();
    await run.advance(6000);
    run.observation.exited(0, null);
    await run.advance(CLOSE_BUDGET_MS - 6000);
    await run.advance(1000);
    run.observation.didClose(0, null);
    const { error } = await result;
    assert.equal(error.closeDiagnostic.classification, "stdio_close_delayed");
    assert.equal(error.closeDiagnostic.exit_observed_ms, 6000);
    assert.equal(error.closeDiagnostic.close_observed_ms, CLOSE_BUDGET_MS + 1000);
  });
  for (const exitAt of [6000, 11000, CLOSE_BUDGET_MS + 1000]) {
    await t.test(`clean exit at ${exitAt}ms with stalled stdio stays bounded`, async (context) => {
      const run = await fixture(context);
      const result = run.check();
      await run.advance(Math.min(exitAt, CLOSE_BUDGET_MS));
      if (exitAt > CLOSE_BUDGET_MS) await run.advance(exitAt - CLOSE_BUDGET_MS);
      run.observation.exited(0, null);
      if (exitAt < CLOSE_BUDGET_MS) await run.advance(CLOSE_BUDGET_MS - exitAt);
      await run.advance(CLOSE_BUDGET_MS + 15000 - Math.max(exitAt, CLOSE_BUDGET_MS));
      const { error } = await result;
      assert.equal(error.closeDiagnostic.classification, "clean_exit_stdio_open");
      assert.equal(error.closeDiagnostic.exit_observed_ms, exitAt);
      if (exitAt > CLOSE_BUDGET_MS) {
        assert.ok(error.closeDiagnostic.exit_observed_ms > error.closeDiagnostic.budget_ms);
      }
      assert.equal(error.closeDiagnostic.close_observed_ms, null);
      assert.equal(error.closeDiagnostic.eventual, null);
      assert.equal(error.closeDiagnostic.budget_ms, CLOSE_BUDGET_MS);
      assert.equal(run.kills(), 1);
    });
  }
  for (const [code, signal] of [[7, null], [null, "SIGTERM"]]) {
    for (const closes of [true, false]) {
      await t.test(`late failed exit code=${code} signal=${signal} closes=${closes}`, async (context) => {
        const run = await fixture(context);
        const result = run.check();
        await run.advance(CLOSE_BUDGET_MS);
        await run.advance(1000);
        run.observation.exited(code, signal);
        if (closes) run.observation.didClose(code, signal);
        else await run.advance(14000);
        const { error } = await result;
        assert.equal(error.closeDiagnostic.classification, "failed_exit");
        assert.equal(error.closeDiagnostic.exit_observed_ms, CLOSE_BUDGET_MS + 1000);
        assert.equal(error.closeDiagnostic.close_observed_ms, closes ? CLOSE_BUDGET_MS + 1000 : null);
        assert.deepEqual(error.closeDiagnostic.eventual, closes ? { code, signal } : null);
        assert.equal(run.kills(), closes ? 0 : 1);
      });
    }
  }
  await t.test("never closed retains the absolute diagnostic bound", async (context) => {
    const run = await fixture(context);
    const result = run.check(() => new Promise(() => {}));
    await run.advance(CLOSE_BUDGET_MS);
    await run.advance(2000);
    await run.advance(13000);
    const { error } = await result;
    assert.equal(error.closeDiagnostic.classification, "never_closed");
    assert.equal(error.closeDiagnostic.exit_observed_ms, null);
    assert.equal(error.closeDiagnostic.close_observed_ms, null);
    assert.deepEqual(error.closeDiagnostic.host_load, { state: "unknown", reason: "collector deadline exceeded" });
    assert.equal(run.kills(), 1);
  });
  await t.test("a stuck collector cannot obscure a late clean close", async (context) => {
    const run = await fixture(context);
    const result = run.check(() => new Promise(() => {}));
    await run.advance(CLOSE_BUDGET_MS);
    await run.advance(1000);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    await run.advance(1000);
    const { error } = await result;
    assert.equal(error.closeDiagnostic.classification, "late_clean_exit");
    assert.equal(error.closeDiagnostic.close_observed_ms, CLOSE_BUDGET_MS + 1000);
    assert.equal(error.closeDiagnostic.host_load.state, "unknown");
    assert.equal(run.kills(), 0);
  });
  for (const [code, signal] of [[7, null], [null, "SIGTERM"]]) {
    await t.test(`non-clean close code=${code} signal=${signal} fails`, async (context) => {
      const run = await fixture(context);
      const result = run.check();
      run.observation.exited(code, signal);
      run.observation.didClose(code, signal);
      assert.ok((await result).error);
    });
  }
  await t.test("a missing load measurement remains unknown", async (context) => {
    const run = await fixture(context);
    const result = run.check();
    await run.advance(CLOSE_BUDGET_MS);
    await run.advance(1000);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    assert.deepEqual((await result).error.closeDiagnostic.host_load, { state: "unknown" });
  });
  await t.test("pending-at-deadline evidence survives eventual exit cleanup", async (context) => {
    const run = await fixture(context);
    let pending = 2;
    let stderr = "";
    run.child.stdin.writableFinished = false;
    run.child.stdin.destroyed = false;
    const result = run.check(undefined, { pending: () => pending, stderr: () => stderr });
    await run.advance(CLOSE_BUDGET_MS);
    pending = 0;
    run.child.stdin.writableFinished = true;
    run.child.stdin.destroyed = true;
    stderr = "late shutdown diagnostic";
    await run.advance(1000);
    run.observation.exited(0, null);
    run.observation.didClose(0, null);
    const { error } = await result;
    assert.equal(error.closeDiagnostic.pending_at_deadline, 2);
    assert.equal(error.closeDiagnostic.stdin_finished_at_deadline, false);
    assert.equal(error.closeDiagnostic.stdin_destroyed_at_deadline, false);
    assert.match(error.message, /late shutdown diagnostic/u);
  });
});

test("MCP close shutdown milestones retain stage times and leave gaps unknown", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  let now = 0;
  const observation = new McpCloseObservation({ now: () => now });
  assert.equal(observation.shutdownReport(0).state, "unknown");
  assert.equal(observation.shutdownMilestone({ engram_mcp_shutdown_trace: 1, stage: "caller private body", server_elapsed_ms: 0 }), false);
  now = 10;
  const client = { stderrBuffer: "", stderr: "", closeObservation: observation };
  const record = { engram_mcp_shutdown_trace: 1, stage: "input_ended", server_elapsed_ms: 25, private: "never retained" };
  const line = `${JSON.stringify(record)}\n`;
  McpClient.prototype.receiveStderr.call(client, Buffer.from(line.slice(0, 20)));
  McpClient.prototype.receiveStderr.call(client, Buffer.from(line.slice(20)));
  assert.equal(client.stderr, "");
  let report = observation.shutdownReport(0);
  assert.equal(report.state, "observed");
  assert.deepEqual(report.milestones, [{ stage: "input_ended", server_elapsed_ms: 25, received_from_eof_ms: 10 }]);
  assert.ok(report.missing.includes("transport_close_finished"));
  const child = { kill() {}, stdin: {} };
  const checked = observation.check({ started: 0, child }).catch(error => error);
  const lateByMs = CLOSE_BUDGET_MS + 1000;
  now = lateByMs; t.mock.timers.tick(lateByMs); await Promise.resolve(); await Promise.resolve();
  observation.shutdownMilestone({ engram_mcp_shutdown_trace: 1, stage: "transport_close_started", server_elapsed_ms: 100 });
  observation.exited(0, null); observation.didClose(0, null);
  const error = await checked;
  assert.equal(error.closeDiagnostic.classification, "late_clean_exit");
  report = error.closeDiagnostic.shutdown_trace;
  assert.equal(report.milestones[1].stage, "transport_close_started");
  assert.ok(report.missing.includes("transport_close_finished"));
});

test("MCP close host samples report interval load and missing evidence", () => {
  let milliseconds = -1000;
  let idle = 10;
  let user = 90;
  const samples = new HostLoadSamples({
    now: () => milliseconds,
    automatic: false,
    read: () => [{ times: { idle, user, sys: 0, nice: 0, irq: 0 } }],
  });
  assert.equal(samples.report(0).state, "unknown");
  milliseconds = 0; idle += 3; user += 97; samples.sample();
  assert.deepEqual(samples.report(0).intervals, [{ from_eof_ms: -1000, to_eof_ms: 0, host_cpu_busy_fraction: 0.97, processors: 1 }]);
  assert.equal(samples.report(0).before_eof, true);
  assert.equal(samples.report(0).sample_failed, false);
  samples.read = () => { throw new Error("unavailable"); };
  samples.sample();
  assert.equal(samples.report(0).sample_failed, true);
  samples.stop();
});

test("MCP shutdown tracing emits milestones only when enabled", async (t) => {
  const engramHome = fixtureHome("engram-shutdown-trace-", t);
  let client;
  try {
    buildAndInit(engramHome);
    for (const phaseTrace of [true, false]) {
      client = new McpClient(engramHome, "shutdown-observer", undefined, "shutdown-observer", [], { phaseTrace });
      await client.initialize();
      const value = receipt(await client.call("next", { peek: true }));
      assert.ok(Array.isArray(value.next));
      assert.equal(client.pending.size, 0);
      await client.close();
      assert.deepEqual([client.closeObservation.close.code, client.closeObservation.close.signal], [0, null]);
      const report = client.closeObservation.shutdownReport(0);
      if (phaseTrace) {
        assert.equal(report.state, "observed");
        for (const stage of ["input_ended", "transport_close_started", "transport_close_finished", "trace_flush_started"]) {
          assert.ok(report.milestones.some(record => record.stage === stage), JSON.stringify(report));
        }
      } else {
        assert.equal(report.state, "unknown");
        assert.deepEqual(report.milestones, []);
        assert.doesNotMatch(client.stderr, /engram_mcp_shutdown_trace/u);
      }
      client = undefined;
    }
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

}

export function registerPhaseDiagnostics(support) {
  const { HASH, phaseTraceLine, test, fixtureHome, removeFixtureHomes, structured, receipt, wait, buildAndInit, cliWord, cliJson, McpClient } = support;
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
  for (const state of ["evicted", "incomplete", "cancelled", "uncorrelated", "send_dropped"]) {
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
    await client.call("next", { peek: true });
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
      const stages = [...traced.callStages.values()].find((rows) => rows[0].id === id);
      assert.ok(stages, `no stage records for call ${id}`);
      assert.deepEqual(stages.map((row) => row.stage), ["receive", "handler_start", "handler_settle", "send_dispatch", "send_finish"]);
      assert.ok(stages.every((row) => row.call_seq === record.call_seq && row.trace_instance === traced.traceInstance));
      assert.ok(stages.every((row, index) => index === 0 || stages[index - 1].server_elapsed_ms <= row.server_elapsed_ms));
      for (const row of stages) {
        const stageLine = JSON.stringify(row);
        assert.ok(Buffer.byteLength(stageLine) <= 4096, stageLine);
        assert.ok(!stageLine.includes(engramHome) && !stageLine.includes("Traced work"), stageLine);
      }
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
    const peek = [...traced.phaseRecords.values()].find((record) => record.tool === "next");
    assert.ok(peek.read_snapshot.count >= 1, JSON.stringify(peek));
    assert.ok(peek.sqlite_statement.count >= 1, JSON.stringify(peek));
    assert.equal(peek.backup_reminder.count, 1);
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
    assert.equal(untraced.stageLines, 0);
    assert.equal(untraced.traceInstance, undefined);
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
    // The client stops reading before it asks and stays stopped until the
    // server's record of the call has arrived on the other pipe. The server's
    // phases can then hold none of the client's stall, however long it lasts,
    // because the record was finished while the stall was still going on.
    client.child.stdout.pause();
    let settled = false;
    let refusal;
    // A gap since the previous call: a server charging its idle time would then
    // report more than the window below. Load can only hide that, never cause it.
    await wait(200);
    const started = performance.now();
    const answered = client
      .request("tools/call", { name: "show", arguments: { work_ref } })
      .then(() => { settled = true; }, (error) => { refusal = error; });
    while (!client.phaseRecords.has(id) && refusal === undefined) {
      await wait(10);
    }
    if (refusal !== undefined) throw refusal;
    // Everything the server timed happened inside this window, which load only widens.
    const window = performance.now() - started;
    const record = client.phaseRecords.get(id);
    assert.equal(settled, false, "the client had read nothing yet");
    await wait(50);
    assert.equal(settled, false, "the client is still not reading");
    client.child.stdout.resume();
    await answered;
    if (refusal !== undefined) throw refusal;
    assert.equal(settled, true);
    const elapsed = performance.now() - started;
    await client.close();
    assert.ok(record.handler_total_ms <= window, `${window}ms window: ${JSON.stringify(record)}`);
    assert.ok(record.wire_encode_send_inclusive_ms <= window, `${window}ms window: ${JSON.stringify(record)}`);
    t.diagnostic(`client elapsed ${elapsed.toFixed(1)}ms; server handler ${record.handler_total_ms.toFixed(2)}ms; send ${record.wire_encode_send_inclusive_ms.toFixed(2)}ms`);
  } catch (error) {
    failure = error;
  } finally {
    try { await client?.close(); } catch { /* already closed */ }
    removeFixtureHomes(engramHome);
  }
  if (failure) throw failure;
});

}

export function registerArgumentRefusal(support) {
  const { test, fixtureHome, removeFixtureHomes, structured, structuredError, receipt, buildAndInit, McpClient } = support;
test("an argument a word's schema rejects is a text-only tool error and the connection stays usable", async (t) => {
  const engramHome = fixtureHome("engram-schema-rejected-", t);
  let client;
  try {
    buildAndInit(engramHome);
    client = new McpClient(engramHome, "schema-rejected");
    await client.initialize();
    const reference = receipt(await client.call("add", { title: "Schema-rejected arguments" })).work.short_ref;
    // The fields these refusals name are the ones the listed input schemas
    // declare; an extractor rmcp's tool macro stopped recognising would list
    // empty schemas while still refusing, so the schemas are checked too.
    const listed = new Map((await client.tools()).map((tool) => [tool.name, tool.inputSchema]));
    assert.ok(listed.get("next")?.properties?.peek, JSON.stringify(listed.get("next")));
    assert.ok(listed.get("update")?.properties?.action, JSON.stringify(listed.get("update")));
    assert.ok(listed.get("evaluate")?.properties?.verdicts, JSON.stringify(listed.get("evaluate")));
    // Engram's argument extractor refuses these before the word runs, and
    // the MCP library routes the refusal as a tool error, not a JSON-RPC
    // failure, whose only content is text describing the problem, without
    // the structured error, reminders or next a word's own refusal carries.
    // Each names the field it concerns, after the
    // "failed to deserialize parameters:" prefix the library routes by.
    const cases = [
      ["an undeclared field", "update", { work_ref: reference, undeclared_field: true }, /^failed to deserialize parameters: field `undeclared_field`: unknown field `undeclared_field`/u],
      ["an unknown action", "update", { work_ref: reference, action: "no_such_action" }, /^failed to deserialize parameters: field `action`: unknown variant `no_such_action`/u],
      ["a wrong type", "next", { peek: "yes" }, /^failed to deserialize parameters: field `peek`: invalid type: string "yes", expected a boolean$/u],
      ["a nested wrong type", "evaluate", { work_ref: reference, mode: "same_session", acceptance_basis: 1, evidence_basis: 1, verdicts: [{ criterion: "one" }] }, /^failed to deserialize parameters: field `verdicts\[0\]\.criterion`: invalid type/u],
    ];
    for (const [label, tool, arguments_, named] of cases) {
      const refused = await client.call(tool, arguments_);
      assert.equal(refused.isError, true, `${label}: ${JSON.stringify(refused)}`);
      assert.equal(refused.structuredContent, undefined, `${label}: ${JSON.stringify(refused)}`);
      assert.ok(refused.content.length > 0, label);
      for (const content of refused.content) assert.equal(content.type, "text", label);
      const text = refused.content.map(({ text }) => text).join("\n");
      assert.match(text, named, `${label}: ${text}`);
      assert.doesNotMatch(text, /"reminders"|"next"/u, `${label}: ${text}`);
      // The connection stays usable: the next valid call answers normally.
      const shown = receipt(await client.call("show", { work_ref: reference }));
      assert.equal(shown.status.work.title, "Schema-rejected arguments", label);
      assert.ok(Array.isArray(shown.next), label);
    }
    // An action must not discard a declared field, including explicit null,
    // false or an empty list. These are structured argument refusals before
    // any work mutation, and the same connection continues to serve reads.
    const before = receipt(await client.call("show", { work_ref: reference, notes: true, gates: true }));
    for (const [tool, action, field, value] of [
      ["update", "release", "title", null],
      ["update", "blocked", "reason", "unused"],
      ["update", "unblock", "text", "unused"],
      ["update", "revise", "reason", "unused"],
      ["update", "evaluation_mode", "labels", []],
      ["update", "cancel", "clear_external", false],
      ["update", "reject", "outcome", "unused"],
      ["update", "after", "child", reference],
      ["update", "drop_after", "replacement", reference],
      ["update", "waive", "prerequisite", reference],
      ["update", "supersede", "child", reference],
      ["update", "detach", "priority", 0],
      ["handoff", "offer", "reason", "unused"],
      ["handoff", "accept", "to", null],
      ["handoff", "accept", "summary", "unused"],
      ["handoff", "accept", "reason", "unused"],
      ["handoff", "accept", "ttl_seconds", 1],
      ["handoff", "cancel", "summary", "unused"],
      ["update", { cancel: null }, "title", "unused"],
      ["handoff", { cancel: null }, "summary", "unused"],
    ]) {
      const actionName = typeof action === "string" ? action : Object.keys(action)[0];
      const refused = structuredError(await client.call(tool, {
        work_ref: reference, action, [field]: value,
        ...(typeof action === "object" ? { reason: "cancel" } : {}),
      }), "invalid_argument");
      assert.equal(refused.details.field, field);
      assert.ok(refused.message.includes(actionName), refused.message);
      assert.ok(refused.message.includes(field), refused.message);
      const after = receipt(await client.call("show", { work_ref: reference, notes: true, gates: true }));
      assert.deepEqual(after.status, before.status, `${tool} ${actionName}: status changed`);
      assert.deepEqual(after.history, before.history, `${tool} ${actionName}: history changed`);
      assert.deepEqual(after.notes, before.notes, `${tool} ${actionName}: notes changed`);
    }
  } finally {
    try {
      if (client) await client.close();
    } finally {
      removeFixtureHomes(engramHome);
    }
  }
});

}

export function registerReadOnlySurface(support) {
  const { test, fixtureHome, removeFixtureHomes, structured, structuredError, buildAndInit, cliJson, McpClient } = support;
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
    assert.deepEqual([...(await client.toolNames())].sort(), ["ls", "memories", "next", "search", "show", "task_sync_settings"]);
    for (const [name, arguments_] of [
      ["next", { peek: true }],
      ["ls", {}],
      ["search", { query: "Read" }],
      ["show", { work_ref: ref }],
      ["memories", {}],
      ["memories", { query: "read-rule", full: true }],
      ["task_sync_settings", { action: "status" }],
    ]) {
      structured(await client.call(name, arguments_));
    }
    const refused = async (name, arguments_, restriction) => {
      const error = structuredError(await client.call(name, arguments_), "mcp_read_only_refused");
      assert.equal(error.details.mode, "read_only");
      assert.equal(error.details.tool, name);
      assert.equal(error.details.restriction, restriction);
      assert.match(error.message, /^MCP read-only mode refused /u);
      // Like every tool error Engram itself returns: reminders in words, and
      // the read to make instead.
      assert.equal(error.reminders.length, 1);
      assert.match(error.reminders[0], /^this connection is read-only: /u);
      const instead = {
        tool_not_admitted: ["engram work next --peek"],
        next_without_peek: ["engram work next --peek"],
        memories_with_context_generation: ["engram work memories"],
        argument_not_admitted: [],
        settings_mutation: ["engram task-sync status --json"],
      };
      assert.deepEqual(error.next, instead[restriction]);
    };
    await refused("next", {}, "next_without_peek");
    await refused("task_sync_settings", { action: "enable", user_instruction: "Enable this project." }, "settings_mutation");
    await refused("task_sync_settings", { action: "disable" }, "settings_mutation");
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

}

export function registerBuildIdentity(support) {
  const { root, binary, test, fixtureHome, removeFixtureHomes, receipt, buildAndInit, cliJson, McpClient } = support;
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

}

export function registerUnknownArguments(support) {
  const { test, fixtureHome, removeFixtureHomes, receipt, buildAndInit, McpClient } = support;
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
    assert.equal(tools.length, 16);
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

}
