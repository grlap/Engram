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
  writeFileSync,
} from "node:fs";
import { dirname, isAbsolute, relative, resolve, sep } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const SNAPSHOT_SCHEMA_VERSION = 1;

function git(root, args, { allowFailure = false } = {}) {
  const result = spawnSync("git", args, {
    cwd: root,
    encoding: null,
    maxBuffer: 128 * 1024 * 1024,
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

function hashUntracked(root, hash, path) {
  const { absolute, normalized } = safeRelativePath(root, path);
  const stat = lstatSync(absolute);
  const executable = (stat.mode & 0o111) !== 0;

  if (stat.isSymbolicLink()) {
    addChunk(hash, `untracked-path:${normalized}`, Buffer.from("symlink", "utf8"));
    addChunk(hash, `untracked-target:${normalized}`, Buffer.from(readlinkSync(absolute)));
    return { path: normalized, kind: "symlink", executable: false };
  }
  if (!stat.isFile()) {
    throw new Error(`unsupported untracked object: ${JSON.stringify(path)}`);
  }

  addChunk(
    hash,
    `untracked-mode:${normalized}`,
    Buffer.from(executable ? "100755" : "100644", "ascii"),
  );
  addChunk(hash, `untracked-content:${normalized}`, readFileSync(absolute));
  return { path: normalized, kind: "file", executable };
}

export function captureFingerprint(startDirectory = process.cwd()) {
  const topLevelResult = git(startDirectory, ["rev-parse", "--show-toplevel"]);
  const root = realpathSync(topLevelResult.stdout.toString("utf8").trim());
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

  const untracked = untrackedEntries(root).map((path) => hashUntracked(root, hash, path));
  return {
    schemaVersion: SNAPSHOT_SCHEMA_VERSION,
    fingerprint: hash.digest("hex"),
    head,
    untracked,
  };
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

export function runCli(arguments_, startDirectory = process.cwd()) {
  const options = parseArguments(arguments_);
  const actual = captureFingerprint(startDirectory);
  if (options.mode === "print") {
    process.stdout.write(`${JSON.stringify(actual, null, 2)}\n`);
    return;
  }

  const snapshotPath = resolve(startDirectory, options.path);
  if (options.mode === "write") {
    mkdirSync(dirname(snapshotPath), { recursive: true });
    writeFileSync(snapshotPath, `${JSON.stringify(actual, null, 2)}\n`, {
      encoding: "utf8",
      mode: 0o600,
    });
    process.stdout.write(`${actual.fingerprint}\n`);
    return;
  }

  const expected = JSON.parse(readFileSync(snapshotPath, "utf8"));
  if (
    expected.schemaVersion !== SNAPSHOT_SCHEMA_VERSION ||
    expected.fingerprint !== actual.fingerprint
  ) {
    throw new Error(
      `review input drifted: expected ${expected.fingerprint ?? "<missing>"}, got ${actual.fingerprint}`,
    );
  }
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
