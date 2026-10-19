#!/usr/bin/env node
// One execution, durable logs, one completion. Notification retries never run tests.
import { spawn, spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import { constants, accessSync, closeSync, createReadStream, existsSync, linkSync, lstatSync, mkdirSync,
  openSync, readFileSync, readlinkSync, realpathSync, renameSync, rmSync, statSync, writeFileSync } from "node:fs";
import { hostname } from "node:os";
import { basename, delimiter, dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { inspect } from "node:util";
import { captureFingerprint, fingerprintLimitations } from "./review-freeze-fingerprint.mjs";
import { requestStop, runJobCommand, stopController, stopProtocol, withStopLock } from "./launcher-stop.mjs";

const script = fileURLToPath(import.meta.url);
const repository = resolve(dirname(script), "..");
const diagnosticLimit = 2400;
const inspectionLimit = 6000;
const usageFailure = (message) => Object.assign(new Error(message), { safeCompletionText: message });
const quote = (value) => JSON.stringify(value).replace(/[\u0085\u2028\u2029]/gu,
  (character) => `\\u${character.charCodeAt(0).toString(16).padStart(4, "0")}`);

function launchText(value, name, maxBytes, address = false) {
  if (typeof value !== "string" || !value.trim() || /[\r\n]/u.test(value)
    || Buffer.byteLength(value) > maxBytes || (address && (value !== value.trim() || value.startsWith("-")))) {
    throw usageFailure(`${name} requires nonblank single-paragraph text of at most ${maxBytes} UTF-8 bytes${address ? "; session addresses have no outer whitespace and cannot start with -" : ""}`);
  }
}

function validateLaunchMetadata({ item, nextTransition }) {
  if (item !== undefined) launchText(item, "--item", 256);
  if (nextTransition !== undefined) {
    if (!nextTransition || typeof nextTransition !== "object" || Array.isArray(nextTransition)
      || Object.keys(nextTransition).some((key) => !["owner", "action"].includes(key))) {
      throw new Error("nextTransition requires only owner and action");
    }
    launchText(nextTransition.owner, "--next-owner", 256, true);
    launchText(nextTransition.action, "--next-action", 2000);
  }
}

// Saved once before admission, for every completion projection. The execution
// request remains separate; stages may rewrite it without changing this intent.
function publishCompletionRequest(runDir, request) {
  const { runId, root, owner, started, expectedFingerprint, item, nextTransition, notifyTo } = request;
  save(join(runDir, "completion.request.json"), {
    runId, root, owner, started, expectedFingerprint, item, nextTransition, notifyTo,
  });
}
function publishLaunchRequest(runDir, request) {
  // Only createRun writes these files, before an executor or notifier can run.
  // Complete rename publication works on filesystems without hard links.
  save(join(runDir, "request.json"), request);
  publishCompletionRequest(runDir, request);
  if (request.notifyTo) save(join(runDir, "notification.target.json"), {
    runId: request.runId, root: request.root, owner: request.owner, notifyTo: request.notifyTo,
    ...(request.nextTransition && request.nextTransition.owner !== request.notifyTo
      && request.nextTransition.owner !== request.owner ? { nextOwner: request.nextTransition.owner } : {}),
  });
}
function completionRequest(runDir) {
  const snapshot = join(runDir, "completion.request.json");
  // Legacy runs have no admitted snapshot. A damaged new snapshot must not
  // fall back to a mutable request and silently change the launch intent.
  return readJson(existsSync(snapshot) ? snapshot : join(runDir, "request.json"));
}

// Publish complete immutable retry material, including under concurrent sends.
function publish(path, text) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    writeFileSync(temporary, text, { flag: "wx", mode: 0o600 });
    try { linkSync(temporary, path); }
    catch (error) { if (error.code !== "EEXIST") throw error; }
  } finally { rmSync(temporary, { force: true }); }
}
// A run without terminal results is alive while its heartbeat is fresh: the
// launcher refreshes `heartbeat.at` every `heartbeat.everyMs`, and a heartbeat
// older than this many intervals means the launcher has most likely stopped,
// whatever process now holds its pid. The one-second floor normally keeps that
// window wider than the launcher's own synchronous steps; an unusually slow
// input capture or executor identity query can briefly read as interrupted
// until the next beat.
export const HEARTBEAT_STALE_INTERVALS = 3;
const validHeartbeatEveryMs = (value) => Number.isInteger(value) && value >= 1_000 && value <= 600_000;
function heartbeatEveryMs(env) {
  const raw = env.ENGRAM_LAUNCHER_HEARTBEAT_MS;
  if (raw === undefined || raw === "") return 10_000;
  const value = Number(raw);
  if (!validHeartbeatEveryMs(value)) {
    throw usageFailure("ENGRAM_LAUNCHER_HEARTBEAT_MS must be an integer from 1000 to 600000");
  }
  return value;
}
const readJson = (path) => JSON.parse(readFileSync(path, "utf8"));
function readResults(runDir) {
  try { return readJson(join(runDir, "results.json")); }
  catch (error) {
    throw Object.assign(usageFailure("results-unreadable"), { runDir, cause: error });
  }
}
const transientRenameCodes = new Set(["EPERM", "EACCES", "EBUSY"]);
// Windows can refuse a rename for a moment while another process, such as an
// indexer or antivirus scanner, holds the target open. Retry that I/O briefly
// so a transient refusal cannot cost a run its terminal result; this never
// reruns a test.
export function renameWithRetry(from, to, { rename = renameSync, attempts = 20, delayMs = 25 } = {}) {
  for (let attempt = 1; ; attempt += 1) {
    try { rename(from, to); return; }
    catch (error) {
      if (!transientRenameCodes.has(error.code) || attempt >= attempts) throw error;
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, delayMs);
    }
  }
}
function save(path, value) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  try {
    writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, { flag: "wx", mode: 0o600 });
    renameWithRetry(temporary, path);
  } finally { rmSync(temporary, { force: true }); }
}

// Why a run did not pass, as the record's reason token.
const because = (reason, error) => Object.assign(error, { reason });

export function requiredStages(platform = process.platform) {
  const rustCommand = platform === "win32" ? "pwsh" : "sh";
  const rustArgs = platform === "win32" ? ["-NoProfile", "-File", "scripts/test-rust.ps1"] : ["scripts/test-rust.sh"];
  return [
    ["fmt", "lint", "cargo", ["fmt", "--check"]],
    ["check", "build", "cargo", ["check"]],
    ["clippy", "lint", "cargo", ["clippy", "--all-targets", "--all-features", "--", "-D", "warnings"]],
    ["rust", "test", rustCommand, [...rustArgs, "--phase", "ordinary"]],
    ["rust-scale-claims", "test", rustCommand, [...rustArgs, "--phase", "scale-claims"]],
    ["rust-scale-roots", "test", rustCommand, [...rustArgs, "--phase", "scale-roots"]],
    ["rust-scale-planning", "test", rustCommand, [...rustArgs, "--phase", "scale-planning"]],
    ["freeze", "test", process.execPath, ["--test", "scripts/review-freeze-fingerprint.test.mjs", "scripts/test-launcher.test.mjs"]],
    ["mcp", "test", process.execPath, ["--test", "scripts/mcp-dogfood.test.mjs"]],
    ["control", "test", process.execPath, ["--test", "scripts/control-dogfood.test.mjs"]],
    ["parity", "test", process.execPath, ["--test", "scripts/parity.test.mjs"]],
    ["docs", "other", process.execPath, ["scripts/check-doc-links.mjs"]],
    // Scale phases name their tests and Node stages name their files. The
    // ordinary Rust phase selects nothing. Keep this true even if unrun.
  ].map(([name, kind, command, args]) => ({ name, kind, command, args,
    ...(kind === "test" ? { selects: name !== "rust" } : {}) }));
}

// The record a host reads: what each stage was and what its runner reported.
// docs/development.md gives the grammar and who may treat it as evidence.
const stageKinds = new Set(["test", "build", "lint", "typecheck", "other"]);
const recordToken = /^[a-z0-9][a-z0-9-]*$/u;
// Either separator ends a directory on every host: a record names the runner
// the same way wherever it was produced.
const runnerName = (command) => command.split(/[\\/]/u).at(-1).toLowerCase().replace(/\.(?:exe|cmd|bat)$/u, "");
// These repository wrappers run Cargo's test binaries serially. A name or
// Cargo-looking log alone does not establish that ownership contract.
const cargoPhase = ({ command, args }) => ["win32", "linux"].some((platform) =>
  requiredStages(platform).slice(3, 7).some((stage) => runnerName(command) === runnerName(stage.command)
    && args.length === stage.args.length && args.every((arg, index) => arg === stage.args[index])));
// Options whose value is the next argument, and the arguments that select a
// part of the tests. Any other option changes how tests run, not which.
const runners = {
  cargo: {
    valued: new Set(["-j", "--jobs", "-F", "--features", "--target", "--profile", "--manifest-path", "--target-dir",
      "--color", "--config", "-Z", "--message-format", "--test-threads", "--format", "--logfile"]),
    // `--all-targets` runs every target but not the doctests.
    selecting: new Set(["--lib", "--bins", "--bin", "--tests", "--test", "--examples", "--example", "--benches",
      "--bench", "--all-targets", "--doc", "-p", "--package", "--exclude", "--ignored", "--skip", "--exact"]),
  },
  node: {
    valued: new Set(["--test-reporter", "--test-reporter-destination", "--test-concurrency", "--test-timeout",
      "--test-name-pattern", "--test-skip-pattern", "--test-shard", "-r", "--require", "--import", "--loader",
      "--experimental-loader", "--env-file", "-C", "--conditions"]),
    selecting: new Set(["--test-name-pattern", "--test-skip-pattern", "--test-only", "--test-shard"]),
    // Options known to take no value. Before `--test`, any option that is
    // neither here nor valued might take `--test` as its value.
    plain: new Set(["--test", "--test-only", "--test-force-exit", "--test-update-snapshots",
      "--experimental-test-coverage", "--experimental-strip-types", "--no-warnings", "--enable-source-maps",
      "--trace-warnings", "--trace-uncaught", "--throw-deprecation"]),
  },
};
// `--name=value` and the attached short form `-nVALUE` name the option too.
const optionName = (arg) => arg.startsWith("--") ? arg.split("=", 1)[0]
  : /^-[A-Za-z]./u.test(arg) ? arg.slice(0, 2) : arg;

// Node's own options end at `--` or at the script; `--test` counts only
// there, and only after options this table knows: behind an option it does
// not know, `--test` could be that option's value.
function nodeRunsTests(args) {
  const { valued, plain } = runners.node;
  // Each reporter prints a summary of its own, so with more than one the
  // log would count the one runner's tests once for each.
  const options = args.includes("--") ? args.slice(0, args.indexOf("--")) : args;
  if (options.filter((arg) => optionName(arg) === "--test-reporter").length > 1) return false;
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index];
    if (arg === "--" || !arg.startsWith("-")) return false;
    if (arg === "--test") return true;
    if (valued.has(arg)) index += 1;
    else if (!plain.has(arg) && !(arg.includes("=") && valued.has(optionName(arg)))) return false;
  }
  return false;
}

// A focused command's kind comes from this fixed table. Only a runner the
// launcher starts itself is a test: its exit is the runner's own. A wrapper,
// `npm test` included, may run anything and end as it likes, so it is
// `other`, as every unknown command is.
export function commandKind(command, args) {
  const name = runnerName(command);
  const first = args.find((arg) => !arg.startsWith("+"));
  if (name === "cargo") {
    return { test: "test", clippy: "lint", fmt: "lint", check: "build", build: "build" }[first] ?? "other";
  }
  if (name === "node") return nodeRunsTests(args) ? "test" : "other";
  return "other";
}

// Whether the command line itself selects a part of the tests: a test name,
// a test file, or an option that restricts what runs. A wrapper script's own
// selection is not visible here; its runners report it.
export function selectsTests(command, args) {
  const name = runnerName(command);
  if (commandKind(command, args) !== "test") return false;
  const { valued, selecting } = runners[name];
  const rest = name === "node" ? args : args.slice(args.indexOf("test") + 1);
  for (let index = 0; index < rest.length; index += 1) {
    const arg = rest[index];
    if (arg === "--") continue;
    if (!arg.startsWith("-") || selecting.has(optionName(arg))) return true;
    if (valued.has(arg)) index += 1;
  }
  return false;
}

const longestLine = 16384;
// Reads a log line by line in bounded memory. A line longer than the bound is
// reported as too long and its content is never interpreted.
const lineHead = 256;
async function eachLine(log, accept) {
  let pending = "", continuation = false;
  for await (const chunk of createReadStream(log, { encoding: "utf8", highWaterMark: 8192 })) {
    pending += chunk;
    let end;
    while ((end = pending.indexOf("\n")) >= 0) {
      if (continuation) continuation = false;
      else if (end > longestLine) accept(pending.slice(0, lineHead), true);
      else accept(pending.slice(0, end).replace(/\r$/u, ""), false);
      pending = pending.slice(end + 1);
    }
    if (pending.length > longestLine) {
      if (!continuation) accept(pending.slice(0, lineHead), true);
      pending = ""; continuation = true;
    }
  }
  if (pending && !continuation) {
    if (pending.length > longestLine) accept(pending.slice(0, lineHead), true);
    else accept(pending.replace(/\r$/u, ""), false);
  }
}

// Reads the complete log once after the stage closed. Generic counts require
// every runner summary in the log to be complete, valid and consistent;
// otherwise it is unknown, with the first reason met. Nothing but a runner's
// own summary is read: no line of a log is taken for a test or a failure.
// A summary is known by its shape, so text of that shape which a test prints
// at the start of a line is counted as one. `failures` is the failed count
// of every complete summary whose failed count reads, whatever became of the
// executed count. For the repository's serial Cargo phases, explicit Cargo
// Running frames own the first announcement and one outer summary. Unequal
// child announcements without summaries do not replace it or add tests.
// Doc-tests frames accept sequential complete announcement/summary pairs;
// overlapping announcements invalidate the whole frame. Text alone cannot
// distinguish a byte-identical forwarded transcript from rustdoc's output.
// Any ambiguous summary attribution still gives no numeric count.
// A log that cannot be read gives no count.
export async function countTests(log, { cargoFrames = false } = {}) {
  const counted = { passed: 0, failed: 0, ignored: 0, runners: 0, filteredOut: 0 };
  const nodeFields = ["tests", "suites", "pass", "fail", "cancelled", "skipped", "todo", "duration_ms"];
  // `running` is the size libtest announced and has not yet reported on;
  // `streams` counts TAP runners that started and have not yet summarized.
  let why = null, running = null, streams = 0, block = null, failures = 0, frame = null;
  const unknown = (reason) => { why ??= reason; };
  const uncertainFrame = (reason) => { unknown(reason); if (frame) frame.invalid = true; };
  const add = (passed, failed, ignored, filteredOut, runners = 1) => {
    counted.passed += passed; counted.failed += failed; counted.ignored += ignored;
    counted.filteredOut += filteredOut; counted.runners += runners;
  };
  const finishFrame = () => {
    if (!frame) return;
    if (frame.docTests) {
      if (frame.announced !== null || frame.runners === 0) uncertainFrame("incomplete-summary");
      else if (!frame.invalid) add(...frame.totals, frame.runners);
    } else if (frame.result === null) uncertainFrame("incomplete-summary");
    else if (!frame.invalid) add(...frame.result);
    frame = null;
  };
  const read = (raw, tooLong) => {
    const line = raw.replace(/\x1b\[[0-9;]*[A-Za-z]/gu, "");
    const cargoMarker = /^\s*(?:Running|Doc-tests)(?:\s|$)/u.test(line);
    if (tooLong) {
      // Only its beginning is known. Inside a summary, or beginning as one,
      // it is a summary line that does not read.
      if (block || /^(?:test result:|(?:ℹ|#) tests(?:\s|$))/u.test(line)) {
        if (cargoFrames) uncertainFrame("malformed-summary");
        else unknown("malformed-summary");
      }
      if (cargoFrames && (cargoMarker || /^running(?:\s|$)/u.test(line) || line.startsWith("test result:")))
        uncertainFrame("malformed-summary");
      if (block?.mark === "#") streams -= 1;
      if (line.startsWith("test result:")) running = null;
      block = null;
      return;
    }
    if (cargoFrames && cargoMarker) {
      finishFrame();
      frame = { docTests: /^\s*Doc-tests(?:\s|$)/u.test(line), announced: null,
        result: null, totals: [0, 0, 0, 0], runners: 0, invalid: false };
      if (!/^\s*Running .+ \(.+\)$/u.test(line) && !/^\s*Doc-tests \S.*$/u.test(line))
        uncertainFrame("malformed-summary");
      return;
    }
    if (block) {
      const field = nodeFields[block.values.length];
      const value = new RegExp(`^${block.mark} ${field} (\\d+(?:\\.\\d+)?)$`, "u").exec(line)?.[1];
      if (value === undefined) { unknown("incomplete-summary"); block = null; }
      else {
        block.values.push(Number(value));
        if (block.values.length === nodeFields.length) {
          const [tests, suites, pass, fail, cancelled, skipped, todo] = block.values;
          if (Number.isSafeInteger(fail)) failures += fail;
          if (![tests, suites, pass, fail, cancelled, skipped, todo].every(Number.isSafeInteger)) unknown("malformed-summary");
          else {
            if (tests !== pass + fail + cancelled + skipped + todo) unknown("inconsistent-summary");
            else if (cancelled !== 0) unknown("cancelled-tests");
            else if (!cargoFrames) add(pass, fail, skipped + todo, 0);
          }
          if (block.mark === "#") streams -= 1;
          block = null;
        }
        return;
      }
    }
    const first = /^(ℹ|#) tests (\d+)$/u.exec(line);
    if (first) {
      if (cargoFrames) uncertainFrame("ambiguous-summary");
      block = { mark: first[1], values: [Number(first[2])] }; return;
    }
    // A summary's first line that does not read is still a summary's.
    if (/^(?:ℹ|#) tests(?:\s|$)/u.test(line)) {
      if (cargoFrames) uncertainFrame("malformed-summary");
      else unknown("malformed-summary");
      return;
    }
    if (line === "TAP version 13") { streams += 1; return; }
    const announced = /^running (\d+) tests?$/u.exec(line);
    if (announced) {
      if (cargoFrames) {
        const size = Number(announced[1]);
        if (!Number.isSafeInteger(size)) uncertainFrame("malformed-summary");
        else if (!frame) unknown("incomplete-summary");
        else if (frame.docTests && frame.announced !== null) uncertainFrame("ambiguous-summary");
        else if (!frame.docTests && (frame.result !== null || frame.announced === size)) uncertainFrame("ambiguous-summary");
        else if (frame.announced === null) frame.announced = size;
        return;
      }
      if (running !== null) unknown("incomplete-summary");
      running = Number(announced[1]);
      return;
    }
    if (cargoFrames && /^running(?:\s|$)/u.test(line)) { uncertainFrame("malformed-summary"); return; }
    if (!line.startsWith("test result:")) return;
    const result = /^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out; finished in \d+(?:\.\d+)?s$/u.exec(line);
    const [passed, failed, ignored, measured, filteredOut] = (result ?? []).slice(1).map(Number);
    if (result && Number.isSafeInteger(failed)) failures += failed;
    if (cargoFrames) {
      if (!result || ![passed, failed, ignored, measured, filteredOut, passed + failed + ignored].every(Number.isSafeInteger))
        uncertainFrame("malformed-summary");
      else if (measured !== 0) uncertainFrame("benchmarks");
      else if (!frame || frame.announced === null)
        uncertainFrame(frame?.docTests && frame.runners > 0 ? "ambiguous-summary" : "incomplete-summary");
      else if (frame.result !== null) uncertainFrame("ambiguous-summary");
      else if (frame.announced !== passed + failed + ignored) uncertainFrame("ambiguous-summary");
      else if (frame.docTests) {
        frame.totals = frame.totals.map((total, index) => total + [passed, failed, ignored, filteredOut][index]);
        frame.runners += 1;
        frame.announced = null;
        if (![...frame.totals, frame.runners, frame.totals[0] + frame.totals[1]].every(Number.isSafeInteger))
          uncertainFrame("malformed-summary");
      } else frame.result = [passed, failed, ignored, filteredOut];
      return;
    }
    if (!result || ![passed, failed, ignored, measured, filteredOut].every(Number.isSafeInteger)) unknown("malformed-summary");
    else {
      if (measured !== 0) unknown("benchmarks");
      else if (running !== passed + failed + ignored) unknown("inconsistent-summary");
      else add(passed, failed, ignored, filteredOut);
    }
    running = null;
  };
  // An unreadable log gives no count; the stage keeps its exit.
  try { await eachLine(log, read); } catch { unknown("no-summary"); }
  finishFrame();
  if (block || running !== null || streams !== 0) unknown("incomplete-summary");
  if (!Object.values(counted).every(Number.isSafeInteger)) unknown("malformed-summary");
  const { passed, failed, ignored, runners: summaries, filteredOut } = counted;
  if (!Number.isSafeInteger(passed + failed)) unknown("malformed-summary");
  if (summaries === 0) unknown("no-summary");
  if (!Number.isSafeInteger(failures)) failures = Number.MAX_SAFE_INTEGER;
  return why
    ? { executed: "unknown", why, runners: summaries, filteredOut, failures }
    : { executed: passed + failed, passed, failed, ignored, runners: summaries, filteredOut, failures };
}

// One line per stage in stage order, then the overall line. Printed once, by
// the process that executed the stages, as the end of its stdout.
export function machineRecord(request, result) {
  const stages = result.stages ?? [];
  if (!recordToken.test(result.runId ?? "") || stages.some(({ name }) => !recordToken.test(name))) return "";
  const prefix = `test-launcher/v1 run=${result.runId}`;
  const described = stages.map((entry, index) => {
    const planned = request.stages?.[index] ?? {};
    const kind = stageKinds.has(planned.kind) ? planned.kind : "other";
    const signalled = entry.signal !== undefined && entry.signal !== null;
    const state = entry.state === "passed" ? "passed" : entry.state === "unrun" ? "skipped"
      : signalled ? "interrupted" : "failed";
    const exited = Number.isInteger(entry.code) && !entry.error && !signalled;
    let line = `${prefix} stage=${entry.name} kind=${kind} state=${state} exit=${exited ? entry.code : "none"}`;
    if (kind === "test") {
      const tests = entry.tests ?? { executed: "unknown", why: exited || signalled ? "no-summary" : "not-run", runners: 0, filteredOut: 0 };
      const filtered = planned.selects === true || tests.filteredOut > 0
        || (typeof planned.command === "string" && Array.isArray(planned.args) && selectsTests(planned.command, planned.args));
      line += ` executed=${tests.executed}`;
      if (tests.executed !== "unknown") line += ` passed=${tests.passed} failed=${tests.failed} ignored=${tests.ignored}`;
      else line += ` why=${recordToken.test(tests.why ?? "") ? tests.why : "no-summary"}`;
      line += ` runners=${tests.runners} filtered=${filtered ? "yes" : "no"}`;
    }
    return { state, line };
  });
  const interrupted = result.interrupted === true || described.some(({ state }) => state === "interrupted");
  const passed = !interrupted && result.state === "passed" && described.every(({ state }) => state === "passed");
  const overall = interrupted ? "interrupted" : passed ? "passed" : "failed";
  const recorded = recordToken.test(result.reason ?? "") ? result.reason : undefined;
  const reason = result.reason === "stopped" ? "stopped" : interrupted ? "stage-interrupted"
    : recorded ?? (stages.some(({ error }) => error) ? "spawn-failed"
      : described.some(({ state }) => state === "failed") ? "stage-failed" : "launcher-error");
  return [...described.map(({ line }) => line),
    `${prefix} overall=${overall} scope=${request.full ? "full" : "focused"} stages=${stages.length}${passed ? "" : ` reason=${reason}`}`,
  ].join("\n").concat("\n");
}

// The file-system calls executable makes; tests replace them.
export const fileProbes = { stat: statSync, lstat: lstatSync, access: accessSync };

// A Windows app execution alias, such as a Store-installed pwsh.exe under
// WindowsApps, is a reparse point that stat refuses with EACCES although the
// entry is present and starting it works. Only that case counts as a file
// stat could not read; every other stat failure stands.
function isFile(candidate, platform, probes) {
  try {
    return probes.stat(candidate).isFile();
  } catch (error) {
    if (platform !== "win32" || error.code !== "EACCES") throw error;
    let alias = false;
    try { alias = probes.lstat(candidate).isSymbolicLink(); } catch { /* the stat refusal is the diagnostic */ }
    if (alias) return true;
    throw error;
  }
}

export function executable(command, cwd, env, { platform = process.platform, probes = fileProbes } = {}) {
  let lastError;
  const path = Object.entries(env).find(([key]) => key.toLowerCase() === "path")?.[1] ?? "";
  const bases = /[\\/]/u.test(command) || isAbsolute(command)
    ? [resolve(cwd, command)] : path.split(delimiter).map((part) => {
      if (platform === "win32" && part.startsWith('"') && part.endsWith('"')) part = part.slice(1, -1);
      return resolve(cwd, part, command);
    });
  // .cmd/.bat require a shell: callers must name that shell explicitly.
  for (const base of bases) {
    for (const candidate of platform === "win32" ? [base, `${base}.exe`] : [base]) {
      try {
        if (platform === "win32" && !/\.exe$/iu.test(candidate)) continue;
        if (!isFile(candidate, platform, probes)) continue;
        probes.access(candidate, platform === "win32" ? constants.F_OK : constants.X_OK);
        return candidate;
      } catch (error) {
        if (error.code !== "ENOENT") lastError = error;
      }
    }
  }
  throw new Error(`executable not found: ${command}${lastError ? `; last error: ${lastError.message}` : ""}; check PATH or supply the shell explicitly`);
}

// Where Windows keeps its own PowerShell. Never a copy found by name, which
// could be one in the working directory or earlier on PATH.
function systemPowerShell(env) {
  const root = Object.entries(env).find(([key]) => key.toLowerCase() === "systemroot")?.[1];
  return root && isAbsolute(root) ? join(root, "System32", "WindowsPowerShell", "v1.0", "powershell.exe") : null;
}

// How long one identity query may take. A run whose executor could not ask
// records that, and recover then treats it as a record without a creation
// time, so a short bound costs nothing but that.
const identityQueryMs = 15_000;

// Where macOS keeps ps.
const systemPs = "/bin/ps";

// The system calls processIdentity makes; tests replace them.
export const systemProbes = {
  powershell(script, env = process.env) {
    const executable = systemPowerShell(env);
    if (!executable) return { error: new Error("SystemRoot names no Windows directory") };
    return spawnSync(executable, ["-NoProfile", "-NonInteractive", "-Command", script],
      { encoding: "utf8", windowsHide: true, timeout: identityQueryMs });
  },
  // Signal 0 only asks whether the process exists; nothing is sent to it.
  exists(pid) {
    try { process.kill(pid, 0); return "exists"; }
    catch (error) { return error.code === "ESRCH" ? "absent" : error.code === "EPERM" ? "exists" : error.code ?? "error"; }
  },
  procStat: (pid) => readFileSync(`/proc/${pid}/stat`, "utf8"),
  // The system's own ps, never one found on PATH, in one clock and one
  // language, so that every query gives a process the same start text.
  ps(pid) {
    if (!existsSync(systemPs)) return { error: new Error(`${systemPs} is not present`) };
    return spawnSync(systemPs, ["-o", "lstart=", "-p", String(pid)],
      { encoding: "utf8", timeout: identityQueryMs, env: { ...process.env, TZ: "UTC", LC_ALL: "C" } });
  },
};

// Where a process id belongs: its system and, on Linux, its process
// namespace. An id means nothing to a query made from another, such as WSL
// asking about a Windows process or a host about a container's.
export function processHost() {
  let pidNamespace = null;
  if (process.platform === "linux") {
    try { pidNamespace = readlinkSync("/proc/self/ns/pid"); } catch { pidNamespace = null; }
  }
  return { platform: process.platform, hostname: hostname(), pidNamespace };
}

const sameHost = (a, b) => a.platform === b.platform && a.hostname === b.hostname && a.pidNamespace === b.pidNamespace;
const describeHost = ({ platform, hostname: name, pidNamespace }) =>
  `${platform} on ${name}${pidNamespace ? ` in ${pidNamespace}` : ""}`;
const wellFormedHost = (host) => host !== null && typeof host === "object" && typeof host.platform === "string"
  && typeof host.hostname === "string" && (host.pidNamespace === null || typeof host.pidNamespace === "string");

// The creation-time family each platform's query gives.
const createdFamilies = { win32: "win32", linux: "linux", darwin: "ps" };

// The form each platform's creation time takes; any other is not an identity.
const createdForms = { win32: /^win32:\d+$/u, linux: /^linux:\d+$/u, ps: /^ps:[!-~]+(?: [!-~]+)*$/u };
export const wellFormedCreated = (created) => {
  if (typeof created !== "string") return false;
  const family = created.split(":", 1)[0];
  return Object.hasOwn(createdForms, family) && createdForms[family].test(created);
};

// WMI's creation time, `yyyyMMddHHmmss.ffffff` in local time followed by its
// offset from UTC in minutes, as a UTC FILETIME: 100 ns since 1601, here to
// the microsecond. The arithmetic uses the offset WMI wrote, so no time-zone
// rule is applied a second time. Anything else is null.
export function wmiFileTime(text) {
  const parts = /^(\d{4})(\d{2})(\d{2})(\d{2})(\d{2})(\d{2})\.(\d{6})([+-]\d{3})$/u.exec(text);
  if (!parts) return null;
  const [year, month, day, hour, minute, second] = parts.slice(1, 7).map(Number);
  const local = Date.UTC(year, month - 1, day, hour, minute, second);
  const back = new Date(local);
  if (back.getUTCFullYear() !== year || back.getUTCMonth() !== month - 1 || back.getUTCDate() !== day
    || back.getUTCHours() !== hour || back.getUTCMinutes() !== minute || back.getUTCSeconds() !== second) {
    return null;
  }
  const utcMilliseconds = BigInt(local) - BigInt(Number(parts[8])) * 60_000n;
  return (utcMilliseconds + 11_644_473_600_000n) * 10_000n + BigInt(parts[7]) * 10n;
}

// Whether a creation time the system gave now is the recorded one. A coarse
// answer, which Windows gives only to the microsecond for a process the query
// may not open, is the recorded time cut to a whole microsecond.
export function sameCreation(recorded, found) {
  if (found.created === recorded) return true;
  if (!found.coarse || !wellFormedCreated(recorded) || !/^win32:\d+$/u.test(recorded)
    || !/^win32:\d+$/u.test(found.created)) {
    return false;
  }
  const exact = BigInt(recorded.slice(6));
  const coarse = BigInt(found.created.slice(6));
  return coarse <= exact && exact < coarse + 10n;
}

// The operating system's word on one process: alive, with a creation time
// that tells it from a later process given the same id; gone, only when the
// system says no such process exists; or unknown when the query cannot say.
// It only asks: it never opens, waits on or signals the process.
export function processIdentity(pid, { platform = process.platform, probes = systemProbes } = {}) {
  const gone = { state: "gone" };
  const unknown = (why) => ({ state: "unknown", why });
  const failed = (query) => query.error ? query.error.message
    : query.signal ? `stopped by ${query.signal}` : `exit ${query.status}${query.stderr?.trim() ? `: ${query.stderr.trim().slice(0, 200)}` : ""}`;
  if (!Number.isSafeInteger(pid) || pid <= 0) return unknown("malformed process id");
  if (platform === "win32") {
    // An exited process some handle still keeps is gone. The getters are
    // called as methods, since PowerShell turns a property's failure into
    // nothing. A process the query may not open, such as a system service
    // that took a dead launcher's id, is asked of WMI through .NET, with no
    // module loaded by name: WMI knows every running process and gives its
    // creation time to the microsecond, as local time with its offset.
    const script = `$ErrorActionPreference = 'Stop'
try { $p = [System.Diagnostics.Process]::GetProcessById(${pid}) } catch [System.ArgumentException] { 'gone'; exit 0 }
try { if ($p.get_HasExited()) { 'gone' } else { 'alive ' + $p.get_StartTime().ToFileTimeUtc() } }
catch {
  try {
    $found = @((New-Object System.Management.ManagementObjectSearcher('SELECT CreationDate FROM Win32_Process WHERE ProcessId = ${pid}')).Get())
    if ($found.Count -eq 0) { 'gone' } else { 'coarse ' + $found[0]['CreationDate'] }
  } catch { 'unknown ' + $_.Exception.GetType().Name }
}`;
    const query = probes.powershell(script);
    if (query.error || query.signal || query.status !== 0) return unknown(`process query failed: ${failed(query)}`);
    const answer = query.stdout.trim();
    if (answer === "gone") return gone;
    const alive = /^alive (\d+)$/u.exec(answer);
    if (alive) return { state: "alive", created: `win32:${alive[1]}` };
    const coarse = /^coarse (\S+)$/u.exec(answer);
    const fileTime = coarse ? wmiFileTime(coarse[1]) : null;
    if (fileTime !== null) return { state: "alive", created: `win32:${fileTime}`, coarse: true };
    return unknown(answer.startsWith("unknown ") ? answer.slice(8) : "unreadable process query");
  }
  const presence = probes.exists(pid);
  if (presence === "absent") return gone;
  if (presence !== "exists") return unknown(`cannot ask whether process ${pid} exists: ${presence}`);
  if (platform === "linux") {
    let stat;
    // The process exists, so a status that cannot be read, as under a
    // hidden /proc, says nothing of its end.
    try { stat = probes.procStat(pid); }
    catch (error) { return unknown(`its status cannot be read: ${error.code ?? error.message}`); }
    // The fields after the command name: state first, start time twentieth.
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
    if (fields[0] === "Z" || fields[0] === "X") return gone;
    return /^\d+$/u.test(fields[19] ?? "") ? { state: "alive", created: `linux:${fields[19]}` }
      : unknown("unreadable process start time");
  }
  // Only macOS keeps a process's start as a wall-clock time. Other kernels
  // report it from uptime, so a clock step moves it and a living process
  // would read as a later one: there a process that exists is unknown.
  if (platform !== "darwin") return unknown("this system's start times move with its clock");
  const query = probes.ps(pid);
  if (query.error || query.signal || query.status !== 0) return unknown(`process query failed: ${failed(query)}`);
  const started = query.stdout.trim().replace(/\s+/gu, " ");
  return started ? { state: "alive", created: `ps:${started}` } : unknown("unreadable process start time");
}

export function createRun({ root = repository, stages, notifyTo, requiredBinaryEnv = [], full = false, detached = false,
  item, nextTransition }, env = process.env) {
  validateLaunchMetadata({ item, nextTransition });
  const everyMs = heartbeatEveryMs(env);
  const git = spawnSync("git", ["rev-parse", "--path-format=absolute", "--git-path", "review-runs"],
    { cwd: root, encoding: "utf8", windowsHide: true });
  if (git.error || git.status !== 0) throw usageFailure(
    "git-run-directory-unavailable; run the launcher from a repository worktree with Git available");
  const runId = `test-${randomUUID()}`;
  const runDir = join(git.stdout.trim(), runId);
  mkdirSync(runDir, { recursive: true, mode: 0o700 });
  const request = { runId, root: resolve(root), stages, notifyTo, requiredBinaryEnv, full, detached,
    owner: env.TERMAL_SESSION_ID ?? null, started: new Date().toISOString(),
    ...(full && detached && process.platform === "win32" ? { stopProtocol } : {}),
    ...(item !== undefined ? { item } : {}), ...(nextTransition !== undefined ? { nextTransition: { ...nextTransition } } : {}) };
  // A worker that never starts leaves this first heartbeat to go stale.
  save(join(runDir, "results.json"), { runId, state: "running", started: request.started,
    heartbeat: { at: request.started, everyMs }, limitations: fingerprintLimitations(),
    stages: (stages ?? []).map(({ name }) => ({ name, state: "unrun", cwd: request.root })) });
  try {
    const input = captureFingerprint(root);
    save(join(runDir, "input.json"), input);
    request.expectedFingerprint = input.fingerprint;
    // The capture blocks the event loop; beat once it returns.
    const created = readJson(join(runDir, "results.json"));
    created.heartbeat.at = new Date().toISOString();
    save(join(runDir, "results.json"), created);
  } catch (error) {
    const result = readJson(join(runDir, "results.json"));
    Object.assign(result, { state: "failed", exitCode: 1, reason: "input-capture-failed",
      ended: new Date().toISOString(), error: error.message });
    save(join(runDir, "results.json"), result);
    try { publishLaunchRequest(runDir, request); }
    catch (publicationError) {
      throw Object.assign(usageFailure("request-publication-failed"), { runDir, cause: publicationError });
    }
    throw Object.assign(new Error(`input capture failed; results: ${join(runDir, "results.json")}`, { cause: error }),
      { reason: "input-capture-failed", runDir });
  }
  // The launcher publishes this once, before worker admission. This is not
  // filesystem protection against another process writing the run directory.
  try { publishLaunchRequest(runDir, request); }
  catch (error) {
    try {
      const result = readJson(join(runDir, "results.json"));
      Object.assign(result, { state: "failed", exitCode: 1, reason: "request-publication-failed",
        ended: new Date().toISOString(), error: error.message });
      save(join(runDir, "results.json"), result);
    }
    catch { /* the locator survives even if the filesystem refuses this save */ }
    throw Object.assign(usageFailure("request-publication-failed"), { runDir, cause: error });
  }
  return runDir;
}

export async function runCommand(command, args, { cwd, env, log }) {
  const fd = openSync(log, "wx", 0o600);
  try {
    return await new Promise((done) => {
      let error;
      let child;
      try { child = spawn(command, args, { cwd, env, windowsHide: true, stdio: ["ignore", fd, fd] }); }
      catch (failure) { throw because("spawn-failed", failure); }
      child.on("error", (value) => { error = value.message; });
      child.on("close", (code, signal) => done({ code, signal, ...(error ? { error } : {}) }));
    });
  } finally { closeSync(fd); }
}

// Inspect logs once AFTER close, never tail them. Bound memory even for a single
// enormous line; keep the full bytes on disk. Filtering never determines success.
export async function diagnostics(log, failed) {
  // A log that cannot be read gives no excerpt, only that note; the stage
  // keeps its exit and the run goes on.
  try { return await readDiagnostics(log, failed); }
  catch (error) { return { text: `[the log could not be read: ${error.code ?? "error"}]\n`, truncated: false }; }
}

async function readDiagnostics(log, failed) {
  let selected = "", errors = "", tail = "", pending = "", matchingBytes = 0, longLine = false;
  let continuation = false;
  let context = 0, errorContext = false;
  const accept = (line) => {
    const clean = line.replace(/\x1b\[[0-9;]*[A-Za-z]/gu, "");
    if (/^\s*(?:ok\s+\d+\b|# Subtest:|test\s+.*\.\.\.\s+ok\b|test result: ok\.|[✔✓])/u.test(clean)) {
      // Concurrent runners can interleave passing rows with failure details.
      // Suppress that row without terminating the bounded diagnostic context.
      return;
    }
    tail = `${tail}${clean}\n`.slice(-diagnosticLimit);
    const nodeWarning = /^\s*(?:#\s*)?\(node:\d+\)\s+(?:\[[^\]\r\n]+\]\s+)?[A-Za-z]*Warning:/u.test(clean);
    const warning = nodeWarning || /\bwarning(?:\b|\[)/iu.test(clean);
    const failure = !nodeWarning && !/^\s*(?:#\s*)?warning(?:\b|\[)/iu.test(clean)
      && /\berror(?:\b|\[)|^\s*(?:test .*\.\.\. FAILED|test result: FAILED|FAIL\s|not ok\b|✖|failures:|thread .*panicked|Caused by:)/iu.test(clean);
    if (warning || failure) { context = 5; errorContext = failure; }
    if (context > 0) {
      const text = `${clean}\n`;
      matchingBytes += text.length;
      if (failed && errorContext) errors += text.slice(0, Math.max(0, diagnosticLimit - errors.length));
      else selected += text.slice(0, Math.max(0, diagnosticLimit - selected.length));
      context -= 1;
    }
  };
  for await (const chunk of createReadStream(log, { encoding: "utf8", highWaterMark: 8192 })) {
    pending += chunk;
    let end;
    while ((end = pending.indexOf("\n")) >= 0) {
      if (!continuation) accept(pending.slice(0, end));
      continuation = false;
      pending = pending.slice(end + 1);
    }
    if (pending.length > 16384) {
      // Retain one bounded prefix per logical line. Never reinterpret a later
      // fragment as a passing-test header or a new failure/context line.
      if (!continuation) accept(pending.slice(0, 16384));
      pending = ""; continuation = true; longLine = true;
    }
  }
  if (pending && !continuation) accept(pending);
  selected = (errors + selected).slice(0, diagnosticLimit);
  const fallback = failed && !selected;
  return { text: selected || (fallback ? tail : ""),
    truncated: longLine || matchingBytes > diagnosticLimit || (fallback && statSync(log).size > diagnosticLimit),
    ...(fallback ? { fallback: "no recognized diagnostic; bounded failure tail" } : {}) };
}

function inputDrift(message, root) {
  const status = spawnSync("git", ["status", "--short"],
    { cwd: root, encoding: "utf8", windowsHide: true });
  const detail = status.error || status.status !== 0
    ? `status unavailable: ${status.error?.message ?? status.stderr}`
    : status.stdout || "(clean; inspect HEAD/index and the saved input)";
  return because("input-changed", new Error(`${message}\nCurrent Git status (investigation context, not a since-capture diff):\n`
    + detail.slice(0, diagnosticLimit).trimEnd()
    + (detail.length > diagnosticLimit ? "\n[status truncated]" : "")));
}

function currentFingerprint(root) {
  try { return captureFingerprint(root).fingerprint; }
  catch (error) { throw because("fingerprint-check-failed", error); }
}

function validateNotification(request, env) {
  if (!request.notifyTo) throw usageFailure("no notification target recorded");
  if (!request.owner || env.TERMAL_SESSION_ID !== request.owner) throw usageFailure("TERMAL_SESSION_ID must match the sender recorded at run creation (asserted context, not authentication)");
  if (request.notifyTo === request.owner) throw usageFailure("self-send is not supported; use a genuine worker and coordinator, or foreground host completion wait");
  if (!env.TERMAL_CLI || !isAbsolute(env.TERMAL_CLI)) throw usageFailure("TERMAL_CLI must name the inherited absolute executable");
  try { return executable(env.TERMAL_CLI, request.root, env); }
  catch { throw usageFailure("notification-cli-unavailable"); }
}

export async function executeRun(runDir, env = process.env, ready = () => {}) {
  // Exclusive admission: an interrupted run is not a retryable test command.
  // A losing invocation must not overwrite the admitted owner's results.
  try { closeSync(openSync(join(runDir, "execution.lock"), "wx", 0o600)); }
  catch (error) {
    throw because(error.code === "EEXIST" ? "execution-lock-held" : "execution-admission-failed", error);
  }
  let request, heartbeat, control;
  let result = { runId: basename(runDir), started: null, stages: [], limitations: fingerprintLimitations() };
  const resultPath = join(runDir, "results.json");
  let cleanupUnconfirmed = false;
  const commandRun = async (binary, args, log) => {
    control?.check();
    let outcome;
    try {
      outcome = control
        ? await runJobCommand(binary, args, { cwd: request.root, env: childEnv, log, runDir, control,
          powerShell: executable(systemPowerShell(childEnv), request.root, childEnv) })
        : await runCommand(binary, args, { cwd: request.root, env: childEnv, log });
    } catch (error) {
      if (error.cleanupUnconfirmed === true) cleanupUnconfirmed = true;
      else if (control && !error.reason) error.reason = "helper-start-failed";
      throw error;
    }
    if (outcome.cleanup) (result.processCleanup ??= []).push(outcome.cleanup);
    // runJobCommand returns only after the job has no remaining members.
    control?.check();
    return outcome;
  };
  // Let Rustup read the repository's toolchain file, not an inherited override.
  const childEnv = { ...env };
  const clearedToolchainOverrides = [];
  for (const key of Object.keys(childEnv)) if (key.toUpperCase() === "RUSTUP_TOOLCHAIN") {
    clearedToolchainOverrides.push({ name: key, value: childEnv[key] });
    delete childEnv[key];
  }
  const probe = async (name, command, args) => {
    const log = join(runDir, `preflight-${name}.log`);
    let outcome;
    try { outcome = await commandRun(command, args, log); }
    catch (error) { if (["stopped", "helper-start-failed"].includes(error.reason)) throw error; throw because("preflight-failed", error); }
    result.preflight.push({ name, ...outcome, log,
      diagnostics: await diagnostics(log, outcome.code !== 0 || Boolean(outcome.error)) });
    save(resultPath, result);
    if (outcome.code !== 0 || outcome.error) throw because("preflight-failed", new Error(`${name} preflight failed (${outcome.error ?? outcome.code}); see ${log}`));
  };
  try {
    request = readJson(join(runDir, "request.json"));
    if (request.stopProtocol === stopProtocol) control = stopController(runDir, request);
    // What the record says of each stage is what was read here, before any
    // stage ran: a stage can write to the run directory.
    const plan = { full: request.full === true, stages: (request.stages ?? []).map(({ kind, selects, command, args }) =>
      ({ kind, selects, command, args: Array.isArray(args) ? [...args] : args })) };
    result.started = request.started;
    result.stages = (request.stages ?? []).map(({ name }) => ({ name, state: "unrun", cwd: request.root }));
    const saved = readJson(resultPath);
    if (!saved || !Array.isArray(saved.stages)) throw new Error("invalid initial results: stages missing");
    result = saved;
    result.plan = plan;
    if (result.state === "failed") { control?.close(); return result; }
    // The cadence createRun recorded, held to the same bounds as the setting;
    // anything else falls back to this worker's own setting.
    const recorded = result.heartbeat?.everyMs;
    const everyMs = validHeartbeatEveryMs(recorded) ? recorded : heartbeatEveryMs(env);
    // Who executes the run, as recover later asks the system for it: the id
    // alone could by then name another process.
    const self = processIdentity(process.pid);
    const executor = self.state === "alive" && !self.coarse
      ? { pid: process.pid, created: self.created, host: processHost() }
      : { pid: process.pid, created: null, host: processHost(), unidentified: self.coarse
        ? "the system gave only a coarse creation time" : self.why ?? "the system did not report this process" };
    Object.assign(result, { pid: process.pid, owner: request.owner, preflight: [], clearedToolchainOverrides, executor,
      heartbeat: { at: new Date().toISOString(), everyMs } });
    save(resultPath, result);
    // Beat on a timer, not only at stage boundaries, so a long stage stays
    // visibly alive. A reader holding results.json open can make one save fail
    // on Windows; the next beat retries, and a missed beat never fails the run.
    heartbeat = setInterval(() => {
      result.heartbeat.at = new Date().toISOString();
      try { save(resultPath, result); } catch { /* retried on the next beat */ }
    }, everyMs);
    heartbeat.unref();
    await ready();
    control?.check();
    if (!Array.isArray(request.stages) || request.stages.length === 0) throw new Error("at least one stage is required");
    const names = new Set();
    for (const stage of request.stages) {
      if (!recordToken.test(stage.name) || names.has(stage.name)) throw new Error("stage names must be unique, of lowercase letters, digits and hyphens");
      names.add(stage.name);
      if (typeof stage.command !== "string" || !Array.isArray(stage.args) || !stage.args.every((arg) => typeof arg === "string")) throw new Error("stage must contain a command and string arguments");
      try { stage.command = executable(stage.command, request.root, childEnv); }
      catch (error) { throw because("preflight-failed", error); }
    }
    if (request.notifyTo) validateNotification(request, env);
    for (const name of request.requiredBinaryEnv) {
      if (!/^[A-Z][A-Z0-9_]*$/u.test(name)) throw new Error("invalid binary environment variable name");
      if (!env[name] || !isAbsolute(env[name])) throw because("preflight-failed", new Error(`required binary environment variable ${name} must be an absolute executable path`));
      let binary;
      try { binary = executable(env[name], request.root, childEnv); }
      catch (error) { throw because("preflight-failed", new Error(`${name}: ${error.message}`)); }
      await probe(name, binary, ["--version"]);
    }
    if (request.full) {
      // Probe toolchain components before the expensive suite, preserving diagnostics.
      for (const [index, args] of [["version", ["--version"]], ["fmt", ["fmt", "--version"]], ["clippy", ["clippy", "--version"]]]) {
        let cargo;
        try { cargo = executable("cargo", request.root, childEnv); }
        catch (error) { throw because("preflight-failed", error); }
        await probe(`cargo-${index}`, cargo, args);
      }
    }
    const before = currentFingerprint(request.root);
    if (before !== request.expectedFingerprint) throw inputDrift("input drift before execution; no stages run", request.root);
    result.expectedFingerprint = request.expectedFingerprint;
    result.before = before;
    // The capture blocked the timer; beat with this save.
    result.heartbeat.at = new Date().toISOString();
    save(resultPath, result);
    for (let index = 0; index < request.stages.length; index += 1) {
      const stage = request.stages[index];
      const entry = result.stages[index];
      Object.assign(entry, { state: "running", cwd: request.root, started: new Date().toISOString(), command: [stage.command, ...stage.args], log: join(runDir, `${stage.name}.log`) });
      save(resultPath, result);
      const outcome = await commandRun(stage.command, stage.args, entry.log);
      Object.assign(entry, outcome, { ended: new Date().toISOString(), state: outcome.code === 0 && !outcome.error ? "passed" : "failed" });
      if (stage.kind === "test") {
        entry.tests = await countTests(entry.log, { cargoFrames: cargoPhase(stage) });
        // Attribute known focused counts to the runner we started, never to
        // the shape of its output. Full gates keep their stage-kind contract.
        if (!request.full && entry.tests.executed !== "unknown"
          && runnerName(stage.command) === "node" && commandKind(stage.command, stage.args) === "test") {
          entry.tests.runner = "node-test";
        }
        // The one thing besides its exit that fails a stage: its runners'
        // own complete summaries counting failed tests.
        if (entry.state === "passed" && entry.tests.failures > 0) {
          entry.state = "failed";
          entry.reported = `the runners' summaries count ${entry.tests.failures} failed tests and the command exited 0`;
        }
      }
      entry.diagnostics = await diagnostics(entry.log, entry.state === "failed");
      save(resultPath, result);
      if (entry.state === "failed") break;
    }
    result.after = currentFingerprint(request.root);
    if (result.after !== result.expectedFingerprint) throw inputDrift("input drift: results do not validate the current source", request.root);
    result.state = result.stages.every((stage) => stage.state === "passed") ? "passed" : "failed";
    result.exitCode = result.stages.find((stage) => stage.state === "failed")?.code || (result.state === "passed" ? 0 : 1);
  } catch (error) {
    result.state = "failed"; result.exitCode = 1; result.error = error.message;
    result.reason = error.reason ?? "launcher-error";
    if (error.reason === "stopped") Object.assign(result, { interrupted: true, stop: error.stop });
    for (const stage of result.stages) if (stage.state === "running") { stage.state = "failed"; stage.error = error.message; }
  }
  clearInterval(heartbeat);
  // The same lock orders stop acceptance against this final terminal save.
  // No process is launched or waited on while the lock is held.
  const commit = () => {
    if (control && !cleanupUnconfirmed) {
      try { control.check(); }
      catch (error) {
        Object.assign(result, { state: "failed", exitCode: 1, reason: error.reason ?? "launcher-error", error: error.message });
        if (error.reason === "stopped") Object.assign(result, { interrupted: true, stop: error.stop });
      }
    }
    if (cleanupUnconfirmed) {
      Object.assign(result, { state: "failed", exitCode: 1, reason: "process-cleanup-unconfirmed" });
      delete result.interrupted;
      delete result.stop;
    }
    result.ended = new Date().toISOString();
    save(resultPath, result);
  };
  try {
    if (control) {
      try {
        await withStopLock(runDir, commit, { onReleaseError: (error) => {
          result.stopLockWarning = `stop lock removal failed: ${error.code ?? "unknown"}`;
          try { save(resultPath, result); } catch { /* terminal result was already saved */ }
        } });
      } catch (error) {
        if (!error.stopLockTimeout) throw error;
        // A dead CLI may have left the lock. No process is running here; do
        // not lose terminal delivery or guess that it is safe to remove it.
        // commit inspects the retained intent again immediately before saving.
        result.stopLockWarning = "stop lock timed out; terminal save used the latest retained intent";
        commit();
      }
    } else commit();
  }
  finally { control?.close(); }
  return result;
}

// A terminal result, as every reader of results.json tells it, TermAl's
// included: never replaced.
const isTerminal = (result) => ["passed", "failed"].includes(result.state) && Boolean(result.ended)
  && Number.isInteger(result.exitCode);

// How a reader tells a run's state from results.json alone. The pid is never
// consulted: after a kill it may already belong to an unrelated process.
export function runLiveness(result, now = Date.now()) {
  if (isTerminal(result)) return result.state;
  const at = Date.parse(result.heartbeat?.at ?? "");
  const everyMs = result.heartbeat?.everyMs;
  if (!Number.isFinite(at) || !Number.isInteger(everyMs) || everyMs <= 0) return "unknown";
  return now - at > HEARTBEAT_STALE_INTERVALS * everyMs ? "interrupted" : "running";
}

const completionReasons = new Set(["input-capture-failed", "request-publication-failed", "input-changed", "fingerprint-check-failed", "preflight-failed",
  "spawn-failed", "stage-failed", "launcher-error", "stopped", "helper-start-failed", "process-cleanup-unconfirmed"]);
const stageHasFailures = (entry) => entry.tests?.failures > 0 || entry.tests?.failed > 0;
function stageState(entry) {
  if (entry.signal) return "interrupted";
  if (entry.outcome === "unknown") return "unknown";
  if (entry.state === "skipped") return "unrun";
  if (entry.state === "passed") {
    if (entry.error || stageHasFailures(entry)) return "failed";
    if (!Number.isSafeInteger(entry.code)) return "unknown";
    return entry.code === 0 ? "passed" : "failed";
  }
  return ["failed", "running", "unrun"].includes(entry.state) ? entry.state : "unknown";
}
function stateCounts(entries, state) {
  const counts = { total: entries.length, passed: 0, failed: 0, interrupted: 0, running: 0, unrun: 0, unknown: 0 };
  for (const entry of entries) counts[state(entry)] += 1;
  return counts;
}
function completionInput(request, result) {
  const values = [request.expectedFingerprint, result.expectedFingerprint, result.before, result.after];
  const known = values.filter((value) => typeof value === "string" && value.length > 0);
  const comparison = result.reason === "input-changed" || new Set(known).size > 1 ? "mismatch"
    : known.length === 4 && known.every((value) => /^[a-f0-9]{64}$/u.test(value)) ? "matched" : "unknown";
  return { expected: /^[a-f0-9]{64}$/u.test(request.expectedFingerprint ?? "") ? request.expectedFingerprint : "unknown", comparison };
}

// Read metadata only. Logs and saved diagnostic/error prose never enter this
// projection. A launch snapshot can be supplied by the executing process.
export function completionMetadata(request, result, now = Date.now()) {
  const entries = (value) => Array.isArray(value)
    ? value.map((entry) => entry !== null && typeof entry === "object" && !Array.isArray(entry) ? entry : {}) : [];
  const stages = entries(result.stages);
  const preflight = entries(result.preflight);
  const input = completionInput(request, result);
  const liveness = runLiveness(result, now);
  const inferredInterruption = liveness === "interrupted" && !isTerminal(result) && result.interrupted !== true;
  const interrupted = result.interrupted === true || stages.some((entry) => Boolean(entry.signal)) || liveness === "interrupted";
  const contradictory = result.exitCode !== 0 || Boolean(result.error) || stages.length === 0
    || stages.some((entry) => entry.state !== "passed" || entry.code !== 0 || entry.error || entry.signal || entry.outcome
      || stageHasFailures(entry))
    || preflight.some((entry) => entry.code !== 0 || entry.error || entry.signal);
  const verdict = interrupted ? "INTERRUPTED" : liveness === "passed"
    ? input.comparison === "mismatch" || contradictory ? "FAIL" : input.comparison === "matched" ? "PASS" : "UNKNOWN"
    : { failed: "FAIL", running: "RUNNING", unknown: "UNKNOWN" }[liveness] ?? "UNKNOWN";
  const terminal = isTerminal(result);
  const start = Date.parse(request.started ?? result.started);
  const end = Date.parse(result.ended);
  const elapsed = !result.interrupted && terminal && Number.isFinite(start) && Number.isFinite(end) && end >= start
    ? { ms: end - start, basis: "launch-to-terminal" }
    : { ms: "unknown", basis: result.interrupted ? "recovery-end" : "unavailable" };
  const failed = stages.find((entry) => stageState(entry) === "failed" || entry.signal);
  const interruptedStage = inferredInterruption ? stages.find((entry) => entry.state === "running") : failed;
  const stage = typeof failed?.name === "string" && recordToken.test(failed.name) ? failed.name : null;
  const recoveredStage = typeof result.recovered?.stage === "string" && recordToken.test(result.recovered.stage)
    ? result.recovered.stage : typeof interruptedStage?.name === "string" && recordToken.test(interruptedStage.name)
      ? interruptedStage.name : stage;
  const exit = Number.isInteger(result.exitCode) ? result.exitCode : null;
  const inputUnverified = verdict === "UNKNOWN" && terminal && liveness === "passed" && input.comparison === "unknown";
  const reason = input.comparison === "mismatch" ? "input-changed"
    : inputUnverified ? "input-unverified"
      : result.reason === "stopped" ? "stopped" : inferredInterruption ? "heartbeat-stale" : interrupted ? "interrupted" : completionReasons.has(result.reason) ? result.reason
      : verdict === "PASS" ? "completed" : !terminal ? "no-terminal-result" : failed ? "stage-failed" : "launcher-error";
  const blocker = interrupted ? { reason: "interrupted", stage: recoveredStage }
    : inputUnverified ? { reason: "no-terminal-result", basis: "input-unverified" }
      : !terminal ? { reason: "no-terminal-result" }
      : failed ? { reason: "stage-failed", stage, exit: Number.isInteger(failed.code) ? failed.code : null }
        : { reason: "runner-failed", exit,
          ...(liveness === "passed" ? { basis: input.comparison === "mismatch" ? "input-changed" : "contradictory-result" } : {}) };
  return { verdict, terminal, exit, reason, input, elapsed, blocker,
    stages: stateCounts(stages, stageState),
    stageDetails: stages.map((entry) => {
      const name = typeof entry.name === "string" && recordToken.test(entry.name) ? entry.name : null;
      return { name, state: name === null ? "unknown" : stageState(entry),
        exit: name !== null && Number.isSafeInteger(entry.code) ? entry.code : "unknown" };
    }),
    preflight: stateCounts(preflight, (entry) => entry.signal ? "interrupted"
      : entry.outcome === "unknown" || !Number.isInteger(entry.code) ? "unknown" : entry.code === 0 && !entry.error ? "passed" : "failed") };
}

function renderCompletion(runDir, request, result, now) {
  const metadata = completionMetadata(request, result, now);
  // These two lines are a live TermAl reader contract, not stylistic choices.
  const lines = [`${metadata.verdict} ${basename(runDir)} exit=${metadata.terminal ? metadata.exit : "unknown"}`,
    `results: ${join(runDir, "results.json")}`,
    // The supported focused/full modes have 1/12 stages, within the host's
    // bounded completion window. Preserve every stage for larger saved runs.
    ...metadata.stageDetails.map((stage) => `${stage.name ?? "unknown-stage"}: ${stage.state} exit=${stage.exit} log=${
      stage.name === null ? "unknown" : join(runDir, `${stage.name}.log`)}`),
    `reason: ${metadata.reason}`,
    ...["stages", "preflight"].map((kind) => `${kind}: ${Object.entries(metadata[kind]).map(([name, count]) => `${name}=${count}`).join(" ")}`),
    `elapsed: ms=${metadata.elapsed.ms} basis=${metadata.elapsed.basis}`,
    `input: ${metadata.input.comparison}; expectedFingerprint=${metadata.input.expected}; boundary comparison only, not host check credit`];
  if (["stage", "startup", "between-stages", "finishing"].includes(result.recovered?.phase)) {
    const stage = typeof result.recovered.stage === "string" && recordToken.test(result.recovered.stage)
      ? result.recovered.stage : null;
    lines.push(`recovery: phase=${result.recovered.phase} stage=${quote(stage)}`);
  }
  if (result.reason === "stopped" && result.stop) {
    lines.push(`stop: actor=${quote(result.stop.actor)} reason=${quote(result.stop.reason)} requestedAt=${quote(result.stop.requestedAt)}`);
  }
  if (request.item !== undefined) lines.push(`ITEM ${quote(request.item)}`);
  else lines.push("item: undeclared");
  if (request.nextTransition !== undefined) {
    lines.push(`NEXT OWNER ${quote(request.nextTransition.owner)}`, `NEXT ACTION ${quote(request.nextTransition.action)}`);
    if (metadata.verdict !== "PASS") lines.push(`BLOCKER ${quote(metadata.blocker)}`);
  }
  // Fixed coverage metadata, never arbitrary text copied from results.json.
  lines.push(...fingerprintLimitations());
  return `${lines.join("\n")}\n`;
}

export async function summarize(runDir, { now = Date.now(), request, result } = {}) {
  if (!request) {
    try { request = completionRequest(runDir); }
    catch { request = {}; }
  }
  result ??= readResults(runDir);
  return renderCompletion(runDir, request, result, now);
}

// Explicit, bounded failure investigation. This is never called by a default
// summary, notification, completion or recovery.
export async function inspectFailure(runDir, selector, maxChars = diagnosticLimit) {
  if (!Number.isInteger(maxChars) || maxChars < 1 || maxChars > inspectionLimit) {
    throw usageFailure("--max-chars requires an integer from 1 to 6000");
  }
  const result = readResults(runDir);
  const preflight = typeof selector === "string" && selector.startsWith("preflight:");
  const name = preflight ? selector.slice("preflight:".length) : selector;
  const entry = (preflight ? result.preflight : result.stages)?.find((candidate) => candidate.name === name);
  if (!entry) throw usageFailure("inspection selector is unavailable");
  const failed = preflight ? entry.code !== 0 || Boolean(entry.error) || Boolean(entry.signal)
    : entry.state === "failed" || stageState(entry) === "failed" || Boolean(entry.signal);
  if (!failed) throw usageFailure("inspection requires a failed stage or preflight probe");
  const lines = [`inspection: ${quote(selector)} max-chars=${maxChars}`, `results: ${join(runDir, "results.json")}`];
  // Only this opt-in path reads the selected saved diagnostic prose. Full logs
  // remain available at their locator, without executing or changing a run.
  if (typeof entry.log === "string") lines.push(`log: ${quote(entry.log)}`);
  const text = [entry.error, entry.reported, entry.diagnostics?.text].filter((value) => typeof value === "string" && value).join("\n");
  lines.push(text ? text.slice(0, maxChars) : "inspection: no-saved-excerpt");
  if (text.length > maxChars || entry.diagnostics?.truncated) lines.push("inspection: truncated; full log at locator");
  return `${lines.join("\n")}\n`;
}

// The executor a run published: its process id, its host and, from the
// executor itself, its creation time. A run from before executors were
// recorded names the id alone, with no host. Anything else cannot be
// identified.
function recordedExecutor(result) {
  const { executor } = result;
  if (executor !== undefined) {
    if (executor === null || typeof executor !== "object" || !Number.isSafeInteger(executor.pid) || executor.pid <= 0
      || !(executor.created === null || wellFormedCreated(executor.created))
      || !wellFormedHost(executor.host)) {
      return { unidentifiable: true };
    }
    return { pid: executor.pid, created: executor.created, host: executor.host };
  }
  if (result.pid === undefined) return null;
  return Number.isSafeInteger(result.pid) && result.pid > 0 ? { pid: result.pid, created: null } : { unidentifiable: true };
}

const refused = (reason, message) => Object.assign(new Error(message), { refused: true, refusalReason: reason });

// Settles a run whose executor has ended without saving a result, once, on
// request. It writes the result TermAl's launcher and reader already agree on
// for an interrupted run: failed, `interrupted: true`, an error beginning
// "interrupted:", and the stage that was running failed with an unknown
// outcome. It asks the system whether the recorded executor still exists and
// never kills, reruns or notifies. A terminal run is left as it is; a run
// whose executor may be alive, cannot be identified or never published itself
// is refused without changing a file. `recovery.lock` serialises recoveries
// and is released after every attempt; one a killed recovery left makes the
// next refuse and name it.
export async function recoverRun(runDir, {
  identify = processIdentity, now = () => new Date(), write = save, host = processHost(),
} = {}) {
  const path = join(runDir, "results.json");
  const read = () => {
    let current;
    try { current = readResults(runDir); }
    catch { throw refused("results-unreadable", "results.json cannot be read as JSON"); }
    if (!current || typeof current !== "object" || current.runId !== basename(runDir) || !Array.isArray(current.stages)) {
      throw refused("results-mismatch", `results.json does not describe run ${basename(runDir)}`);
    }
    return current;
  };
  const seen = read();
  if (isTerminal(seen)) return { settled: false, result: seen };
  const executor = recordedExecutor(seen);
  if (executor === null) throw refused("executor-unpublished", "the run has not published its executor; it may still be starting");
  if (executor.unidentifiable) throw refused("executor-unidentifiable", "the run's record of its executor is malformed, so it cannot be identified");
  // Its id is asked of this system only where the executor ran on it.
  if (executor.host && !sameHost(executor.host, host)) {
    throw refused("executor-elsewhere", `executor ${executor.pid} ran as ${describeHost(executor.host)}, not ${describeHost(host)}, so its id cannot be asked here`);
  }
  if (executor.created !== null && executor.created.split(":", 1)[0] !== createdFamilies[host.platform]) {
    throw refused("executor-elsewhere", `executor ${executor.pid}'s creation time is not one ${host.platform} gives, so it ran elsewhere`);
  }
  const found = identify(executor.pid);
  if (found.state === "unknown") throw refused("executor-unknown", `cannot tell whether executor ${executor.pid} is alive: ${found.why}`);
  if (found.state === "alive") {
    if (executor.created === null) {
      throw refused("executor-unidentifiable", `a process with executor id ${executor.pid} exists, and the run recorded no creation time to tell it from a later one`);
    }
    if (sameCreation(executor.created, found)) throw refused("executor-alive", `executor ${executor.pid} is alive; a stale heartbeat does not end a run`);
    if (!wellFormedCreated(found.created) || found.created.split(":", 1)[0] !== executor.created.split(":", 1)[0]) {
      throw refused("executor-unidentifiable", `executor ${executor.pid}'s creation time cannot be compared with the recorded one`);
    }
  } else if (found.state !== "gone") {
    throw refused("executor-unknown", `cannot tell whether executor ${executor.pid} is alive: unreadable answer`);
  }
  const lock = join(runDir, "recovery.lock");
  let locked = false;
  try { closeSync(openSync(lock, "wx", 0o600)); locked = true; }
  catch (error) { if (error.code !== "EEXIST") throw error; }
  try {
    // Gone is final: a dead executor writes nothing more. Read again under
    // the lock, since the executor may have saved a terminal result before
    // it ended, or an earlier recovery may have settled the run.
    const result = read();
    if (isTerminal(result)) return { settled: false, result };
    if (!locked) throw refused("recovery-lock-held", `another recovery of this run is in progress, or one was killed before releasing ${lock}`);
    const { stages } = result;
    const running = stages.find((stage) => stage.state === "running");
    const finished = stages.filter((stage) => stage.state !== "unrun" && stage.state !== "running");
    const phase = running ? "stage" : finished.length === 0 ? "startup"
      : stages.some((stage) => stage.state === "unrun") && !finished.some((stage) => stage.state === "failed")
        ? "between-stages" : "finishing";
    const at = now().toISOString();
    const why = found.state === "gone" ? "no process has its id" : "its id now names a later process";
    const interruption = `executor ${executor.pid} ended without saving a terminal result (${why})`;
    if (running) {
      Object.assign(running, { state: "failed", outcome: "unknown",
        error: `interrupted: ${interruption}; this stage's outcome is unknown` });
    }
    Object.assign(result, {
      state: "failed",
      exitCode: 1,
      interrupted: true,
      error: `interrupted: ${interruption}; no stage was rerun`,
      ended: at,
      recovered: { at, phase, stage: running?.name ?? (phase === "between-stages" ? finished.at(-1).name : null),
        executor, found: why, lastHeartbeat: result.heartbeat?.at ?? null },
    });
    write(path, result);
    return { settled: true, result };
  } finally {
    if (locked) rmSync(lock, { force: true });
  }
}

export async function notifyRun(runDir, env = process.env, send = runCommand, launchRequest) {
  const targetFile = join(runDir, "notification.target.json");
  const messageFile = join(runDir, "notification.message.txt");
  let request = launchRequest;
  if (!request && (!existsSync(targetFile) || !existsSync(messageFile))) request = completionRequest(runDir);
  // New runs pin the route before execution; old frozen bodies keep their
  // original request route and key. Only new targets pin a second recipient.
  if (!existsSync(targetFile)) {
    // Pin a legacy run's single recipient before its first retry. Never add
    // a next-owner send to a historical body. Concurrent retries read the
    // same published winner rather than use a local, possibly different intent.
    const { runId, root, owner, notifyTo } = request;
    publish(targetFile, `${JSON.stringify({ runId, root, owner, notifyTo })}\n`);
  }
  const target = readJson(targetFile);
  const result = readJson(join(runDir, "results.json"));
  if (!["passed", "failed"].includes(result.state) || !result.ended) throw usageFailure("cannot notify before terminal results are saved");
  const cli = validateNotification(target, env);
  if (!existsSync(messageFile)) {
    publish(messageFile, await summarize(runDir, { request, result }));
  }
  const recipients = [{ to: target.notifyTo, key: `engram-tests:${target.runId}` }];
  if (target.nextOwner && target.nextOwner !== target.notifyTo && target.nextOwner !== target.owner) {
    recipients.push({ to: target.nextOwner, key: `engram-tests:${target.runId}:next-owner` });
  }
  const deliveries = [];
  for (const { to, key } of recipients) {
    const args = ["mailbox", "send", "--to", to, "--message-file", messageFile,
      "--idempotency-key", key, "--json"];
    const log = join(runDir, `notification-${randomUUID()}.log`);
    let outcome;
    try { outcome = await send(cli, args, { cwd: target.root, env, log }); }
    catch (error) { outcome = { code: null, error: error.message }; }
    deliveries.push({ ...outcome, log, attempted: new Date().toISOString(), to, key });
  }
  const { to: _to, key: _key, ...primary } = deliveries[0];
  const ok = deliveries.every(({ code, error }) => code === 0 && !error);
  const receipt = { ...primary, ok, ...(deliveries.length > 1 ? { deliveries } : {}) };
  save(join(runDir, "notification.json"), receipt);
  if (!ok) throw Object.assign(usageFailure("notification failed; tests were NOT rerun"),
    { runDir, retry: "notify" });
  return receipt;
}

function startupReceipt(runDir, pid, completion) {
  const request = readJson(join(runDir, "request.json"));
  return `STARTED ${runDir}\npid=${pid} completion=${completion}\nmanifest=${join(runDir, "input.json")}\nexpectedFingerprint=${request.expectedFingerprint}\n`;
}

async function finish(runDir, { foregroundReceipt = false, workerHandshake = false } = {}) {
  // Whether to notify is read before any stage runs: a stage can rewrite or
  // break request.json. A request that cannot be read fails the run anyway.
  let request;
  try { request = completionRequest(runDir); } catch { request = {}; }
  const result = await executeRun(runDir, process.env, async () => {
    if (foregroundReceipt) process.stdout.write(startupReceipt(runDir, process.pid, "host-process-wait"));
    if (workerHandshake && process.send) {
      await new Promise((done, reject) => process.send({ ready: true }, (error) => error ? reject(error) : done()));
      process.disconnect();
    }
  });
  process.exitCode = result.exitCode;
  try {
    process.stdout.write(await summarize(runDir, { request, result }));
    if (request.notifyTo) {
      try { await notifyRun(runDir, process.env, runCommand, request); }
      catch { process.stderr.write(`notification failed; tests were NOT rerun\nresults: ${join(runDir, "results.json")}\nretry: notify ${quote(runDir)}\n`); process.exitCode ||= 1; }
    }
  } finally {
    // The record describes validation only, and ends this process's stdout
    // whatever happened after the stages.
    process.stdout.write(machineRecord(result.plan ?? {}, result));
  }
}

export async function startDetached(runDir, env = process.env, write = (text) => process.stdout.write(text)) {
  const request = readJson(join(runDir, "request.json"));
  if (request.detached !== true) throw new Error("detached launch requires a run created with detached: true");
  validateNotification(request, env);
  // No inherited terminal handles: process lifetime is independent of the turn.
  const fd = openSync(join(runDir, "launcher.log"), "wx", 0o600);
  try {
    const child = spawn(process.execPath, [script, "_run", runDir],
      { detached: true, windowsHide: true, stdio: ["ignore", fd, fd, "ipc"], env });
    // The completion handle also lets fixtures await child exit before cleanup.
    const completion = new Promise((done) => {
      let error;
      child.once("error", (value) => { error = value.message; });
      child.once("close", (code, signal) => done({ code, signal, error }));
    });
    // One startup handshake, not test polling. Early admission/read errors are
    // returned to the caller before it is told to yield; EEXIST preserves owner.
    await new Promise((done, reject) => {
      child.once("error", reject);
      const failed = (code) => reject(new Error(`worker startup failed (${code}); see ${join(runDir, "launcher.log")}`));
      // Wait for channel closure, not just process exit: a readiness message
      // can still be pending when the process has already ended.
      child.once("close", failed);
      child.once("message", (message) => {
        if (message.ready !== true) { reject(new Error("invalid worker startup response")); return; }
        child.removeListener("close", failed);
        done();
      });
    });
    child.unref();
    write(startupReceipt(runDir, child.pid, `mailbox:${request.notifyTo}`)
      + "End your turn; do not poll. Missing terminal results mean running/interrupted, not PASS; "
      + `\`node scripts/test-launcher.mjs summary "${runDir}"\` tells which from the heartbeat.\n`);
    return { child, completion };
  } catch (error) {
    throw Object.assign(error, { runDir, safeCompletionText: "worker-startup-failed",
      launcherLog: join(runDir, "launcher.log") });
  } finally { closeSync(fd); }
}

// An error escaping a worker must not replace the admitted owner's artifacts.
// Keep its stack and cause in a separate exclusive file, never in completion.
function workerFailure(runDir, error) {
  const failure = Object.assign(new Error("worker failure", { cause: error }), {
    runDir, safeCompletionText: ["execution-lock-held", "execution-admission-failed"].includes(error?.reason)
      ? error.reason : "worker-runtime-failed",
  });
  try {
    const workerLog = join(runDir, `worker-error-${randomUUID()}.log`);
    const text = `${inspect({ stack: error?.stack ?? String(error), cause: error?.cause },
      { customInspect: false, depth: 8 })}\n`;
    writeFileSync(workerLog, text, { flag: "wx", mode: 0o600 });
    failure.workerDiagnosticLog = workerLog;
  } catch {
    // Missing paths, disk space or permissions cannot mask the worker failure.
    failure.workerDiagnosticMissing = true;
  }
  return failure;
}

async function main(args) {
  const mode = args.shift();
  if (mode === "stop") {
    const runId = args.shift();
    let actor, reason;
    while (args.length) {
      const flag = args.shift(), value = args.shift();
      if (flag === "--actor" && actor === undefined) actor = value;
      else if (flag === "--reason" && reason === undefined) reason = value;
      else throw usageFailure("stop requires RUN_ID --reason TEXT --actor TEXT, each once");
    }
    const stopped = await requestStop(runId, { root: repository, actor, reason });
    process.stdout.write(stopped.terminal ? `Already terminal; nothing stopped. ${runId}\n`
      : `Stop requested; cleanup and terminal notification are pending. ${runId}\nrequest: ${join(stopped.runDir, "stop.request.json")}\n`);
    return;
  }
  if (["summary", "notify", "recover", "_run"].includes(mode)) {
    if (mode === "summary" && args.length > 1) {
      const runDir = resolve(args.shift());
      let selector, maxChars = diagnosticLimit;
      const seen = new Set();
      while (args.length) {
        const flag = args.shift();
        if (seen.has(flag)) throw usageFailure("duplicate inspection option");
        seen.add(flag);
        if (flag === "--stage") selector = args.shift();
        else if (flag === "--max-chars") maxChars = Number(args.shift());
        else throw usageFailure("invalid inspection option");
      }
      if (!selector) throw usageFailure("explicit inspection requires --stage NAME");
      process.stdout.write(await inspectFailure(runDir, selector, maxChars)); return;
    }
    if (args.length !== 1) throw usageFailure(`${mode} requires one run directory`);
    const runDir = resolve(args[0]);
    if (mode === "summary") { process.stdout.write(await summarize(runDir)); return; }
    if (mode === "recover") {
      // Only the summary follows: a recovered run is not a run to record again.
      let settled;
      try { ({ settled } = await recoverRun(runDir)); }
      catch (error) {
        if (!error.refused) throw error;
        process.stderr.write(`REFUSED recover; ${error.refusalReason}; nothing changed\nresults: ${join(runDir, "results.json")}\n`);
        if (error.refusalReason === "recovery-lock-held") process.stderr.write(`lock: ${join(runDir, "recovery.lock")}\n`);
        process.exitCode = 1;
        return;
      }
      process.stdout.write(await summarize(runDir));
      process.stdout.write(`${settled ? "Settled as interrupted" : "Already terminal"}; tests not rerun.\n`);
      return;
    }
    if (mode === "notify") {
      try { await notifyRun(runDir); }
      catch (error) { throw Object.assign(error, { runDir, retry: "notify" }); }
      console.log(`Notification sent; tests not rerun. ${runDir}`); return;
    }
    try { await finish(runDir, { workerHandshake: true }); }
    catch (error) { throw workerFailure(runDir, error); }
    return;
  }
  if (!["full", "focused"].includes(mode)) throw usageFailure("usage: test-launcher.mjs full|focused [--notify SESSION] [--detach] [--item TEXT] [--next-owner SESSION --next-action TEXT] [--require-binary-env NAME] [-- COMMAND ARGS...] | summary RUN_DIR [--stage NAME --max-chars N] | notify|recover RUN_DIR | stop RUN_ID --reason TEXT --actor TEXT");
  let notifyTo, detach = false, item, nextOwner, nextAction;
  const metadataFlags = new Set();
  const requiredBinaryEnv = [];
  while (args.length && args[0] !== "--") {
    const flag = args.shift();
    if (flag === "--detach") detach = true;
    else if (["--item", "--next-owner", "--next-action"].includes(flag)) {
      if (metadataFlags.has(flag)) throw usageFailure(`${flag} may be supplied only once`);
      metadataFlags.add(flag);
      const value = args.shift();
      if (value?.startsWith("--")) throw usageFailure(`${flag} requires a value, not a flag`);
      if (flag === "--item") item = value;
      else if (flag === "--next-owner") nextOwner = value;
      else nextAction = value;
      launchText(value, flag, flag === "--next-action" ? 2000 : 256, flag === "--next-owner");
    }
    else if (flag === "--notify") {
      if (notifyTo !== undefined) throw usageFailure("--notify may be supplied only once");
      if (!args[0]?.trim() || args[0].trim().startsWith("-")) throw usageFailure("--notify requires a session id (nonblank, not a flag)");
      notifyTo = args.shift().trim();
    }
    else if (flag === "--require-binary-env") {
      if (!/^[A-Z][A-Z0-9_]*$/u.test(args[0] ?? "")) throw usageFailure("--require-binary-env requires an uppercase variable name");
      const name = args.shift();
      if (requiredBinaryEnv.includes(name)) throw usageFailure(`duplicate binary prerequisite: ${name}`);
      requiredBinaryEnv.push(name);
    }
    else throw usageFailure("invalid launcher option");
  }
  if (args[0] === "--") args.shift();
  if ((nextOwner === undefined) !== (nextAction === undefined)) throw usageFailure("--next-owner and --next-action must be supplied together");
  const nextTransition = nextOwner === undefined ? undefined : { owner: nextOwner, action: nextAction };
  if ((mode === "full" && args.length) || (mode === "focused" && !args.length)) throw usageFailure("full takes no command; focused requires -- COMMAND ARGS");
  if (detach && !notifyTo) throw usageFailure("detached mode requires a different coordinator --notify SESSION; otherwise use foreground host completion wait");
  if (notifyTo) validateNotification({ notifyTo, owner: process.env.TERMAL_SESSION_ID, root: repository }, process.env);
  const stages = mode === "full" ? requiredStages()
    : [{ name: "focused", kind: commandKind(args[0], args.slice(1)), command: args[0], args: args.slice(1) }];
  const runDir = createRun({ stages, notifyTo, requiredBinaryEnv, full: mode === "full", detached: detach, item, nextTransition });
  if (!detach) { await finish(runDir, { foregroundReceipt: true }); return; }
  try { await startDetached(runDir); }
  catch (error) { throw Object.assign(error, { runDir,
    safeCompletionText: error.safeCompletionText ?? "worker-startup-failed", launcherLog: join(runDir, "launcher.log") }); }
}

// Node resolves this module's own path through links (macOS temp paths run
// through /var -> /private/var), so resolve the invoked path the same way.
if (process.argv[1] && realpathSync(process.argv[1]) === script) {
  main(process.argv.slice(2)).catch(async (error) => {
    if (error.runDir) {
      if (error.reason === "input-capture-failed") {
        try { process.stdout.write(await summarize(error.runDir)); } catch { /* the artifact locator remains available */ }
      }
      process.stderr.write(`FAIL launcher: ${error.safeCompletionText ?? error.reason ?? "request-or-runtime-failed"}\nresults: ${join(error.runDir, "results.json")}\n`);
      if (error.launcherLog) process.stderr.write(`launcher.log: ${error.launcherLog}\n`);
      if (error.workerDiagnosticLog) process.stderr.write(`worker-diagnostics: ${error.workerDiagnosticLog}\n`);
      else if (error.workerDiagnosticMissing) process.stderr.write("worker-diagnostics: unavailable\n");
      if (error.retry === "notify") process.stderr.write(`retry: notify ${quote(error.runDir)}\n`);
    } else console.error(`FAIL launcher: ${error.safeCompletionText ?? "request-or-runtime-failed"}`);
    process.exitCode = 1;
  });
}
