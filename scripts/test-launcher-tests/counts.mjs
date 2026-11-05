import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { basename, delimiter, dirname, isAbsolute, join } from "node:path";
import test, { after } from "node:test";
import { fileURLToPath } from "node:url";
import { commandKind, completionMetadata, countTests, createRun, diagnostics, executable, executeRun, inspectFailure, machineRecord, requiredStages, selectsTests, startDetached, summarize } from "../test-launcher.mjs";

export function registerCountTests(support) {
  const { env, entryUrl, launcher, launcherFixtureFiles, stage, json, git, repository, rustWrapperFixture, nodeFixtureExecutable, parseRecord, directoryBytes } = support;
// The rule the host applies to a record, as agreed with it.
const countsAsPassedTests = ({ state, stages }) => state === "passed"
  && stages.every((stage) => stage.state !== "failed" && stage.state !== "interrupted")
  && stages.filter(({ kind }) => kind === "test").every((stage) =>
    stage.state === "passed" && stage.executed !== "unknown" && stage.failed === "0")
  && stages.some((stage) => stage.kind === "test" && Number(stage.executed) >= 1);
const libtest = (passed, failed, ignored, filteredOut, verdict = failed ? "FAILED" : "ok") =>
  `running ${passed + failed + ignored} tests\ntest result: ${verdict}. ${passed} passed; ${failed} failed; ${ignored} ignored; 0 measured; ${filteredOut} filtered out; finished in 1.25s\n`;
const nodeSummary = (mark, { tests, pass, fail = 0, cancelled = 0, skipped = 0, todo = 0 }) =>
  [`tests ${tests}`, "suites 0", `pass ${pass}`, `fail ${fail}`, `cancelled ${cancelled}`, `skipped ${skipped}`,
    `todo ${todo}`, "duration_ms 12.5"].map((line) => `${mark} ${line}\n`).join("");
const printing = (name, kind, text, exit = 0) => ({ name, kind, command: process.execPath,
  args: ["-e", `process.stdout.write(${JSON.stringify(text)}); process.exitCode = ${exit}`] });
async function recordOf(root, stages, options = {}) {
  const runDir = await createRun({ root, stages, ...options }, env);
  const result = await executeRun(runDir, env);
  return { runDir, result, record: parseRecord(machineRecord(result.plan, result)) };
}
async function counted(root, text, options) {
  const log = join(root, `${randomUUID()}.log`);
  writeFileSync(log, text);
  try { return await countTests(log, options); } finally { unlinkSync(log); }
}
const complete = (executed, passed, failed, ignored, runners, filteredOut) =>
  ({ executed, passed, failed, ignored, runners, filteredOut, failures: failed });
const cargoFrame = (text, marker = "     Running unittests src/lib.rs (target/debug/deps/fixture)") => `${marker}\n${text}`;

function directCargoFixture(root, observe) {
  const bin = join(root, ".git", "cargo fixture");
  mkdirSync(bin);
  const binary = join(bin, process.platform === "win32" ? "cargo.exe" : "cargo");
  // An executable with Cargo's command shape emits a retained transcript.
  // Selection, execution, log parsing and record generation remain unmodified.
  nodeFixtureExecutable(binary);
  for (const name of ["test", "+stable", "run", "nextest"]) writeFileSync(join(root, name),
    "process.stdout.write(process.env.FIXTURE_TRANSCRIPT); process.exitCode = Number(process.env.FIXTURE_EXIT);\n");
  const runnerEnv = { ...env };
  for (const key of Object.keys(runnerEnv)) if (key.toLowerCase() === "path") delete runnerEnv[key];
  runnerEnv.PATH = `${bin}${delimiter}${process.env.PATH ?? process.env.Path ?? ""}`;
  return { binary, async run(text, { command = "cargo", args = ["test", "--lib", "fixture"], code = 0 } = {}) {
    const runDir = await createRun({ root, stages: [{ name: "rust", kind: "test", command, args }] }, runnerEnv);
    observe(runDir, binary);
    const result = await executeRun(runDir, { ...runnerEnv, FIXTURE_TRANSCRIPT: text, FIXTURE_EXIT: String(code) });
    return { result, record: parseRecord(machineRecord(result.plan, result)) };
  } };
}

test("direct Cargo execution selects outer frames for filtered counts, not generic wrappers", async () => {
  await repository(async (root, observe) => {
    const fixture = directCargoFixture(root, observe);
    const text = cargoFrame(libtest(17, 0, 0, 2471)
      .replace("tests\n", "tests\nrunning 1 test\nrunning 1 test\n"),
    "     Running unittests src\\lib.rs (target\\debug\\deps\\fixture.exe)");
    for (const [command, args, framed] of [
      ["cargo", ["test", "--lib", "backup::target::tests"], true],
      [fixture.binary, ["test", "--lib", "backup::target::tests"], true],
      ["cargo", ["+stable", "test", "--lib", "backup::target::tests"], true],
      ["cargo", ["test", "--workspace", "--all-targets"], true],
      ["cargo", ["run", "--", "test"], false],
      ["cargo", ["nextest", "run"], false],
      [process.execPath, ["test"], false],
    ]) {
      const { result, record } = await fixture.run(text, { command, args });
      assert.equal(result.state, "passed");
      assert.equal(record.stages[0].exit, "0");
      if (framed) {
        assert.deepEqual(result.stages[0].tests, complete(17, 17, 0, 0, 1, 2471));
        assert.deepEqual([record.stages[0].executed, record.stages[0].passed, record.stages[0].ignored,
          record.stages[0].runners, record.stages[0].filtered], ["17", "17", "0", "1", "yes"]);
      } else {
        assert.equal(record.stages[0].executed, "unknown");
        assert.equal(record.stages[0].why, "incomplete-summary");
      }
    }
  });
});

test("direct Cargo execution preserves frame refusals, doctests and failure outcomes", async () => {
  await repository(async (root, observe) => {
    const fixture = directCargoFixture(root, observe);
    const outer = libtest(3, 0, 0, 7);
    const frames = cargoFrame(outer) + cargoFrame(libtest(2, 0, 0, 4))
      + cargoFrame(libtest(1, 0, 0, 0).repeat(2), "   Doc-tests fixture");
    const summed = await fixture.run(frames, { args: ["test", "--workspace"] });
    assert.deepEqual(summed.result.stages[0].tests, complete(7, 7, 0, 0, 4, 11));
    // Quiet Cargo omits frame markers; a complete summary alone cannot own a frame.
    for (const option of ["-q", "--quiet"]) {
      const quiet = await fixture.run(outer, { args: ["test", option] });
      assert.deepEqual([quiet.record.stages[0].state, quiet.record.stages[0].exit,
        quiet.record.stages[0].executed, quiet.record.stages[0].why],
      ["passed", "0", "unknown", "incomplete-summary"]);
    }
    for (const [text, code, state, why, failures] of [
      [outer, 0, "passed", "incomplete-summary", 0],
      [cargoFrame("running 3 tests\n"), 0, "passed", "incomplete-summary", 0],
      [cargoFrame(outer.replace("tests\n", "tests\nrunning 3 tests\n")), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame(outer + outer), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame(outer.replace("3 passed", "2 passed")), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame(outer.replace("3 passed", "9007199254740992 passed")), 0, "passed", "malformed-summary", 0],
      [cargoFrame("running 2 tests\n" + libtest(1, 0, 0, 0), "   Doc-tests fixture"), 0, "passed", "ambiguous-summary", 0],
      [cargoFrame("running 3 tests\n" + libtest(0, 1, 0, 0) + outer.split("\n")[1] + "\n"),
        0, "failed", "ambiguous-summary", 1],
      [cargoFrame(outer), 101, "failed", undefined, 0],
    ]) {
      const { result, record } = await fixture.run(text, { code });
      assert.deepEqual([result.stages[0].state, result.stages[0].code, result.stages[0].signal], [state, code, null]);
      assert.deepEqual([record.stages[0].state, record.stages[0].exit, record.stages[0].why], [state, String(code), why]);
      assert.equal(result.stages[0].tests.failures, failures);
      if (why) assert.equal(record.stages[0].executed, "unknown");
      else assert.equal(record.stages[0].executed, "3");
    }
  });
});

test("Cargo frames preserve outer counts beside killed children without summaries", async () => {
  await repository(async (root) => {
    const outer = libtest(5, 0, 1, 0).replace("tests\n", "tests\nrunning 1 test\nrunning 1 test\n");
    for (const marker of ["     Running unittests src/lib.rs (target/debug/deps/fixture)",
      "     Running tests/contract.rs (C:\\build dir\\fixture.exe)"]) {
      const text = `${cargoFrame(outer, marker)}${cargoFrame(libtest(2, 0, 0, 17))}${cargoFrame(libtest(0, 0, 0, 4))}`;
      for (const styled of [text, `\x1b[32m${text.replaceAll("\n", "\r\n")}\x1b[0m`]) {
        assert.deepEqual(await counted(root, styled, { cargoFrames: true }), complete(7, 7, 0, 1, 3, 21));
        assert.equal((await counted(root, styled)).executed, "unknown", "Cargo-looking text alone never opts in");
      }
    }
  });
});

test("Cargo Doc-tests count the captured mixed edition-2024 rustdoc runners", async () => {
  await repository(async (root) => {
    const transcript = readFileSync(new URL("./fixtures/mixed-edition-doctests.txt", entryUrl), "utf8");
    assert.equal(transcript.match(/^\s*Doc-tests .+$/gmu).length, 1);
    assert.equal(transcript.match(/^running 1 test$/gmu).length, 2);
    assert.equal(transcript.match(/^test result: ok\./gmu).length, 2);
    assert.deepEqual(await counted(root, transcript, { cargoFrames: true }), complete(2, 2, 0, 0, 2, 0));
    // cargo test --doc does not run unit tests; an ordinary Cargo stage does.
    const ordinary = cargoFrame(libtest(0, 0, 0, 0)) + transcript;
    assert.deepEqual(await counted(root, ordinary, { cargoFrames: true }), complete(2, 2, 0, 0, 3, 0));
  });
});

test("Cargo Doc-tests accept sequential pairs, including zero runners and frame transitions", async () => {
  await repository(async (root) => {
    const pairs = libtest(0, 0, 0, 2) + libtest(2, 0, 1, 3) + libtest(2, 0, 0, 4);
    const text = cargoFrame(pairs, "   Doc-tests fixture") + cargoFrame(libtest(1, 0, 0, 5));
    for (const styled of [text, `\x1b[32m${text.replaceAll("\n", "\r\n")}\x1b[0m`, text.trimEnd()]) {
      assert.deepEqual(await counted(root, styled, { cargoFrames: true }), complete(5, 5, 0, 1, 4, 14));
    }
    assert.deepEqual(await counted(root, cargoFrame(libtest(0, 0, 0, 0).repeat(2), "   Doc-tests fixture"),
      { cargoFrames: true }), complete(0, 0, 0, 0, 2, 0));
  });
});

test("Cargo Doc-tests invalidate the whole frame for overlapping or unfinished pairs", async () => {
  await repository(async (root) => {
    const pair = libtest(3, 0, 0, 0), summary = pair.split("\n")[1];
    const max = Number.MAX_SAFE_INTEGER;
    const cases = [
      ["", "incomplete-summary"],
      [summary + "\n", "incomplete-summary"],
      ["running 3 tests\n", "incomplete-summary"],
      [pair + "running 1 test\n", "incomplete-summary"],
      [pair + summary + "\n", "ambiguous-summary"],
      ["running 3 tests\nrunning 1 test\n" + summary + "\n", "ambiguous-summary"],
      ["running 3 tests\nrunning 3 tests\n" + summary + "\n", "ambiguous-summary"],
      ["running 3 tests\n" + libtest(1, 0, 0, 0) + summary + "\n", "ambiguous-summary"],
      [pair + pair.replace("3 passed", "2 passed"), "ambiguous-summary"],
      [pair + pair.replace("0 measured", "1 measured"), "benchmarks"],
      [pair + "running invalid tests\n", "malformed-summary"],
      [pair + "running 1 test\ntest result: ok. 1 passed;", "malformed-summary"],
      [pair + pair.replace("running 3", "running 9007199254740992"), "malformed-summary"],
      [pair + pair.replace("3 passed", "9007199254740992 passed"), "malformed-summary"],
      [pair + "running " + "0".repeat(20_000) + "1 test\n", "malformed-summary"],
      [pair + "test result: " + "x".repeat(20_000) + "\n", "malformed-summary"],
      [pair + nodeSummary("#", { tests: 1, pass: 1 }), "ambiguous-summary"],
      [pair + "# tests invalid\n", "malformed-summary"],
      [pair + "# tests " + "0".repeat(20_000) + "1\n", "malformed-summary"],
      [libtest(max, 0, 0, 0) + libtest(1, 0, 0, 0), "malformed-summary"],
      [libtest(max, 0, 0, 0) + libtest(0, 1, 0, 0), "malformed-summary"],
      [libtest(0, 0, 0, max) + libtest(0, 0, 0, 1), "malformed-summary"],
    ];
    for (const [body, why] of cases) {
      for (const next of ["", cargoFrame(libtest(1, 0, 0, 0))]) {
        const separator = next && body && !body.endsWith("\n") ? "\n" : "";
        const result = await counted(root, cargoFrame(body, "   Doc-tests fixture") + separator + next, { cargoFrames: true });
        assert.equal(result.executed, "unknown", body.slice(0, 200));
        assert.equal(result.why, why, body.slice(0, 200));
        assert.equal(result.runners, next ? 1 : 0,
          "a later defect discards every earlier pair in its frame: " + body.slice(0, 200));
        for (const field of ["passed", "failed", "ignored"]) assert.equal(Object.hasOwn(result, field), false);
      }
    }
    const failing = pair + "running 3 tests\n" + libtest(0, 1, 0, 0) + summary + "\n";
    assert.deepEqual(await counted(root, cargoFrame(failing, "   Doc-tests fixture"), { cargoFrames: true }),
      { executed: "unknown", why: "ambiguous-summary", runners: 0, filteredOut: 0, failures: 1 });
    assert.deepEqual(await counted(root, cargoFrame(pair + nodeSummary("#", { tests: 1, pass: 0, fail: 1 }),
      "   Doc-tests fixture"), { cargoFrames: true }),
      { executed: "unknown", why: "ambiguous-summary", runners: 0, filteredOut: 0, failures: 1 });
  });
});

test("Cargo frames refuse incomplete or ambiguous ownership and retain readable failures", async () => {
  await repository(async (root) => {
    const outer = libtest(3, 0, 0, 0);
    const summary = outer.split("\n")[1];
    const cases = [
      ["running 3 tests\nrunning 1 test\n" + summary + "\n", "incomplete-summary"],
      [cargoFrame("running 3 tests\n"), "incomplete-summary"],
      [cargoFrame("running 3 tests\n") + cargoFrame(outer), "incomplete-summary"],
      [cargoFrame("") + cargoFrame(outer), "incomplete-summary"],
      [cargoFrame("running 3 tests\nrunning 3 tests\n" + summary + "\n"), "ambiguous-summary"],
      [cargoFrame("running 3 tests\nrunning 3 tests\n" + summary + "\n" + summary + "\n"), "ambiguous-summary"],
      [cargoFrame(outer + summary + "\n"), "ambiguous-summary"],
      [cargoFrame("running 3 tests\n" + libtest(1, 0, 0, 0) + summary + "\n"), "ambiguous-summary"],
      [cargoFrame(outer + libtest(1, 0, 0, 0)), "ambiguous-summary"],
      [cargoFrame(outer + "running 1 test\n"), "ambiguous-summary"],
      [cargoFrame(outer.replace("3 passed", "2 passed")), "ambiguous-summary"],
      [cargoFrame(outer.replace("0 measured", "1 measured")), "benchmarks"],
      [cargoFrame(outer.replace("3 passed", "9007199254740992 passed")), "malformed-summary"],
      [cargoFrame(outer.replace("running 3", "running 9007199254740992")), "malformed-summary"],
      [cargoFrame("running invalid tests\n" + summary + "\n"), "malformed-summary"],
      [cargoFrame(outer, "     Running malformed") , "malformed-summary"],
      [cargoFrame(outer, "   Doc-tests"), "malformed-summary"],
      [cargoFrame(outer, "     Running " + "x".repeat(20_000)), "malformed-summary"],
      [cargoFrame("running " + "0".repeat(20_000) + "3 tests\n" + summary + "\n"), "malformed-summary"],
      [cargoFrame(outer.replace("3 passed", "0".repeat(20_000) + "3 passed")), "malformed-summary"],
    ];
    for (const [text, why] of cases) {
      const result = await counted(root, text, { cargoFrames: true });
      assert.equal(result.executed, "unknown", text.slice(0, 200));
      assert.equal(result.why, why, text.slice(0, 200));
      assert.equal(Object.hasOwn(result, "passed"), false);
    }
    const childFailure = cargoFrame("running 3 tests\n" + libtest(0, 1, 0, 0) + summary + "\n");
    assert.deepEqual(await counted(root, childFailure, { cargoFrames: true }),
      { executed: "unknown", why: "ambiguous-summary", runners: 0, filteredOut: 0, failures: 1 });
    // Every field can be safe while the sum across confirmed runners is not.
    const max = Number.MAX_SAFE_INTEGER;
    for (const options of [undefined, { cargoFrames: true }]) {
      const wrap = (text) => options ? cargoFrame(text) : text;
      for (const text of [wrap(libtest(max, 0, 0, 0)) + wrap(libtest(0, 1, 0, 0)),
        wrap(libtest(max, 0, 0, 0)) + wrap(libtest(1, 0, 0, 0))]) {
        const result = await counted(root, text, options);
        assert.equal(result.executed, "unknown");
        assert.equal(result.why, "malformed-summary");
      }
    }
  });
});

test("Cargo phase execution preserves native exits, failure masking and count records", { timeout: 120_000 }, async () => {
  await repository(async (root) => {
    const fixture = rustWrapperFixture(root, process.platform === "win32" ? "win32" : "linux");
    const ordinary = requiredStages()[3];
    const outer = libtest(3, 0, 1, 0);
    const nested = cargoFrame(outer.replace("tests\n", "tests\nrunning 1 test\nrunning 1 test\n"));
    const failingChild = cargoFrame("running 4 tests\n" + libtest(0, 1, 0, 0) + outer.split("\n")[1] + "\n");
    const doctests = cargoFrame(libtest(1, 0, 0, 0).repeat(2), "   Doc-tests fixture");
    const doctestFailure = cargoFrame(libtest(1, 0, 0, 0) + libtest(0, 1, 0, 0), "   Doc-tests fixture");
    const doctestOverlap = cargoFrame("running 2 tests\n" + libtest(0, 1, 0, 0), "   Doc-tests fixture");
    for (const [text, code, state, why] of [[nested, 0, "passed", undefined],
      [cargoFrame("running 4 tests\n"), 0, "passed", "incomplete-summary"],
      [failingChild, 0, "failed", "ambiguous-summary"],
      [cargoFrame(libtest(0, 1, 0, 0)), 0, "failed", undefined],
      [nested, 101, "failed", undefined],
      [cargoFrame("running 4 tests\n"), 101, "failed", "incomplete-summary"],
      [doctests, 0, "passed", undefined], [doctests, 101, "failed", undefined],
      [doctestFailure, 0, "failed", undefined], [doctestFailure, 101, "failed", undefined],
      [doctestOverlap, 0, "failed", "ambiguous-summary"],
      [doctestOverlap, 101, "failed", "ambiguous-summary"]]) {
      writeFileSync(join(root, "scripts", "test-temp.mjs"), `process.stdout.write(${JSON.stringify(text)}); process.exitCode = ${code};`);
      const { result, record } = await recordOf(root, [{ ...ordinary, name: "owner-from-command" },
        printing("later", "test", libtest(1, 0, 0, 0))]);
      assert.deepEqual([result.stages[0].state, result.stages[0].code, result.stages[0].signal], [state, code, null]);
      assert.deepEqual([record.stages[0].state, record.stages[0].exit, record.stages[0].why], [state, String(code), why]);
      assert.deepEqual(json(join(dirname(result.stages[0].log), "results.json")).stages[0].tests, result.stages[0].tests);
      assert.equal(result.stages[1].state, state === "failed" ? "unrun" : "passed");
      if (why) {
        assert.equal(record.stages[0].executed, "unknown");
        assert.equal(record.stages[0].passed, undefined);
        assert.equal(countsAsPassedTests(record), false);
      } else assert.equal(record.stages[0].executed, text === nested ? "3"
        : text === doctests || text === doctestFailure ? "2" : "1");
    }
    // The same full-stage name and framed text through another command stay generic.
    const generic = await recordOf(root, [printing("rust", "test", nested)], { full: true });
    assert.equal(generic.record.stages[0].executed, "unknown");
    assert.equal(generic.record.stages[0].why, "incomplete-summary");
  });
});

test("every required stage has a kind, and only runner stages are tests", () => {
  for (const platform of ["win32", "linux"]) {
    assert.deepEqual(requiredStages(platform).map(({ name, kind }) => [name, kind]), [
      ["fmt", "lint"], ["check", "build"], ["clippy", "lint"], ["rust", "test"],
      ["rust-scale-claims", "test"], ["rust-scale-roots", "test"], ["rust-scale-planning", "test"], ["freeze", "test"],
      ["mcp", "test"], ["control", "test"], ["parity", "test"], ["docs", "other"],
    ]);
  }
});

test("a focused command is a test only when the launcher starts the runner itself", () => {
  for (const [command, args, kind] of [
    ["cargo", ["test", "--lib", "name"], "test"],
    ["cargo", ["+stable", "test"], "test"],
    ["C:\\tools\\cargo.exe", ["test"], "test"],
    ["/usr/local/bin/cargo", ["test"], "test"],
    ["cargo", ["clippy", "--all-targets"], "lint"],
    ["cargo", ["fmt", "--check"], "lint"],
    ["cargo", ["check"], "build"],
    ["cargo", ["build", "--release"], "build"],
    ["cargo", ["run", "--", "test"], "other"],
    ["cargo", ["nextest", "run"], "other"],
    [process.execPath, ["--test", "scripts/a.test.mjs"], "test"],
    ["node", ["--test-reporter=tap", "--test", "a.mjs"], "test"],
    ["node", ["--no-warnings", "--test-reporter=tap", "--test"], "test"],
    ["node", ["--test-reporter", "spec", "--test", "a.mjs"], "test"],
    // Each reporter prints a summary of its own.
    ["node", ["--test-reporter=spec", "--test-reporter-destination=stdout", "--test-reporter=tap", "--test-reporter-destination=stderr", "--test"], "other"],
    ["node", ["--test", "--test-reporter", "spec", "--test-reporter=tap", "a.mjs"], "other"],
    ["node", ["--test-reporter=tap", "--test", "a.mjs", "--", "--test-reporter=spec"], "test"],
    ["node", ["scripts/check-doc-links.mjs", "--test"], "other"],
    ["node", ["-e", "console.log('cargo test')"], "other"],
    // `--` ends Node's options: what follows is a script, whatever its name.
    ["node", ["--", "--test"], "other"],
    ["node", ["--import", "./setup.mjs", "run.mjs", "--test"], "other"],
    // An option the table does not know may take `--test` as its value.
    ["node", ["--disable-warning", "--test", "fake.mjs"], "other"],
    ["node", ["--title", "--test", "fake.mjs"], "other"],
    ["node", ["--unknown-option=1", "--test"], "other"],
    // A wrapper may run anything and end as it likes.
    ["npm", ["test"], "other"],
    ["npm", ["run", "test"], "other"],
    ["npx", ["vitest", "run"], "other"],
    ["sh", ["scripts/test-rust.sh"], "other"],
    ["pwsh", ["-NoProfile", "-File", "scripts/test-rust.ps1"], "other"],
    ["echo", ["cargo", "test"], "other"],
  ]) assert.equal(commandKind(command, args), kind, `${command} ${args.join(" ")}`);
});

test("counts are sums over complete runner summaries, including runners that ran nothing", async () => {
  await repository(async (root) => {
    assert.deepEqual(await counted(root, `${libtest(1057, 0, 4, 0)}noise\n${libtest(3, 0, 0, 1066)}`
      + `   Doc-tests engram\n${libtest(0, 0, 0, 0)}`), complete(1060, 1060, 0, 4, 3, 1066));
    assert.deepEqual(await counted(root, libtest(2, 1, 0, 0)), complete(3, 2, 1, 0, 1, 0));
    for (const [mark, opening] of [["ℹ", ""], ["#", "TAP version 13\n"]]) {
      assert.deepEqual(await counted(root, `${opening}✔ a test named tests 5\n${nodeSummary(mark, { tests: 7, pass: 4, fail: 1, skipped: 1, todo: 1 })}`),
        complete(5, 4, 1, 2, 1, 0));
    }
    // Coloured output and Windows line endings are the same summaries.
    assert.deepEqual(await counted(root, `\x1b[32m${libtest(2, 0, 0, 0).replaceAll("\n", "\r\n")}`.replace("running", "\x1b[0mrunning")),
      complete(2, 2, 0, 0, 1, 0));
    assert.deepEqual(await counted(root, libtest(0, 0, 3, 40)), complete(0, 0, 0, 3, 1, 40));
    // A line too long to read is skipped, not interpreted and not held in memory.
    assert.deepEqual(await counted(root, `${"x".repeat(200_000)}test result: ok. 9 passed; 0 failed\n${libtest(2, 0, 0, 0)}`),
      complete(2, 2, 0, 0, 1, 0));
  });
});

test("nothing but a runner's own summary is read: no line is taken for a test or a failure", async () => {
  await repository(async (root) => {
    // What a log may hold beside the summaries: failed tests' own lines, a
    // reporter's list of them, cargo's closing lines, a compiler's or another
    // tool's complaint. The summaries count; none of these lines does.
    for (const line of [
      "test storage::tests::breaks ... FAILED", "not ok 3 - breaks", "not ok 3 - pending # TODO",
      "✖ breaks (1.2ms)", "⚠ pending (0.4ms) # TODO", "✖ failing tests:",
      "✖ 2 problems (0 errors, 2 warnings)", "error: test failed, to rerun pass `--lib`",
      "error: 2 targets failed:", "error[E0425]: cannot find value", "warning: unused variable",
    ]) {
      assert.deepEqual(await counted(root, `${line}\n${libtest(2, 0, 0, 0)}${line}\n`), complete(2, 2, 0, 0, 1, 0), line);
      assert.deepEqual(await counted(root, `${line}\n${nodeSummary("ℹ", { tests: 3, pass: 2, todo: 1 })}${line}\n`),
        complete(2, 2, 0, 1, 1, 0), line);
      assert.deepEqual(await counted(root, `${line}\n`),
        { executed: "unknown", why: "no-summary", runners: 0, filteredOut: 0, failures: 0 }, line);
    }
  });
});

test("a missing, partial, contradictory or unsupported summary makes the count unknown and says why", async () => {
  await repository(async (root) => {
    const unknown = async (text, why, runners, because, failures = 0) => {
      assert.deepEqual(await counted(root, text), { executed: "unknown", why, runners, filteredOut: 0, failures }, because);
    };
    await unknown("", "no-summary", 0, "an empty log");
    await unknown("error[E0425]: cannot find value\nerror: could not compile `engram`\n", "no-summary", 0,
      "compilation failed before any runner");
    await unknown(`${libtest(5, 0, 0, 0)}running 3 tests\ntest a ... ok\n`, "incomplete-summary", 1,
      "a runner that started and never reported");
    await unknown(`running 4 tests\n${libtest(5, 0, 0, 0)}`, "incomplete-summary", 1,
      "a runner that announced tests and never reported");
    await unknown(`${nodeSummary("ℹ", { tests: 2, pass: 2 })}TAP version 13\n# Subtest: unfinished\n`, "incomplete-summary", 1,
      "a TAP stream that started and never summarized");
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 2 }).split("\n").slice(0, 4).join("\n"), "incomplete-summary", 0,
      "a node summary cut short");
    await unknown(`${libtest(5, 0, 0, 0)}${nodeSummary("#", { tests: 2, pass: 2 }).replace("# fail 0\n", "")}`, "incomplete-summary", 1,
      "a complete runner does not stand for an incomplete one");
    await unknown("test result: ok. 2 passed; 0 failed\n", "malformed-summary", 0, "a result line cut short");
    await unknown(libtest(5, 0, 0, 0).replace("5 passed", "99999999999999999999 passed"), "malformed-summary", 0,
      "a count that is not an exact integer");
    await unknown(libtest(5, 0, 0, 0).replace("0 measured", "2 measured"), "benchmarks", 0, "benchmarks are not tests");
    await unknown(nodeSummary("ℹ", { tests: 3, pass: 2, cancelled: 1 }), "cancelled-tests", 0,
      "a cancelled test may or may not have run");
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 2, cancelled: 1 }), "inconsistent-summary", 0,
      "cancelled tests are part of the total");
    await unknown(nodeSummary("ℹ", { tests: 9, pass: 2 }), "inconsistent-summary", 0, "totals that do not add up");
    for (const mark of ["ℹ", "#"]) {
      await unknown(nodeSummary(mark, { tests: 2, pass: 1, fail: 1 }).replace(`${mark} suites 0`, `${mark} suites 0.5`),
        "malformed-summary", 0, "a suite count that is not an integer", 1);
    }
    // A summary's first line that does not read, before or after a good one.
    for (const mark of ["ℹ", "#"]) {
      const good = nodeSummary(mark, { tests: 1, pass: 1 });
      for (const opener of [`${mark} tests`, `${mark} tests invalid`, `${mark} tests 1 2`, `${mark} tests ${"1".repeat(20_000)}`]) {
        await unknown(`${good}${opener}\n`, "malformed-summary", 1, `${opener.slice(0, 20)} after a summary`);
        await unknown(`${opener}\n${good}`, "malformed-summary", 1, `${opener.slice(0, 20)} before a summary`);
      }
    }
    await unknown(`${libtest(2, 0, 0, 0)}running 1 test\ntest result: ok. ${"1".repeat(20_000)} passed\n`, "malformed-summary", 1,
      "a result line too long to read");
    // Any other line may be as long as it likes.
    assert.equal((await counted(root, `${"x".repeat(20_000)}\n${libtest(2, 0, 0, 0)}tests ${"y".repeat(20_000)}\n`)).executed, 2);
    await unknown("test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n",
      "inconsistent-summary", 0, "a result that no runner announced");
    await unknown(libtest(5, 0, 0, 0).replace("running 5 tests", "running 6 tests"), "inconsistent-summary", 0,
      "a result that does not account for the tests announced");
    // The first reason met is the one given, and a later clean summary does
    // not take it back.
    await unknown(`running 3 tests\n${nodeSummary("ℹ", { tests: 9, pass: 2 })}${libtest(1, 0, 0, 0)}`, "inconsistent-summary", 1,
      "the first of several reasons");
    // Only its length is wrong with this field: read whole, it would be 2.
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 2 }).replace("ℹ pass 2", `ℹ pass ${"0".repeat(20_000)}2`),
      "malformed-summary", 0, "a summary field too long to read");
    // The bound holds wherever the line falls in the 8,192-byte chunks read.
    for (const offset of [0, 1, 4_000, 8_191, 8_192, 8_193, 16_000]) {
      const padded = (zeroes) => `${"x".repeat(offset)}\n${nodeSummary("ℹ", { tests: 2, pass: 2 })
        .replace("ℹ pass 2", `ℹ pass ${"0".repeat(zeroes)}2`)}`;
      await unknown(padded(16_384 - "ℹ pass 2".length + 1), "malformed-summary", 0, `one character over the bound at offset ${offset}`);
      assert.equal((await counted(root, padded(16_384 - "ℹ pass 2".length))).executed, 2,
        `exactly at the bound at offset ${offset}`);
    }
    // The failed count of a complete summary is kept whatever became of the rest.
    await unknown(`${libtest(1, 2, 0, 0)}running 1 test\n`, "incomplete-summary", 1, "failures beside a runner that never reported", 2);
    await unknown(nodeSummary("ℹ", { tests: 2, pass: 0, fail: 1, cancelled: 1 }), "cancelled-tests", 0,
      "failures beside cancelled tests", 1);
    await unknown(nodeSummary("#", { tests: 9, pass: 2, fail: 4 }), "inconsistent-summary", 0,
      "failures in a summary that does not add up", 4);
    await unknown(libtest(1, 3, 0, 0).replace("0 measured", "2 measured"), "benchmarks", 0, "failures beside benchmarks", 3);
    await unknown(`${nodeSummary("ℹ", { tests: 9, pass: 2 })}${libtest(2, 3, 0, 0)}`, "inconsistent-summary", 1,
      "failures after a summary that does not add up", 3);
    // Each runner keeps its failed count when that count reads, whatever
    // another field of the same summary says.
    await unknown(libtest(1, 3, 0, 0).replace("1 passed", "99999999999999999999 passed"), "malformed-summary", 0,
      "libtest failures beside a count that is not an exact integer", 3);
    // A log that cannot be read gives no count and throws nothing: the stage
    // keeps its exit.
    for (const unreadable of [join(root, "no-such.log"), root]) {
      assert.deepEqual(await countTests(unreadable), { executed: "unknown", why: "no-summary", runners: 0, filteredOut: 0, failures: 0 },
        unreadable);
    }
  });
});

test("the counts agree with what Node's own runner prints, for each reporter", async () => {
  await repository(async (root) => {
    // An independent runner, not a worker of the one running this file.
    const runnerEnv = { ...env };
    delete runnerEnv.NODE_TEST_CONTEXT;
    const run = (name, source, reporter) => {
      const file = join(root, `${name}.test.mjs`);
      writeFileSync(file, `import test from "node:test";\nimport assert from "node:assert/strict";\n${source}`);
      try {
        return spawnSync(process.execPath, ["--test", ...(reporter ? [`--test-reporter=${reporter}`] : []), file],
          { cwd: root, env: runnerEnv, encoding: "utf8", windowsHide: true });
      } finally { unlinkSync(file); }
    };
    // A failing test marked todo is todo to its runner, which exits 0. How a
    // reporter marks it differs between Node versions; its summary does not.
    const todo = `test("passes", () => {});
      test("pending", { todo: true }, () => { assert.equal(1, 2); });
      test("pending with a reason", { todo: "not written yet" }, () => { throw new Error("no"); });
      test("left out", { skip: true }, () => {});`;
    const failing = `test("passes", () => {});
      test("breaks", () => { assert.equal(1, 2); });`;
    for (const reporter of [undefined, "spec", "tap"]) {
      const passed = run("todo", todo, reporter);
      assert.equal(passed.status, 0, `${reporter}: ${passed.stderr}`);
      assert.deepEqual(await counted(root, passed.stdout), complete(1, 1, 0, 3, 1, 0), `${reporter}\n${passed.stdout}`);
      const failed = run("failing", failing, reporter);
      assert.equal(failed.status, 1, `${reporter}: ${failed.stderr}`);
      assert.deepEqual(await counted(root, failed.stdout), complete(2, 1, 1, 0, 1, 0), `${reporter}\n${failed.stdout}`);
      // Without its summary a run gives no count, and no failure is guessed.
      const cut = failed.stdout.split("\n").filter((line) => !/^(?:ℹ|#) /u.test(line)).join("\n");
      const partial = await counted(root, cut);
      assert.equal(partial.executed, "unknown", reporter);
      assert.equal(partial.failures, 0, `${reporter}\n${cut}`);
    }
    // TAP prints what a test writes behind `# `, which gives a line that
    // begins with `tests` the beginning of a summary: no count. The default
    // reporter prints the line as it is.
    const talking = `test("passes", () => { console.log("tests passed"); });`;
    for (const [reporter, expected] of [["tap", { executed: "unknown", why: "malformed-summary", runners: 1, filteredOut: 0, failures: 0 }],
      ["spec", complete(1, 1, 0, 0, 1, 0)]]) {
      const printed = run("talking", talking, reporter);
      assert.equal(printed.status, 0, `${reporter}: ${printed.stderr}`);
      assert.match(printed.stdout, reporter === "tap" ? /^# tests passed$/mu : /^tests passed$/mu, reporter);
      assert.deepEqual(await counted(root, printed.stdout), expected, `${reporter}\n${printed.stdout}`);
    }
  });
});

test("the record follows the agreed grammar for a passing run of several kinds", async () => {
  await repository(async (root) => {
    const { result, record } = await recordOf(root, [
      printing("lint-first", "lint", "test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1s\n"),
      printing("rust", "test", `${libtest(10, 0, 1, 0)}${libtest(2, 0, 0, 30)}`),
      printing("node-tests", "test", nodeSummary("ℹ", { tests: 4, pass: 4 })),
      printing("unclassified", undefined, ""),
    ], { full: true });
    assert.equal(result.state, "passed");
    const bare = { executed: undefined, passed: undefined, failed: undefined, ignored: undefined, why: undefined,
      runners: undefined, filtered: undefined };
    assert.deepEqual(record, { run: result.runId, state: "passed", scope: "full", reason: undefined, stages: [
      { run: result.runId, name: "lint-first", kind: "lint", state: "passed", exit: "0", ...bare },
      { run: result.runId, name: "rust", kind: "test", state: "passed", exit: "0", executed: "12",
        passed: "12", failed: "0", ignored: "1", why: undefined, runners: "2", filtered: "yes" },
      { run: result.runId, name: "node-tests", kind: "test", state: "passed", exit: "0", executed: "4",
        passed: "4", failed: "0", ignored: "0", why: undefined, runners: "1", filtered: "no" },
      { run: result.runId, name: "unclassified", kind: "other", state: "passed", exit: "0", ...bare },
    ] });
    assert.equal(countsAsPassedTests(record), true);
    // Counts are kept with the results, not only printed.
    assert.deepEqual(json(join(dirname(result.stages[1].log), "results.json")).stages[1].tests, complete(12, 12, 0, 1, 2, 30));
  });
});

test("a command that exits 0 passes, and a count that is not complete keeps the run from counting", async () => {
  await repository(async (root) => {
    // Each of these stages ends with exit 0 beside a stage with a good count.
    // The launcher calls none of them failed: it cannot read them. The host's
    // rule asks every test stage for a complete count, so none of these runs
    // counts as passed tests.
    for (const [text, why] of [
      ["finished\n", "no-summary"],
      [`${libtest(2, 0, 0, 0)}running 1 test\ntest breaks ... FAILED\n`, "incomplete-summary"],
      [`TAP version 13\nnot ok 1 - breaks\nTAP version 13\n${nodeSummary("#", { tests: 1, pass: 1 })}`, "incomplete-summary"],
      [`✖ breaks (1ms)\n${nodeSummary("ℹ", { tests: 1, pass: 1 }).split("\n").slice(0, 4).join("\n")}\n`, "incomplete-summary"],
      [nodeSummary("ℹ", { tests: 3, pass: 2, cancelled: 1 }), "cancelled-tests"],
    ]) {
      const { result, record } = await recordOf(root, [printing("unread", "test", text), printing("good", "test", libtest(3, 0, 0, 0))]);
      assert.equal(result.state, "passed", why);
      assert.equal(record.state, "passed", why);
      assert.deepEqual(record.stages.map(({ state, exit, executed, why: given, passed }) => [state, exit, executed, given, passed]),
        [["passed", "0", "unknown", why, undefined], ["passed", "0", "3", undefined, "3"]], why);
      assert.equal(countsAsPassedTests(record), false, why);
    }
    // Tests that ran and were all ignored or filtered out are counted, as none.
    const none = await recordOf(root, [printing("all-ignored", "test", libtest(0, 0, 6, 0)), printing("no-match", "test", libtest(0, 0, 0, 1070))]);
    assert.deepEqual(none.record.stages.map(({ executed, passed, runners, filtered }) => [executed, passed, runners, filtered]),
      [["0", "0", "1", "no"], ["0", "0", "1", "yes"]]);
    assert.equal(countsAsPassedTests(none.record), false, "no test was executed");
    // Another tool's line that looks like a failure fails nothing.
    const linted = await recordOf(root, [printing("with-lint", "test",
      `${nodeSummary("ℹ", { tests: 2, pass: 1, todo: 1 })}✖ failing tests:\n✖ pending (1ms) # TODO\n✖ 2 problems (0 errors, 2 warnings)\n`)]);
    assert.equal(linted.record.state, "passed");
    assert.equal(countsAsPassedTests(linted.record), true);
  });
});

test("complete summaries that count failed tests fail the stage whatever the command's exit", async () => {
  await repository(async (root) => {
    const { result, record } = await recordOf(root, [
      printing("masked", "test", nodeSummary("ℹ", { tests: 1, pass: 0, fail: 1 }), 0),
      printing("later", "test", libtest(1, 0, 0, 0)),
    ]);
    assert.equal(result.state, "failed");
    assert.notEqual(result.exitCode, 0);
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "stage-failed");
    // The exit the process gave is kept; the state is what the runner counted.
    assert.deepEqual(record.stages.map(({ state, exit, executed, passed, failed, why }) => [state, exit, executed, passed, failed, why]),
      [["failed", "0", "1", "0", "1", undefined], ["skipped", "none", "unknown", undefined, undefined, "not-run"]]);
    assert.match(await summarize(dirname(result.stages[0].log)), /^FAIL /u);
    assert.match(await inspectFailure(dirname(result.stages[0].log), result.stages[0].name), /the runners' summaries count 1 failed tests and the command exited 0/u);
    assert.equal(countsAsPassedTests(record), false);

    // The same when another runner of the stage left no complete summary.
    const partial = await recordOf(root, [
      printing("masked", "test", `${libtest(1, 1, 0, 0)}running 1 test\n`, 0),
      printing("later", "test", libtest(1, 0, 0, 0)),
    ]);
    assert.equal(partial.record.state, "failed");
    assert.equal(partial.record.reason, "stage-failed");
    assert.deepEqual(partial.record.stages.map(({ state, exit, executed, why, runners }) => [state, exit, executed, why, runners]),
      [["failed", "0", "unknown", "incomplete-summary", "1"], ["skipped", "none", "unknown", "not-run", "0"]]);

    // And when the summary that counts the failure counts cancelled tests too.
    const cancelled = await recordOf(root, [
      printing("masked", "test", nodeSummary("ℹ", { tests: 2, pass: 0, fail: 1, cancelled: 1 }), 0),
      printing("later", "test", libtest(1, 0, 0, 0)),
    ]);
    assert.equal(cancelled.record.reason, "stage-failed");
    assert.deepEqual(cancelled.record.stages.map(({ state, exit, executed, why }) => [state, exit, executed, why]),
      [["failed", "0", "unknown", "cancelled-tests"], ["skipped", "none", "unknown", "not-run"]]);
    assert.equal(countsAsPassedTests(cancelled.record), false);
  });
});

test("a stage that ran and was never counted is not printed as not run", () => {
  const plan = { full: false, stages: [{ name: "ran", kind: "test" }, { name: "killed", kind: "test" }, { name: "waiting", kind: "test" }] };
  const record = parseRecord(machineRecord(plan, { runId: "test-0a1b", state: "failed", reason: "launcher-error", stages: [
    { name: "ran", state: "passed", code: 0 },
    { name: "killed", state: "failed", code: null, signal: "SIGTERM" },
    { name: "waiting", state: "unrun" },
  ] }));
  assert.deepEqual(record.stages.map(({ state, exit, executed, why }) => [state, exit, executed, why]),
    [["passed", "0", "unknown", "no-summary"], ["interrupted", "none", "unknown", "no-summary"], ["skipped", "none", "unknown", "not-run"]]);
  assert.equal(countsAsPassedTests(record), false);
});

test("filtered says whether the command line or a runner selected a part of the tests", async () => {
  for (const [command, args, selects] of [
    ["cargo", ["test"], false],
    ["cargo", ["test", "--all-features", "--", "--nocapture"], false],
    ["cargo", ["test", "--jobs", "1"], false],
    ["cargo", ["test", "-j8"], false],
    ["cargo", ["test", "-j", "8"], false],
    ["cargo", ["test", "--jobs=1", "--features", "x", "--", "--test-threads", "8"], false],
    ["cargo", ["test", "root_delta"], true],
    ["cargo", ["test", "--lib"], true],
    ["cargo", ["test", "-p", "engram"], true],
    ["cargo", ["test", "-pengram"], true],
    ["cargo", ["test", "--package=engram"], true],
    // Every target, but not the doctests.
    ["cargo", ["test", "--all-targets"], true],
    ["cargo", ["test", "--", "--ignored"], true],
    ["cargo", ["test", "--", "--skip", "slow"], true],
    ["cargo", ["clippy", "--lib"], false],
    ["node", ["--test"], false],
    ["node", ["--test", "--test-reporter", "spec"], false],
    ["node", ["--test-reporter=spec", "--test", "--test-timeout=5000"], false],
    ["node", ["--test", "scripts/parity.test.mjs"], true],
    ["node", ["--test", "--test-name-pattern=record"], true],
    ["node", ["--test-name-pattern", "record", "--test"], true],
    ["node", ["scripts/check-doc-links.mjs"], false],
    // Not a test to the launcher, so it selects none.
    ["npm", ["test", "--", "--grep", "x"], false],
  ]) assert.equal(selectsTests(command, args), selects, `${command} ${args.join(" ")}`);
  await repository(async (root) => {
    // Known from the request alone, so a stage that never ran still says it.
    const runDir = await createRun({ root, stages: [
      { name: "absent", kind: "test", command: join(root, "no-such-cargo", "cargo"), args: ["test", "--lib", "name"] },
    ] }, env);
    const executed = await executeRun(runDir, env);
    const record = parseRecord(machineRecord(executed.plan, executed));
    assert.deepEqual(record.stages.map(({ state, executed: count, why, filtered }) => [state, count, why, filtered]),
      [["skipped", "unknown", "not-run", "yes"]]);
  });
  // The gate's own stages say it themselves, run or not, whatever runs them.
  for (const platform of ["win32", "linux"]) {
    const stages = requiredStages(platform);
    const record = parseRecord(machineRecord({ full: true, stages },
      { runId: "test-0a1b", state: "failed", reason: "preflight-failed", stages: stages.map(({ name }) => ({ name, state: "unrun" })) }));
    assert.deepEqual(record.stages.filter(({ kind }) => kind === "test").map(({ name, why, filtered }) => [name, why, filtered]),
      [["rust", "not-run", "no"], ["rust-scale-claims", "not-run", "yes"], ["rust-scale-roots", "not-run", "yes"],
        ["rust-scale-planning", "not-run", "yes"], ["freeze", "not-run", "yes"], ["mcp", "not-run", "yes"],
        ["control", "not-run", "yes"], ["parity", "not-run", "yes"]]);
  }
});

test("a failing stage keeps its exit and counts, and the stages after it are skipped without an exit", async () => {
  await repository(async (root) => {
    const { result, record } = await recordOf(root, [
      printing("before", "build", ""),
      printing("failing", "test", libtest(3, 2, 0, 0), 101),
      printing("after", "test", libtest(1, 0, 0, 0)),
      printing("last", "other", ""),
    ]);
    assert.equal(result.state, "failed");
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "stage-failed");
    assert.deepEqual(record.stages.map(({ state, exit, executed, failed, why }) => [state, exit, executed, failed, why]), [
      ["passed", "0", undefined, undefined, undefined], ["failed", "101", "5", "2", undefined],
      ["skipped", "none", "unknown", undefined, "not-run"], ["skipped", "none", undefined, undefined, undefined],
    ]);
  });
});

test("a run whose input changed is failed in the record although every stage passed", async () => {
  await repository(async (root) => {
    const { runDir, result, record } = await recordOf(root, [{ name: "changes-source", kind: "test", command: process.execPath, args: ["-e",
      `require('node:fs').writeFileSync('tracked.txt', 'changed\\n'); process.stdout.write(${JSON.stringify(libtest(4, 0, 0, 0))})`] }]);
    assert.equal(record.state, "failed");
    assert.equal(record.reason, "input-changed");
    assert.deepEqual(record.stages.map(({ state, executed }) => [state, executed]), [["passed", "4"]]);
    assert.equal(countsAsPassedTests(record), false);
    const summary = await summarize(runDir);
    assert.deepEqual(summary.split("\n").slice(0, 3), [
      `FAIL ${basename(runDir)} exit=${result.exitCode}`,
      `results: ${join(runDir, "results.json")}`,
      `changes-source: passed exit=0 log=${join(runDir, "changes-source.log")}`,
    ]);
    assert.match(summary, /^reason: input-changed$/mu);
    assert.equal(completionMetadata(json(join(runDir, "request.json")), result).input.comparison, "mismatch");
  });
});

test("a command that cannot be found or spawned leaves stages without an exit and says why", async () => {
  await repository(async (root) => {
    const missing = await recordOf(root, [printing("first", "test", libtest(1, 0, 0, 0)),
      { name: "absent", kind: "test", command: join(root, "no-such-program"), args: [] }]);
    assert.equal(missing.record.state, "failed");
    assert.equal(missing.record.reason, "preflight-failed");
    assert.deepEqual(missing.record.stages.map(({ state, exit, why }) => [state, exit, why]),
      [["skipped", "none", "not-run"], ["skipped", "none", "not-run"]]);

    const unspawnable = await recordOf(root, [
      { name: "invalid-argument", kind: "test", command: process.execPath, args: ["\0"] }, printing("later", "lint", "")]);
    assert.equal(unspawnable.record.state, "failed");
    assert.equal(unspawnable.record.reason, "spawn-failed");
    assert.deepEqual(unspawnable.record.stages.map(({ state, exit, why }) => [state, exit, why]),
      [["failed", "none", "not-run"], ["skipped", "none", undefined]]);

    const prerequisite = await createRun({ root, stages: [printing("unused", "test", "")], requiredBinaryEnv: ["ENGRAM_LAUNCHER_ABSENT_BIN"] }, env);
    const refused = await executeRun(prerequisite, env);
    assert.equal(parseRecord(machineRecord(refused.plan, refused)).reason, "preflight-failed");

    // A toolchain the full gate cannot find is a failed prerequisite, not a stage.
    const unstartable = { ...env, ENGRAM_LAUNCHER_FIXTURE_BIN: process.execPath, PATH: "", Path: "" };
    const probed = await createRun({ root, stages: [printing("unused", "test", "")], full: true }, env);
    const stopped = await executeRun(probed, unstartable);
    const probedRecord = parseRecord(machineRecord(stopped.plan, stopped));
    assert.equal(probedRecord.reason, "preflight-failed");
    assert.deepEqual(probedRecord.stages.map(({ state, exit }) => [state, exit]), [["skipped", "none"]]);
  });
});

test("any other refusal and a fingerprint that cannot be checked each have their own reason", async () => {
  await repository(async (root) => {
    const duplicated = await recordOf(root, [printing("twice", "test", ""), printing("twice", "test", "")]);
    assert.equal(duplicated.record.state, "failed");
    assert.equal(duplicated.record.reason, "launcher-error");
    assert.deepEqual(duplicated.record.stages.map(({ state }) => state), ["skipped", "skipped"]);

    const misnamed = await createRun({ root, stages: [printing("Upper_Case", "test", "")] }, env);
    const refused = await executeRun(misnamed, env);
    assert.match(refused.error, /lowercase letters, digits and hyphens/u);
    assert.equal(machineRecord(refused.plan, refused), "",
      "a name outside the grammar is refused before it runs and is never printed");

    const unreadable = await recordOf(root, [{ name: "breaks-git", kind: "test", command: process.execPath, args: ["-e",
      `require('node:fs').writeFileSync('.git/HEAD', 'not a ref\\n'); process.stdout.write(${JSON.stringify(libtest(2, 0, 0, 0))})`] }]);
    assert.equal(unreadable.record.state, "failed");
    assert.equal(unreadable.record.reason, "fingerprint-check-failed");
    assert.deepEqual(unreadable.record.stages.map(({ state, executed }) => [state, executed]), [["passed", "2"]]);
  });
});

test("a child that a signal ended is an interrupted stage", { skip: process.platform === "win32" }, async () => {
  await repository(async (root) => {
    const { record } = await recordOf(root, [
      { name: "killed", kind: "test", command: process.execPath, args: ["-e", "process.kill(process.pid, 'SIGKILL')"] },
      printing("later", "lint", ""),
    ]);
    assert.equal(record.state, "interrupted");
    assert.equal(record.reason, "stage-interrupted");
    assert.deepEqual(record.stages.map(({ state, exit }) => [state, exit]), [["interrupted", "none"], ["skipped", "none"]]);
  });
});

test("a stage ended by a signal is interrupted, and nothing about such a run is passed", () => {
  const request = { full: true, stages: [{ kind: "lint" }, { kind: "test" }, { kind: "test" }] };
  const record = parseRecord(machineRecord(request, { runId: "test-0a1b", state: "failed", stages: [
    { name: "fmt", state: "passed", code: 0, signal: null },
    { name: "rust", state: "failed", code: null, signal: "SIGKILL", tests: complete(7, 7, 0, 0, 1, 0) },
    { name: "mcp", state: "unrun" },
  ] }));
  assert.equal(record.state, "interrupted");
  assert.equal(record.reason, "stage-interrupted");
  assert.deepEqual(record.stages.map(({ state, exit }) => [state, exit]),
    [["passed", "0"], ["interrupted", "none"], ["skipped", "none"]]);
  assert.equal(countsAsPassedTests(record), false);
  // A reason or a why that is not a token is replaced, never printed.
  const odd = parseRecord(machineRecord({ stages: [{ kind: "test" }] },
    { runId: "test-0a1b", state: "failed", reason: "Not A Token", stages: [{ name: "only", state: "failed", code: 1,
      tests: { executed: "unknown", why: "Not A Token", runners: 0, filteredOut: 0 } }] }));
  assert.equal(odd.reason, "stage-failed");
  assert.deepEqual(odd.stages.map(({ why }) => why), ["no-summary"]);
});

test("a run in a linked worktree lives under that worktree's Git directory and prints the same record", async () => {
  await repository(async (root) => {
    const linked = join(root, "linked");
    git(["worktree", "add", "--quiet", linked], root);
    const runDir = await createRun({ root: linked, stages: [printing("unit", "test", libtest(2, 0, 1, 0))] }, env);
    assert.match(runDir.replaceAll("\\", "/"), /\/\.git\/worktrees\/linked\/review-runs\/test-[0-9a-f-]+$/u, runDir);
    const result = await executeRun(runDir, env);
    const record = parseRecord(machineRecord(result.plan, result));
    assert.equal(record.run, basename(runDir));
    assert.equal(record.state, "passed");
    assert.deepEqual(record.stages.map(({ kind, state, executed, passed, ignored }) => [kind, state, executed, passed, ignored]),
      [["test", "passed", "2", "2", "1"]]);
    assert.ok(countsAsPassedTests(record));
  });
});

test("a stage whose log cannot be read keeps its exit, gives no count and does not fail the run", async () => {
  await repository(async (root) => {
    // The stage deletes its own log and exits 0.
    const deleting = `const fs = require('node:fs'), path = require('node:path');
      process.stdout.write(${JSON.stringify(libtest(2, 0, 0, 0))});
      const runs = path.join('.git', 'review-runs');
      for (const run of fs.readdirSync(runs)) fs.rmSync(path.join(runs, run, 'gone.log'), { force: true });`;
    const runDir = await createRun({ root, stages: [{ name: "gone", kind: "test", command: process.execPath, args: ["-e", deleting] },
      printing("after", "test", libtest(1, 0, 0, 0))] }, env);
    const result = await executeRun(runDir, env);
    assert.equal(existsSync(join(runDir, "gone.log")), false, "the stage removed its log");
    const record = parseRecord(machineRecord(result.plan, result));
    assert.equal(record.state, "passed", "an unreadable log does not fail the run");
    assert.deepEqual(record.stages.map(({ name, state, exit, executed, why }) => [name, state, exit, executed, why]),
      [["gone", "passed", "0", "unknown", "no-summary"], ["after", "passed", "0", "1", undefined]]);
    assert.equal(countsAsPassedTests(record), false, "the host still asks every test stage for a count");
    assert.match(json(join(runDir, "results.json")).stages[0].diagnostics.text, /the log could not be read/u);
    for (const unreadable of [join(root, "no-such.log"), root]) {
      assert.match((await diagnostics(unreadable, true)).text, /^\[the log could not be read: [A-Z]+\]\n$/u, unreadable);
    }
  });
});

test("full Node provenance keeps the stage-kind contract", async () => {
  await repository(async (root) => {
    writeFileSync(join(root, "provenance.test.mjs"), `import test from "node:test";
test("passes", () => {});
test("skipped", { skip: true }, () => {});
`);
    git(["add", "provenance.test.mjs"], root);
    const runnerEnv = { ...env };
    delete runnerEnv.NODE_TEST_CONTEXT;
    for (const full of [false, true]) {
      const stages = [{ name: "node-tests", kind: "test", command: process.execPath,
        args: ["--test", "--test-reporter=tap", "provenance.test.mjs"] }];
      const runDir = await createRun({ root, full, stages }, runnerEnv);
      const result = await executeRun(runDir, runnerEnv);
      assert.equal(result.state, "passed", JSON.stringify(result));
      assert.equal(json(join(runDir, "request.json")).full, full);
      const saved = json(join(runDir, "results.json"));
      const entry = saved.stages[0];
      assert.equal(saved.state, "passed");
      assert.equal(saved.exitCode, 0);
      assert.equal(entry.state, "passed");
      assert.equal(entry.code, 0);
      assert.equal(entry.signal, null);
      assert.equal(Object.hasOwn(entry.tests, "runner"), !full, "full-mode counts omit the runner label");
      assert.deepEqual(entry.tests, { ...complete(1, 1, 0, 1, 1, 0), ...(!full ? { runner: "node-test" } : {}) });
    }
  });
});

test("focused Node provenance preserves mode, counts and native outcomes", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
    }
    const cases = [
      ["passing", `test("passes", () => assert.equal(process.cwd(), ${JSON.stringify(root)})); test("skipped", { skip: true }, () => {});`, "tap", 0, [1, 0, 1]],
      ["failing", `test("breaks", () => { throw new Error("fixture failure"); });`, "spec", 1, [0, 1, 0]],
      ["skipped", `test("skipped", { skip: true }, () => {});`, "tap", 0, [0, 0, 1]],
      ["unknown", `test("passes", () => {});`, "dot", 0, null],
    ];
    for (const [name, source] of cases) {
      writeFileSync(join(root, `${name}.test.mjs`), `import test from "node:test";\nimport assert from "node:assert/strict";\n${source}\n`);
    }
    git(["add", "scripts", "*.test.mjs"], root);
    const runnerEnv = { ...env };
    delete runnerEnv.NODE_TEST_CONTEXT;
    const run = (...args) => spawnSync(process.execPath, [join(root, "scripts", "test-launcher.mjs"), ...args],
      { cwd: root, env: runnerEnv, encoding: "utf8", windowsHide: true });
    for (const [name, , reporter, code, counts] of cases) {
      const launched = run("focused", "--", process.execPath, "--test", `--test-reporter=${reporter}`, `${name}.test.mjs`);
      assert.equal(launched.status, code, `${name}: ${launched.stderr}`);
      const runDir = /^STARTED (.+)$/mu.exec(launched.stdout)?.[1];
      assert.ok(runDir, launched.stdout);
      assert.equal(json(join(runDir, "request.json")).detached, false, name);
      const result = json(join(runDir, "results.json"));
      const entry = result.stages[0];
      assert.equal(entry.cwd, root, name);
      assert.equal(isAbsolute(entry.cwd), true, name);
      assert.equal(entry.code, code, name);
      assert.equal(entry.signal, null, name);
      assert.equal(result.exitCode, code, name);
      if (counts) {
        assert.equal(entry.tests.runner, "node-test", name);
        assert.deepEqual([entry.tests.passed, entry.tests.failed, entry.tests.ignored], counts, name);
        assert.equal(entry.tests.executed, counts[0] + counts[1], name);
        assert.equal(code === 0, entry.tests.failed === 0, name);
      } else {
        assert.equal(entry.tests.executed, "unknown", name);
        assert.equal(Object.hasOwn(entry.tests, "runner"), false, name);
        for (const field of ["passed", "failed", "ignored"]) assert.equal(Object.hasOwn(entry.tests, field), false, field);
      }
      const record = parseRecord(launched.stdout.slice(launched.stdout.indexOf("\ntest-launcher/v1") + 1));
      assert.equal(record.stages[0].exit, String(code), name);
      assert.equal(countsAsPassedTests(record), name === "passing", name);
    }
    const opaque = run("focused", "--", process.execPath, "-e",
      `process.stdout.write(${JSON.stringify(nodeSummary("#", { tests: 1, pass: 1 }))})`);
    assert.equal(opaque.status, 0, opaque.stderr);
    const opaqueDir = /^STARTED (.+)$/mu.exec(opaque.stdout)?.[1];
    assert.equal(Object.hasOwn(json(join(opaqueDir, "results.json")).stages[0], "tests"), false);
    // Notification does not make a foreground run detached. The fixture CLI
    // refuses the mailbox arguments; validation still finished successfully.
    const notified = run("focused", "--notify", "fixture-parent", "--", process.execPath, "--test", "passing.test.mjs");
    assert.equal(notified.status, 1);
    assert.match(notified.stderr, /notification failed; tests were NOT rerun/u);
    const notifiedDir = /^STARTED (.+)$/mu.exec(notified.stdout)?.[1];
    assert.equal(json(join(notifiedDir, "request.json")).detached, false);
    assert.equal(json(join(notifiedDir, "results.json")).stages[0].tests.runner, "node-test");
  });
});

test("detached launch refuses a foreground request without changing it", async () => {
  await repository(async (root) => {
    const runDir = await createRun({ root, notifyTo: "fixture-parent", stages: [stage("unused")] }, env);
    assert.equal(json(join(runDir, "request.json")).detached, false);
    const before = directoryBytes(runDir);
    await assert.rejects(startDetached(runDir, env), /requires a run created with detached: true/u);
    assert.deepEqual(directoryBytes(runDir), before);
    assert.equal(existsSync(join(runDir, "execution.lock")), false);
  });
});

test("the record ends the executing process's stdout, and summary and notify never print one", async () => {
  await repository(async (root) => {
    mkdirSync(join(root, "scripts"));
    for (const name of launcherFixtureFiles) {
      copyFileSync(fileURLToPath(new URL(name, entryUrl)), join(root, "scripts", name));
    }
    const copied = join(root, "scripts", "test-launcher.mjs");
    git(["add", "scripts"], root);
    const run = (...args) => spawnSync(process.execPath, [copied, ...args], { cwd: root, env, encoding: "utf8", windowsHide: true });
    // A warning on stderr and a bounded diagnostic excerpt on stdout precede it.
    const source = `console.error('warning: noise on stderr'); process.stdout.write(${JSON.stringify(libtest(3, 0, 0, 0))})`;
    const passed = run("focused", "--", process.execPath, "--test-reporter=spec", "-e", source);
    assert.equal(passed.status, 0, passed.stderr);
    const runDir = /^STARTED (.+)$/mu.exec(passed.stdout)?.[1];
    assert.ok(runDir);
    const human = await summarize(runDir);
    assert.ok(passed.stdout.includes(human), "the human summary is unchanged and still printed");
    const tail = passed.stdout.slice(passed.stdout.indexOf(human) + human.length);
    const record = parseRecord(tail);
    assert.equal(record.run, basename(runDir));
    assert.equal(record.state, "passed");
    assert.equal(record.scope, "focused");
    // `node -e` is not the test runner, whatever it prints.
    assert.deepEqual(record.stages.map(({ name, kind, executed }) => [name, kind, executed]), [["focused", "other", undefined]]);

    // A stage can write to its run directory. What the record says of the
    // stage is what the launcher read before the stage ran.
    const rewriting = `const fs = require('node:fs'), path = require('node:path');
      const runs = path.join('.git', 'review-runs');
      for (const run of fs.readdirSync(runs)) {
        const file = path.join(runs, run, 'request.json');
        const request = JSON.parse(fs.readFileSync(file, 'utf8'));
        request.full = true;
        for (const stage of request.stages) Object.assign(stage, { kind: 'test', selects: true });
        fs.writeFileSync(file, JSON.stringify(request));
      }
      process.stdout.write(${JSON.stringify(libtest(3, 0, 0, 0))});`;
    const rewritten = run("focused", "--", process.execPath, "-e", rewriting);
    assert.equal(rewritten.status, 0, rewritten.stderr);
    const unchanged = parseRecord(rewritten.stdout.slice(rewritten.stdout.indexOf("\ntest-launcher/v1") + 1));
    assert.equal(unchanged.scope, "focused");
    assert.deepEqual(unchanged.stages.map(({ kind, executed, filtered }) => [kind, executed, filtered]), [["other", undefined, undefined]]);

    const failed = run("focused", "--", process.execPath, "-e", "process.exit(7)");
    assert.equal(failed.status, 7);
    const failure = parseRecord(failed.stdout.slice(failed.stdout.lastIndexOf("\ntest-launcher/v1", failed.stdout.lastIndexOf("\ntest-launcher/v1") - 1) + 1));
    assert.equal(failure.state, "failed");
    assert.deepEqual(failure.stages.map(({ state, exit }) => [state, exit]), [["failed", "7"]]);

    const replayed = run("summary", runDir);
    assert.equal(replayed.status, 0);
    assert.equal(replayed.stdout, human);
    assert.doesNotMatch(replayed.stdout, /test-launcher\/v1/u);

    // The fixture's transport is `node`, which refuses the mailbox arguments:
    // the notification is attempted and fails, and neither the executing run
    // nor a later `notify` lets that change or repeat the record.
    const notified = run("focused", "--notify", "coordinator", "--", process.execPath, "-e", "");
    assert.equal(notified.status, 1);
    assert.match(notified.stderr, /notification failed; tests were NOT rerun/u);
    const kept = parseRecord(notified.stdout.slice(notified.stdout.indexOf("\ntest-launcher/v1") + 1));
    assert.equal(kept.state, "passed", "the record covers validation, not the notification");
    const notifiedRun = /^STARTED (.+)$/mu.exec(notified.stdout)?.[1];
    assert.doesNotMatch(readFileSync(join(notifiedRun, "notification.message.txt"), "utf8"), /test-launcher\/v1/u);
    const resent = run("notify", notifiedRun);
    assert.equal(resent.status, 1);
    assert.doesNotMatch(resent.stdout, /test-launcher\/v1/u);

    // A stage that breaks request.json cannot take the record away. Last,
    // because it breaks every run's request in this fixture.
    const breaking = `const fs = require('node:fs'), path = require('node:path');
      const runs = path.join('.git', 'review-runs');
      for (const run of fs.readdirSync(runs)) fs.writeFileSync(path.join(runs, run, 'request.json'), '{');
      process.stdout.write(${JSON.stringify(libtest(3, 0, 0, 0))});`;
    const broken = run("focused", "--", process.execPath, "-e", breaking);
    assert.equal(broken.status, 0, broken.stderr);
    const survived = parseRecord(broken.stdout.slice(broken.stdout.lastIndexOf("\ntest-launcher/v1", broken.stdout.lastIndexOf("\ntest-launcher/v1") - 1) + 1));
    assert.equal(survived.state, "passed");
    assert.deepEqual(survived.stages.map(({ state, exit }) => [state, exit]), [["passed", "0"]]);
  });
});
}
