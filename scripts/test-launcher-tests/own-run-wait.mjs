import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { once } from "node:events";
import { createServer } from "node:http";
import { join } from "node:path";
import test from "node:test";
import { createRun, executeRun, startDetached, summarize } from "../test-launcher.mjs";
import { registerOwnRunWait, ownRunWaitLine } from "../test-run-registration.mjs";

export function registerOwnRunWaitTests({ env, repository, stage, json, copyLauncher }) {
  const response = (body, status = 201) => new Response(JSON.stringify(body), { status });
  const hostEnv = { ...env, TERMAL_BASE_URL: "http://127.0.0.1:1" };
  const registered = (request, directory, source = "index") => ({ source, run: {
    runId: request.runId, ownerSessionId: request.owner, initiatingSessionId: request.owner,
    registrationState: "registered", trackingDurability: "durable", trackingProvenances: ["launcherRegistered"],
    creatorPid: request.creatorPid, startedAt: request.started, detached: request.detached,
    runDir: directory, worktree: request.root,
  } });
  const acknowledgement = (request, directory) => ({ revision: 1, waitId: "test-run-wait-fixture", serverInstanceId: "fixture-host",
    runIds: [request.runId], mode: "all", resumePromptQueued: false, resumeDispatchRequested: false,
    wait: { id: "test-run-wait-fixture", sessionId: request.owner, runIds: [request.runId], mode: "all",
      runs: [{ runId: request.runId, ownerSessionId: request.owner, startedAt: request.started,
        preset: request.full ? "full" : "focused", runDir: directory, worktree: request.root }] } });
  const intent = (root) => ({ runId: "test-fixture", root, detached: true, owner: env.TERMAL_SESSION_ID,
    started: new Date().toISOString(), registration: { state: "registered", hostOrigin: hostEnv.TERMAL_BASE_URL } });

  test("own-run wait accepts indexed and registration-only runs with one exact POST", async () => {
    await repository(async (root) => {
      for (const state of ["registered", "registered-but-not-indexed"]) {
        const request = intent(root);
        request.registration.state = state;
        request.registration.nativeWaitReady = state === "registered";
        let calls = 0;
        const result = await registerOwnRunWait(request, root, hostEnv, { fetch: async (url, options) => {
          calls++;
          assert.equal(url.pathname, `/api/sessions/${request.owner}/test-run-waits`);
          assert.equal(options.method, "POST");
          assert.equal(options.redirect, "manual");
          assert.deepEqual(JSON.parse(options.body), { runIds: [request.runId], mode: "all" });
          return response(acknowledgement(request, root));
        } });
        assert.equal(calls, 1);
        assert.equal(result.state, "registered");
        assert.equal(result.waitId, "test-run-wait-fixture");
      }
    });
  });

  test("own-run wait refuses invalid acknowledgements without replaying the POST", async () => {
    await repository(async (root) => {
      const request = intent(root);
      const invalid = [
        (ack) => { ack.runIds = ["another-run"]; }, (ack) => { ack.mode = "any"; },
        (ack) => { ack.wait.sessionId = "another-session"; }, (ack) => { ack.wait.id = "another-wait"; },
        (ack) => { ack.wait.runIds.push("another-run"); }, (ack) => { ack.wait.mode = "any"; },
        (ack) => { ack.wait.runs[0].runId = "another-run"; }, (ack) => { ack.wait.runs[0].runDir = join(root, "absent"); },
        (ack) => { ack.wait.runs[0].ownerSessionId = "another-session"; },
        (ack) => { ack.wait.runs[0].worktree = join(root, "absent"); },
        (ack) => { ack.wait.runs[0].preset = "full"; }, (ack) => { ack.wait.runs[0].startedAt = "other"; },
        (ack) => { ack.wait.runs.push(ack.wait.runs[0]); }, (ack) => { ack.revision = -1; },
        (ack) => { ack.waitId = "spoof\nline"; }, (ack) => { delete ack.serverInstanceId; },
        (ack) => { delete ack.resumePromptQueued; }, (ack) => { ack.resumeDispatchRequested = "false"; },
      ];
      const replies = invalid.map((change) => () => {
        const ack = acknowledgement(request, root); change(ack); return response(ack);
      });
      replies.push(() => response(acknowledgement(request, root), 200), () => new Response("{"),
        () => new Response("x".repeat(65_537)), () => { throw Object.assign(new Error("lost response"), { code: "ECONNRESET" }); });
      for (const reply of replies) {
        let calls = 0;
        const result = await registerOwnRunWait(request, root, hostEnv, { fetch: async () => { calls++; return reply(); } });
        assert.equal(calls, 1);
        assert.deepEqual([result.state, result.reason], ["unconfirmed", "test-run-wait-unconfirmed"]);
        assert.match(ownRunWaitLine(result), /a wait may already exist; do not retry automatically/u);
      }
    });
  });

  test("own-run wait retains bounded refusal diagnostics and times out stalled transport and bodies", async () => {
    await repository(async (root) => {
      const request = intent(root);
      for (const status of [307, 400, 404, 409]) {
        const result = await registerOwnRunWait(request, root, hostEnv,
          { fetch: async () => response({ error: `host\n${"x".repeat(500)}`, secret: "never print this" }, status) });
        assert.deepEqual([result.state, result.reason, result.httpStatus], ["refused", "test-run-wait-refused", status]);
        assert.equal(result.hostMessage.length, 384);
        assert.doesNotMatch(ownRunWaitLine(result), /\n|never print this/u);
      }
      for (const reply of [() => new Response("private raw text", { status: 409 }),
        () => response({ error: "x".repeat(65_537) }, 409), () => response({ error: {} }, 409)]) {
        const result = await registerOwnRunWait(request, root, hostEnv, { fetch: async () => reply() });
        assert.equal(result.hostMessage, null);
        assert.match(ownRunWaitLine(result), /host message unavailable/u);
      }
      for (const body of [false, true]) {
        let calls = 0, signal;
        const result = await registerOwnRunWait(request, root, hostEnv, { budgetMs: 10,
          fetch: async (_url, options) => {
            calls++; signal = options.signal;
            return body ? new Response(new ReadableStream({ start() {} }), { status: 503 }) : await new Promise(() => {});
          } });
        assert.deepEqual([result.state, result.reason], ["unconfirmed", "test-run-wait-unconfirmed"]);
        assert.equal(calls, 1);
        assert.equal(signal.aborted, true);
        if (body) assert.deepEqual([result.httpStatus, result.hostMessage], [503, null]);
      }
    });
  });

  test("own-run wait cannot use unavailable registration or a different host context", async () => {
    await repository(async (root) => {
      for (const change of [{ registration: { state: "unconfirmed" } }, { detached: false }, { notifyTo: "other-session" }]) {
        const result = await registerOwnRunWait({ ...intent(root), ...change }, root, hostEnv,
          { fetch: () => assert.fail("unavailable intent must not POST") });
        assert.deepEqual([result.state, result.reason], ["unavailable", "own-run-registration-unconfirmed"]);
      }
      for (const context of [{}, { TERMAL_SESSION_ID: env.TERMAL_SESSION_ID }, { ...hostEnv, TERMAL_SESSION_ID: "other" },
        { ...hostEnv, TERMAL_BASE_URL: "http://127.0.0.1:2" }]) {
        assert.equal((await registerOwnRunWait(intent(root), root, context,
          { fetch: () => assert.fail("unavailable context must not POST") })).state, "unavailable");
      }
    });
  });

  test("own-run worker confirmation and wait failures run no stage and never announce readiness", async (t) => {
    await repository(async (root) => {
      let transport;
      t.mock.method(globalThis, "fetch", (...args) => transport(...args));
      for (const failure of ["initial", "confirmation", "refused", "acknowledgement"]) {
        let request, directory, gets = 0, waits = 0;
        transport = async (url, options) => {
          if (url.pathname.endsWith("/test-run-waits")) {
            waits++;
            return failure === "refused" ? response({ error: "root session required" }, 409) : response({});
          }
          if (options.method === "POST") {
            directory = JSON.parse(options.body).runDirectory;
            request = json(join(directory, "request.json"));
            return failure === "initial" ? response({}, 403)
              : response({ contract: "termal-durable-test-run-registration", version: 1, runId: request.runId, registered: true });
          }
          gets++;
          return failure === "confirmation" && gets > 1 ? response({}, 503) : response(registered(request, directory), 200);
        };
        const marker = join(root, ".git", `failed-${failure}`);
        const runDir = await createRun({ root, detached: true,
          stages: [stage("never", `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`)] }, hostEnv, { fetch: transport });
        const result = await executeRun(runDir, hostEnv, () => assert.fail("failed wait must not announce readiness"));
        const expected = ["initial", "confirmation"].includes(failure) ? "own-run-registration-unconfirmed"
          : failure === "refused" ? "test-run-wait-refused" : "test-run-wait-unconfirmed";
        assert.deepEqual([result.state, result.reason], ["failed", expected]);
        assert.equal(existsSync(marker), false);
        assert.equal(result.stages[0].state, "unrun");
        assert.equal(waits, ["initial", "confirmation"].includes(failure) ? 0 : 1);
        const summary = await summarize(runDir);
        assert.ok(summary.includes(`reason: ${expected}`));
        assert.match(summary, /host-wait: (?:unavailable|refused|unconfirmed)/u);
        if (failure === "refused") assert.match(summary, /POST HTTP 409; host message="root session required"/u);
      }
    });
  });

  test("own-run detached worker saves one wait before readiness and stage execution without a mailbox", async () => {
    await repository(async (root) => {
      for (const source of ["index", "registration"]) {
        let request, directory, releaseWait, sawWait;
        const waitReceived = new Promise((done) => { sawWait = done; });
        const calls = [];
        const errors = [];
        const server = createServer(async (incoming, outgoing) => {
          try {
            calls.push([incoming.method, incoming.url]);
            if (incoming.method === "GET") { outgoing.end(JSON.stringify(registered(request, directory, source))); return; }
            const chunks = [];
            for await (const chunk of incoming) chunks.push(chunk);
            const body = JSON.parse(Buffer.concat(chunks));
            if (incoming.url.endsWith("/test-run-waits")) {
              assert.deepEqual(body, { runIds: [request.runId], mode: "all" });
              assert.ok(existsSync(join(directory, "execution.lock")));
              releaseWait = () => {
                if (!outgoing.writableEnded && !outgoing.destroyed) outgoing.writeHead(201).end(JSON.stringify(acknowledgement(request, directory)));
              };
              sawWait();
            } else {
              directory = body.runDirectory;
              request = json(join(directory, "request.json"));
              outgoing.end(JSON.stringify({ contract: "termal-durable-test-run-registration", version: 1,
                runId: request.runId, registered: true }));
            }
          } catch (error) { errors.push(error); outgoing.writeHead(500).end("{}"); sawWait(); }
        });
        server.listen(0, "127.0.0.1");
        await once(server, "listening");
        const context = { ...env, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` };
        delete context.TERMAL_CLI; // HTTP own-run completion does not use a mailbox executable.
        let pending, worker;
        try {
          const marker = join(root, ".git", `own-run-${source}`);
          const runDir = await createRun({ root, detached: true, stages: [stage("once", `
            const fs = require('node:fs');
            const result = JSON.parse(fs.readFileSync(process.env.OWN_RUN_RESULTS_PATH));
            require('node:assert/strict').equal(result.hostWait.state, 'registered');
            fs.appendFileSync(${JSON.stringify(marker)}, 'once');`)] }, context);
          context.OWN_RUN_RESULTS_PATH = join(runDir, "results.json");
          let receipt = "";
          pending = startDetached(runDir, context, (text) => { receipt += text; });
          await Promise.race([waitReceived, pending.then(() => assert.fail("readiness before wait acknowledgement"))]);
          assert.equal(receipt, "");
          assert.equal(existsSync(marker), false);
          releaseWait();
          worker = await pending;
          worker.child.ref();
          const outcome = await worker.completion;
          assert.equal(outcome.code, 0);
          assert.match(receipt, /completion=host-test-run-wait/u);
          assert.match(receipt, /host-wait: registered; waitId=test-run-wait-fixture/u);
          assert.doesNotMatch(receipt, /mailbox:|use --notify/u);
          assert.equal(readFileSync(marker, "utf8"), "once");
          assert.equal(json(join(runDir, "results.json")).hostWait.state, "registered");
          assert.equal(existsSync(join(runDir, "notification.json")), false);
          assert.equal(calls.filter(([, url]) => url.endsWith("/test-run-waits")).length, 1);
          await assert.rejects(executeRun(runDir, context), (error) => error.reason === "execution-lock-held");
          await summarize(runDir);
          assert.equal(calls.length, 4, "registration POST, launch GET, worker GET and one wait POST; no reader POST");
          assert.deepEqual(errors, []);
        } finally {
          releaseWait?.();
          if (pending) { try { const child = worker ?? await pending; await child.completion; } catch { /* startup assertion retains the result */ } }
          await new Promise((done) => { server.close(done); server.closeAllConnections(); });
        }
      }
    });
  });

  test("own-run CLI detachment reports a named wait refusal without STARTED or stage execution", async () => {
    await repository(async (root) => {
      const launcher = copyLauncher(root);
      let request, directory, waits = 0;
      const server = createServer(async (incoming, outgoing) => {
        if (incoming.method === "GET") { outgoing.end(JSON.stringify(registered(request, directory, "registration"))); return; }
        const chunks = [];
        for await (const chunk of incoming) chunks.push(chunk);
        const body = JSON.parse(Buffer.concat(chunks));
        if (incoming.url.endsWith("/test-run-waits")) {
          waits++;
          outgoing.writeHead(409).end(JSON.stringify({ error: "only a root session can wait", private: "not retained" }));
        } else {
          directory = body.runDirectory;
          request = json(join(directory, "request.json"));
          outgoing.end(JSON.stringify({ contract: "termal-durable-test-run-registration", version: 1,
            runId: request.runId, registered: true }));
        }
      });
      server.listen(0, "127.0.0.1");
      await once(server, "listening");
      try {
        const marker = join(root, ".git", "refused-stage");
        const context = { ...env, TERMAL_BASE_URL: `http://127.0.0.1:${server.address().port}` };
        delete context.TERMAL_CLI;
        const child = spawn(process.execPath, [launcher, "focused", "--detach", "--", process.execPath, "-e",
          `require('node:fs').writeFileSync(${JSON.stringify(marker)}, 'ran')`],
        { cwd: root, env: context, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
        let stdout = "", stderr = "";
        child.stdout.on("data", (chunk) => { stdout += chunk; });
        child.stderr.on("data", (chunk) => { stderr += chunk; });
        assert.equal((await once(child, "close"))[0], 1);
        assert.equal(waits, 1);
        assert.match(stderr, /FAIL launcher: test-run-wait-refused/u);
        assert.match(stdout, /POST HTTP 409; host message="only a root session can wait"/u);
        assert.doesNotMatch(stdout + stderr, /STARTED|End your turn|not retained/u);
        assert.equal(existsSync(marker), false);
        assert.equal(json(join(directory, "results.json")).reason, "test-run-wait-refused");
      } finally { await new Promise((done) => { server.close(done); server.closeAllConnections(); }); }
    });
  });
}
