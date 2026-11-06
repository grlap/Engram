import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { createServer } from "node:http";
import { join } from "node:path";
import test, { after } from "node:test";
import { registerTestRun, confirmDetachedRegistration, registrationLine } from "../test-run-registration.mjs";
import { createRun, executeRun, notifyRun, summarize } from "../test-launcher.mjs";

export function registerRegistrationTests(support) {
  const { env, stage, json, git, repository } = support;
const registrationAck = (request) => ({ contract: "termal-durable-test-run-registration", version: 1,
  runId: request.runId, registered: true });
const registrationQuery = (request, runDir, source = "index") => ({ source, run: {
  runId: request.runId, ownerSessionId: request.owner, initiatingSessionId: request.owner,
  registrationState: "registered", trackingDurability: "durable", trackingProvenances: ["launcherRegistered"],
  creatorPid: request.creatorPid, startedAt: request.started, detached: request.detached,
  runDir, worktree: request.root,
} });
const response = (body, status = 200) => new Response(JSON.stringify(body), { status });
const registrationEnv = { ...env, TERMAL_BASE_URL: "http://127.0.0.1:1" };

test("registration confirms the canonical paths of a mixed-case checkout", async () => {
  await repository(async (root) => {
    const checkout = join(root, "UpperCaseCheckout");
    mkdirSync(checkout);
    git(["init", "--quiet", "--template="], checkout);
    git(["config", "core.autocrlf", "false"], checkout);
    writeFileSync(join(checkout, "tracked.txt"), "baseline\n");
    git(["add", "tracked.txt"], checkout);
    const canonicalRoot = realpathSync.native(checkout);
    assert.ok(canonicalRoot.endsWith("UpperCaseCheckout"));
    let request, directory;
    const fetch = async (_url, options) => {
      if (options.method === "POST") {
        directory = JSON.parse(options.body).runDirectory;
        request = json(join(directory, "request.json"));
        return response(registrationAck(request));
      }
      const query = registrationQuery(request, realpathSync.native(directory));
      query.run.worktree = canonicalRoot;
      return response(query);
    };
    const runDir = await createRun({ root: checkout, detached: true, stages: [stage("once")] },
      registrationEnv, { fetch });
    request = json(join(runDir, "request.json"));
    assert.equal(request.registration.state, "registered");
    assert.equal(request.registration.nativeWaitReady, true);
    const confirmed = await confirmDetachedRegistration(request, runDir, registrationEnv, { fetch });
    assert.equal(confirmed.state, "registered");
    assert.equal(confirmed.nativeWaitReady, true);
  });
});

test("registration distinguishes real case-sensitive roots and run directories", async (t) => {
  await repository(async (root) => {
    const parent = join(root, "case-sensitive");
    mkdirSync(parent);
    if (process.platform === "win32") {
      const fsutil = (action) => spawnSync("fsutil.exe", ["file", action, parent,
        ...(action === "setCaseSensitiveInfo" ? ["enable"] : [])], {
        encoding: "utf8", windowsHide: true, timeout: 60_000, maxBuffer: 4_096,
      });
      for (const action of ["setCaseSensitiveInfo", "queryCaseSensitiveInfo"]) {
        const result = fsutil(action);
        // A hang fails: only fsutil being unavailable may drop this coverage.
        assert.notEqual(result.error?.code, "ETIMEDOUT", `fsutil ${action} hung`);
        if (result.error || result.status !== 0) {
          const detail = String(result.error?.message ?? [result.stdout, result.stderr].filter(Boolean).join(" "))
            .trim().slice(0, 512);
          t.skip(`case-sensitive Windows directory unavailable: ${action}: ${detail || result.status}`);
          return;
        }
      }
    }
    const worktree = join(parent, "RegistrationRoot"), otherWorktree = join(parent, "registrationroot");
    const directory = join(parent, "RunDirectory"), otherDirectory = join(parent, "rundirectory");
    mkdirSync(worktree);
    try { mkdirSync(otherWorktree); }
    catch (error) {
      if (error.code !== "EEXIST") throw error;
      t.skip(`case-sensitive filesystem unavailable on ${process.platform}: case-only sibling already exists`);
      return;
    }
    mkdirSync(directory);
    mkdirSync(otherDirectory);
    for (const [path, marker] of [[worktree, "worktree"], [otherWorktree, "other-worktree"],
      [directory, "run"], [otherDirectory, "other-run"]]) {
      writeFileSync(join(path, "marker"), marker);
      assert.equal(readFileSync(join(path, "marker"), "utf8"), marker);
    }
    assert.deepEqual(readdirSync(parent).sort(), ["RegistrationRoot", "RunDirectory", "registrationroot", "rundirectory"]);
    assert.notEqual(realpathSync.native(worktree), realpathSync.native(otherWorktree));
    assert.notEqual(realpathSync.native(directory), realpathSync.native(otherDirectory));
    assert.equal(readFileSync(join(worktree, "marker"), "utf8"), "worktree");
    assert.equal(readFileSync(join(directory, "marker"), "utf8"), "run");
    const request = { runId: randomUUID(), root: worktree, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    const fetch = (change = {}) => async (_url, options) => {
      const query = registrationQuery(request, realpathSync.native(directory));
      query.run.worktree = realpathSync.native(worktree);
      Object.assign(query.run, change);
      return response(options.method === "POST" ? registrationAck(request) : query);
    };
    const wrongRoot = await registerTestRun(request, directory, registrationEnv,
      { fetch: fetch({ worktree: realpathSync.native(otherWorktree) }) });
    assert.equal(wrongRoot.state, "unconfirmed");
    assert.equal(wrongRoot.reason, "query-identity-mismatch");
    assert.equal(wrongRoot.nativeWaitReady, false);
    request.registration = await registerTestRun(request, directory, registrationEnv, { fetch: fetch() });
    assert.equal(request.registration.state, "registered");
    const wrongDirectory = await confirmDetachedRegistration(request, directory, registrationEnv,
      { fetch: fetch({ runDir: realpathSync.native(otherDirectory) }) });
    assert.equal(wrongDirectory.state, "unconfirmed");
    assert.equal(wrongDirectory.reason, "query-identity-mismatch");
    assert.equal(wrongDirectory.nativeWaitReady, false);
  });
});

test("registration covers all launch modes before fingerprint capture through the supported HTTP contract", async () => {
  await repository(async (root) => {
    const requests = new Map(), calls = [], errors = [];
    const server = createServer(async (incoming, outgoing) => {
      try {
        calls.push([incoming.method, incoming.url]);
        let body;
        if (incoming.method === "POST") {
          const chunks = [];
          for await (const chunk of incoming) chunks.push(chunk);
          body = JSON.parse(Buffer.concat(chunks));
          assert.deepEqual(Object.keys(body).sort(), ["runDirectory", "runId"]);
          const request = json(join(body.runDirectory, "request.json"));
          assert.equal(request.runId, body.runId);
          assert.equal(request.creatorPid, process.pid);
          assert.equal(request.owner, env.TERMAL_SESSION_ID);
          assert.equal(existsSync(join(body.runDirectory, "input.json")), false);
          assert.equal(request.expectedFingerprint, undefined);
          // The capture must include a source change made during registration.
          writeFileSync(join(root, "tracked.txt"), `${request.runId}\n`);
          requests.set(request.runId, { request, directory: body.runDirectory });
          outgoing.end(JSON.stringify(registrationAck(request)));
        } else {
          const found = requests.get(incoming.url.split("/").at(-1));
          assert.ok(found);
          outgoing.end(JSON.stringify(registrationQuery(found.request, found.directory)));
        }
      } catch (error) { errors.push(error); outgoing.statusCode = 500; outgoing.end("{}"); }
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const hostEnv = { ...env, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` };
    try {
      for (const full of [false, true]) for (const detached of [false, true]) {
        const runDir = await createRun({ root, full, detached, notifyTo: "fixture-parent",
          stages: [stage("once", "console.log('ok 1 - registration fixture')")] }, hostEnv);
        const request = json(join(runDir, "request.json"));
        assert.equal(request.registration.state, "registered");
        assert.equal(request.registration.nativeWaitReady, true);
        assert.equal(json(join(runDir, "results.json")).registration.state, "registered");
        assert.equal(json(join(runDir, "input.json")).fingerprint, request.expectedFingerprint);
        assert.equal(calls.at(-2)[1], `/api/sessions/${env.TERMAL_SESSION_ID}/test-runs`);
        assert.equal(calls.at(-1)[1], `/api/test-runs/${request.runId}`);
        if (!full) {
          const before = calls.length;
          const result = await executeRun(runDir, hostEnv);
          assert.equal(result.state, "passed");
          assert.equal(result.before, request.expectedFingerprint);
          assert.equal(result.after, result.before);
          assert.equal(calls.length, before + (detached ? 1 : 0));
          assert.match(await summarize(runDir), /registration: registered; native-wait=ready; registration succeeded; host check credit decided at delivery/u);
        }
      }
      assert.deepEqual(errors, []);
    } finally { await new Promise((done) => { server.close(done); server.closeAllConnections(); }); }
  });
});

test("registration fallback preserves stage execution, terminal results and notification retry intent", async () => {
  await repository(async (root) => {
    const attempts = [];
    const runDir = await createRun({ root, detached: true, notifyTo: "fixture-parent",
      stages: [stage("once", "require('node:fs').appendFileSync('.git/registration-count', 'once')")] },
    registrationEnv, { fetch: async (_url, options) => {
      attempts.push(options.body);
      throw Object.assign(new Error("lost response"), { cause: { code: "EPIPE" } });
    } });
    assert.equal(attempts.length, 2);
    assert.equal(attempts[0], attempts[1]);
    const result = await executeRun(runDir, registrationEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.registration.state, "unconfirmed");
    assert.equal(readFileSync(join(root, ".git", "registration-count"), "utf8"), "once");
    const summary = await summarize(runDir);
    assert.match(summary, /registration: unconfirmed; native-wait=unavailable; use --notify/u);
    const sends = [];
    const send = async (_command, args) => {
      sends.push({ key: args[args.indexOf("--idempotency-key") + 1],
        message: readFileSync(args[args.indexOf("--message-file") + 1], "utf8") });
      return { code: sends.length === 1 ? 1 : 0 };
    };
    await assert.rejects(notifyRun(runDir, registrationEnv, send));
    await notifyRun(runDir, registrationEnv, send);
    assert.equal(sends.length, 2);
    assert.deepEqual(sends[0], sends[1]);
    assert.equal(sends[0].message, summary);
    assert.equal(readFileSync(join(root, ".git", "registration-count"), "utf8"), "once");
  });
});

test("registration refuses malformed or unavailable context locally without a host call", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner" };
    const forbidden = { fetch: () => assert.fail("no host call for unavailable context") };
    assert.equal((await registerTestRun(request, root, {}, forbidden)).state, "untracked");
    for (const context of [{ TERMAL_SESSION_ID: "fixture-owner" }, { ...registrationEnv, TERMAL_BASE_URL: "file:///x" },
      { ...registrationEnv, TERMAL_BASE_URL: "http://user:secret@localhost" },
      { ...registrationEnv, TERMAL_SESSION_ID: "wrong-owner" }]) {
      assert.equal((await registerTestRun(request, root, context, forbidden)).state, "unconfirmed");
    }
  });
});

test("registration never replays a definitive redirect or a refused connection and preserves fallback execution", async () => {
  await repository(async (root) => {
    const calls = [];
    const server = createServer((request, reply) => {
      calls.push([request.method, request.url]);
      reply.writeHead(307, { location: "/unexpected-target" });
      reply.end();
    });
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const hostEnv = { ...env, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` };
    try {
      const runDir = await createRun({ root, stages: [stage("once",
        "require('node:fs').appendFileSync('.git/redirect-count', 'once')")] }, hostEnv);
      const request = json(join(runDir, "request.json"));
      assert.deepEqual(calls, [["POST", `/api/sessions/${env.TERMAL_SESSION_ID}/test-runs`]]);
      assert.equal(request.registration.state, "unconfirmed");
      assert.equal(request.registration.reason, "registration-refused");
      assert.equal((await executeRun(runDir, hostEnv)).state, "passed");
      assert.equal(readFileSync(join(root, ".git", "redirect-count"), "utf8"), "once");
      let refusedCalls = 0;
      const result = await registerTestRun(request, runDir, hostEnv, { fetch: async () => {
        refusedCalls++;
        throw Object.assign(new Error("connection refused"), { cause: { code: "ECONNREFUSED" } });
      } });
      assert.equal(refusedCalls, 1);
      assert.equal(result.state, "unconfirmed");
    } finally { await new Promise((done) => { server.close(done); server.closeAllConnections(); }); }
  });
});

test("registration and detached confirmation keep short-cadence heartbeats fresh", async (t) => {
  t.mock.timers.enable({ apis: ["Date", "setInterval"], now: Date.now() });
  await repository(async (root) => {
    const hostEnv = { ...registrationEnv, ENGRAM_LAUNCHER_HEARTBEAT_MS: "1000" };
    let request, directory;
    const fetch = async (_url, options) => {
      if (options.method === "POST") {
        directory = JSON.parse(options.body).runDirectory;
        request = json(join(directory, "request.json"));
        const before = Date.parse(json(join(directory, "results.json")).heartbeat.at);
        t.mock.timers.tick(4000);
        assert.equal(Date.parse(json(join(directory, "results.json")).heartbeat.at), before + 4000);
        assert.match(await summarize(directory), /^RUNNING /u);
        return response(registrationAck(request));
      }
      return response(registrationQuery(request, directory));
    };
    const runDir = await createRun({ root, detached: true, stages: [stage("once")] }, hostEnv, { fetch });
    t.mock.method(globalThis, "fetch", async () => {
      const before = Date.parse(json(join(runDir, "results.json")).heartbeat.at);
      t.mock.timers.tick(4000);
      assert.equal(Date.parse(json(join(runDir, "results.json")).heartbeat.at), before + 4000);
      assert.match(await summarize(runDir), /^RUNNING /u);
      return response(registrationQuery(request, runDir));
    });
    const result = await executeRun(runDir, hostEnv);
    assert.equal(result.state, "passed");
    assert.equal(result.registration.state, "registered");
    const terminal = readFileSync(join(runDir, "results.json"));
    t.mock.timers.tick(4000);
    assert.deepEqual(readFileSync(join(runDir, "results.json")), terminal, "the timer stops before terminal completion");
  });
});

test("registration distinguishes durable registration from indexing and keeps ambiguous fallback sticky", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    const fetch = async (_url, options) => response(options.method === "POST"
      ? registrationAck(request) : registrationQuery(request, root, "registration"));
    request.registration = await registerTestRun(request, root, registrationEnv, { fetch });
    assert.equal(request.registration.state, "registered-but-not-indexed");
    assert.equal(request.registration.nativeWaitReady, false);
    assert.equal(registrationLine(request.registration), "registration: registered-but-not-indexed; native-wait=unavailable; use --notify; registration succeeded; host check credit decided at delivery");
    let savedDirectory;
    const runDir = await createRun({ root, stages: [stage("once")] }, registrationEnv,
      { fetch: async (_url, options) => {
        if (options.method === "POST") savedDirectory = JSON.parse(options.body).runDirectory;
        const saved = json(join(savedDirectory, "request.json"));
        return response(options.method === "POST" ? registrationAck(saved) : registrationQuery(saved, savedDirectory, "registration"));
      } });
    assert.match(await summarize(runDir), /registration: registered-but-not-indexed; native-wait=unavailable; use --notify; registration succeeded; host check credit decided at delivery/u);
    const indexed = async () => response(registrationQuery(request, root));
    assert.equal((await confirmDetachedRegistration(request, root, registrationEnv, { fetch: indexed })).nativeWaitReady, false);
    request.registration = { state: "unconfirmed", nativeWaitReady: false };
    assert.deepEqual(await confirmDetachedRegistration(request, root, registrationEnv,
      { fetch: () => assert.fail("ambiguous registration cannot be promoted by a late query") }), request.registration);
  });
});

test("registration wording preserves untracked and ordinary unconfirmed messages", () => {
  for (const state of ["untracked", "unconfirmed"]) {
    assert.equal(registrationLine({ state }), `registration: ${state}; native-wait=unavailable; use --notify; not host check credit`);
  }
});

test("registration preserves bounded POST refusal diagnostics in metadata and summary", async () => {
  await repository(async (root) => {
    let calls = 0;
    const runDir = await createRun({ root, stages: [stage("once")] }, registrationEnv,
      { fetch: async (_url, options) => {
        calls++;
        assert.equal(options.method, "POST");
        return response({ error: "Registration does not match the current work root", secret: "never-print-this-secret" }, 409);
      } });
    const registration = json(join(runDir, "request.json")).registration;
    assert.equal(calls, 1);
    assert.equal(registration.state, "unconfirmed");
    assert.equal(registration.reason, "registration-refused");
    assert.equal(registration.nativeWaitReady, false);
    assert.equal(registration.httpStatus, 409);
    assert.equal(registration.hostMessage, "Registration does not match the current work root");
    assert.deepEqual(json(join(runDir, "results.json")).registration, registration);
    assert.match(await summarize(runDir), /POST HTTP 409; host message="Registration does not match the current work root"/u);
    assert.doesNotMatch(JSON.stringify(registration) + await summarize(runDir), /never-print-this-secret/u);
    assert.equal((await executeRun(runDir, registrationEnv)).state, "passed");
    assert.deepEqual(json(join(runDir, "results.json")).registration, registration);
  });
});

test("registration refusal bodies are bounded and never dumped as diagnostics", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner" };
    for (const reply of [() => new Response(null, { status: 403 }),
      () => new Response("private raw body", { status: 403 }),
      () => response({ error: { secret: "private" }, message: "private" }, 403),
      () => response({ error: "x".repeat(65_537) }, 403)]) {
      let calls = 0;
      const result = await registerTestRun(request, root, registrationEnv, { fetch: async () => { calls++; return reply(); } });
      assert.deepEqual(result, { state: "unconfirmed", reason: "registration-refused", nativeWaitReady: false,
        httpStatus: 403, hostMessage: null });
      assert.equal(calls, 1);
      assert.match(registrationLine(result), /POST HTTP 403; host message unavailable$/u);
      assert.doesNotMatch(registrationLine(result), /private/u);
    }
    const result = await registerTestRun(request, root, registrationEnv, { fetch: async () => response({
      error: `\u001b[31mfirst\nsecond\u2028${"z".repeat(500)}`, secret: "never-print-this-secret",
      registrationState: "durabilityPending", retryAfterMs: 1,
    }, 503) });
    assert.equal(result.reason, "registration-refused");
    assert.equal(result.httpStatus, 503);
    assert.equal(result.hostMessage.length, 384);
    assert.match(result.hostMessage, /^\[31mfirst second /u);
    assert.doesNotMatch(result.hostMessage, /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u);
    assert.doesNotMatch(registrationLine(result), /never-print-this-secret/u);
    const persisted = { ...result, hostMessage: `\u001b${"y".repeat(800)}` };
    assert.ok(registrationLine(persisted).length < 600);
    assert.doesNotMatch(registrationLine(persisted), /\u001b/u);
    assert.doesNotMatch(registrationLine({ ...result, httpStatus: "503\nprivate" }), /POST HTTP/u);
  });
});

test("registration retains refusal status when its body exhausts the shared budget", async () => {
  await repository(async (root) => {
    let signal, calls = 0;
    const result = await registerTestRun({ runId: randomUUID(), root, owner: "fixture-owner" }, root, registrationEnv,
      { budgetMs: 10, fetch: async (_url, options) => {
        calls++;
        signal = options.signal;
        return new Response(new ReadableStream({ start() {} }), { status: 503 });
      } });
    assert.equal(calls, 1);
    assert.equal(signal.aborted, true);
    assert.deepEqual(result, { state: "unconfirmed", reason: "registration-timeout", nativeWaitReady: false,
      httpStatus: 503, hostMessage: null });
    assert.match(registrationLine(result), /POST HTTP 503; host message unavailable$/u);
  });
});

test("registration validates exact replies, retries only ambiguous transport, and bounds stalled requests and bodies", async () => {
  await repository(async (root) => {
    const request = { runId: randomUUID(), root, owner: "fixture-owner", creatorPid: process.pid,
      started: new Date().toISOString(), detached: true, stages: [] };
    for (const reply of [() => response({}, 503), () => response({ ...registrationAck(request), runId: "other" }),
      () => new Response("{"), () => new Response("x".repeat(65_537))]) {
      let calls = 0;
      const result = await registerTestRun(request, root, registrationEnv, { fetch: async () => { calls++; return reply(); } });
      assert.equal(result.state, "unconfirmed");
      assert.equal(calls, 1);
    }
    for (const change of [{ ownerSessionId: "other" }, { creatorPid: -1 }, { trackingDurability: "volatile" },
      { runDir: join(root, "missing") }, { detached: false }]) {
      const result = await registerTestRun(request, root, registrationEnv, { fetch: async (_url, options) => {
        const query = registrationQuery(request, root);
        Object.assign(query.run, change);
        return response(options.method === "POST" ? registrationAck(request) : query);
      } });
      assert.equal(result.state, "unconfirmed");
    }
    let calls = 0;
    const reset = Object.assign(new Error("reset"), { cause: { code: "ECONNRESET" } });
    const fetch = async (_url, options) => {
      calls++;
      if (calls === 1 || calls === 3) throw reset;
      return response(options.method === "POST" ? registrationAck(request) : registrationQuery(request, root));
    };
    request.registration = await registerTestRun(request, root, registrationEnv, { fetch });
    assert.equal(request.registration.state, "registered");
    assert.equal(calls, 4);
    request.stages.push(stage("changed"));
    assert.equal((await confirmDetachedRegistration(request, root, registrationEnv,
      { fetch: () => assert.fail("changed worker request cannot reuse registration") })).state, "unconfirmed");
    for (const stalledBody of [false, true]) {
      let signal;
      const result = await registerTestRun(request, root, registrationEnv, { budgetMs: 10,
        fetch: async (_url, options) => {
          signal = options.signal;
          return stalledBody ? new Response(new ReadableStream({ start() {} })) : await new Promise(() => {});
        } });
      assert.equal(result.state, "unconfirmed");
      assert.equal(result.reason, "registration-timeout");
      assert.equal(signal.aborted, true);
    }
  });
});
}
