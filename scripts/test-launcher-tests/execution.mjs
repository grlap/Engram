import assert from "node:assert/strict";
import { execFileSync, spawn, spawnSync } from "node:child_process";
import { chmodSync, constants, copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, realpathSync, rmdirSync, statSync, symlinkSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { basename, delimiter, dirname, join, relative, resolve } from "node:path";
import test, { after, describe } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fingerprintLimitations, NORMALIZATION_LIMITATION } from "../review-freeze-fingerprint.mjs";
import { completionMetadata, createRun, diagnostics, executable, executeRun, fileProbes, inspectFailure, machineRecord, notifyRun, requiredStages, summarize } from "../test-launcher.mjs";
import { fixtureHome, fixtureRoot, removeFixturePath, assertTempClean } from "../test-temp.mjs";

export function registerExecutionFixtures(support) {
  const { env, entryUrl, launcher, stage, json, logPath, git, repository, rustPhaseArgs, rustWrapperFixture, parseRecord } = support;
for (const code of [0, 23]) {
  test(`fixture failures report child diagnostics before cleanup (exit ${code})`, async () => {
    const original = new assert.AssertionError({ message: "original fixture assertion", actual: "actual", expected: "expected", operator: "strictEqual" });
    const cause = new Error("original cause");
    original.cause = cause;
    const stack = original.stack;
    let home, resultsPath;
    await assert.rejects(repository(async (root, observe) => {
      home = root;
      const runDir = await createRun({ root, stages: [stage("child", `console.error('x'.repeat(6000) + ' stderr-tail-marker'); process.exitCode = ${code}`)] }, env);
      resultsPath = join(runDir, "results.json");
      observe(runDir, process.execPath);
      await executeRun(runDir, env);
      throw original;
    }), (error) => {
      assert.equal(error, original);
      assert.equal(error.cause, cause);
      assert.equal(error.actual, "actual");
      assert.equal(error.expected, "expected");
      assert.ok(error.stack.startsWith(stack));
      assert.ok(error.stack.includes(`results: ${resultsPath}`));
      assert.match(error.stack, /stderr-tail-marker/u);
      assert.ok(error.message.includes(`results: ${resultsPath}`));
      assert.ok(error.message.includes(`"code":${code}`));
      assert.match(error.message, /"signal":null/u);
      assert.match(error.message, /stderr-tail-marker/u);
      assert.match(error.message, /\[truncated\]/u);
      assert.match(error.message, /"bytes":\d+,"mode":"[0-7]+"/u);
      assert.ok(error.message.length < 12_000);
      return true;
    });
    assert.equal(existsSync(home), false, "the existing fixture cleanup still runs");
  });
}

test("missing child diagnostics do not replace the fixture assertion", async () => {
  const original = new assert.AssertionError({ message: "missing result assertion", actual: 1, expected: 2, operator: "strictEqual" });
  let home;
  await assert.rejects(repository(async (root, observe) => {
    home = root;
    observe(join(root, ".git", "missing-run"), join(root, "missing-executable"));
    throw original;
  }), (error) => {
    assert.equal(error, original);
    assert.match(error.message, /child results: absent/u);
    assert.match(error.stack, /child results: absent/u);
    assert.match(error.message, /missing-executable: absent/u);
    return true;
  });
  assert.equal(existsSync(home), false);
});

describe("documentation link checker", () => {
  function checkFixture(context, sourceParts) {
    const root = fixtureHome("doc-links-", context);
    const scripts = join(root, "scripts");
    mkdirSync(scripts);
    copyFileSync(fileURLToPath(new URL("check-doc-links.mjs", entryUrl)), join(scripts, "check-doc-links.mjs"));
    writeFileSync(join(root, "README.md"), "[Kept](kept.md)\n");
    writeFileSync(join(root, "kept.md"), "# Kept\n");
    const source = join(root, ...sourceParts);
    mkdirSync(dirname(source), { recursive: true });
    writeFileSync(source, "[Dead link](missing.md)\n");
    const result = spawnSync(process.execPath, [join(scripts, "check-doc-links.mjs")], {
      cwd: root, encoding: "utf8", windowsHide: true,
    });
    assert.ifError(result.error);
    return result;
  }

  test("skips the repository's .claude/worktrees subtree", (t) => {
    const result = checkFixture(t, [".claude", "worktrees", "child", "docs", "stale.md"]);
    assert.equal(result.status, 0, result.stderr);
    assert.equal(result.stdout, "Documentation links are valid.\n");
    assert.equal(result.stderr, "");
  });

  // Retained archives may contain copied Markdown whose original links no
  // longer resolve. Run the copied real checker on each nested fixture.
  for (const sourceParts of [
    [".evidence", "notes.md"],
    [".logs", "day.md"],
    [".evidence", "abc123", "wt", "review-runs", "run-1", "notes.md"],
    [".evidence", "provisional", "run-2", "wt", "seed.md"],
    [".logs", "session", "day.md"],
    [".worktrees", "other", "README.md"],
  ]) {
    test(`skips retained dead links under ${sourceParts.slice(0, -1).join("/")}`, (t) => {
      const result = checkFixture(t, sourceParts);
      assert.equal(result.status, 0, result.stderr);
      assert.equal(result.stdout, "Documentation links are valid.\n");
      assert.equal(result.stderr, "");
    });
  }

  for (const sourceParts of [
    ["docs", "guide.md"],
    ["docs", ".evidence", "notes.md"],
    ["docs", ".logs", "day.md"],
    ["docs", ".evidence", "nested", "notes.md"],
    ["docs", ".logs", "nested", "day.md"],
    [".claude", "commands", "command.md"],
    ["worktrees", "document.md"],
    ["other", ".claude", "worktrees", "document.md"],
  ]) {
    test(`checks dead links under ${sourceParts.slice(0, -1).join("/")}`, (t) => {
      const result = checkFixture(t, sourceParts);
      assert.equal(result.status, 1, result.stderr);
      assert.equal(result.stderr, `${join(...sourceParts)}:1: missing missing.md\n`);
      assert.equal(result.stdout, "");
    });
  }
});

test("required stages preserve all gates with four independent Rust phases", () => {
  for (const platform of ["win32", "linux"]) {
    const stages = requiredStages(platform);
    assert.equal(stages.length, 12);
    assert.equal(new Set(stages.map(({ name }) => name)).size, 12);
    assert.deepEqual(stages.slice(0, 3).map(({ command, args }) => [command, ...args]), [
      ["cargo", "fmt", "--check"],
      ["cargo", "check"],
      ["cargo", "clippy", "--all-targets", "--all-features", "--", "-D", "warnings"],
    ]);
    assert.match([stages[3].command, ...stages[3].args].join(" "), platform === "win32"
      ? /pwsh.*-NoProfile.*-File scripts\/test-rust\.ps1/u
      : /scripts\/test-rust\.sh/u);
    assert.deepEqual(stages.slice(3, 7).map(({ name, args, selects }) => [name, args.slice(-2), selects]), [
      ["rust", ["--phase", "ordinary"], false],
      ["rust-scale-claims", ["--phase", "scale-claims"], true],
      ["rust-scale-roots", ["--phase", "scale-roots"], true],
      ["rust-scale-planning", ["--phase", "scale-planning"], true],
    ]);
    assert.deepEqual(stages[7].args, ["--test", "scripts/review-freeze-fingerprint.test.mjs", "scripts/test-launcher.test.mjs", "scripts/eo-cutover-values.test.mjs", "scripts/eo-cutover.test.mjs"]);
    assert.deepEqual(stages.slice(8).map(({ args }) => args), [
      ["--test", "scripts/mcp-dogfood.test.mjs"],
      ["--test", "scripts/control-dogfood.test.mjs"],
      ["--test", "scripts/parity.test.mjs"],
      ["scripts/check-doc-links.mjs"],
    ]);
  }
});

for (const platform of ["win32", "linux"]) {
  const supported = platform === "win32" ? process.platform === "win32" : process.platform !== "win32";
  test(`Rust phase wrapper (${platform}) preserves default, selected and Cargo passthrough execution`,
    { skip: !supported, timeout: 120_000 }, async () => {
      await repository(async (root) => {
        const fixture = rustWrapperFixture(root, platform);
        const invoke = (args) => {
          if (existsSync(fixture.callsPath)) unlinkSync(fixture.callsPath);
          const result = spawnSync(fixture.command, [...fixture.args, ...args],
            { cwd: root, env: fixture.env, encoding: "utf8", windowsHide: true });
          const calls = existsSync(fixture.callsPath) ? json(fixture.callsPath) : [];
          return { result, calls };
        };
        const assertCalls = (args, expected) => {
          const { result, calls } = invoke(args);
          assert.equal(result.status, 0, result.stderr);
          assert.deepEqual(calls, expected.map((args) => ({ args, cwd: root, threads: "2" })));
        };
        assertCalls([], rustPhaseArgs);
        for (const [index, phase] of ["ordinary", "scale-claims", "scale-roots", "scale-planning"].entries()) {
          assertCalls(["--phase", phase], [rustPhaseArgs[index]]);
        }
        assertCalls(["--lib", "contract", "--", "--exact"], [["--", "cargo", "test", "--lib", "contract", "--", "--exact"]]);
        assertCalls(["--PHASE", "ordinary"], [["--", "cargo", "test", "--PHASE", "ordinary"]]);
        for (const args of [["--phase"], ["--phase", "unknown"], ["--phase", "Ordinary"], ["--phase", "ordinary", "extra"]]) {
          const { result, calls } = invoke(args);
          assert.equal(result.status, 2, result.stderr);
          assert.match(result.stderr, /Usage: test-rust\.(?:ps1|sh) --phase/u);
          assert.deepEqual(calls, [], "invalid selectors never reach the harness");
        }
      });
    });

  test(`Rust phase launcher (${platform}) saves the ordinary result before a scale failure`,
    { skip: !supported, timeout: 120_000 }, async () => {
      await repository(async (root) => {
        const fixture = rustWrapperFixture(root, platform);
        const stages = requiredStages(platform).slice(3, 7);
        const runDir = await createRun({ root, full: true, stages }, fixture.env);
        const snapshot = join(root, ".git", "phase-snapshot.json");
        const result = await executeRun(runDir, { ...fixture.env,
          PHASE_RESULTS: join(runDir, "results.json"), PHASE_SNAPSHOT: snapshot });
        assert.equal(result.state, "failed");
        assert.equal(result.exitCode, 101, JSON.stringify({ error: result.error,
          stages: result.stages.map(({ name, state, code, diagnostics }) => ({ name, state, code, diagnostics })) }));
        const saved = json(join(runDir, "results.json"));
        assert.deepEqual(saved.stages.map(({ state }) => state), ["passed", "failed", "unrun", "unrun"]);
        assert.deepEqual(saved.stages[0], json(snapshot), "the already durable ordinary result remains unchanged");
        assert.equal(saved.stages[1].code, 101);
        assert.equal(saved.stages[1].tests.failed, 1);
        assert.deepEqual(json(fixture.callsPath).map(({ args }) => args), rustPhaseArgs.slice(0, 2));
        const record = parseRecord(machineRecord(result.plan, result));
        assert.deepEqual(record.stages.map(({ name, state, exit, why }) => [name, state, exit, why]), [
          ["rust", "passed", "0", undefined], ["rust-scale-claims", "failed", "101", undefined],
          ["rust-scale-roots", "skipped", "none", "not-run"], ["rust-scale-planning", "skipped", "none", "not-run"],
        ]);
      });
    });
}

test("clean pass retains logs and summarizes without passing-test lists", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("clean", "console.log('ok 1 - passing-marker')")] }, env);
    assert.match(relative(join(realpathSync.native(root), ".git", "review-runs"), runDir), /^test-[^\\/]+$/u);
    assert.ok(existsSync(join(runDir, "request.json")));
    assert.equal(json(join(runDir, "results.json")).state, "running");
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "passed");
    assert.equal(result.exitCode, 0);
    assert.equal(result.stages[0].state, "passed");
    assert.deepEqual(result.limitations, fingerprintLimitations());
    assert.match(readFileSync(logPath(runDir, result.stages[0]), "utf8"), /passing-marker/u);
    const summary = await summarize(runDir);
    assert.match(summary, /PASS/iu);
    assert.ok(summary.includes(NORMALIZATION_LIMITATION));
    assert.doesNotMatch(summary, /passing-marker/u);
    assert.ok(Buffer.byteLength(summary) < 12_288);
  });
});

test("launcher fixtures ignore inherited Git routing and configuration", async () => {
  await repository(async (root) => {
    const config = join(root, ".git", "hostile-global-config");
    writeFileSync(config, "[core]\n bare = true\n");
    const nestedEnv = { ...env, GIT_DIR: join(root, "not-a-repository"), GIT_CONFIG_GLOBAL: config,
      GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "core.bare", GIT_CONFIG_VALUE_0: "true" };
    // Start an independent test runner, not an inherited node:test worker or
    // a second owner of this process's temporary fixture root.
    delete nestedEnv.NODE_TEST_CONTEXT;
    delete nestedEnv.ENGRAM_TEST_RUN_ROOT;
    const result = spawnSync(process.execPath, ["--test", "--test-name-pattern=^clean pass", fileURLToPath(entryUrl)], {
      env: nestedEnv,
      encoding: "utf8", windowsHide: true,
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /clean pass retains logs/u);
  });
});

test("passing rows interleaved with a failure do not discard its detail", async () => {
  await repository(async (root) => {
    const output = ["thread 'fixture' panicked at test.rs:1", "ok 1 - unrelated passing-name",
      "  left: 1", " ✓ another passing-name", " right: 2", "  at test.rs:1:2"].join("\n");
    const runDir = await createRun({ root, stages: [stage("interleaved",
      `console.error(${JSON.stringify(output)}); process.exitCode = 7`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 7);
    assert.equal(result.stages[0].diagnostics.text,
      "thread 'fixture' panicked at test.rs:1\n  left: 1\n right: 2\n  at test.rs:1:2\n");
    assert.doesNotMatch(await summarize(runDir), /passing-name/u);
  });
});

test("nonzero exit is preserved and later stages remain unrun", async () => {
  await repository(async (root) => {
    const marker = join(root, ".git", "should-not-run");
    const runDir = await createRun({ root, stages: [
      stage("failure", "console.error('error: deliberate fixture failure'); process.exit(23)"),
      stage("later", `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`),
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.equal(result.exitCode, 23);
    assert.deepEqual(result.stages.map(({ state }) => state), ["failed", "unrun"]);
    assert.deepEqual(result.stages.map(({ cwd }) => cwd), [root, root]);
    assert.equal(result.stages[0].code, 23);
    assert.equal(existsSync(marker), false);
    assert.doesNotMatch(await summarize(runDir), /deliberate fixture failure/u);
    assert.match(await inspectFailure(runDir, "failure"), /deliberate fixture failure/u);
  });
});

test("failed exits with only passing output retain exit and log, not passing lists", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("interrupted",
      "console.log('ok 1 - passing-marker'); console.log('test pass_marker ... ok'); process.exitCode = 23")] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 23);
    assert.equal(result.stages[0].diagnostics.text, "");
    const summary = await summarize(runDir);
    assert.match(summary, /FAIL .*exit=23/u);
    assert.ok(summary.includes(join(runDir, "results.json")));
    assert.doesNotMatch(summary, /passing-marker|pass_marker/u);
    assert.match(readFileSync(result.stages[0].log, "utf8"), /passing-marker/u);
  });
});

test("exclusive execution admission cannot rerun or overwrite a completed run", async () => {
  await repository(async (root) => {
    const counter = join(root, ".git", "counter");
    const runDir = await createRun({ root, stages: [stage("once",
      `require('node:fs').appendFileSync(${JSON.stringify(counter)}, 'run\\n')`)] }, env);
    assert.equal((await executeRun(runDir, env)).state, "passed");
    const saved = readFileSync(join(runDir, "results.json"), "utf8");
    await assert.rejects(executeRun(runDir, env), { code: "EEXIST" });
    assert.equal(readFileSync(counter, "utf8"), "run\n");
    assert.equal(readFileSync(join(runDir, "results.json"), "utf8"), saved);
  });
});

test("unrecognized failures preserve the bounded tail and fallback explanation", async () => {
  await repository(async (root) => {
    for (const output of ["opaque unsuccessful outcome\n", `${"x".repeat(2800)}\nopaque final context\n`]) {
      const runDir = await createRun({ root, stages: [stage("opaque",
        `process.stdout.write(${JSON.stringify(output)}); process.exitCode = 19`)] }, env);
      const result = await executeRun(runDir, env);
      assert.equal(result.exitCode, 19);
      assert.deepEqual(result.stages[0].diagnostics, {
        text: output.slice(-2400),
        truncated: output.length > 2400,
        fallback: "no recognized diagnostic; bounded failure tail",
      });
    }
  });
});

}

export function registerExecutionAdmission(support) {
  const { env, entryUrl, launcher, launcherFixtureFiles, stage, json, logPath, git, repository, nodeFixtureExecutable } = support;
test("bare executable names resolve through PATH and Windows shims refuse", async () => {
  await repository(async (root, observe) => {
    const bin = join(root, ".git", "fixture bin");
    mkdirSync(bin);
    const binary = join(bin, process.platform === "win32" ? "fixture-gate.exe" : "fixture-gate");
    nodeFixtureExecutable(binary);
    const fixtureEnv = { ...env };
    for (const key of Object.keys(fixtureEnv)) if (key.toLowerCase() === "path") delete fixtureEnv[key];
    fixtureEnv.PATH = `${join(root, "absent-path-entry")}${delimiter}${bin}`;
    const runDir = await createRun({ root, stages: [{ name: "path-command", command: "fixture-gate", args: ["-e", "console.log('resolved-binary')"] }] }, env);
    observe(runDir, binary);
    const result = await executeRun(runDir, fixtureEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.stages[0].command[0], binary);
    assert.equal(readFileSync(result.stages[0].log, "utf8"), "resolved-binary\n");
    assert.notEqual(realpathSync.native(root), realpathSync.native(process.cwd()));
    const relativeEnv = { ...fixtureEnv, PATH: relative(root, bin) };
    const relativeRun = await createRun({ root, stages: [{ name: "relative-path", command: "fixture-gate", args: ["-e", "console.log('relative-binary')"] }] }, env);
    observe(relativeRun, binary);
    const relativeResult = await executeRun(relativeRun, relativeEnv);
    assert.equal(relativeResult.state, "passed");
    assert.equal(relativeResult.stages[0].command[0], binary);
    assert.equal(readFileSync(relativeResult.stages[0].log, "utf8"), "relative-binary\n");
    if (process.platform === "win32") {
      const quotedRun = await createRun({ root, stages: [{ name: "quoted-path", command: "fixture-gate", args: ["-e", "console.log('quoted-binary')"] }] }, env);
      observe(quotedRun, binary);
      const quotedResult = await executeRun(quotedRun, { ...fixtureEnv, PATH: `"${bin}"` });
      assert.equal(quotedResult.state, "passed");
      assert.equal(quotedResult.stages[0].command[0], binary);
      assert.equal(readFileSync(quotedResult.stages[0].log, "utf8"), "quoted-binary\n");
      for (const extension of ["cmd", "bat"]) {
        const shim = join(bin, `only-shim.${extension}`);
        writeFileSync(shim, "@echo must-not-run\r\n");
        const blocked = await createRun({ root, stages: [{ name: "shim", command: shim, args: [] }] }, env);
        observe(blocked, binary);
        const refused = await executeRun(blocked, fixtureEnv);
        assert.equal(refused.state, "failed");
        assert.equal(refused.stages[0].state, "unrun");
        assert.match(refused.error, /executable not found/u);
      }
    } else {
      chmodSync(binary, 0o600);
      const blocked = await createRun({ root, stages: [{ name: "permissions", command: "fixture-gate", args: [] }] }, env);
      observe(blocked, binary);
      const refused = await executeRun(blocked, fixtureEnv);
      assert.equal(refused.state, "failed");
      assert.match(refused.error, /EACCES/u);
      assert.ok(refused.error.includes(binary));
    }
  });
});

test("a Windows app execution alias that stat refuses resolves, and other stat refusals stand", () => {
  // The calls are replaced, so no real alias, file or platform is needed, and
  // the fixture directory is never created.
  const directory = join(fixtureRoot, "never-created-aliases");
  const candidate = `${resolve(process.cwd(), directory, "pwsh")}.exe`;
  const pathEnv = { PATH: directory };
  const refusal = (code) => () => { throw Object.assign(new Error(`${code}: refused, stat '${candidate}'`), { code }); };
  const file = (isFile) => () => ({ isFile: () => isFile });
  const link = (isLink) => () => ({ isSymbolicLink: () => isLink });
  const accessed = [];
  const probes = (overrides) => ({ ...fileProbes, access: (path, mode) => accessed.push([path, mode]), ...overrides });
  const find = (platform, overrides) => executable("pwsh", process.cwd(), pathEnv, { platform, probes: probes(overrides) });
  const refused = (platform, overrides, lastError) => {
    assert.throws(() => find(platform, overrides), (error) => {
      assert.match(error.message, /^executable not found: pwsh/u);
      if (lastError) assert.match(error.message, new RegExp(`last error: ${lastError}`, "u"));
      else assert.doesNotMatch(error.message, /last error/u);
      return true;
    });
  };
  // A Store-installed pwsh.exe: stat refuses with EACCES, the entry is a
  // reparse point, and an existence check passes.
  assert.equal(find("win32", { stat: refusal("EACCES"), lstat: link(true) }), candidate);
  assert.deepEqual(accessed, [[candidate, constants.F_OK]]);
  // An ordinary file still resolves as before.
  assert.equal(find("win32", { stat: file(true) }), candidate);
  // Absence, a non-file, a refused entry that is no link, and a link gone
  // between the two calls are each still refused with the existing message.
  refused("win32", { stat: refusal("ENOENT") });
  refused("win32", { stat: file(false) });
  refused("win32", { stat: refusal("EACCES"), lstat: link(false) }, "EACCES");
  refused("win32", { stat: refusal("EACCES"), lstat: refusal("ENOENT") }, "EACCES");
  // A recognized alias still has to pass the existence check.
  refused("win32", { stat: refusal("EACCES"), lstat: link(true), access: refusal("EACCES") }, "EACCES");
  // Other stat errors are not read as an alias.
  refused("win32", { stat: refusal("EPERM"), lstat: link(true) }, "EPERM");
  // Off Windows there are no app aliases, so the same answers are refused.
  refused("linux", { stat: refusal("EACCES"), lstat: link(true) }, "EACCES");
});

test("input capture failure reports once before detached admission or notification", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
    }
    // A real failing Git clean filter makes captureFingerprint refuse.
    writeFileSync(join(root, ".gitattributes"), "tracked.txt filter=refuse\n");
    writeFileSync(join(root, "tracked.txt"), "modified\n");
    git(["config", "filter.refuse.clean", `"${process.execPath.replaceAll("\\", "/")}" -e "process.exit(7)"`], root);
    git(["config", "filter.refuse.required", "true"], root);
    const result = spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), "focused",
      "--detach", "--notify", "fixture-parent", "--next-owner", "fixture-next", "--next-action", "Inspect the capture failure.",
      "--", process.execPath, "-e", ""],
      { cwd: root, env, encoding: "utf8", windowsHide: true });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /input-capture-failed/u);
    assert.match(result.stdout, /^reason: input-capture-failed$/mu);
    assert.doesNotMatch(result.stderr, /git diff|refuse.clean/u);
    assert.doesNotMatch(result.stdout, /STARTED/u);
    const runBase = join(root, ".git", "review-runs");
    const runs = readdirSync(runBase);
    assert.equal(runs.length, 1);
    const runDir = join(runBase, runs[0]);
    assert.equal(json(join(runDir, "request.json")).detached, true, "the CLI records --detach at creation");
    assert.equal(json(join(runDir, "results.json")).state, "failed");
    assert.equal(json(join(runDir, "results.json")).reason, "input-capture-failed");
    assert.equal(json(join(runDir, "notification.target.json")).nextOwner, "fixture-next");
    assert.equal(existsSync(join(runDir, "notification.message.txt")), false, "capture failure never sends automatically");
    const deliveries = [];
    await notifyRun(runDir, env, async (_command, args) => { deliveries.push(args); return { code: 0 }; });
    assert.deepEqual(deliveries.map((args) => args[args.indexOf("--to") + 1]), ["fixture-parent", "fixture-next"]);
    assert.match(readFileSync(join(runDir, "notification.message.txt"), "utf8"), /NEXT ACTION "Inspect the capture failure\."/u);
    assert.equal(existsSync(join(runDir, "execution.lock")), false);
    assert.equal(existsSync(join(runDir, "notification.message.txt")), true);
    assert.equal(existsSync(join(runDir, "launcher.log")), false);
  });
});

test("Rust zero-failure totals and TAP passing names are not failure diagnostics", async () => {
  await repository(async (root) => {
    const source = [
      "test success_test ... ok",
      "Compiling warn-macros v0.1.0",
      "test result: ok. 1026 passed; 0 failed; 4 ignored; 0 measured; 0 filtered out; finished in 90.11s",
      "# Subtest: reports an error and warning correctly",
      "ok 1 - reports an error and warning correctly",
      "warning: actual compiler caution",
      "test next_success ... ok",
      "test result: ok. 50 passed; 0 failed; 0 ignored",
    ].join("\n");
    const runDir = await createRun({ root, stages: [stage("rust-report", `console.log(${JSON.stringify(source)})`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "passed");
    assert.equal(result.stages[0].diagnostics.text, "warning: actual compiler caution\n");
    assert.doesNotMatch(await summarize(runDir), /1026 passed|50 passed|success_test|next_success|Subtest/u);
  });
});

test("missing command preflight prevents every stage from running", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [
      stage("would-pass"),
      { name: "missing", command: join(root, "missing-executable"), args: [] },
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.deepEqual(result.stages.map(({ state }) => state), ["unrun", "unrun"]);
    assert.match(await summarize(runDir), /reason: preflight-failed/u);
  });
});

test("passing Vitest rows cannot consume diagnostics before a genuine later failure", async () => {
  await repository(async (root) => {
    const passing = Array.from({ length: 100 }, (_, index) =>
      ` \u001b[32m✓\u001b[39m fixture.test.ts > handles error and warning passing-marker-${index} 2ms`);
    const failure = [" FAIL fixture.test.ts > real failure", "Error: actual failure marker", "  at fixture.test.ts:5:7"];
    const source = [...passing, ...failure, " ✓ fixture.test.ts > another passing-marker 1ms"].join("\n");
    const runDir = await createRun({ root, stages: [stage("vitest-report",
      `console.log(${JSON.stringify(source)}); process.exitCode = 1`)] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.exitCode, 1);
    assert.equal(result.stages[0].diagnostics.text, `${failure.join("\n")}\n`);
    assert.equal(result.stages[0].diagnostics.truncated, false);
    assert.doesNotMatch(await summarize(runDir), /passing-marker/u);
    assert.match(readFileSync(result.stages[0].log, "utf8"), /passing-marker-99/u);
  });
});

test("explicit binary prerequisites fail before running stages", async () => {
  await repository(async (root) => {
    const fixtureEnv = { ...env, ENGRAM_LAUNCHER_FIXTURE_BIN: join(root, "missing-binary") };
    const runDir = await createRun({ root, stages: [stage("unused")], requiredBinaryEnv: ["ENGRAM_LAUNCHER_FIXTURE_BIN"] }, fixtureEnv);
    const result = await executeRun(runDir, fixtureEnv);
    assert.equal(result.state, "failed");
    assert.equal(result.stages[0].state, "unrun");
    assert.match(await summarize(runDir), /reason: preflight-failed/u);
  });
});

test("a successful binary prerequisite is recorded before stages and CLI summary agrees", async () => {
  await repository(async (root) => {
    const fixtureEnv = { ...env, ENGRAM_LAUNCHER_FIXTURE_BIN: process.execPath };
    const runDir = await createRun({ root, stages: [stage("clean")],
      requiredBinaryEnv: ["ENGRAM_LAUNCHER_FIXTURE_BIN"] }, fixtureEnv);
    const result = await executeRun(runDir, fixtureEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.preflight.length, 1);
    assert.equal(result.preflight[0].name, "ENGRAM_LAUNCHER_FIXTURE_BIN");
    assert.equal(result.preflight[0].code, 0);
    assert.equal(readFileSync(result.preflight[0].log, "utf8").trim(), process.version);
    const summary = spawnSync(process.execPath, [launcher, "summary", runDir],
      { env, encoding: "utf8", windowsHide: true });
    assert.equal(summary.status, 0);
    assert.equal(summary.stdout, await summarize(runDir));
  });
});

test("summary of a silent failing preflight retains its exit and log header", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    // Rendering fixture, not claimed execution evidence. A silent failed
    // probe needs a header even when there is no diagnostic body to render.
    const result = json(join(runDir, "results.json"));
    const log = join(runDir, "preflight-silent.log");
    writeFileSync(log, "");
    Object.assign(result, { state: "failed", exitCode: 1, ended: new Date().toISOString(),
      preflight: [{ name: "silent", code: 13, log, diagnostics: { text: "", truncated: false } }] });
    writeFileSync(join(runDir, "results.json"), JSON.stringify(result));
    const summary = await summarize(runDir);
    assert.match(summary, /^FAIL/u);
    assert.match(summary, /preflight: total=1 passed=0 failed=1/u);
    assert.match(summary, /unrun=1/u);
    assert.ok((await inspectFailure(runDir, "preflight:silent")).includes(JSON.stringify(log)));
  });
});

test("runtime spawn exceptions persist terminal failure and leave later stages unrun", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [
      { name: "invalid-argument", command: process.execPath, args: ["\0"] }, stage("later"),
    ] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.match(result.error, /null bytes/iu);
    assert.equal(result.stages[0].error, result.error);
    assert.deepEqual(result.stages.map(({ state }) => state), ["failed", "unrun"]);
    assert.equal(json(join(runDir, "results.json")).state, "failed");
    assert.doesNotMatch(await summarize(runDir), /null bytes/iu);
    assert.ok((await inspectFailure(runDir, "invalid-argument")).includes(result.error));
  });
});

test("large diagnostic output is bounded in results while the complete log survives", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("large",
      "require('node:fs').writeSync(2, 'error: ' + 'x'.repeat(2 * 1024 * 1024) + '\\nTAIL-MARKER\\n'); process.exitCode = 4") ] }, env);
    const result = await executeRun(runDir, env);
    const entry = result.stages[0];
    assert.equal(result.exitCode, 4);
    assert.equal(entry.diagnostics.truncated, true);
    assert.ok(Buffer.byteLength(entry.diagnostics.text) < 12_288);
    assert.ok(statSync(logPath(runDir, entry)).size > 2 * 1024 * 1024);
    assert.match(readFileSync(logPath(runDir, entry), "utf8"), /TAIL-MARKER/u);
    const summary = await summarize(runDir);
    assert.ok(Buffer.byteLength(summary) < 12_288);
    assert.doesNotMatch(summary, /TAIL-MARKER|error: x/u);
    assert.match(await inspectFailure(runDir, "large"), /truncated/u);
  });
});

test("source drift before execution refuses to run the captured input", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("unused")] }, env);
    writeFileSync(join(root, "tracked.txt"), "changed after capture\n");
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.equal(result.stages[0].state, "unrun");
    assert.match(result.error, /^input drift before execution; no stages run\n/u);
    assert.match(result.error, /^Current Git status.*$/mu);
    assert.match(result.error, /^AM tracked\.txt$/mu);
  });
});

test("source drift during execution cannot be reported as a passing run", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, stages: [stage("changes-source",
      "require('node:fs').writeFileSync('tracked.txt', 'changed during execution\\n')")] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.match(result.error, /^input drift: results do not validate the current source\n/u);
    assert.match(result.error, /^Current Git status.*$/mu);
    assert.match(result.error, /^AM tracked\.txt$/mu);
  });
});

describe("an input that changed only by new untracked paths after every stage passed", () => {
  const writes = (...files) => files.map(([path, text]) =>
    `require('node:fs').writeFileSync(${JSON.stringify(path)}, ${JSON.stringify(text)});`).join(" ");
  const run = async (root, source, setup = () => {}) => {
    setup(root);
    const runDir = await createRun({ root, stages: [stage("makes-files", source)] }, env);
    return { runDir, result: await executeRun(runDir, env) };
  };

  test("an unchanged input passes with its fingerprint matched", async () => {
    await repository(async (root) => {
      const { runDir, result } = await run(root, "");
      assert.equal(result.state, "passed");
      assert.equal(result.after, result.before);
      assert.equal(result.inputCheck, undefined);
      assert.match(await summarize(runDir), /^input: matched;/mu);
    });
  });

  test("new untracked paths alone are reported as such, not as a pass or an input change", async () => {
    await repository(async (root) => {
      const { runDir, result } = await run(root, writes(["debris-a.log", "a\n"], ["debris-b.log", "b\n"]));
      assert.equal(result.state, "failed");
      assert.equal(result.exitCode, 1);
      assert.equal(result.reason, "new-untracked-only");
      assert.notEqual(result.after, result.before);
      assert.deepEqual(result.stages.map(({ state, code }) => [state, code]), [["passed", 0]]);
      assert.deepEqual(result.inputCheck, { outcome: "new-untracked-only", newPaths: ["debris-a.log", "debris-b.log"],
        totalNewPaths: 2, omittedPaths: 0, admittedInput: join(runDir, "admitted-input.json"),
        afterInput: join(runDir, "after-input.json") });
      assert.equal(json(join(runDir, "admitted-input.json")).fingerprint, result.before);
      assert.equal(json(join(runDir, "after-input.json")).fingerprint, result.after);
      const summary = await summarize(runDir);
      assert.match(summary, /^FAIL .* exit=1$/mu);
      assert.match(summary, /^reason: new-untracked-only$/mu);
      assert.match(summary, /^input: mismatch;/mu);
      assert.match(summary, /^input-change: new-untracked-only; every stage passed and only new untracked paths appeared; total=2 listed=2 omitted=0;/mu);
      const request = json(join(runDir, "request.json"));
      assert.deepEqual(completionMetadata(request, result).blocker, { reason: "runner-failed", exit: 1 });
      // A result that claims a pass names the same classification as its reason.
      assert.equal(completionMetadata(request, { ...result, state: "passed" }).blocker.basis, "new-untracked-only");
      assert.match(machineRecord(json(join(runDir, "request.json")), result), / overall=failed scope=focused stages=1 reason=new-untracked-only$/mu);
    });
  });

  test("more new untracked paths than the listing holds are counted, with the rest left out", async () => {
    await repository(async (root) => {
      const source = "for (let i = 0; i < 201; i += 1) require('node:fs').writeFileSync('debris-' + String(i).padStart(3, '0') + '.log', 'x');";
      const { runDir, result } = await run(root, source);
      assert.equal(result.reason, "new-untracked-only");
      assert.equal(result.inputCheck.totalNewPaths, 201);
      assert.equal(result.inputCheck.newPaths.length, 200);
      assert.equal(result.inputCheck.omittedPaths, 1);
      assert.equal(result.inputCheck.newPaths[0], "debris-000.log");
      assert.equal(result.inputCheck.newPaths.at(-1), "debris-199.log");
      // The complete captures still hold every path.
      assert.equal(json(join(runDir, "after-input.json")).untracked.length, 201);
      assert.match(await summarize(runDir), /^input-change: new-untracked-only; .* total=201 listed=200 omitted=1;/mu);
    });
  });

  for (const [what, source, setup] of [
    ["a changed tracked file", writes(["debris.log", "x\n"], ["tracked.txt", "changed\n"])],
    ["a changed staged index", `${writes(["debris.log", "x\n"])} require('node:child_process').execFileSync('git', ['update-index', '--chmod=+x', 'tracked.txt']);`],
    ["a changed pre-existing untracked file", writes(["debris.log", "x\n"], ["existing.txt", "rewritten\n"]),
      (root) => writeFileSync(join(root, "existing.txt"), "existing\n")],
    ["a pre-existing ignored file made visible by an exclude change",
      `${writes(["debris.log", "x\n"])} require('node:fs').writeFileSync('.git/info/exclude', '');`,
      (root) => {
        mkdirSync(join(root, ".git", "info"), { recursive: true });
        writeFileSync(join(root, ".git", "info", "exclude"), "secret.log\n");
        writeFileSync(join(root, "secret.log"), "already here\n");
      }],
    ["a moved HEAD",`${writes(["debris.log", "x\n"])} require('node:child_process').execFileSync('git', ['-c', 'user.name=Engram Test', '-c', 'user.email=engram-test@example.invalid', '-c', 'maintenance.auto=false', 'commit', '--quiet', '--allow-empty', '-m', 'moved']);`],
  ]) {
    test(`new untracked paths beside ${what} are an input change`, async () => {
      await repository(async (root) => {
        const { runDir, result } = await run(root, source, setup);
        assert.equal(result.reason, "input-changed");
        assert.equal(result.inputCheck, undefined);
        assert.match(result.error, /^input drift: results do not validate the current source\n/u);
        const request = json(join(runDir, "request.json"));
        assert.deepEqual(completionMetadata(request, result).blocker, { reason: "runner-failed", exit: 1 });
        assert.equal(completionMetadata(request, { ...result, state: "passed" }).blocker.basis, "input-changed");
      });
    });
  }

  test("new ignored paths leave the input matched", async () => {
    await repository(async (root) => {
      const { result } = await run(root, writes(["ignored.log", "x\n"]), (fixture) => {
        writeFileSync(join(fixture, ".gitignore"), "ignored.log\n");
        git(["add", ".gitignore"], fixture);
      });
      assert.equal(result.state, "passed");
      assert.equal(result.after, result.before);
    });
  });

  test("new untracked paths after a failed stage stay an input change", async () => {
    await repository(async (root) => {
      const { result } = await run(root, `${writes(["debris.log", "x\n"])} process.exitCode = 3;`);
      assert.equal(result.reason, "input-changed");
      assert.deepEqual(result.stages.map(({ state }) => state), ["failed"]);
      assert.equal(result.inputCheck, undefined);
    });
  });

  test("a stage that rewrites the saved input capture cannot make a tracked change look like debris", async () => {
    // The stage edits a tracked file, saves a capture of that edited state as
    // the run's input.json, then adds a new file. Measured against the saved
    // file, only the new file would differ; measured against the input the
    // run admitted, the tracked file changed too.
    const fingerprintModule = pathToFileURL(fileURLToPath(new URL("./review-freeze-fingerprint.mjs", entryUrl))).href;
    const source = "const fs = require('node:fs'), path = require('node:path');"
      + " const runs = require('node:child_process').execFileSync('git', ['rev-parse', '--path-format=absolute', '--git-path', 'review-runs'], { encoding: 'utf8' }).trim();"
      + " const run = path.join(runs, fs.readdirSync(runs).find((name) => name.startsWith('test-')));"
      + ` fs.writeFileSync('tracked.txt', 'changed\\n'); import(${JSON.stringify(fingerprintModule)}).then((m) => {`
      + " fs.writeFileSync(path.join(run, 'input.json'), JSON.stringify(m.captureFingerprint('.')));"
      + " fs.writeFileSync('debris.log', 'x\\n'); });";
    await repository(async (root) => {
      const { runDir, result } = await run(root, source);
      assert.equal(json(join(runDir, "input.json")).untracked.length, 0, "the stage replaced the saved capture");
      assert.equal(result.reason, "input-changed");
      assert.equal(result.inputCheck, undefined);
    });
  });
});

}

export function registerExecutionEnvironment(support) {
  const { env, entryUrl, launcher, git, repository } = support;
test("fixture removal refuses paths outside its root and deletes inside it", (t) => {
  const home = fixtureHome("engram-guard-", t);
  const target = join(fileURLToPath(new URL("..", entryUrl)), "target");
  assert.ok(realpathSync.native(home).startsWith(realpathSync.native(target)),
    `fixtures must lie in this repository's target/: ${home}`);
  const root = join(home, "root");
  const inside = join(root, "inside");
  mkdirSync(join(inside, "nested"), { recursive: true });
  const sibling = join(home, "sibling");
  mkdirSync(sibling);
  // Shares the root's name as a string prefix but is not below it.
  const collision = join(home, "root-evil");
  mkdirSync(collision);
  for (const refused of [sibling, join(root, "..", "sibling"), collision, root, dirname(fixtureRoot)]) {
    assert.throws(() => removeFixturePath(refused, root), /Refusing to delete/u, refused);
  }
  assert.throws(() => removeFixturePath(dirname(fixtureRoot)), /Refusing to delete/u);
  assert.ok(existsSync(sibling) && existsSync(collision) && existsSync(inside));

  // A link inside the root that leads outside it is refused, and so is
  // anything reached through it.
  writeFileSync(join(sibling, "kept.txt"), "outside the root");
  const link = join(root, "link");
  symlinkSync(sibling, link, process.platform === "win32" ? "junction" : "dir");
  for (const refused of [link, join(link, "kept.txt")]) {
    assert.throws(() => removeFixturePath(refused, root), /must not be a link/u, refused);
  }
  assert.ok(existsSync(join(sibling, "kept.txt")));

  removeFixturePath(inside, root);
  assert.equal(existsSync(inside), false);
  assert.ok(existsSync(root));
  // A path that is already gone is not an error.
  removeFixturePath(inside, root);
});

test("fixture removal is anchored to this repository's target/tmp/engram", (t) => {
  // Set up the fixture first: it re-checks target/ for links, so the probe
  // below is never written through one.
  const home = fixtureHome("engram-anchor-", t);
  // A victim inside this repository but outside target/tmp/engram. A caller
  // that passes the path's own parent as its root must still be refused.
  const target = join(fileURLToPath(new URL("..", entryUrl)), "target");
  const probe = join(target, `guard-probe-${randomUUID().slice(0, 8)}`);
  const victim = join(probe, "victim");
  const inner = join(victim, "inner");
  mkdirSync(inner, { recursive: true });
  writeFileSync(join(victim, "kept.txt"), "outside the anchor");
  try {
    for (const [root, refused] of [[probe, victim], [victim, inner]]) {
      assert.throws(() => removeFixturePath(refused, root), /Refusing to delete/u, refused);
    }
    assert.ok(existsSync(join(victim, "kept.txt")) && existsSync(inner));

    // A junction above the given root: root and path read as below the
    // anchor but really lie outside it.
    const linked = join(home, "linked");
    symlinkSync(probe, linked, process.platform === "win32" ? "junction" : "dir");
    assert.throws(() => removeFixturePath(join(linked, "victim", "inner"), join(linked, "victim")), /must not be a link/u);
    assert.ok(existsSync(inner));
    (process.platform === "win32" ? rmdirSync : unlinkSync)(linked);

    // The anchor itself is not a fixture root.
    assert.throws(() => removeFixturePath(home, dirname(fixtureRoot)), /Refusing to delete with a fixture root outside/u);
    assert.ok(existsSync(home));
  } finally {
    // One entry at a time: no recursive delete outside the guard.
    unlinkSync(join(victim, "kept.txt"));
    rmdirSync(inner);
    rmdirSync(victim);
    rmdirSync(probe);
  }
});

test("a linked target/ is refused before anything is created at its destination", async () => {
  await repository(async (root) => {
    const scripts = join(root, "scripts");
    mkdirSync(scripts);
    copyFileSync(fileURLToPath(new URL("test-temp.mjs", entryUrl)), join(scripts, "test-temp.mjs"));
    const elsewhere = join(root, "elsewhere");
    mkdirSync(elsewhere);
    const linkedTarget = join(root, "target");
    symlinkSync(elsewhere, linkedTarget, process.platform === "win32" ? "junction" : "dir");
    try {
      const auditEnv = { ...env };
      delete auditEnv.ENGRAM_TEST_RUN_ROOT;
      const result = spawnSync(process.execPath, [join(scripts, "test-temp.mjs"), "--", process.execPath, "-e", ""],
        { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
      assert.notEqual(result.status, 0, "a linked target/ must be refused");
      assert.match(result.stderr, /must not be a link/u);
      assert.deepEqual(readdirSync(elsewhere), [], "nothing may be created at the link's destination");
    } finally {
      (process.platform === "win32" ? rmdirSync : unlinkSync)(linkedTarget);
    }
  });
});

test("fixture creation re-checks the product chain for links on every call", async () => {
  await repository(async (root) => {
    const scripts = join(root, "scripts");
    mkdirSync(scripts);
    copyFileSync(fileURLToPath(new URL("test-temp.mjs", entryUrl)), join(scripts, "test-temp.mjs"));
    const victim = join(root, "victim");
    mkdirSync(victim);
    // Import first, then swap target/tmp for a link, then ask for a fixture.
    const driver = join(scripts, "driver.mjs");
    writeFileSync(driver, [
      'import { renameSync, symlinkSync } from "node:fs";',
      'import { join } from "node:path";',
      'import { fixtureHome } from "./test-temp.mjs";',
      `const root = ${JSON.stringify(root)};`,
      'renameSync(join(root, "target", "tmp"), join(root, "target", "tmp-real"));',
      `symlinkSync(${JSON.stringify(victim)}, join(root, "target", "tmp"), process.platform === "win32" ? "junction" : "dir");`,
      'fixtureHome("engram-swap-");',
    ].join("\n"));
    const link = join(root, "target", "tmp");
    try {
      const auditEnv = { ...env };
      delete auditEnv.ENGRAM_TEST_RUN_ROOT;
      const result = spawnSync(process.execPath, [driver], { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
      assert.notEqual(result.status, 0, "fixtureHome must refuse a product chain that became a link");
      assert.match(result.stderr, /must not be a link/u);
      assert.deepEqual(readdirSync(victim), [], "nothing may be created through the link");
    } finally {
      // Remove only the link itself; the fixture cleanup refuses any link.
      try { (process.platform === "win32" ? rmdirSync : unlinkSync)(link); } catch (error) { if (error.code !== "ENOENT") throw error; }
    }
  });
});

test("the run audit removes its empty run root only while no link leads to it", async () => {
  // Each case swaps one step for a link to an empty run directory outside
  // the copied helper's target/tmp/engram, then runs the end-of-run audit.
  for (const [swap, message] of [["tmp", /Test fixture product root must not be a link/u], ["run", /Test fixture root must not be a link/u]]) {
    await repository(async (root) => {
      const scripts = join(root, "scripts");
      mkdirSync(scripts);
      copyFileSync(fileURLToPath(new URL("test-temp.mjs", entryUrl)), join(scripts, "test-temp.mjs"));
      const victim = join(root, "victim");
      const driver = join(scripts, "driver.mjs");
      writeFileSync(driver, [
        'import { mkdirSync, renameSync, symlinkSync, writeFileSync } from "node:fs";',
        'import { basename, join } from "node:path";',
        'import { assertTempClean, fixtureRoot } from "./test-temp.mjs";',
        `const root = ${JSON.stringify(root)};`,
        `const victim = ${JSON.stringify(victim)};`,
        'const victimRun = join(victim, "engram", basename(fixtureRoot));',
        "mkdirSync(victimRun, { recursive: true });",
        'writeFileSync(join(root, "victim-run.txt"), victimRun);',
        'const kind = process.platform === "win32" ? "junction" : "dir";',
        `if (${JSON.stringify(swap)} === "tmp") {`,
        '  renameSync(join(root, "target", "tmp"), join(root, "target", "tmp-real"));',
        '  symlinkSync(victim, join(root, "target", "tmp"), kind);',
        "} else {",
        '  renameSync(fixtureRoot, `${fixtureRoot}-real`);',
        "  symlinkSync(victimRun, fixtureRoot, kind);",
        "}",
        "assertTempClean([]);",
      ].join("\n"));
      const tmp = join(root, "target", "tmp");
      try {
        const auditEnv = { ...env };
        delete auditEnv.ENGRAM_TEST_RUN_ROOT;
        const result = spawnSync(process.execPath, [driver], { cwd: root, env: auditEnv, encoding: "utf8", windowsHide: true });
        assert.notEqual(result.status, 0, `the audit must refuse a linked ${swap} step`);
        assert.match(result.stderr, message);
        const victimRun = readFileSync(join(root, "victim-run.txt"), "utf8");
        assert.ok(existsSync(victimRun), `the empty run directory behind the link must survive: ${victimRun}`);
      } finally {
        // Remove only the links themselves; the fixture cleanup refuses any link.
        const isLink = (path) => { try { return lstatSync(path).isSymbolicLink(); } catch { return false; } };
        const candidates = isLink(tmp) ? [tmp] : readdirSync(join(tmp, "engram")).map((name) => join(tmp, "engram", name));
        for (const path of candidates.filter(isLink)) (process.platform === "win32" ? rmdirSync : unlinkSync)(path);
      }
    });
  }
});

test("the Node fixture run root leaves Windows path headroom", { skip: process.platform !== "win32" }, () => {
  // Engram stores in Node fixtures put SQLite files about 106 characters below
  // the run root; keep that under the 260-character path limit.
  assert.ok(fixtureRoot.length <= 130, `fixture run root is too long (${fixtureRoot.length}): ${fixtureRoot}`);
});

test("git in a fixture without its own repository cannot climb into this checkout", (t) => {
  const home = fixtureHome("engram-ceiling-", t);
  const result = spawnSync("git", ["rev-parse", "--show-toplevel"],
    { cwd: home, env: process.env, encoding: "utf8", windowsHide: true });
  assert.notEqual(result.status, 0, `git found a repository above the fixture: ${result.stdout}`);
  assert.match(result.stderr, /not a git repository/u);
});

// The Rust gate's Unix entry point takes its thread count and its
// file-descriptor limit from the host through scripts/test-rust-host.sh. No
// gate host is a small, refusing or misreporting one, so the tests below run
// those functions in a POSIX shell against stubbed hosts.

// A shell counts only when it runs a command successfully. On Windows the one
// installed beside Git comes first: a PATH there often holds Git's cmd
// directory without its usr/bin, and a bare name is looked up in the current
// directory before PATH.
function posixShell({ platform = process.platform, run = spawnSync, exists = existsSync } = {}) {
  const candidates = [];
  if (platform === "win32") {
    const execPath = run("git", ["--exec-path"], { encoding: "utf8" });
    if (!execPath.error && execPath.status === 0) {
      const beside = join(execPath.stdout.trim(), "..", "..", "..", "usr", "bin", "sh.exe");
      if (exists(beside)) candidates.push(beside);
    }
  }
  candidates.push("sh");
  const shell = candidates.find((candidate) => {
    const probe = run(candidate, ["-c", ":"]);
    return !probe.error && probe.status === 0;
  });
  assert.ok(shell, "these tests need a POSIX sh: the one installed with Git on Windows, or one on PATH");
  return shell;
}

function runHostFunction(harness, variables, name, { replaceEnvironment = false } = {}) {
  const result = spawnSync(posixShell(), ["-c", harness], { encoding: "utf8", env: { ...(replaceEnvironment ? {} : process.env),
    HOST_FUNCTIONS: fileURLToPath(new URL("./test-rust-host.sh", entryUrl)), ...variables } });
  assert.equal(result.error, undefined, name);
  assert.equal(result.stderr, "", name);
  assert.equal(result.status, 0, name);
  return result.stdout.trim();
}

test("a shell is chosen only when it runs, and Git's own comes first on Windows", () => {
  // Git for Windows keeps its programs in <root>/mingw64/libexec/git-core and its shell in <root>/usr/bin.
  const execPath = "C:/Git/mingw64/libexec/git-core\n";
  const beside = join("C:/Git", "usr", "bin", "sh.exe");
  const host = ({ git = { status: 0, stdout: execPath }, installed = true, working = [] }) => ({
    run: (command, args) => {
      if (args[0] === "--exec-path") return command === "git" ? git : { status: 1, stdout: "" };
      assert.deepEqual(args, ["-c", ":"], `unexpected probe of ${command}`);
      return working.includes(command) ? { status: 0 } : { status: 1 };
    },
    exists: (path) => installed && path === beside,
  });
  assert.equal(posixShell({ platform: "win32", ...host({ working: [beside, "sh"] }) }), beside);
  assert.equal(posixShell({ platform: "win32", ...host({ working: ["sh"] }) }), "sh", "a failing Git shell is passed over");
  assert.equal(posixShell({ platform: "win32", ...host({ installed: false, working: [beside, "sh"] }) }), "sh");
  assert.equal(posixShell({ platform: "win32", ...host({ git: { status: 1, stdout: "" }, working: [beside, "sh"] }) }), "sh");
  assert.equal(posixShell({ platform: "win32", ...host({ git: { status: 1, stdout: execPath }, working: [beside, "sh"] }) }), "sh",
    "a failed Git answer is not used, whatever it printed");
  assert.equal(posixShell({ platform: "win32", ...host({ git: { error: new Error("ENOENT") }, working: ["sh"] }) }), "sh");
  assert.equal(posixShell({ platform: "linux", ...host({ working: [beside, "sh"] }) }), "sh", "Git's shell is a Windows fallback only");
  assert.throws(() => posixShell({ platform: "win32", ...host({ working: [] }) }), /need a POSIX sh/u, "a shell that starts and fails is not a shell");
  assert.throws(() => posixShell({ platform: "linux", run: () => ({ error: new Error("ENOENT") }), exists: () => false }), /need a POSIX sh/u);
});

// Stubs refuse any call but the ones the functions may make, and report it on
// descriptor 3, the test's stderr: the functions discard the stderr and status
// of what they call. Like real hosts, the stubs complain on stderr when they
// fail, so an empty stderr also shows that the functions keep a host's
// complaints to themselves.
test("fd soft-limit step-down asks for smaller limits largest first and never leaves a host below the previous target", () => {
  const harness = `set -eu
exec 3>&2
. "$HOST_FUNCTIONS"
requested=
sysctl() {
  if [ "$#" -ne 2 ] || [ "$1" != -n ] || [ "$2" != kern.maxfilesperproc ]; then
    echo "unexpected call: sysctl $*" >&3
    return 1
  fi
  if [ -n "$REPORTED" ]; then echo "$REPORTED"; return 0; fi
  echo "sysctl: cannot stat /proc/sys/kern/maxfilesperproc: No such file or directory" >&2
  return 1
}
ulimit() {
  if [ "$#" -ne 3 ] || [ "$1" != -S ] || [ "$2" != -n ]; then
    echo "unexpected call: ulimit $*" >&3
    exit 98
  fi
  requested="$requested $3"
  if [ "$3" -le "$ACCEPTED" ]; then current_soft_limit=$3; return 0; fi
  echo "ulimit: open files: cannot modify limit: Invalid argument" >&2
  return 1
}
current_soft_limit=$INHERITED
desired_soft_limit=$DESIRED
if raise_fd_soft_limit; then outcome=kept; else outcome=refused; fi
echo "$outcome final=$current_soft_limit requested=$requested"`;
  // [inherited limit, desired limit, maximum the host reports, largest limit the host accepts]
  const cases = [
    ["the host accepts the target", [1024, 16384, "", 1048576], "kept final=16384 requested= 16384"],
    ["a reported maximum below the target is asked for second", [256, 16384, "10240", 10240], "kept final=10240 requested= 16384 10240"],
    ["a reported maximum above the target is never requested", [256, 16384, "61440", 10000], "kept final=4096 requested= 16384 4096"],
    ["a reported maximum equal to the target is asked for once", [256, 10240, "10240", 100], "refused final=256 requested= 10240 4096"],
    ["no reported maximum falls back to the previous target", [256, 16384, "", 4096], "kept final=4096 requested= 16384 4096"],
    ["a reported maximum below the previous target is asked for after it", [256, 16384, "2048", 4096], "kept final=4096 requested= 16384 4096"],
    ["the smallest candidate is the last resort", [256, 16384, "2048", 2048], "kept final=2048 requested= 16384 4096 2048"],
    ["a reported maximum equal to the previous target is asked for once", [256, 16384, "4096", 100], "refused final=256 requested= 16384 4096"],
    ["a host that accepts nothing keeps the inherited limit and reports failure", [256, 16384, "", 100], "refused final=256 requested= 16384 4096"],
    ["a target equal to the previous one is asked for once", [256, 4096, "", 100], "refused final=256 requested= 4096"],
    ["a target below the previous one is never exceeded", [256, 1024, "", 512], "refused final=256 requested= 1024"],
    ["a target and reported maximum below the previous one are asked for once", [256, 2048, "2048", 100], "refused final=256 requested= 2048"],
    ["an inherited limit above the previous target is kept", [5000, 16384, "", 4096], "kept final=5000 requested= 16384"],
    ["a non-numeric reported maximum is left out", [256, 16384, "n/a", 100], "refused final=256 requested= 16384 4096"],
    ["a zero reported maximum is left out", [256, 16384, "0", 100], "refused final=256 requested= 16384 4096"],
  ];
  for (const [name, [inherited, desired, reported, accepted], expected] of cases) {
    assert.equal(runHostFunction(harness, { INHERITED: String(inherited), DESIRED: String(desired), REPORTED: reported,
      ACCEPTED: String(accepted) }, name), expected, name);
  }
});

test("default test threads are the available processors, at most eight, whatever OpenMP is told", () => {
  // Each source prints its value, or is absent when the value is empty. The
  // nproc stub honours both OpenMP variables as the real one does: the thread
  // count replaces the processor count and the thread limit caps it.
  const harness = `set -eu
exec 3>&2
. "$HOST_FUNCTIONS"
absent() { echo "$1: command not found" >&2; return 127; }
nproc() {
  if [ "$#" -ne 0 ]; then echo "unexpected call: nproc $*" >&3; return 1; fi
  if [ -z "$NPROC" ]; then absent nproc; return 127; fi
  count=\${OMP_NUM_THREADS:-$NPROC}
  if [ -n "\${OMP_THREAD_LIMIT:-}" ] && [ "$OMP_THREAD_LIMIT" -lt "$count" ]; then count=$OMP_THREAD_LIMIT; fi
  echo "$count"
}
getconf() {
  if [ "$#" -ne 1 ] || [ "$1" != _NPROCESSORS_ONLN ]; then echo "unexpected call: getconf $*" >&3; return 1; fi
  if [ -z "$GETCONF" ]; then absent getconf; return 127; fi
  echo "$GETCONF"
}
sysctl() {
  if [ "$#" -ne 2 ] || [ "$1" != -n ] || [ "$2" != hw.ncpu ]; then echo "unexpected call: sysctl $*" >&3; return 1; fi
  if [ -z "$SYSCTL" ]; then absent sysctl; return 127; fi
  echo "$SYSCTL"
}
default_test_threads`;
  // [nproc, getconf, sysctl, OMP_NUM_THREADS, OMP_THREAD_LIMIT]
  const cases = [
    ["a large host is capped at eight", ["24", "24", "", "", ""], "8"],
    ["exactly eight processors", ["8", "", "", "", ""], "8"],
    ["a small host uses what it has", ["4", "16", "", "", ""], "4"],
    ["affinity to one processor", ["1", "16", "", "", ""], "1"],
    ["getconf answers where nproc is absent", ["", "2", "", "", ""], "2"],
    ["sysctl answers where both are absent", ["", "", "6", "", ""], "6"],
    ["four where nothing answers", ["", "", "", "", ""], "4"],
    ["four where the answer is not a number", ["many", "16", "", "", ""], "4"],
    ["four where the answer is zero", ["0", "16", "", "", ""], "4"],
    ["an OpenMP thread count does not lower the gate's", ["24", "", "", "1", ""], "8"],
    ["an OpenMP thread limit does not lower the gate's", ["24", "", "", "", "1"], "8"],
    ["an OpenMP thread count does not raise a small host's", ["2", "", "", "16", ""], "2"],
  ];
  for (const [name, [nproc, getconf, sysctl, threadCount, threadLimit], expected] of cases) {
    // The gate host's own OpenMP settings must not decide a case.
    const { OMP_NUM_THREADS: _count, OMP_THREAD_LIMIT: _limit, ...inherited } = process.env;
    const variables = { ...inherited, NPROC: nproc, GETCONF: getconf, SYSCTL: sysctl };
    if (threadCount) variables.OMP_NUM_THREADS = threadCount;
    if (threadLimit) variables.OMP_THREAD_LIMIT = threadLimit;
    assert.equal(runHostFunction(harness, variables, name, { replaceEnvironment: true }), expected, name);
  }
});

// The record a host reads. Its grammar is a contract with that host; these
// tests hold the launcher to it, and to never giving a count that the runners'
// own summaries do not give.
}
