import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { fixtureHome, removeFixtureHomes, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { join } from "node:path";
import test, { after } from "node:test";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));
import { fileURLToPath } from "node:url";

import { captureFingerprint, runCli } from "./review-freeze-fingerprint.mjs";

function run(program, args, cwd) {
  return execFileSync(program, args, { cwd, encoding: "utf8" });
}

function repository(root) {
  run("git", ["init", "--quiet"], root);
  run("git", ["config", "user.name", "Engram Test"], root);
  run("git", ["config", "user.email", "engram-test@example.invalid"], root);
  writeFileSync(join(root, "tracked.txt"), "baseline\n");
  run("git", ["add", "tracked.txt"], root);
  run("git", ["commit", "--quiet", "-m", "baseline"], root);
}

function withRepository(callback) {
  const root = fixtureHome("engram-review-freeze-");
  try {
    repository(root);
    callback(root);
  } finally {
    removeFixtureHomes(root);
  }
}

test("fixture ownership cleans failures and Temp audit detects replacement and leftover entries", (t) => {
  const root = fixtureHome("engram-temp-audit-", t);
  const parallel = fixtureHome("engram-parallel-audit-", t);
  try {
    // Another run's directory cannot fail this run's empty-root check.
    const empty = tempSnapshot(root);
    assertTempClean(empty, root);
    const prior = join(root, ".tmp-prior");
    mkdirSync(prior);
    const before = tempSnapshot(root);
    rmSync(prior, { recursive: true });
    mkdirSync(join(root, ".tmp-new"));
    assert.throws(() => assertTempClean(before, root), /new entries=.*tmp-new/u);
    rmSync(join(root, ".tmp-new"), { recursive: true });
    mkdirSync(join(root, "leftover"));
    assert.throws(() => assertTempClean(before, root), /remaining=.*leftover/u);
  } finally {
    removeFixtureHomes(root, parallel);
  }
  assert.equal(existsSync(root), false);
  const failed = fixtureHome("engram-setup-failure-", t);
  assert.throws(() => {
    try { throw new Error("setup failed"); }
    finally { removeFixtureHomes(failed); }
  }, /setup failed/u);
  assert.equal(existsSync(failed), false);
});

test("fixture shutdown attempts every client and retains shutdown failures", async () => {
  const closed = [];
  await assert.rejects(closeFixtureClients(
    { close() { closed.push("first"); throw new Error("close failed"); } },
    undefined,
    { async close() { closed.push("second"); } },
  ), /Fixture process shutdown failed/u);
  assert.deepEqual(closed, ["first", "second"]);
});

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
