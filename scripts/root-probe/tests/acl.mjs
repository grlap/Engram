import assert from "node:assert/strict";
import { fixtureHome } from "../../test-temp.mjs";

export function registerAclReader(test, support) {
  const {
    fakeAclReader,
    drainAclEvents,
    DeniedCreate,
    cleanupDeniedResources,
    writeProject,
    daclWithoutAutoInherited,
  } = support;
  test("ACL reader separates delayed startup from each read and reuses one process", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const fixture = fakeAclReader(t);
    const captured = fixture.reader.read("capture");
    t.mock.timers.tick(14999);
    fixture.child.stdout.emit("data", Buffer.from("rea"));
    fixture.child.stdout.emit("data", Buffer.from("dy\r\n"));
    await drainAclEvents();
    t.mock.timers.tick(1000); // The old shared startup/read deadline would expire here.
    fixture.child.stdout.emit("data", Buffer.from('"D:(A;;FA;;;BA)"\n'));
    assert.equal(await captured, "D:(A;;FA;;;BA)");
    const verified = fixture.reader.read("restore-verification");
    await drainAclEvents();
    fixture.child.stdout.emit("data", Buffer.from('"D:AI(A;;FA;;;BA)"\n'));
    assert.equal(await verified, "D:AI(A;;FA;;;BA)");
    assert.equal(fixture.spawns, 1);
    assert.deepEqual(fixture.calls, ["read\n", "read\n"]);
    await fixture.reader.close();
    assert.deepEqual(fixture.calls, ["read\n", "read\n", "close\n"]);
    assert.equal(fixture.kills, 0);
    for (const emitter of [fixture.child, fixture.child.stdin, fixture.child.stdout, fixture.child.stderr]) {
      assert.deepEqual(emitter.eventNames(), []);
    }
    t.mock.timers.tick(60000); // No startup, read or shutdown timer remains active.
    assert.equal(fixture.reader.failure, undefined);
  });

  test("ACL reader blocked startup is classified and closes before capture can deny", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const fixture = fakeAclReader(t);
    let denied = false;
    const denial = new DeniedCreate(fixture.directory, {
      reader: fixture.reader,
      addDeny: () => { denied = true; },
    });
    denial.windows = true;
    const refused = assert.rejects(() => denial.apply(), (error) => {
      assert.equal(error.code, "ROOT_PROBE_ACL_TIMEOUT");
      assert.equal(error.phase, "startup");
      assert.equal(error.operation, "capture");
      assert.equal(error.cause.code, "ETIMEDOUT");
      return true;
    });
    t.mock.timers.tick(15000);
    await refused;
    assert.equal(denied, false);
    assert.equal(denial.applied, false);
    assert.equal(fixture.kills, 1);
    assert.deepEqual(fixture.calls, []);
    await denial.closeReader();
  });

  test("ACL reader blocked verification retains the restore obligation and root", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const fixture = fakeAclReader(t);
    const writes = [];
    let setDacl;
    const denial = new DeniedCreate(fixture.directory, {
      projectId: "reader-project", reader: fixture.reader, addDeny: () => {},
      setDacl: (_, dacl) => { setDacl = dacl; },
      writeFileSync: (path) => {
        writes.push(path);
        if (path.endsWith("canary-denied-create")) throw Object.assign(new Error("denied"), { code: "EACCES" });
      },
      readFileSync: () => "reader-project",
    });
    denial.windows = true;
    const applied = denial.apply();
    fixture.child.stdout.emit("data", Buffer.from("ready\n"));
    await drainAclEvents();
    fixture.child.stdout.emit("data", Buffer.from('"D:(A;;FA;;;BA)"\n'));
    await applied;
    const removed = [];
    const cleanup = cleanupDeniedResources({
      shutdown: async () => {}, restore: () => denial.restore(), closeReader: () => denial.closeReader(),
      deleteRoot: () => { removed.push("root"); }, deleteHome: () => { removed.push("home"); },
    });
    await drainAclEvents();
    t.mock.timers.tick(15000);
    const result = await cleanup;
    assert.equal(setDacl, "D:(A;;FA;;;BA)");
    assert.equal(result.errors[0].code, "ROOT_PROBE_ACL_TIMEOUT");
    assert.equal(result.errors[0].phase, "read");
    assert.equal(result.errors[0].operation, "restore-verification");
    assert.equal(denial.applied, true);
    assert.equal(writes.some((path) => path.endsWith(".postrestore-write")), false);
    assert.deepEqual(removed, ["home"]); // Reader closure is confirmed, but ACL verification failed.
  });

  test("ACL reader preserves process errors and rejects malformed or excessive output", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const missing = fakeAclReader(t);
    const cause = Object.assign(new Error("pwsh unavailable"), { code: "ENOENT" });
    const failed = assert.rejects(missing.reader.read("capture"), (error) => {
      assert.equal(error.code, "ROOT_PROBE_ACL_FAILED");
      assert.equal(error.cause, cause);
      return true;
    });
    missing.child.emit("error", cause);
    await failed;
    for (const data of ["not JSON\n", '"O:BA"\n', "x".repeat(65537)]) {
      const fixture = fakeAclReader(t);
      const reading = fixture.reader.read("capture");
      const rejected = assert.rejects(reading, { code: "ROOT_PROBE_ACL_FAILED", phase: "read" });
      fixture.child.stdout.emit("data", Buffer.from("ready\n"));
      await drainAclEvents();
      fixture.child.stdout.emit("data", Buffer.from(data));
      await rejected;
      assert.equal(fixture.kills, 1);
    }
  });

  test("ACL reader reports the requested operation after an idle failure", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    for (const idleEvent of ["close", "unsolicited output"]) {
      const fixture = fakeAclReader(t);
      const captured = fixture.reader.read("capture");
      fixture.child.stdout.emit("data", Buffer.from("ready\n"));
      await drainAclEvents();
      fixture.child.stdout.emit("data", Buffer.from('"D:(A;;FA;;;BA)"\n'));
      await captured;
      if (idleEvent === "close") fixture.child.emit("close", 1, null);
      else fixture.child.stdout.emit("data", Buffer.from('"D:(A;;FA;;;BA)"\n'));
      const idleFailure = fixture.reader.failure;
      assert.equal(idleFailure.phase, "idle");
      assert.equal(idleFailure.operation, "observation");
      await assert.rejects(fixture.reader.read("restore-verification"), (error) => {
        assert.equal(error.code, "ROOT_PROBE_ACL_FAILED");
        assert.equal(error.phase, "read");
        assert.equal(error.operation, "restore-verification");
        assert.equal(error.cause, idleFailure);
        assert.match(error.cause.cause.message, /exited|unsolicited/u);
        return true;
      });
    }
  });

  test("ACL reader slow graceful shutdown confirms closure before fixture removal", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const fixture = fakeAclReader(t, { endCloses: false });
    fixture.child.stdout.emit("data", Buffer.from("ready\n"));
    const removed = [];
    const cleanup = cleanupDeniedResources({
      shutdown: async () => {}, restore: async () => {}, closeReader: () => fixture.reader.close(),
      deleteRoot: () => { removed.push("root"); }, deleteHome: () => { removed.push("home"); },
    });
    await drainAclEvents();
    t.mock.timers.tick(14999);
    assert.equal(fixture.kills, 0);
    assert.deepEqual(removed, []);
    fixture.child.emit("close", 0, null);
    const result = await cleanup;
    assert.deepEqual(result.errors, []);
    assert.equal(result.shutdownOk, true);
    assert.deepEqual(removed, ["root", "home"]);
    assert.equal(fixture.kills, 0);
  });

  test("ACL reader graceful-close deadline forces termination and confirms closure", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const fixture = fakeAclReader(t, { endCloses: false });
    fixture.child.stdout.emit("data", Buffer.from("ready\n"));
    const closed = fixture.reader.close();
    t.mock.timers.tick(15000);
    await assert.rejects(closed, { code: "ROOT_PROBE_ACL_TIMEOUT", phase: "shutdown" });
    assert.equal(fixture.kills, 1);
    assert.equal(fixture.reader.result.signal, "SIGTERM");
  });

  test("ACL reader graceful nonzero or signalled exit refuses fixture removal", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    for (const [code, signal] of [[1, null], [null, "SIGTERM"]]) {
      const fixture = fakeAclReader(t, { endCloses: false });
      fixture.child.stdout.emit("data", Buffer.from("ready\n"));
      const cleanup = cleanupDeniedResources({
        shutdown: async () => {}, restore: async () => {}, closeReader: () => fixture.reader.close(),
        deleteRoot: () => assert.fail("root deleted after failed reader exit"),
        deleteHome: () => assert.fail("home deleted after failed reader exit"),
      });
      await drainAclEvents();
      assert.deepEqual(fixture.calls, ["close\n"]);
      fixture.child.emit("close", code, signal);
      const result = await cleanup;
      assert.equal(result.shutdownOk, false);
      assert.equal(result.errors.length, 1);
      assert.equal(result.errors[0].code, "ROOT_PROBE_ACL_FAILED");
      assert.equal(result.errors[0].phase, "shutdown");
      assert.equal(result.errors[0].operation, "closure");
      assert.match(result.errors[0].message, /reader exited/u);
      assert.deepEqual(result.steps, ["shutdown", "restore", "reader-close-failed", "no-delete-root", "no-delete-home"]);
      assert.equal(fixture.kills, 0);
    }
  });

  test("ACL reader graceful close surfaces an earlier idle failure", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    for (const idleEvent of ["stdin error", "unsolicited output"]) {
      const fixture = fakeAclReader(t);
      fixture.child.stdout.emit("data", Buffer.from("ready\n"));
      if (idleEvent === "stdin error") fixture.child.stdin.emit("error", new Error("idle stdin failed"));
      else fixture.child.stdout.emit("data", Buffer.from('"D:(A;;FA;;;BA)"\n'));
      const idleFailure = fixture.reader.failure;
      assert.equal(idleFailure.phase, "idle");
      await assert.rejects(fixture.reader.close(), (error) => {
        assert.equal(error.code, "ROOT_PROBE_ACL_FAILED");
        assert.equal(error.phase, "shutdown");
        assert.equal(error.operation, "closure");
        assert.equal(error.cause, idleFailure);
        return true;
      });
      assert.equal(fixture.reader.result.code, 0);
      assert.equal(fixture.kills, 0);
    }
  });

  test("ACL reader unconfirmed termination retains both fixtures and all failures", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const fixture = fakeAclReader(t, { killCloses: false, endCloses: false });
    const refused = assert.rejects(fixture.reader.read("capture"), (error) => {
      assert.ok(error instanceof AggregateError);
      assert.equal(error.errors[0].phase, "startup");
      assert.equal(error.errors[0].code, "ROOT_PROBE_ACL_TIMEOUT");
      assert.equal(error.errors[1].phase, "shutdown");
      assert.match(error.errors[1].message, /closure unconfirmed/u);
      return true;
    });
    t.mock.timers.tick(15000);
    await drainAclEvents();
    t.mock.timers.tick(1000);
    await drainAclEvents();
    t.mock.timers.tick(1000);
    await refused;
    assert.equal(fixture.kills, 2);
    const result = await cleanupDeniedResources({
      shutdown: async () => {}, restore: async () => {}, closeReader: () => fixture.reader.close(),
      deleteRoot: () => assert.fail("root deleted before reader closure"),
      deleteHome: () => assert.fail("home deleted before reader closure"),
    });
    assert.equal(result.shutdownOk, false);
    assert.deepEqual(result.steps, ["shutdown", "restore", "reader-close-failed", "no-delete-root", "no-delete-home"]);
    fixture.child.emit("close", null, "SIGTERM"); // Complete the fake process's ownership after the assertions.
  });
}

export function registerAclRestoration(test, support) {
  const {
    fakeAclReader,
    drainAclEvents,
    DeniedCreate,
    cleanupDeniedResources,
    writeProject,
    daclWithoutAutoInherited,
  } = support;
  test("denied-create canary success and restore failure are not swallowed", async (t) => {
    const dir = fixtureHome("rpj-canary-", t);
    writeProject(dir, "canary-project");
    const succeeded = new DeniedCreate(dir, {
      projectId: "canary-project",
      deny: () => {},
      restoreAcl: () => {},
      writeFileSync: () => {},
      readFileSync: () => "canary-project",
    });
    await assert.rejects(() => succeeded.apply(), /canary write succeeded/u);

    const restoreFailed = new DeniedCreate(dir, {
      projectId: "canary-project",
      deny: () => {},
      restoreAcl: () => {
        throw new Error("restore exploded");
      },
      writeFileSync: () => {},
      readFileSync: () => "canary-project",
    });
    await assert.rejects(() => restoreFailed.apply(), (error) => {
      assert.ok(error instanceof AggregateError);
      assert.match(error.errors[0].message, /canary write succeeded/u);
      assert.match(error.errors[1].message, /restore exploded/u);
      return true;
    });
  });

  test("a restored DACL may gain only the auto-inherited flag", async (t) => {
    const dir = fixtureHome("rpj-dacl-", t);
    writeProject(dir, "dacl-project");
    const aces = "(A;ID;FA;;;BA)(A;OICIIOID;GA;;;BA)";
    // Windows ACL calls are faked, so this runs on every platform.
    const denial = async (restoredAs) => {
      let current = `D:${aces}`;
      const denied = new DeniedCreate(dir, {
        projectId: "dacl-project",
        getDacl: () => current,
        setDacl: (_, dacl) => {
          current = restoredAs(dacl);
        },
        addDeny: () => {
          current = `D:(D;;0x6;;;WD)${aces}`;
        },
        writeFileSync: (path) => {
          if (path.endsWith("canary-denied-create")) {
            throw Object.assign(new Error("denied"), { code: "EACCES" });
          }
        },
        readFileSync: () => "dacl-project",
      });
      denied.windows = true;
      await denied.apply();
      return denied;
    };
    // Set-Acl adds AI to a DACL captured without it: the restore holds.
    await (await denial((dacl) => dacl.replace(/^D:/u, "D:AI"))).restore();
    await (await denial((dacl) => dacl)).restore();
    // Anything else that differs is still refused: a changed ACE, a lost ACE,
    // or a protected flag the capture did not have.
    for (const restoredAs of [
      (dacl) => dacl.replace("GA;;;BA", "GR;;;BA"),
      (dacl) => dacl.replace("(A;ID;FA;;;BA)", ""),
      (dacl) => dacl.replace(/^D:/u, "D:PAI"),
    ]) {
      const denied = await denial(restoredAs);
      await assert.rejects(() => denied.restore(), /restored DACL does not equal the captured DACL/u);
    }
    assert.equal(daclWithoutAutoInherited("D:PAI(A;;FA;;;BA)"), "D:P(A;;FA;;;BA)");
    assert.equal(daclWithoutAutoInherited("D:ARAI(A;;FA;;;BA)"), "D:AR(A;;FA;;;BA)");
    assert.equal(daclWithoutAutoInherited("D:(A;;FA;;;AI)"), "D:(A;;FA;;;AI)");
  });
}
