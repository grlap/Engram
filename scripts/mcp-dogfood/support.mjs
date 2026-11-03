import assert from "node:assert/strict";
import { fixtureHome as ownedFixtureHome, removeFixtureHomes as cleanupFixtureHomes } from "../test-temp.mjs";
import { readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import nodeTest, { after } from "node:test";
import { createMcpClient } from "./client.mjs";

// One instance owns all timing maps, fixture wrappers and client dependencies.
export function createSupport({ root, target, binary }) {
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

// The opt-in trace emits lifecycle stages and a settled-handler summary.
// Only safely correlated summaries are indexed, within a fixed bound.
const PHASE_TRACE_ENV = "ENGRAM_MCP_PHASE_TRACE";
const MAX_PHASE_RECORDS = 256;

// The record states that time a whole call, its response's send included.
const PHASE_TIMED_STATES = new Set(["complete", "send_failed"]);

/**
 * The line printed beside a slow call: its phase record, or why there is
 * none. A record that did not time the whole call, such as an incomplete,
 * uncorrelated or send_dropped one, reads as unavailable, so it never passes for
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


function shortRef(workId) {
  assert.match(workId, /^[0-9a-f-]{36}$/u);
  return `w-${workId.replaceAll("-", "").slice(20)}`;
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

// Over MCP, a sentence that tells the caller to pass an argument names the
// field: no CLI flag appears in an answer's system prose (a refusal's
// message, reason, remedy and reminders, or a receipt's reminders, hint,
// remedy and memory-retirement instruction) outside an embedded runnable
// command, which keeps CLI syntax: anything in backticks, or a bare
// `engram …` command up to a semicolon, a comma before a space or a line
// end. Other fields carry caller data and are not swept. Every MCP call this
// suite makes is swept, so a sentence added later without its MCP spelling
// fails here. The Rust sweep in src/mcp/prose_sweep.rs applies the same rule.
function assertProseNamesFields(word, result) {
  const value = result?.structuredContent;
  if (value === undefined || value === null) return;
  const error = value.error;
  const source = error ?? value;
  const texts = error
    ? [error.message, error.details?.reason, error.details?.remedy]
    : [value.hint, value.remedy, value.memory_retirement?.instruction];
  texts.push(...(Array.isArray(source.reminders) ? source.reminders : []));
  for (const text of texts) {
    if (typeof text !== "string") continue;
    const prose = text
      .split("`")
      .filter((_, index) => index % 2 === 0)
      .join("")
      .replace(/engram .*?(?=;|, |\n|$)/gu, "");
    assert.doesNotMatch(prose, /(^|[\s('"])--[a-z]/u, `${word}: a CLI flag in MCP prose: ${text}`);
  }
}

function structuredError(result, code) {
  assert.equal(result.isError, true, JSON.stringify(result));
  assert.equal(result.structuredContent.error.code, code);
  return result.structuredContent.error;
}

async function wait(milliseconds) {
  await new Promise((resolvePromise) => setTimeout(resolvePromise, milliseconds));
}

function buildAndInit(engramHome) {
  const started = performance.now();
  const built = spawnSync("cargo", ["build", "--quiet", "--bin", "engram"], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(built.status, 0, built.stderr);
  const buildMs = performance.now() - started;
  const initStarted = performance.now();
  const initialized = spawnSync(binary, ["--home", engramHome, "init"], {
    cwd: root,
    encoding: "utf8",
  });
  assert.equal(initialized.status, 0, initialized.stderr);
  return { build_ms: buildMs, init_ms: performance.now() - initStarted };
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

  const McpClient = createMcpClient({ root, binary, SOFT_TIMING_MS, PHASE_TRACE_ENV, MAX_PHASE_RECORDS, timingLine, recordWalSample, phaseNotice, phaseTraceLine, assertProseNamesFields });
  return { root, target, binary, AGENT_TOOLS, HASH, SOFT_TIMING_MS, fixtureTimings, testTimings, walBytes, timingLine, PHASE_TRACE_ENV, MAX_PHASE_RECORDS, PHASE_TIMED_STATES, phaseTraceLine, phaseNotice, test, fixtureHome, removeFixtureHomes, recordWalSample, shortRef, structured, receipt, assertRecordParity, assertProseNamesFields, structuredError, wait, buildAndInit, cliWord, cliText, cliJson, McpClient };
}
