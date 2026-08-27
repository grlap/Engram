import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  chmodSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { captureFingerprint, runCli } from "./review-freeze-fingerprint.mjs";

function run(program, args, cwd) {
  return execFileSync(program, args, { cwd, encoding: "utf8" });
}

function repository() {
  const root = mkdtempSync(join(tmpdir(), "engram-review-freeze-"));
  run("git", ["init", "--quiet"], root);
  run("git", ["config", "user.name", "Engram Test"], root);
  run("git", ["config", "user.email", "engram-test@example.invalid"], root);
  writeFileSync(join(root, "tracked.txt"), "baseline\n");
  run("git", ["add", "tracked.txt"], root);
  run("git", ["commit", "--quiet", "-m", "baseline"], root);
  return root;
}

function withRepository(callback) {
  const root = repository();
  try {
    callback(root);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("fingerprint is stable for unchanged review input", () => {
  withRepository((root) => {
    assert.deepEqual(captureFingerprint(root), captureFingerprint(root));
  });
});

test("tracked worktree and index changes alter the fingerprint", () => {
  withRepository((root) => {
    const baseline = captureFingerprint(root).fingerprint;
    writeFileSync(join(root, "tracked.txt"), "unstaged\n");
    const unstaged = captureFingerprint(root).fingerprint;
    run("git", ["add", "tracked.txt"], root);
    const staged = captureFingerprint(root).fingerprint;

    assert.notEqual(unstaged, baseline);
    assert.notEqual(staged, baseline);
    assert.notEqual(staged, unstaged);
  });
});

test("untracked contents are length-delimited and reviewable", () => {
  withRepository((root) => {
    const baseline = captureFingerprint(root).fingerprint;
    const unusualName = "spaced-ł-name.txt";
    writeFileSync(join(root, unusualName), "first\0payload");
    const first = captureFingerprint(root);
    writeFileSync(join(root, unusualName), "second\0payload");
    const second = captureFingerprint(root);

    assert.notEqual(first.fingerprint, baseline);
    assert.notEqual(second.fingerprint, first.fingerprint);
    assert.equal(first.untracked[0].path, unusualName);
  });
});

test(
  "newline-bearing untracked names survive length delimiting",
  { skip: process.platform === "win32" },
  () => {
    withRepository((root) => {
      const unusualName = "line\nbreak.txt";
      writeFileSync(join(root, unusualName), "payload");
      assert.equal(captureFingerprint(root).untracked[0].path, unusualName);
    });
  },
);

test(
  "executable mode is part of untracked identity",
  { skip: process.platform === "win32" },
  () => {
    withRepository((root) => {
      const path = join(root, "helper.sh");
      writeFileSync(path, "#!/bin/sh\nexit 0\n");
      chmodSync(path, 0o644);
      const regular = captureFingerprint(root).fingerprint;
      chmodSync(path, 0o755);
      const executable = captureFingerprint(root).fingerprint;

      assert.notEqual(executable, regular);
    });
  },
);

test(
  "symlink target is fingerprinted without following it",
  { skip: process.platform === "win32" },
  () => {
    withRepository((root) => {
      symlinkSync("tracked.txt", join(root, "link"));
      const first = captureFingerprint(root);
      rmSync(join(root, "link"));
      symlinkSync("missing.txt", join(root, "link"));
      const second = captureFingerprint(root);

      assert.notEqual(second.fingerprint, first.fingerprint);
      assert.equal(first.untracked[0].kind, "symlink");
    });
  },
);

test("check mode rejects drift after a snapshot", () => {
  withRepository((root) => {
    const snapshot = join(root, ".git", "engram-review-freeze.json");
    runCli(["--write", snapshot], root);
    runCli(["--check", snapshot], root);

    writeFileSync(join(root, "tracked.txt"), "drifted\n");
    assert.throws(() => runCli(["--check", snapshot], root), /review input drifted/u);
    assert.equal(JSON.parse(readFileSync(snapshot, "utf8")).schemaVersion, 1);
  });
});

test("CLI exits nonzero when review input drifted", () => {
  withRepository((root) => {
    const script = fileURLToPath(
      new URL("./review-freeze-fingerprint.mjs", import.meta.url),
    );
    const snapshot = join(root, ".git", "engram-review-freeze.json");
    run(process.execPath, [script, "--write", snapshot], root);
    writeFileSync(join(root, "untracked.txt"), "new\n");

    const result = spawnSync(
      process.execPath,
      [script, "--check", snapshot],
      { cwd: root, encoding: "utf8" },
    );
    assert.equal(result.status, 1);
    assert.match(result.stderr, /review input drifted/u);
  });
});
