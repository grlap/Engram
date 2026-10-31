#!/usr/bin/env node

// Produce a deterministic identity for exactly what read-only delegated
// reviewers can inspect: HEAD, index, tracked worktree, and untracked objects.

import { createHash } from "node:crypto";
import {
  lstatSync,
  mkdirSync,
  readFileSync,
  readlinkSync,
  realpathSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const SNAPSHOT_SCHEMA_VERSION = 1;

function git(root, args, { allowFailure = false, env = process.env } = {}) {
  // The test launcher's detached worker has no console. Without windowsHide,
  // Windows opens a visible terminal window for every git call it makes.
  const result = spawnSync("git", args, {
    cwd: root,
    encoding: null,
    maxBuffer: 128 * 1024 * 1024,
    windowsHide: true,
    env,
  });
  if (result.error) throw result.error;
  if (result.status !== 0 && !allowFailure) {
    throw new Error(
      `git ${args.join(" ")} failed (${result.status}): ${result.stderr.toString("utf8")}`,
    );
  }
  return result;
}

function addChunk(hash, label, bytes) {
  const labelBytes = Buffer.from(label, "utf8");
  const lengths = Buffer.allocUnsafe(16);
  lengths.writeBigUInt64BE(BigInt(labelBytes.length), 0);
  lengths.writeBigUInt64BE(BigInt(bytes.length), 8);
  hash.update(lengths);
  hash.update(labelBytes);
  hash.update(bytes);
}

function safeRelativePath(root, path) {
  if (isAbsolute(path) || path === "" || path.split(/[\\/]/u).includes("..")) {
    throw new Error(`unsafe untracked path from Git: ${JSON.stringify(path)}`);
  }
  const absolute = resolve(root, path);
  const back = relative(root, absolute);
  if (back === ".." || back.startsWith(`..${sep}`) || isAbsolute(back)) {
    throw new Error(`untracked path escapes repository: ${JSON.stringify(path)}`);
  }
  return { absolute, normalized: path.replaceAll("\\", "/") };
}

function untrackedEntries(root) {
  const listing = git(root, [
    "ls-files",
    "--others",
    "--exclude-standard",
    "-z",
  ]).stdout;
  return listing
    .toString("utf8")
    .split("\0")
    .filter(Boolean)
    .sort((left, right) => Buffer.from(left).compare(Buffer.from(right)));
}

const sha256 = (bytes) => createHash("sha256").update(bytes).digest("hex");

function hashUntracked(root, hash, path) {
  const { absolute, normalized } = safeRelativePath(root, path);
  const stat = lstatSync(absolute);
  const executable = (stat.mode & 0o111) !== 0;
  // The entry's own digest takes exactly the chunks the aggregate takes for
  // it, so two captures can tell which entry differs without changing how
  // the aggregate fingerprint is computed.
  const own = createHash("sha256");
  const chunk = (label, bytes) => {
    addChunk(hash, label, bytes);
    addChunk(own, label, bytes);
  };

  if (stat.isSymbolicLink()) {
    chunk(`untracked-path:${normalized}`, Buffer.from("symlink", "utf8"));
    chunk(`untracked-target:${normalized}`, Buffer.from(readlinkSync(absolute)));
    return { path: normalized, kind: "symlink", executable: false, digest: own.digest("hex") };
  }
  if (!stat.isFile()) {
    throw new Error(`unsupported untracked object: ${JSON.stringify(path)}`);
  }

  chunk(
    `untracked-mode:${normalized}`,
    Buffer.from(executable ? "100755" : "100644", "ascii"),
  );
  chunk(`untracked-content:${normalized}`, readFileSync(absolute));
  return { path: normalized, kind: "file", executable, digest: own.digest("hex") };
}

const RULE_FILES = new Set([".gitignore", ".gitattributes"]);
// Git can read a differently cased name on a case-insensitive filesystem;
// taking more names in only makes the digest stricter.
const isRuleFile = (path) => RULE_FILES.has(path.split("/").pop().toLowerCase());

// Where Git looks for a default global file (`ignore` for core.excludesFile,
// `attributes` for core.attributesFile) when the setting is unset:
// $XDG_CONFIG_HOME/git/NAME, else $HOME/.config/git/NAME. With HOME unset,
// Git for Windows takes HOMEDRIVE+HOMEPATH or USERPROFILE as HOME, so every
// such candidate is taken in.
export function defaultGlobalFiles(name, env = process.env) {
  if (env.XDG_CONFIG_HOME) return [resolve(env.XDG_CONFIG_HOME, "git", name)];
  const homes = env.HOME ? [env.HOME]
    : [env.HOMEDRIVE && env.HOMEPATH ? env.HOMEDRIVE + env.HOMEPATH : "", env.USERPROFILE ?? ""];
  return [...new Set(homes.filter(Boolean).map((home) => resolve(home, ".config", "git", name)))];
}

// The ignored paths Git reports, with every ignored directory collapsed to
// one `dir/` entry, sorted; null when Git cannot list them.
function ignoredEntries(root) {
  try {
    return git(root, ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"])
      .stdout.toString("utf8").split("\0").filter(Boolean)
      .sort((left, right) => Buffer.from(left).compare(Buffer.from(right)));
  } catch {
    return null;
  }
}

// A digest of every ignore and attribute rule the tracked tree does not hold:
// - the repository's info/exclude and info/attributes files;
// - the global files: core.excludesFile and core.attributesFile when set (an
//   empty value turns the default off), or Git's default when unset;
// - every .gitignore and .gitattributes in the worktree that is not tracked,
//   whether visible or itself ignored.
// A changed ignore rule can make a file that was already there visible, and
// a changed attribute can hide a tracked change behind Git's normalization;
// a new rule file is such a change. Tracked rule files are covered by the
// tracked diffs. Null when a source cannot be read, so a capture without it
// never qualifies.
function ruleSourcesDigest(root, untracked, ignored) {
  try {
    if (ignored === null) throw new Error("cannot list ignored paths");
    const hash = createHash("sha256");
    const source = (label, path) => {
      let bytes;
      try { bytes = readFileSync(path); }
      catch (error) {
        if (error.code !== "ENOENT" && error.code !== "ENOTDIR") throw error;
        bytes = null;
      }
      addChunk(hash, `${label}-path`, Buffer.from(path, "utf8"));
      addChunk(hash, `${label}-state`, Buffer.from(bytes === null ? "absent" : "present", "ascii"));
      if (bytes !== null) addChunk(hash, `${label}-content`, bytes);
    };
    for (const name of ["exclude", "attributes"]) {
      const info = git(root, ["rev-parse", "--path-format=absolute", "--git-path", `info/${name}`])
        .stdout.toString("utf8").trim();
      source(`info-${name}`, info);
    }
    for (const [setting, name] of [["core.excludesFile", "ignore"], ["core.attributesFile", "attributes"]]) {
      const configured = git(root, ["config", "--path", "--get", setting], { allowFailure: true });
      if (configured.status === 0) {
        const value = configured.stdout.toString("utf8").replace(/\r?\n$/u, "");
        addChunk(hash, `${setting}-setting`, Buffer.from(`configured:${value}`, "utf8"));
        if (value) source(setting, resolve(root, value));
      } else if (configured.status === 1) {
        addChunk(hash, `${setting}-setting`, Buffer.from("unset", "ascii"));
        for (const candidate of defaultGlobalFiles(name)) source(setting, candidate);
      } else {
        throw new Error(`cannot read ${setting}`);
      }
    }
    const files = [...new Set([...untracked, ...ignored.filter((path) => !path.endsWith("/"))].filter(isRuleFile))]
      .sort((left, right) => Buffer.from(left).compare(Buffer.from(right)));
    for (const path of files) source(`worktree-rules:${path}`, safeRelativePath(root, path).absolute);
    return hash.digest("hex");
  } catch {
    return null;
  }
}

function worktreeRoot(startDirectory) {
  const topLevelResult = git(startDirectory, ["rev-parse", "--show-toplevel"]);
  return realpathSync.native(topLevelResult.stdout.toString("utf8").trim());
}

function physicalManifestPath(path) {
  let existing = path;
  const missing = [];
  for (;;) {
    try {
      lstatSync(existing);
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
      const parent = dirname(existing);
      if (parent === existing) throw error;
      missing.unshift(basename(existing));
      existing = parent;
      continue;
    }
    // Keep resolution outside the missing-path catch: a dangling link must
    // refuse, rather than fall back to its lexical parent as an unowned path.
    const canonical = realpathSync.native(existing);
    if (missing.length && !statSync(canonical).isDirectory()) {
      throw new Error(`review manifest parent is not a directory: ${JSON.stringify(existing)}`);
    }
    return resolve(canonical, ...missing);
  }
}

function admitManifestLocation(root, snapshotPath) {
  const currentGitDir = realpathSync.native(
    git(root, ["rev-parse", "--absolute-git-dir"]).stdout.toString("utf8").trim(),
  );
  const physical = physicalManifestPath(snapshotPath);
  // A linked private Git directory sits below the common main directory.
  // The nearest actual admin directory wins, regardless of its name.
  for (let candidate = dirname(physical); ; candidate = dirname(candidate)) {
    const result = git(root, ["rev-parse", "--resolve-git-dir", candidate], {
      allowFailure: true,
      // Distinguish Git's ordinary negative probe from an operational error
      // without depending on the user's message locale.
      env: { ...process.env, LC_ALL: "C" },
    });
    if (result.status === 0) {
      const ownerGitDir = realpathSync.native(result.stdout.toString("utf8").trim());
      if (ownerGitDir !== currentGitDir) {
        throw new Error(
          `review manifest Git directory mismatch: manifest ${JSON.stringify(physical)} resolves under ${JSON.stringify(ownerGitDir)}; current worktree ${JSON.stringify(root)} uses ${JSON.stringify(currentGitDir)}. Run from the owning worktree, or choose this worktree's manifest path using git rev-parse --path-format=absolute --git-path engram-review-freeze.json.`,
        );
      }
      return;
    }
    const stderr = result.stderr.toString("utf8");
    if (result.status !== 128 || !stderr.startsWith("fatal: not a gitdir ")) {
      throw new Error(`cannot determine review manifest Git directory (${result.status}): ${stderr}`);
    }
    if (dirname(candidate) === candidate) return; // Custom non-admin output.
  }
}

export function captureFingerprint(startDirectory = process.cwd()) {
  return captureWorktree(worktreeRoot(startDirectory));
}

function captureWorktree(root) {
  const headResult = git(root, ["rev-parse", "--verify", "HEAD"], {
    allowFailure: true,
  });
  const head = headResult.status === 0 ? headResult.stdout.toString("utf8").trim() : null;
  const staged = git(root, ["diff", "--cached", "--binary", "--full-index", "--no-ext-diff"]).stdout;
  const unstaged = git(root, ["diff", "--binary", "--full-index", "--no-ext-diff"]).stdout;

  const hash = createHash("sha256");
  addChunk(hash, "schema", Buffer.from(String(SNAPSHOT_SCHEMA_VERSION), "ascii"));
  addChunk(hash, "head", Buffer.from(head ?? "UNBORN", "ascii"));
  addChunk(hash, "staged-diff", staged);
  addChunk(hash, "unstaged-diff", unstaged);

  const paths = untrackedEntries(root);
  const untracked = paths.map((path) => hashUntracked(root, hash, path));
  const ignored = ignoredEntries(root);
  return {
    schemaVersion: SNAPSHOT_SCHEMA_VERSION,
    root,
    fingerprint: hash.digest("hex"),
    head,
    // Digests of the two tracked parts the aggregate covers, for telling which
    // part of an input changed. They are not part of the aggregate.
    components: { stagedDiff: sha256(staged), unstagedDiff: sha256(unstaged),
      ruleSources: ruleSourcesDigest(root, paths, ignored) },
    untracked,
    // The ignored paths at this capture, for telling a newly created path
    // from one that was already there and only became visible. Not part of
    // the aggregate.
    ignored,
  };
}

const DIGEST = /^[a-f0-9]{64}$/u;
const OBJECT_ID = /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/u;
const pathList = (paths) => paths.every((path) => typeof path === "string" && path !== "") &&
  new Set(paths).size === paths.length;
// Whether a capture has every part this module produces, in its own shape: a
// capture that does not is never compared part by part.
const completeCapture = (capture) =>
  capture !== null && typeof capture === "object" && capture.schemaVersion === SNAPSHOT_SCHEMA_VERSION &&
  typeof capture.root === "string" && capture.root !== "" &&
  (capture.head === null || OBJECT_ID.test(capture.head ?? "")) &&
  DIGEST.test(capture.fingerprint ?? "") && DIGEST.test(capture.components?.stagedDiff ?? "") &&
  DIGEST.test(capture.components?.unstagedDiff ?? "") &&
  DIGEST.test(capture.components?.ruleSources ?? "") && Array.isArray(capture.untracked) &&
  capture.untracked.every((entry) => DIGEST.test(entry?.digest ?? "")) &&
  pathList(capture.untracked.map((entry) => entry.path)) &&
  Array.isArray(capture.ignored) && pathList(capture.ignored);

// One spelling for comparing paths: Unicode-composed (NFC) and lowercase, so
// neither a case nor a composition difference makes an old path look new.
// Folding more spellings together can only refuse more.
const comparable = (path) => path.normalize("NFC").toLowerCase();

// Whether a path was ignored at a capture: it is one of the ignored paths, or
// lies inside an ignored directory, compared in one spelling.
function ignoredAt(capture) {
  const ignored = new Set(capture.ignored.map(comparable));
  return (path) => {
    const lower = comparable(path);
    if (ignored.has(lower)) return true;
    for (let slash = lower.indexOf("/"); slash !== -1; slash = lower.indexOf("/", slash + 1)) {
      if (ignored.has(lower.slice(0, slash + 1))) return true;
    }
    return false;
  };
}

// How the input captured after a run differs from the one captured before
// it: "unchanged" when the fingerprints are equal; "new-untracked-only" when
// the root, HEAD, staged and unstaged tracked content and every ignore and
// attribute source outside the tracked tree are unchanged, every path
// untracked before is still there with the same digest, and the only
// difference is new untracked, non-ignored paths, listed in `newPaths`, none
// of which was ignored before (so none was already there, however it became
// visible); "input-changed" for every other difference, and whenever either
// capture is not complete in this module's shape, even with equal
// fingerprints. Tracked content is compared as Git normalizes it, as the
// fingerprint does.
export function classifyInputChange(before, after) {
  const changed = { outcome: "input-changed", newPaths: [] };
  if (!completeCapture(before) || !completeCapture(after)) return changed;
  if (before.fingerprint === after.fingerprint) return { outcome: "unchanged", newPaths: [] };
  if (before.root !== after.root || before.head !== after.head ||
    before.components.stagedDiff !== after.components.stagedDiff ||
    before.components.unstagedDiff !== after.components.unstagedDiff ||
    before.components.ruleSources !== after.components.ruleSources) return changed;
  const later = new Map(after.untracked.map((entry) => [entry.path, entry.digest]));
  if (before.untracked.some((entry) => later.get(entry.path) !== entry.digest)) return changed;
  const earlier = new Set(before.untracked.map((entry) => entry.path));
  const newPaths = after.untracked.map((entry) => entry.path).filter((path) => !earlier.has(path));
  if (newPaths.some(ignoredAt(before))) return changed;
  return newPaths.length ? { outcome: "new-untracked-only", newPaths } : changed;
}

function parseArguments(arguments_) {
  if (arguments_.length === 0) return { mode: "print" };
  if (arguments_.length === 2 && arguments_[0] === "--write") {
    return { mode: "write", path: arguments_[1] };
  }
  if (arguments_.length === 2 && arguments_[0] === "--check") {
    return { mode: "check", path: arguments_[1] };
  }
  throw new Error("usage: review-freeze-fingerprint.mjs [--write FILE | --check FILE]");
}

// Keep the schema-1 algorithm aligned with the independent compiled host checker.
export const NORMALIZATION_LIMITATION = "Review freeze limitation: tracked content follows Git clean/eol normalization; changes erased by that conversion are not detected (not raw-byte coverage).";
export const WINDOWS_LIMITATION = "Review freeze limitation: untracked executable-mode and filesystem symlink properties are unverified on Windows; Git index modes and symlink targets are covered separately.";
export const fingerprintLimitations = (platform = process.platform) =>
  [NORMALIZATION_LIMITATION, ...(platform === "win32" ? [WINDOWS_LIMITATION] : [])];

export function runCli(arguments_, startDirectory = process.cwd()) {
  const options = parseArguments(arguments_);
  process.stderr.write(`${fingerprintLimitations().join("\n")}\n`);
  if (options.mode === "print") {
    process.stdout.write(`${JSON.stringify(captureFingerprint(startDirectory), null, 2)}\n`);
    return;
  }

  const root = worktreeRoot(startDirectory);
  const snapshotPath = resolve(startDirectory, options.path);
  admitManifestLocation(root, snapshotPath);
  if (options.mode === "check") {
    const expected = JSON.parse(readFileSync(snapshotPath, "utf8"));
    if (
      expected === null || typeof expected !== "object" || Array.isArray(expected) ||
      expected.schemaVersion !== SNAPSHOT_SCHEMA_VERSION ||
      typeof expected.root !== "string" || !isAbsolute(expected.root) ||
      typeof expected.fingerprint !== "string" || !/^[a-f0-9]{64}$/u.test(expected.fingerprint)
    ) {
      throw new Error("invalid review freeze manifest: expected current schema, absolute root, and SHA-256 fingerprint; create a new freeze");
    }
    // Root identity is an admission check, not content drift. Do not inspect a
    // different tree's index or files just because its bytes might match.
    if (expected.root !== root) {
      throw new Error(
        `review worktree mismatch: expected root ${JSON.stringify(expected.root)}, current root ${JSON.stringify(root)}`,
      );
    }
    const actual = captureWorktree(root);
    if (expected.fingerprint !== actual.fingerprint) {
      throw new Error(
        `review input drifted: expected ${expected.fingerprint}, got ${actual.fingerprint}`,
      );
    }
    process.stdout.write(`${actual.fingerprint}\n`);
    return;
  }

  const actual = captureWorktree(root);
  mkdirSync(dirname(snapshotPath), { recursive: true });
  writeFileSync(snapshotPath, `${JSON.stringify(actual, null, 2)}\n`, {
    encoding: "utf8",
    mode: 0o600,
  });
  process.stdout.write(`${actual.fingerprint}\n`);
}

const invokedPath = process.argv[1] ? realpathSync(process.argv[1]) : null;
if (invokedPath === fileURLToPath(import.meta.url)) {
  try {
    runCli(process.argv.slice(2));
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  }
}
