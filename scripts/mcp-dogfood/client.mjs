import { resolve } from "node:path";
import { spawn } from "node:child_process";
import { HostLoadSamples, McpCloseObservation } from "../mcp-close.mjs";

// Ends a request that will never be answered. It says nothing about speed: a
// slow call gets a timing line. The replay of growing history that once took a
// call 14 s is counted in Rust, at any host speed, by
// advisory_child_guidance_stays_bounded_as_root_history_grows and
// gate_heavy_evidence_membership_has_constant_decode_cost. A slow path those do
// not cover has no failing check here, only that line and its phase record.
const REQUEST_HANG_GUARD_MS = 60000;

// Bind the one client class to the entry point's shared support closures.
export function createMcpClient({ root, binary, SOFT_TIMING_MS, PHASE_TRACE_ENV, MAX_PHASE_RECORDS, timingLine, recordWalSample, phaseNotice, phaseTraceLine, assertProseNamesFields }) {
class McpClient {
  constructor(engramHome, sessionId, actorContext, actorId = sessionId, extraArgs = [], { phaseTrace = true, softTimingMs = SOFT_TIMING_MS, cwd = root } = {}) {
    this.engramHome = engramHome;
    this.nextId = 1;
    this.pending = new Map();
    this.stderr = "";
    this.buffer = "";
    this.stderrBuffer = "";
    this.phaseTrace = phaseTrace;
    this.phaseRecords = new Map();
    this.phaseLines = 0;
    this.callStages = new Map();
    this.stageLines = 0;
    this.traceInstance = undefined;
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
      cwd,
      env: environment,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.closeObservation = new McpCloseObservation();
    this.hostLoad = new HostLoadSamples();
    this.child.stderr.on("data", (chunk) => this.receiveStderr(chunk));
    this.child.stdout.on("data", (chunk) => this.#receive(chunk));
    this.closed = new Promise((resolvePromise) => {
      this.child.once("close", (code, signal) => {
        this.closeObservation.didClose(code, signal);
        this.hostLoad.stop();
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
    this.closeObservation?.exited(code, signal);
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
      if (line.startsWith('{"') && (line.includes('"engram_mcp_call_stage"') || line.includes('"engram_mcp_trace_instance"'))) {
        try { record = JSON.parse(line); } catch { record = undefined; }
        if (record?.engram_mcp_trace_instance === 1) {
          this.traceInstance = record.trace_instance;
          console.error(`MCP trace instance: ${line.trimEnd()}`);
          continue;
        }
        if (record?.engram_mcp_call_stage === 1) {
          this.stageLines = (this.stageLines ?? 0) + 1;
          console.error(`MCP call stage: ${line.trimEnd()}`);
          // Only the explicit safe association is indexed. Raw unassociated
          // rows remain in the log, never joined by a guessed wire ID.
          if (record.correlation === "numeric" && Number.isSafeInteger(record.call_seq)) {
            this.callStages ??= new Map();
            const key = `${record.trace_instance}:${record.call_seq}`;
            const rows = this.callStages.get(key) ?? [];
            if (rows.length < 8) rows.push(record);
            this.callStages.set(key, rows);
            if (this.callStages.size > MAX_PHASE_RECORDS) this.callStages.delete(this.callStages.keys().next().value);
          }
          continue;
        }
        record = undefined;
      }
      if (line.startsWith('{"') && line.includes('"engram_mcp_shutdown_trace"')) {
        try { record = JSON.parse(line); } catch { record = undefined; }
        if (record && this.closeObservation?.shutdownMilestone(record)) {
          console.error(`MCP shutdown record: ${line.trimEnd()}`);
          continue;
        }
        record = undefined;
      }
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
      }, REQUEST_HANG_GUARD_MS);
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
    assertProseNamesFields(name, result);
    return result;
  }

  async close() {
    // Closing the last SQLite owner may checkpoint/remove the WAL. Capture
    // it before EOF as well as on slow calls and before fixture cleanup.
    recordWalSample(this.engramHome);
    this.hostLoad?.sample();
    // Plain stand-ins in the phase-rendering tests have only a close promise.
    const observation = this.closeObservation ?? new McpCloseObservation();
    if (!this.closeObservation) this.closed.then(({ code, signal }) => observation.didClose(code, signal));
    const started = observation.now();
    if (!this.child.stdin.destroyed) this.child.stdin.end();
    try {
      await observation.check({
        started,
        child: this.child,
        pending: () => this.pending.size,
        stderr: () => this.stderr,
        collect: async () => this.hostLoad?.report(started) ?? { state: "unknown" },
      });
    } finally {
      this.hostLoad?.stop();
      // Slow calls whose record never came read as unavailable, on a failed
      // close as much as on a clean one.
      for (const id of this.awaitingPhase) {
        phaseNotice(this, phaseTraceLine(id, this.phaseRecords.get(id), "no record before close"));
      }
      this.awaitingPhase.clear();
    }
  }

}

  return McpClient;
}
