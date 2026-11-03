import assert from "node:assert/strict";
import { existsSync, readFileSync, realpathSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import { fixtureHome } from "../../test-temp.mjs";

export function registerJournalStartup(test, support) {
  const {
    RootJournal,
    withJournal,
    fakeJournalStartup,
    silentWatch,
  } = support;
  test("journal startup observes delayed primary delivery and cleans its marker before the body", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
    const fixture = fakeJournalStartup(t);
    const { journal, writes, removals, directory } = fixture;
    // A previous callback and unrelated traffic do not establish startup.
    fixture.report("rename", ".watch-barrier-startup");
    let bodies = 0;
    const running = withJournal(directory, () => {
      bodies += 1;
      assert.deepEqual(removals, [writes[0].path]);
      assert.equal(existsSync(writes[0].path), false);
    }, { createJournal: () => journal });
    assert.equal(writes.length, 1); // Registration has not caught this write.
    assert.equal(bodies, 0);
    fixture.report("rename", "unrelated");
    t.mock.timers.tick(25);
    assert.equal(writes.length, 2);
    assert.equal(bodies, 0);
    fixture.report("change", basename(writes[1].path));
    await running;
    assert.equal(bodies, 1);
    assert.equal(journal.probeEvents(0, journal.cursor()).length, 0);
    assert.deepEqual(journal.foreignEvents(2, journal.cursor(), ".engram-project"), []);
    const count = writes.length;
    t.mock.timers.tick(10000);
    assert.equal(writes.length, count);
    assert.equal(journal.listenerCount("event"), 0);
    assert.equal(fixture.watcher.listenerCount("close"), 0);
  });

  test("deaf journal startup has one deadline and a failed start cannot restart writes", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
    const fixture = fakeJournalStartup(t);
    const started = fixture.journal.start();
    const refused = assert.rejects(started, /journal startup: timed out/u);
    t.mock.timers.tick(4999);
    assert.ok(fixture.writes.length > 1);
    assert.equal(fixture.journal.listenerCount("event"), 1);
    t.mock.timers.tick(1);
    await refused;
    assert.equal(fixture.journal.start(), started);
    await assert.rejects(fixture.journal.start(), /journal startup: timed out/u);
    const count = fixture.writes.length;
    t.mock.timers.tick(10000);
    assert.equal(fixture.writes.length, count);
    assert.equal(fixture.journal.listenerCount("event"), 0);
    assert.equal(fixture.watcher.listenerCount("close"), 0);
    assert.equal(fixture.removals.length, 1);
  });

  test("a callback at the startup deadline cannot rescue a delayed timeout handler", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
    const fixture = fakeJournalStartup(t);
    const refused = assert.rejects(fixture.journal.start(), /journal startup: timed out/u);
    // The monotonic deadline expired, although the timer queue has not run.
    fixture.setNow(5000);
    fixture.report("change", basename(fixture.writes[0].path));
    await refused;
    assert.equal(fixture.removals.length, 1);
    t.mock.timers.tick(10000);
    assert.equal(fixture.writes.length, 1);
  });

  test("synchronous startup delivery stops the writer but cannot satisfy a later one-shot barrier", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
    const fixture = fakeJournalStartup(t, {
      onWrite: ({ path, payload, options, report }) => {
        writeFileSync(path, payload, options);
        report("rename", basename(path));
      },
    });
    const started = fixture.journal.start();
    assert.equal(fixture.journal.start(), started);
    await started;
    assert.equal(fixture.removals.length, 1);
    const barrier = assert.rejects(fixture.journal.barrier("calibration-ready"), /barrier calibration-ready: timed out/u);
    t.mock.timers.tick(4999);
    assert.equal(fixture.journal.listenerCount("event"), 1);
    t.mock.timers.tick(1);
    await barrier;
    assert.equal(fixture.writes.length, 1);
    assert.equal(fixture.journal.listenerCount("event"), 0);
  });

  test("journal startup refuses null names and closure and cleans all wait resources", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
    for (const event of ["null filename", "close", "error"]) {
      const fixture = fakeJournalStartup(t);
      const refused = assert.rejects(fixture.journal.start(), /filename missing|closed during|watcher inconclusive/u);
      if (event === "null filename") fixture.report("rename", null);
      else if (event === "close") fixture.watcher.emit("close");
      else fixture.watcher.emit("error", new Error("native delivery failed"));
      await refused;
      assert.equal(fixture.removals.length, 1);
      assert.equal(fixture.journal.listenerCount("event"), 0);
      assert.equal(fixture.watcher.listenerCount("close"), 0);
      t.mock.timers.tick(10000);
      assert.equal(fixture.writes.length, 1);
    }
  });

  test("startup write and cleanup failures preserve both causes and never enter the body", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
    const writeFailure = new Error("startup write refused");
    const cleanupFailure = new Error("startup removal refused");
    const fixture = fakeJournalStartup(t, {
      onWrite: ({ path, payload, options, writes }) => {
        if (writes.length > 1) throw writeFailure;
        writeFileSync(path, payload, options);
      },
      onRemove: () => { throw cleanupFailure; },
    });
    const refused = assert.rejects(withJournal(fixture.directory, () => assert.fail("body entered"), {
      createJournal: () => fixture.journal,
    }), (error) => {
      assert.ok(error instanceof AggregateError);
      assert.deepEqual(error.errors, [writeFailure, cleanupFailure]);
      return true;
    });
    t.mock.timers.tick(25);
    await refused;
    assert.equal(fixture.journal.errors[0], writeFailure);
    assert.equal(fixture.journal.listenerCount("event"), 0);
    t.mock.timers.tick(10000);
    assert.equal(fixture.writes.length, 2);
  });

  test("startup refuses an existing marker without overwriting or deleting it", async (t) => {
    const fixture = fakeJournalStartup(t);
    const path = join(fixture.directory, ".watch-barrier-startup");
    writeFileSync(path, "not owned by startup");
    await assert.rejects(fixture.journal.start(), { code: "EEXIST" });
    assert.equal(readFileSync(path, "utf8"), "not owned by startup");
    assert.deepEqual(fixture.removals, []);
    assert.equal(fixture.journal.listenerCount("event"), 0);
    assert.equal(fixture.watcher.listenerCount("close"), 0);
  });

  test("startup cleanup failure prevents the body even after the primary callback", async (t) => {
    const cleanupFailure = new Error("startup cleanup refused");
    const fixture = fakeJournalStartup(t, {
      onWrite: ({ path, payload, options, report }) => {
        writeFileSync(path, payload, options);
        report("rename", basename(path));
      },
      onRemove: () => { throw cleanupFailure; },
    });
    await assert.rejects(withJournal(fixture.directory, () => assert.fail("body entered"), {
      createJournal: () => fixture.journal,
    }), (error) => {
      assert.ok(error instanceof AggregateError);
      assert.deepEqual(error.errors, [cleanupFailure]);
      return true;
    });
    assert.equal(fixture.removals.length, 1);
  });
}

export function registerJournalEvents(test, support) {
  const {
    RootJournal,
    withJournal,
    fakeJournalStartup,
    silentWatch,
  } = support;
  test("root journal waitFor times out and cleans listeners", async (t) => {
    const dir = fixtureHome("rpj-timeout-", t);
    const journal = new RootJournal(dir, { watchFn: () => silentWatch() });
    t.after(() => journal.close());
    await assert.rejects(
      () => journal.waitFor(() => false, 40, "never"),
      /timed out waiting for journal event/u,
    );
    assert.equal(journal.listenerCount("event"), 0);
  });

  test("root journal barrier timeout retains bounded unmatched callback diagnostics", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const dir = fixtureHome("rpj-diagnostic-", t);
    let report;
    const journal = new RootJournal(dir, {
      watchFn: (_directory, _options, callback) => {
        report = callback;
        return silentWatch();
      },
    });
    t.after(() => journal.close());
    const waiting = journal.barrier("calibration-ready");
    const refused = assert.rejects(waiting, (error) => {
      assert.match(error.message, /^barrier calibration-ready: timed out waiting for journal event; journal=/u);
      assert.ok(error.stack.includes("; journal="));
      const diagnostic = JSON.parse(error.message.split("; journal=")[1]);
      assert.equal(diagnostic.directory, dir);
      assert.equal(diagnostic.realDirectory, realpathSync(dir));
      assert.equal(diagnostic.barrier.filename, ".watch-barrier-calibration-ready");
      assert.equal(diagnostic.barrier.path, join(dir, ".watch-barrier-calibration-ready"));
      assert.equal(diagnostic.barrier.fromIndex, 0);
      assert.equal(diagnostic.timeoutMs, 5000);
      assert.ok(diagnostic.sinceWatchMs >= diagnostic.waitElapsedMs);
      assert.ok(diagnostic.barrier.writeFinishedSinceWatchMs >= diagnostic.barrier.writeStartedSinceWatchMs);
      assert.equal(diagnostic.cursor, 40);
      assert.equal(diagnostic.callbacks.length, 16);
      assert.equal(diagnostic.omittedCallbacks, 24);
      assert.equal(diagnostic.callbacks[0].callback, 24);
      assert.equal(diagnostic.callbacks.at(-1).eventType, "change");
      assert.equal(diagnostic.callbacks.at(-1).filenameType, "string");
      assert.equal(diagnostic.callbacks.at(-1).filename.length, 512);
      assert.equal(diagnostic.callbacks.at(-1).filenameLength, "unexpected-39-".length + 1000);
      assert.deepEqual(diagnostic.errors, []);
      assert.ok(error.message.length < 20_000);
      return true;
    });
    for (let index = 0; index < 40; index += 1) {
      report(index % 2 ? "change" : "rename", `unexpected-${index}-${"x".repeat(1000)}`);
    }
    assert.equal(journal.events.length, 40, "diagnostic bounds do not trim the assertion journal");
    t.mock.timers.tick(4999);
    assert.equal(journal.listenerCount("event"), 1, "the original deadline is retained");
    t.mock.timers.tick(1);
    await refused;
    assert.equal(journal.listenerCount("event"), 0);
  });

  test("root journal exact barrier success does not emit timeout diagnostics", async (t) => {
    const dir = fixtureHome("rpj-seen-", t);
    let report;
    const journal = new RootJournal(dir, {
      watchFn: (_directory, _options, callback) => {
        report = callback;
        return silentWatch();
      },
    });
    t.after(() => journal.close());
    t.mock.method(journal, "timeoutDiagnostic", () => assert.fail("success requested diagnostics"));
    const waiting = journal.barrier("seen");
    report("rename", ".watch-barrier-seen");
    assert.equal(await waiting, 1);
    assert.deepEqual(journal.events, [{ filename: ".watch-barrier-seen", index: 0 }]);
    assert.equal(journal.listenerCount("event"), 0);
  });

  test("root journal diagnostic failure preserves the timeout and listener cleanup", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const dir = fixtureHome("rpj-diagnostic-error-", t);
    const journal = new RootJournal(dir, { watchFn: () => silentWatch() });
    t.after(() => journal.close());
    t.mock.method(journal, "timeoutDiagnostic", () => { throw new Error("diagnostic unavailable"); });
    const refused = assert.rejects(journal.waitFor(() => false, 40, "never"), (error) => {
      assert.equal(error.message, "never: timed out waiting for journal event");
      return true;
    });
    t.mock.timers.tick(40);
    await refused;
    assert.equal(journal.listenerCount("event"), 0);
  });

  test("root journal rejects preexisting errors and null filenames", async (t) => {
    const dir = fixtureHome("rpj-null-", t);
    const preexisting = new RootJournal(dir, {
      watchFn: () => {
        const watcher = silentWatch();
        queueMicrotask(() => watcher.emit("error", new Error("overflow")));
        return watcher;
      },
    });
    t.after(() => preexisting.close());
    await new Promise((resolvePromise) => setImmediate(resolvePromise));
    await assert.rejects(
      () => preexisting.waitFor(() => true, 40, "pre"),
      /preexisting watcher error: Error: overflow/u,
    );

    const nullName = new RootJournal(dir, {
      watchFn: (_directory, _options, callback) => {
        const watcher = silentWatch();
        queueMicrotask(() => callback("rename", null));
        return watcher;
      },
    });
    t.after(() => nullName.close());
    await assert.rejects(
      () => nullName.waitFor(() => false, 200, "null-name"),
      /filename missing or overflow/u,
    );
  });
}
