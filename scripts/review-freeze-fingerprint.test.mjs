import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import {
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { fixtureHome, removeFixtureHomes, removeFixturePath, closeFixtureClients, tempSnapshot, assertTempClean } from "./test-temp.mjs";
import { basename, dirname, join, relative } from "node:path";
import test, { after } from "node:test";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));
import { fileURLToPath } from "node:url";

import { captureFingerprint, runCli } from "./review-freeze-fingerprint.mjs";

function run(program, args, cwd) {
  // Fixture Git writes must not start maintenance that outlives cleanup.
  const commandArgs = program === "git" ? ["-c", "maintenance.auto=false", ...args] : args;
  return execFileSync(program, commandArgs, { cwd, encoding: "utf8" });
}

function repository(root) {
  run("git", ["init", "--quiet"], root);
  assert.equal(run("git", ["config", "--bool", "--get", "maintenance.auto"], root).trim(), "false");
  run("git", ["config", "user.name", "Engram Test"], root);
  run("git", ["config", "user.email", "engram-test@example.invalid"], root);
  run("git", ["config", "core.autocrlf", "false"], root);
  writeFileSync(join(root, "tracked.txt"), "baseline\n");
  run("git", ["add", "tracked.txt"], root);
  run("git", ["commit", "--quiet", "-m", "baseline"], root);
}

function withRepository(callback) {
  // A Unicode parent catches accidental full-path case expansion on Windows.
  const home = fixtureHome("engram-review-unicode-");
  const root = join(home, "straße", "engram-review-freeze");
  try {
    mkdirSync(root, { recursive: true });
    repository(root);
    // Vary only the owned ASCII leaf; Unicode parents are not case aliases.
    const alias = process.platform === "win32"
      ? join(dirname(root), basename(root).replace(/[a-z]/gu, (letter) => letter.toUpperCase()))
      : root;
    assert.ok(existsSync(alias), "fixture case alias must exist on this filesystem");
    assert.equal(realpathSync.native(alias), realpathSync.native(root),
      "fixture case alias must resolve to the same native directory");
    callback(alias);
  } finally {
    removeFixtureHomes(home);
  }
}

function manifestPath(root) {
  return run("git", ["rev-parse", "--path-format=absolute", "--git-path", "engram-review-freeze.json"], root).trim();
}

function withLinkedRepositories(callback) {
  withRepository((root) => {
    const first = fixtureHome("engram-review-owner-a-");
    const second = fixtureHome("engram-review-owner-b-");
    try {
      for (const linked of [first, second]) {
        run("git", ["worktree", "add", "--quiet", "--detach", linked, "HEAD"], root);
      }
      callback(root, first, second);
    } finally {
      removeFixtureHomes(first, second);
    }
  });
}

for (const mode of ["--write", "--check"]) {
  for (const [direction, directories] of [
    ["main to linked", (main, first) => [main, first]],
    ["linked to main", (main, first) => [first, main]],
    ["linked to linked", (_main, first, second) => [first, second]],
  ]) {
    test(`manifest location guard refuses ${mode} from ${direction}`, () => {
      withLinkedRepositories((...roots) => {
        const [cwd, owner] = directories(...roots);
        const snapshot = manifestPath(owner);
        const ownerGitDir = realpathSync.native(dirname(snapshot));
        const currentGitDir = realpathSync.native(dirname(manifestPath(cwd)));
        // The payload deliberately names the current root: validating its
        // recorded root alone cannot detect the wrongly placed manifest.
        const before = Buffer.from(JSON.stringify(captureFingerprint(cwd)));
        writeFileSync(snapshot, before);
        assert.throws(() => runCli([mode, snapshot], cwd), (error) => {
          assert.match(error.message, /review manifest Git directory mismatch/u);
          assert.ok(error.message.includes(JSON.stringify(ownerGitDir)));
          assert.ok(error.message.includes(JSON.stringify(currentGitDir)));
          return true;
        });
        assert.deepEqual(readFileSync(snapshot), before);
      });
    });
  }
}

test("manifest location guard refuses before missing or malformed payloads and capture", () => {
  withLinkedRepositories((root, linked) => {
    const snapshot = manifestPath(linked);
    const missing = join(dirname(snapshot), "absent", "nested", "freeze.json");
    for (const mode of ["--write", "--check"]) {
      assert.throws(() => runCli([mode, missing], root), /review manifest Git directory mismatch/u);
      assert.equal(existsSync(dirname(dirname(missing))), false);
    }
    writeFileSync(snapshot, "malformed manifest\n");
    writeFileSync(join(root, ".git", "index"), "invalid fixture index");
    for (const mode of ["--write", "--check"]) {
      assert.throws(() => runCli([mode, snapshot], root), /review manifest Git directory mismatch/u);
      assert.equal(readFileSync(snapshot, "utf8"), "malformed manifest\n");
    }
  });
});

test("manifest location guard allows own native and forward-slash aliases and nested paths", () => {
  withLinkedRepositories((root, linked) => {
    for (const cwd of [root, linked]) {
      const snapshot = manifestPath(cwd);
      for (const alias of [cwd, cwd.replaceAll("\\", "/")]) {
        runCli(["--write", snapshot.replaceAll("\\", "/")], alias);
        runCli(["--check", snapshot], alias);
      }
      const nested = join(cwd, "subdirectory");
      mkdirSync(nested);
      const deepSnapshot = join(dirname(snapshot), "review-runs", "new", "input.json");
      runCli(["--write", relative(nested, deepSnapshot)], nested);
      runCli(["--check", relative(nested, deepSnapshot)], nested);
    }
  });
});

test("manifest location guard refuses a linked worktree's relative .git-file path", () => {
  withLinkedRepositories((_root, linked) => {
    const gitfile = join(linked, ".git");
    const before = readFileSync(gitfile);
    const index = run("git", ["rev-parse", "--path-format=absolute", "--git-path", "index"], linked).trim();
    writeFileSync(index, "invalid fixture index");
    assert.throws(() => captureFingerprint(linked), /git diff/u);
    for (const mode of ["--write", "--check"]) {
      assert.throws(() => runCli([mode, join(".git", "engram-review-freeze.json")], linked),
        (error) => process.platform === "win32"
          ? /review manifest parent is not a directory/u.test(error.message)
          : error.code === "ENOTDIR" || /review manifest parent is not a directory/u.test(error.message));
      assert.deepEqual(readFileSync(gitfile), before);
      assert.equal(readFileSync(index, "utf8"), "invalid fixture index");
    }
  });
});

test("manifest location guard keeps custom non-admin locations including another working tree", () => {
  withLinkedRepositories((root, linked) => {
    const custom = join(linked, "not.git", "custom-freeze.json");
    runCli(["--write", custom], root);
    runCli(["--check", custom], root);
    assert.equal(JSON.parse(readFileSync(custom, "utf8")).root, realpathSync.native(root));
  });
});

test("manifest location guard canonicalizes directory aliases on both sides", () => {
  withLinkedRepositories((root, linked) => {
    const aliases = fixtureHome("engram-review-admin-alias-");
    const rootAlias = join(aliases, "root");
    const ownAlias = join(aliases, "own-admin");
    const foreignAlias = join(aliases, "foreign-admin");
    const kind = process.platform === "win32" ? "junction" : "dir";
    const created = [];
    try {
      for (const [target, alias] of [
        [root, rootAlias],
        [dirname(manifestPath(root)), ownAlias],
        [dirname(manifestPath(linked)), foreignAlias],
      ]) {
        symlinkSync(target, alias, kind);
        created.push(alias);
      }
      const own = join(ownAlias, "alias-freeze.json");
      runCli(["--write", own], rootAlias);
      runCli(["--check", own], rootAlias);
      const foreign = join(foreignAlias, "alias-freeze.json");
      const before = Buffer.from(JSON.stringify(captureFingerprint(root)));
      writeFileSync(foreign, before);
      for (const mode of ["--write", "--check"]) {
        assert.throws(() => runCli([mode, foreign], rootAlias), /review manifest Git directory mismatch/u);
        assert.deepEqual(readFileSync(foreign), before);
      }
    } finally {
      for (const alias of created.reverse()) unlinkSync(alias);
      removeFixtureHomes(aliases);
    }
  });
});

test("manifest location guard refuses dangling directory aliases", () => {
  withRepository((root) => {
    const aliases = fixtureHome("engram-review-dangling-");
    const target = join(aliases, "target");
    const alias = join(aliases, "alias");
    let created = false;
    try {
      mkdirSync(target);
      symlinkSync(target, alias, process.platform === "win32" ? "junction" : "dir");
      created = true;
      removeFixturePath(target);
      for (const mode of ["--write", "--check"]) {
        assert.throws(() => runCli([mode, join(alias, "freeze.json")], root),
          (error) => error.code === "ENOENT");
      }
      assert.equal(existsSync(target), false);
    } finally {
      if (created) unlinkSync(alias);
      removeFixtureHomes(aliases);
    }
  });
});

test("manifest location guard recognizes a separate Git directory without a .git name", () => {
  withRepository((root) => {
    const admin = join(dirname(root), "private-admin");
    run("git", ["init", "--quiet", "--separate-git-dir", admin], root);
    const snapshot = manifestPath(root);
    assert.equal(realpathSync.native(dirname(snapshot)), realpathSync.native(admin));
    runCli(["--write", snapshot], root);
    runCli(["--check", snapshot], root);
    withRepository((other) => {
      const before = Buffer.from(JSON.stringify(captureFingerprint(other)));
      writeFileSync(snapshot, before);
      for (const mode of ["--write", "--check"]) {
        assert.throws(() => runCli([mode, snapshot], other), /review manifest Git directory mismatch/u);
        assert.deepEqual(readFileSync(snapshot), before);
      }
    });
  });
});

test("a custom non-admin manifest still enforces its recorded root", () => {
  withRepository((root) => {
    const snapshot = join(dirname(root), "custom-freeze.json");
    runCli(["--write", snapshot], root);
    withRepository((other) => {
      const frozen = JSON.parse(readFileSync(snapshot, "utf8"));
      frozen.fingerprint = captureFingerprint(other).fingerprint;
      writeFileSync(snapshot, JSON.stringify(frozen));
      assert.throws(() => runCli(["--check", snapshot], other), /review worktree mismatch/u);
    });
  });
});

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
    removeFixturePath(prior);
    mkdirSync(join(root, ".tmp-new"));
    assert.throws(() => assertTempClean(before, root), /new entries=.*tmp-new/u);
    removeFixturePath(join(root, ".tmp-new"));
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
    const canonical = realpathSync.native(root);
    if (process.platform === "win32") assert.notEqual(root, canonical);
    assert.deepEqual(captureFingerprint(root), captureFingerprint(canonical));
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
      unlinkSync(join(root, "link"));
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

test("a linked worktree freeze refuses another root before comparing identical input", () => {
  withRepository((root) => {
    const linked = fixtureHome("engram-review-linked-");
    try {
      run("git", ["worktree", "add", "--quiet", "--detach", linked, "HEAD"], root);
      const snapshot = run("git", ["rev-parse", "--path-format=absolute", "--git-path", "engram-review-freeze.json"], linked).trim();
      runCli(["--write", snapshot], linked);
      const frozen = JSON.parse(readFileSync(snapshot, "utf8"));
      assert.equal(frozen.root, realpathSync.native(linked));
      assert.equal(frozen.head, captureFingerprint(root).head);
      assert.equal(frozen.fingerprint, captureFingerprint(root).fingerprint);
      runCli(["--check", snapshot], linked);
      assert.throws(() => runCli(["--check", snapshot], root), (error) => {
        assert.match(error.message, /review manifest Git directory mismatch/u);
        assert.ok(error.message.includes(JSON.stringify(realpathSync.native(dirname(snapshot)))));
        assert.ok(error.message.includes(JSON.stringify(realpathSync.native(root))));
        assert.doesNotMatch(error.message, /review input drifted/u);
        return true;
      });
      // If capture ran first, a broken index would mask the root refusal.
      writeFileSync(join(root, ".git", "index"), "invalid fixture index");
      assert.throws(() => captureFingerprint(root), /git diff/u);
      const script = fileURLToPath(new URL("./review-freeze-fingerprint.mjs", import.meta.url));
      for (const mode of ["--write", "--check"]) {
        const result = spawnSync(process.execPath, [script, mode, snapshot], {
          cwd: root, encoding: "utf8",
        });
        assert.equal(result.status, 1);
        assert.equal(result.stdout, "");
        assert.match(result.stderr, /review manifest Git directory mismatch/u);
        assert.doesNotMatch(result.stderr, /review input drifted|git diff/u);
      }
    } finally {
      removeFixtureHomes(linked);
    }
  });
});

test("a freeze refuses an unrelated repository even when content fingerprints match", () => {
  withRepository((root) => {
    withRepository((other) => {
      const snapshot = join(root, ".git", "engram-review-freeze.json");
      runCli(["--write", snapshot], root);
      // A matching digest must never override the manifest's worktree identity.
      const frozen = JSON.parse(readFileSync(snapshot, "utf8"));
      frozen.fingerprint = captureFingerprint(other).fingerprint;
      writeFileSync(snapshot, JSON.stringify(frozen));
      assert.throws(() => runCli(["--check", snapshot], other), (error) => {
        assert.match(error.message, /review manifest Git directory mismatch/u);
        assert.ok(error.message.includes(JSON.stringify(realpathSync.native(dirname(snapshot)))));
        assert.ok(error.message.includes(JSON.stringify(realpathSync.native(other))));
        return true;
      });
    });
  });
});

test("subdirectory checks preserve the entire worktree scope", () => {
  withRepository((root) => {
    const nested = join(root, "nested");
    mkdirSync(nested);
    writeFileSync(join(nested, "inside.txt"), "inside\n");
    const outside = join(root, "outside.txt");
    writeFileSync(outside, "outside\n");
    const snapshot = run("git", ["rev-parse", "--path-format=absolute", "--git-path", "engram-review-freeze.json"], nested).trim();
    runCli(["--write", snapshot], nested);
    assert.deepEqual(captureFingerprint(nested), captureFingerprint(root));
    assert.equal(JSON.parse(readFileSync(snapshot, "utf8")).root, realpathSync.native(root));
    runCli(["--check", snapshot], root);
    runCli(["--check", snapshot], nested);
    writeFileSync(outside, "changed outside the invocation directory\n");
    assert.throws(() => runCli(["--check", snapshot], nested), /review input drifted/u);
  });
});

test("invalid manifests refuse before trying to capture a worktree", () => {
  withRepository((root) => {
    const snapshot = join(root, ".git", "engram-review-freeze.json");
    const frozen = captureFingerprint(root);
    // Schema refusal must precede capture even when Git's index is invalid.
    writeFileSync(join(root, ".git", "index"), "invalid fixture index");
    const invalid = [null, [], {}, { ...frozen, root: undefined },
      { ...frozen, root: "relative" }, { ...frozen, fingerprint: "bad" },
      { ...frozen, schemaVersion: 0 }];
    for (const value of invalid) {
      writeFileSync(snapshot, JSON.stringify(value));
      assert.throws(
        () => runCli(["--check", snapshot], root),
        /invalid review freeze manifest:.*create a new freeze/u,
      );
    }
    writeFileSync(snapshot, JSON.stringify({ ...frozen, root: undefined }));
    const script = fileURLToPath(new URL("./review-freeze-fingerprint.mjs", import.meta.url));
    const result = spawnSync(process.execPath, [script, "--check", snapshot], {
      cwd: root, encoding: "utf8",
    });
    assert.equal(result.status, 1);
    assert.equal(result.stdout, "");
    assert.match(result.stderr, /invalid review freeze manifest:.*create a new freeze/u);
  });
});

test("index executable mode changes are detected even with core.filemode false", () => {
  withRepository((root) => {
    run("git", ["config", "core.filemode", "false"], root);
    const original = captureFingerprint(root).fingerprint;
    run("git", ["update-index", "--chmod=+x", "tracked.txt"], root);
    assert.match(run("git", ["ls-files", "--stage"], root), /^100755 /u);
    assert.notEqual(captureFingerprint(root).fingerprint, original);
    run("git", ["update-index", "--chmod=-x", "tracked.txt"], root);
    assert.equal(captureFingerprint(root).fingerprint, original);
  });
});

test("index symlink targets and type changes are detected without filesystem symlinks", () => {
  withRepository((root) => {
    run("git", ["config", "core.symlinks", "false"], root);
    const blob = (target) => execFileSync("git", ["hash-object", "-w", "--stdin"], {
      cwd: root, encoding: "utf8", input: target,
    }).trim();
    const first = blob("tracked.txt");
    const second = blob("missing.txt");
    const stage = (mode, hash) => run("git", [
      "update-index", "--add", "--cacheinfo", mode, hash, "link",
    ], root);
    stage("120000", first);
    assert.match(run("git", ["ls-files", "--stage", "link"], root), /^120000 /u);
    const original = captureFingerprint(root).fingerprint;
    stage("120000", second);
    const changedTarget = captureFingerprint(root).fingerprint;
    assert.notEqual(changedTarget, original);
    stage("100644", second);
    assert.notEqual(captureFingerprint(root).fingerprint, changedTarget);
    stage("120000", first);
    assert.equal(captureFingerprint(root).fingerprint, original);
  });
});

test("CLI discloses normalization and Windows limitations separately from stdout", () => {
  withRepository((root) => {
    const script = fileURLToPath(new URL("./review-freeze-fingerprint.mjs", import.meta.url));
    const snapshot = join(root, ".git", "engram-review-freeze.json");
    for (const args of [[], ["--write", snapshot], ["--check", snapshot]]) {
      const result = spawnSync(process.execPath, [script, ...args], {
        cwd: root, encoding: "utf8",
      });
      assert.equal(result.status, 0, result.stderr);
      if (args.length === 0) assert.equal(JSON.parse(result.stdout).root, realpathSync.native(root));
      else assert.equal(result.stdout, `${JSON.parse(readFileSync(snapshot, "utf8")).fingerprint}\n`);
      assert.match(result.stderr, /tracked content follows Git clean\/eol normalization/u);
      if (process.platform === "win32") {
        assert.match(result.stderr, /untracked executable-mode and filesystem symlink properties are unverified on Windows/u);
        assert.match(result.stderr, /Git index modes and symlink targets are covered separately/u);
      } else assert.doesNotMatch(result.stderr, /unverified on Windows/u);
    }
  });
});
