import assert from "node:assert/strict";
import { resolve } from "node:path";
import win32 from "node:path/win32";
import { fixtureHome, fixtureRoot } from "../../test-temp.mjs";

export function registerOwnershipGuards(test, support) {
  const {
    assertExactOwnedFixture,
    isExactOwnedFixturePath,
    setupOwnedStore,
    cleanupDeniedResources,
    withMcp,
    createSession,
    withDeniedRoot,
    DeniedCreate,
    withWritableRoot,
    waitForChildClose,
  } = support;
  test("owned-fixture guard accepts a child and refuses same-root or sibling", (t) => {
    const child = fixtureHome("rpj-guard-", t);
    assert.doesNotThrow(() => assertExactOwnedFixture(child));
    assert.throws(() => assertExactOwnedFixture(fixtureRoot), /non-owned fixture path/u);
    const sibling = resolve(fixtureRoot, "..", "sibling-not-owned");
    assert.throws(() => assertExactOwnedFixture(sibling), /non-owned fixture path/u);
  });

  test("owned-fixture guard refuses Windows different-drive and UNC relatives", () => {
    const root = "C:\\fixture";
    assert.equal(isExactOwnedFixturePath("C:\\fixture\\child", root, win32), true);
    assert.equal(isExactOwnedFixturePath("C:\\fixture", root, win32), false);
    assert.equal(isExactOwnedFixturePath("C:\\other", root, win32), false);
    assert.equal(isExactOwnedFixturePath("D:\\outside", root, win32), false);
    assert.equal(isExactOwnedFixturePath("\\\\server\\share\\outside", root, win32), false);
  });
}

export function registerResourceCleanup(test, support) {
  const {
    assertExactOwnedFixture,
    isExactOwnedFixturePath,
    setupOwnedStore,
    cleanupDeniedResources,
    withMcp,
    createSession,
    withDeniedRoot,
    DeniedCreate,
    withWritableRoot,
    waitForChildClose,
  } = support;
  test("denied setup failure owns temps from the first create", () => {
    const created = [];
    const removed = [];
    assert.throws(
      () =>
        setupOwnedStore("rpj-setup-", {
          fixtureHome(prefix) {
            created.push(prefix);
            return `/tmp/${prefix}${created.length}`;
          },
          writeProject() {},
          init() {
            throw new Error("init failed");
          },
          remove(path) {
            removed.push(path);
          },
        }),
      /init failed/u,
    );
    assert.deepEqual(created, ["rpj-setup-home-", "rpj-setup-root-"]);
    assert.deepEqual(removed, ["/tmp/rpj-setup-home-1", "/tmp/rpj-setup-root-2"]);
  });

  test("denied cleanup shuts down before restore and skips root delete after restore failure", async () => {
    const failed = await cleanupDeniedResources({
      shutdown: async () => {},
      restore: async () => {
        throw new Error("restore failed");
      },
      deleteRoot: async () => {
        throw new Error("root should not be deleted");
      },
      deleteHome: async () => {},
    });
    assert.equal(failed.restored, false);
    assert.deepEqual(failed.steps, [
      "shutdown",
      "restore-failed",
      "no-delete-root",
      "delete-home",
    ]);
    assert.equal(failed.errors.length, 1);
    assert.match(failed.errors[0].message, /restore failed/u);

    const ok = await cleanupDeniedResources({
      shutdown: async () => {},
      restore: async () => {},
      deleteRoot: async () => {},
      deleteHome: async () => {},
    });
    assert.equal(ok.restored, true);
    assert.deepEqual(ok.steps, ["shutdown", "restore", "delete-root", "delete-home"]);
  });

  test("withMcp keeps the body error when shutdown also fails", async () => {
    const bodyError = new Error("body A");
    const shutdownError = new Error("shutdown B");
    await assert.rejects(
      () =>
        withMcp(
          {
            attach(client) {
              return client;
            },
            async shutdown() {
              throw shutdownError;
            },
          },
          "home",
          "project",
          async () => {
            throw bodyError;
          },
          { open: () => ({ close: async () => {} }) },
        ),
      (error) => {
        assert.ok(error instanceof AggregateError);
        assert.equal(error.errors[0], bodyError);
        assert.equal(error.errors[1], shutdownError);
        assert.equal(error.message, bodyError.message);
        return true;
      },
    );
  });

  test("failed session close stays failed on retry and skips fixture delete", async () => {
    const journal = [];
    const session = createSession();
    session.attach({
      async close() {
        journal.push("close");
        throw new Error("close failed");
      },
    });
    await assert.rejects(() => session.shutdown(), /close failed/u);
    await assert.rejects(() => session.shutdown(), /close failed/u);
    assert.deepEqual(journal, ["close", "close"]);
    assert.equal(session.shutdownConfirmed, false);

    const cleanup = await cleanupDeniedResources({
      shutdown: () => session.shutdown(),
      restore: async () => {
        journal.push("restore");
      },
      deleteRoot: async () => {
        journal.push("delete-root");
      },
      deleteHome: async () => {
        journal.push("delete-home");
      },
    });
    assert.equal(cleanup.shutdownOk, false);
    assert.deepEqual(cleanup.steps, [
      "shutdown-failed",
      "restore",
      "no-delete-root",
      "no-delete-home",
    ]);
    assert.deepEqual(journal, ["close", "close", "close", "restore"]);
    assert.match(cleanup.errors[0].message, /close failed/u);
  });

  test("denied apply owes restore before mutation; wrapper restores injected deny throw", async (t) => {
    const journal = [];
    const state = { current: "captured" };
    await assert.rejects(
      () =>
        withDeniedRoot(
          t,
          "rpj-deny-",
          async () => {
            throw new Error("body should not run");
          },
          {
            setupOwnedStore: () => ({
              home: "H",
              projectDir: "R",
              projectFile: "R/.engram-project",
              projectId: "rpj-deny-project",
            }),
            DeniedCreate: class InjectedDeny extends DeniedCreate {
              constructor(directory, options) {
                super(directory, {
                  ...options,
                  deny() {
                    journal.push("deny");
                    state.current = "mutated";
                    throw new Error("deny failed after mutation");
                  },
                  restoreAcl() {
                    journal.push("restore");
                    state.current = "captured";
                  },
                  writeFileSync() {},
                  readFileSync: () => options.projectId,
                });
              }
            },
            remove(path) {
              journal.push(`delete:${path}`);
            },
          },
        ),
      (error) => {
        assert.match(error.message, /deny failed after mutation/u);
        assert.equal(error instanceof AggregateError, false);
        return true;
      },
    );
    assert.equal(state.current, "captured");
    assert.deepEqual(journal, ["deny", "restore", "delete:R", "delete:H"]);
  });

  test("writable wrapper keeps both fixtures when MCP close fails", async (t) => {
    const journal = [];
    await assert.rejects(
      () =>
        withWritableRoot(
          t,
          "rpw-close-",
          async ({ session }) => {
            await withMcp(session, "H", "project", async () => {}, {
              open: () => ({
                async close() {
                  journal.push("close");
                  throw new Error("close failed");
                },
              }),
            });
          },
          {
            setupOwnedStore: () => ({
              home: "H",
              projectDir: "R",
              projectFile: "R/.engram-project",
              projectId: "rpw-close-project",
            }),
            remove(path) {
              journal.push(`delete:${path}`);
            },
          },
        ),
      (error) => {
        assert.match(error.message, /close failed/u);
        return true;
      },
    );
    assert.ok(journal.includes("close"), journal.join(","));
    assert.equal(journal.includes("delete:H"), false, journal.join(","));
    assert.equal(journal.includes("delete:R"), false, journal.join(","));
  });

  test("MCP graceful close avoids forced kill", async (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    let killed = 0;
    const closed = new Promise((resolvePromise) => {
      setTimeout(() => resolvePromise({ code: 0, signal: null }), 25);
    });
    const waiting = waitForChildClose(closed, () => {
      killed += 1;
    }, { gracefulMs: 10000, forcedMs: 1000 });
    t.mock.timers.tick(25);
    await waiting;
    assert.equal(killed, 0);
  });
}
