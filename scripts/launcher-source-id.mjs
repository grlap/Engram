import { spawnSync } from "node:child_process";
import { realpathSync } from "node:fs";
import { devNull, homedir } from "node:os";
import { isAbsolute, resolve } from "node:path";
import { namedIfSlow, operationMinimumMs, warnWhileBlocking } from "./operation-warning.mjs";

// Independent hang guard for the child process. The owner alone chooses and
// enforces its source-read budget.
// The owner keeps its own one-minute source budget, which starts only once it
// runs; this watchdog for a hung child allows its start-up and the settling
// of its answer on top, so it never cuts that budget short.
const ownerProcessHangGuardMs = operationMinimumMs + 15_000;

// Launcher Git probes use the same pinned settings as source observation.
export function isolatedGitEnvironment(source = process.env) {
  const env = { ...source };
  for (const key of Object.keys(env)) if (key.toUpperCase().startsWith("GIT_")) delete env[key];
  Object.assign(env, { GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: process.platform === "win32" ? "NUL" : devNull,
    GIT_ATTR_NOSYSTEM: "1", GIT_OPTIONAL_LOCKS: "0", GIT_TERMINAL_PROMPT: "0", GIT_NO_REPLACE_OBJECTS: "1", GIT_NO_LAZY_FETCH: "1" });
  const settings = [["core.fsmonitor", "false"], ["core.excludesFile", env.GIT_CONFIG_GLOBAL],
    ["core.attributesFile", env.GIT_CONFIG_GLOBAL], ["core.checkStat", "default"], ["core.trustctime", "true"],
    ["core.ignoreStat", "false"], ["core.untrackedCache", "false"], ["status.showUntrackedFiles", "all"], ["diff.ignoreSubmodules", "none"]];
  env.GIT_CONFIG_COUNT = String(settings.length);
  settings.forEach(([key, value], index) => { env[`GIT_CONFIG_KEY_${index}`] = key; env[`GIT_CONFIG_VALUE_${index}`] = value; });
  return env;
}

// Binary selection is setup, not another source predicate. Never search PATH or
// build an owner during admission. Tests explicitly select their current build.
export function sourceBinaryPath(env = process.env) {
  if (Object.hasOwn(env, "ENGRAM_SOURCE_BINARY")) {
    if (typeof env.ENGRAM_SOURCE_BINARY !== "string" || !isAbsolute(env.ENGRAM_SOURCE_BINARY)) {
      throw new Error("ENGRAM_SOURCE_BINARY must name an absolute engram executable");
    }
    return resolve(env.ENGRAM_SOURCE_BINARY);
  }
  const cargoHome = Object.hasOwn(env, "CARGO_HOME") ? env.CARGO_HOME : resolve(homedir(), ".cargo");
  if (typeof cargoHome !== "string" || !isAbsolute(cargoHome)) {
    throw new Error("CARGO_HOME must be absolute, or set ENGRAM_SOURCE_BINARY explicitly");
  }
  return resolve(cargoHome, "bin", process.platform === "win32" ? "engram.exe" : "engram");
}

const SOURCE_JSON_BYTES = 1024 * 1024;
// Mirror only the source owner's operation_time diagnostic at this subprocess
// boundary. It is noncanonical stderr, never part of the source identity.
const SOURCE_WARNING_BYTES = 256 * 1024;
const sourceWarningLine = /^warning: (?:source observation|local Git query) is still running after 10 seconds; attempt=[0-9]+\/[0-9]+; elapsed_ms=[0-9]+\r?$/u;
function sourceStderrFailure(stderr, status) {
  if (stderr === undefined || stderr === null) return null;
  if (!Buffer.isBuffer(stderr)) return "owner returned unreadable process stderr";
  if (stderr.length > SOURCE_WARNING_BYTES) return "owner process stderr exceeded its byte bound";
  if (stderr.length === 0) return null;
  if (status !== 0) return "owner refusal carried process stderr";
  try {
    // Preserve a BOM so that it, like every other unexpected byte, is refused.
    const text = new TextDecoder("utf-8", { fatal: true, ignoreBOM: true }).decode(stderr);
    if (text.endsWith("\n") && text.slice(0, -1).split("\n").every((line) => sourceWarningLine.test(line))) return null;
  } catch { /* malformed UTF-8 is unusable diagnostic output */ }
  return "owner carried unexpected or incomplete process stderr";
}
const sourceCodes = new Set(["input_unavailable", "deadline", "output_limit"]);
const fieldsExactly = (value, fields) => value !== null && typeof value === "object" && !Array.isArray(value)
  && Object.keys(value).sort().join("\0") === [...fields].sort().join("\0");
const sourceText = (value) => typeof value === "string" && value.length > 0 && !value.includes("\0")
  && Buffer.from(value).toString("utf8") === value;
const sourceOid = (value) => typeof value === "string" && /^[a-f0-9]{40}$/u.test(value);
const sourcePath = (value) => sourceText(value) && !isAbsolute(value) && !/[\r\n]/u.test(value)
  && value.split("/").every((part) => part !== "" && part !== "." && part !== "..");
const byteOrder = (a, b) => Buffer.compare(Buffer.from(a), Buffer.from(b));
const changeOrder = (a, b) => byteOrder(a.path, b.path) || byteOrder(a.status, b.status)
  || byteOrder(a.origin ?? "", b.origin ?? "");

function sourceChanges(changes) {
  if (!Array.isArray(changes)) return null;
  const result = [];
  for (const change of changes) {
    if (!fieldsExactly(change, ["path", "status", "origin", "blob", "mode"])
        || !sourcePath(change.path) || typeof change.status !== "string"
        || !/^[ MADRCUT?]{2}$/u.test(change.status) || change.status === "  "
        || (/[RC]/u.test(change.status) ? !sourcePath(change.origin) : change.origin !== null)
        || (change.blob === null ? change.mode !== null || !change.status.includes("D")
          : !sourceOid(change.blob) || !["100644", "100755", "120000"].includes(change.mode))) return null;
    if (result.length && changeOrder(result.at(-1), change) >= 0) return null;
    result.push({ path: change.path, status: change.status, origin: change.origin, blob: change.blob, mode: change.mode });
  }
  return result;
}

// Rebuild only object key order. Array/path order is validated, never repaired.
// This compares the owner's complete value without introducing another hash.
function sourceValue(value) {
  if (fieldsExactly(value, ["commit", "tree", "git_delta"])) {
    const delta = value.git_delta;
    if (!sourceOid(value.commit) || !sourceOid(value.tree)
        || !fieldsExactly(delta, ["sha256", "dirty", "gitlinks"])
        || typeof delta.sha256 !== "string" || !/^[a-f0-9]{64}$/u.test(delta.sha256)
        || typeof delta.dirty !== "boolean" || !Array.isArray(delta.gitlinks)
        || (!delta.dirty && delta.gitlinks.length !== 0)) return null;
    const gitlinks = [];
    for (const link of delta.gitlinks) {
      if (!fieldsExactly(link, ["path", "head_pin", "index_pin", "checkout_head"])
          || !sourcePath(link.path)
          || [link.head_pin, link.index_pin, link.checkout_head].some((oid) => oid !== null && !sourceOid(oid))
          || (link.head_pin === null && link.index_pin === null)
          || (link.index_pin !== null && link.checkout_head === null)
          || (gitlinks.length && byteOrder(gitlinks.at(-1).path, link.path) >= 0)) return null;
      gitlinks.push({ path: link.path, head_pin: link.head_pin, index_pin: link.index_pin,
        checkout_head: link.checkout_head });
    }
    return { commit: value.commit, tree: value.tree,
      git_delta: { sha256: delta.sha256, dirty: delta.dirty, gitlinks } };
  }
  if (!fieldsExactly(value, ["commit", "tree", "submodules", "changes"])
      || !sourceOid(value.commit) || !sourceOid(value.tree) || !Array.isArray(value.submodules)) return null;
  const changes = sourceChanges(value.changes);
  if (changes === null) return null;
  const submodules = [];
  for (const child of value.submodules) {
    if (!fieldsExactly(child, ["path", "commit", "changes"]) || !sourcePath(child.path) || !sourceOid(child.commit)
        || (submodules.length && byteOrder(submodules.at(-1).path, child.path) >= 0)) return null;
    const local = sourceChanges(child.changes);
    if (local === null) return null;
    submodules.push({ path: child.path, commit: child.commit, changes: local });
  }
  return { commit: value.commit, tree: value.tree, submodules, changes };
}

export function sourceIdsEqual(before, after) {
  return compareSourceIds(before, after) === "matched";
}

export function compareSourceIds(before, after) {
  const a = sourceValue(before), b = sourceValue(after);
  if (a === null || b === null || Boolean(a.git_delta) !== Boolean(b.git_delta)) return "unknown";
  return JSON.stringify(a) === JSON.stringify(b) ? "matched" : "mismatch";
}

export const sourceIdLimitations = () => [
  "Source identity limitation: ordinary Git normalization and ignored-path policy apply; boundary equality does not prove the absence of transient edits or provide raw-byte coverage.",
];

// Investigation context from the already captured values; never query Git or
// emit the potentially large complete identities into a diagnostic.
export function sourceIdDifference(before, after) {
  const a = sourceValue(before), b = sourceValue(after);
  if (!a || !b) return "source identity unavailable or malformed; inspect results.json";
  const lines = [];
  const add = (text) => { if (lines.length < 12) lines.push(text); };
  if (a.commit !== b.commit) add(`root commit: ${a.commit} -> ${b.commit}`);
  if (a.tree !== b.tree) add(`root tree: ${a.tree} -> ${b.tree}`);
  if (a.git_delta || b.git_delta) {
    if (!a.git_delta || !b.git_delta) add("source identity representations differ");
    else {
      if (a.git_delta.sha256 !== b.git_delta.sha256) add("Git delta changed");
      if (a.git_delta.dirty !== b.git_delta.dirty) add("Git dirty status changed");
      if (JSON.stringify(a.git_delta.gitlinks) !== JSON.stringify(b.git_delta.gitlinks)) {
        add("selected gitlink state changed");
      }
    }
    return lines.join("\n") + "\nComplete observations: results.json sourceIdBefore/sourceIdAfter";
  }
  const repositories = [["", a.changes, b.changes]];
  const children = new Map(b.submodules.map((child) => [child.path, child]));
  const label = (path) => JSON.stringify(path.slice(0, 160)) + (path.length > 160 ? "…" : "");
  for (const child of a.submodules) {
    const later = children.get(child.path);
    if (!later) add(`child removed: ${label(child.path)}`);
    else {
      if (child.commit !== later.commit) add(`child ${label(child.path)} commit: ${child.commit} -> ${later.commit}`);
      repositories.push([`${child.path}/`, child.changes, later.changes]);
      children.delete(child.path);
    }
  }
  for (const child of children.values()) add(`child added: ${label(child.path)} at ${child.commit}`);
  const key = (entry) => JSON.stringify([entry.path, entry.status, entry.origin]);
  for (const [prefix, earlier, later] of repositories) {
    const remaining = new Map(later.map((entry) => [key(entry), entry]));
    for (const entry of earlier) {
      const next = remaining.get(key(entry));
      if (!next || JSON.stringify(entry) !== JSON.stringify(next)) {
        add(`${label(prefix + entry.path)} [${entry.status}]: ${entry.blob ?? "deleted"}/${entry.mode ?? "-"} -> ${next ? `${next.blob ?? "deleted"}/${next.mode ?? "-"}` : "record removed"}`);
      }
      remaining.delete(key(entry));
    }
    for (const entry of remaining.values()) add(`${label(prefix + entry.path)} [${entry.status}]: record added (${entry.blob ?? "deleted"}/${entry.mode ?? "-"})`);
  }
  return Buffer.from(lines.join("\n")).subarray(0, 4096).toString("utf8") + "\nComplete observations: results.json sourceIdBefore/sourceIdAfter";
}

export function captureCommitInputStatus(startDirectory = process.cwd(), sourceEnv = process.env, { run = spawnSync } = {}) {
  const binary = sourceBinaryPath(sourceEnv);
  const root = resolve(startDirectory);
  const unavailable = (message, refusal) => Object.assign(new Error(
    `committed input unavailable (${binary}; set ENGRAM_SOURCE_BINARY to select an owner): ${message.slice(0, 4096)}`),
  { code: "ENGRAM_SOURCE_UNAVAILABLE", ...(refusal ? { refusal } : {}) });
  // The owner prints JSON and keeps its own warnings off; the launcher names a
  // slow observation itself, while the child still runs.
  const result = warnWhileBlocking("source observation", () => run(binary, ["source", "--root", root, "--json"], {
    cwd: root, env: sourceEnv, encoding: null, windowsHide: true, stdio: ["ignore", "pipe", "pipe"],
    // Stop a hung child without supplying or extending the owner's source budget.
    timeout: ownerProcessHangGuardMs, maxBuffer: SOURCE_JSON_BYTES + 1,
  }));
  if (result.error || result.signal || ![0, 1].includes(result.status)) {
    throw unavailable(result.error?.message ?? `owner exited ${result.status ?? "without a status"}${result.signal ? ` (${result.signal})` : ""}`);
  }
  const output = result.stdout;
  if (!Buffer.isBuffer(output) || output.length > SOURCE_JSON_BYTES + 1) {
    throw unavailable("owner output exceeded its byte bound");
  }
  const stderrFailure = sourceStderrFailure(result.stderr, result.status);
  if (stderrFailure) throw unavailable(stderrFailure);
  let observation;
  try {
    const json = new TextDecoder("utf-8", { fatal: true }).decode(output);
    if (Buffer.byteLength(json.replace(/\n$/u, "")) > SOURCE_JSON_BYTES) throw new Error("JSON byte limit");
    observation = JSON.parse(json);
  } catch { throw unavailable("owner returned malformed, non-UTF-8 or oversized JSON"); }
  if (!fieldsExactly(observation, ["version", "status", "root", "source", "tree", "entries", "refusal", "source_id"])
      || observation.version !== 1 || !["clean", "not_clean", "unavailable"].includes(observation.status)
      || !Array.isArray(observation.entries)) throw unavailable("owner returned an unsupported JSON v1 envelope");
  const refused = observation.status === "unavailable";
  if (refused) {
    if (result.status !== 1 || observation.source !== null || observation.tree !== null || observation.entries.length !== 0
        || observation.source_id !== null
        || !(observation.root === null || (sourceText(observation.root) && isAbsolute(observation.root)))
        || !fieldsExactly(observation.refusal, ["code", "message"]) || !sourceCodes.has(observation.refusal.code)
        || !sourceText(observation.refusal.message) || Buffer.byteLength(observation.refusal.message) > 4096) {
      throw unavailable("owner returned an inconsistent unavailable envelope");
    }
    throw unavailable(`${observation.refusal.code}: ${observation.refusal.message}`, observation.refusal);
  }
  if (result.status !== 0 || observation.refusal !== null || !sourceText(observation.root) || !isAbsolute(observation.root)
      || typeof observation.source !== "string" || !/^git-commit:[a-f0-9]{40}$/u.test(observation.source)
      || typeof observation.tree !== "string" || !/^[a-f0-9]{40}$/u.test(observation.tree)
      || (observation.status === "clean") !== (observation.entries.length === 0)) {
    throw unavailable("owner returned an inconsistent completed envelope");
  }
  const sourceId = sourceValue(observation.source_id);
  if (sourceId === null || sourceId.commit !== observation.source.slice("git-commit:".length)
      || sourceId.tree !== observation.tree
      || (sourceId.git_delta
        ? (observation.status === "clean") === sourceId.git_delta.dirty
        : observation.status === "clean" && (sourceId.changes.length
          || sourceId.submodules.some((child) => child.changes.length)))) {
    throw unavailable("owner returned an inconsistent source_id");
  }
  for (const entry of observation.entries) {
    const renamed = /[RC]/u.test(entry?.status ?? "");
    if (!fieldsExactly(entry, renamed ? ["status", "path", "origin"] : ["status", "path"])
        || typeof entry.status !== "string" || !/^[ A-Z?!]{2}$/u.test(entry.status)
        || !sourceText(entry.path) || (renamed && !sourceText(entry.origin))) {
      throw unavailable("owner returned malformed changed-path metadata");
    }
  }
  try {
    // Canonicalizing both roots can block after the observation has ended.
    namedIfSlow("source root check", () => {
      if (realpathSync.native(observation.root) !== realpathSync.native(root)) throw new Error("root mismatch");
    });
  } catch { throw unavailable("owner canonical root differs from the requested root or cannot be resolved"); }
  return { root: observation.root, commit: observation.source.slice("git-commit:".length), tree: observation.tree,
    status: observation.status, entries: observation.entries, sourceId };
}
