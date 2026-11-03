import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { basename, join } from "node:path";

export function registerRootIdentity(test, support) {
  const {
    withWritableRoot,
    withDeniedRoot,
    withOwnedStore,
    withJournal,
    calibrateJournal,
    assertNoProbe,
    assertResolvedDoctorPolicy,
    WARNING,
    RESOLVED_POLICY,
    CONTROL_ARGS,
    workArgs,
    addedRef,
    withMcp,
    run,
    BARRIER_PREFIX,
    setupOwnedStore,
  } = support;
  test("host commands resolve identity without a probe marker; agent and MCP emit none", async (t) => {
    await withWritableRoot(t, "rpw-", async ({ home, projectDir, projectFile, session: mcpSession }) => {
    await withJournal(projectDir, async (journal) => {
      await calibrateJournal(journal, projectDir);
      const doctor = await assertNoProbe(
        journal,
        home,
        projectFile,
        ["doctor", "--json"],
        "doctor",
      );
      assertResolvedDoctorPolicy(doctor);
      assert.doesNotMatch(doctor.stderr, new RegExp(WARNING), doctor.stderr);
      const readiness = await assertNoProbe(
        journal,
        home,
        projectFile,
        ["readiness", "--json"],
        "readiness",
      );
      const ready = JSON.parse(readiness.stdout);
      assert.equal(ready.host_path_policy.status, "matched", readiness.stdout);
      assert.match(ready.host_path_policy.resolved, RESOLVED_POLICY, readiness.stdout);
      // The host control process a TermAl session starts: it resolves the
      // root identity at startup, then serves until its stdin closes.
      const control = await assertNoProbe(
        journal,
        home,
        projectFile,
        CONTROL_ARGS,
        "control",
      );
      assert.doesNotMatch(control.stderr, new RegExp(WARNING), control.stderr);

      const session = "probe-session";
      for (const [label, args] of [
        ["next", workArgs(session, "next")],
        ["peek", workArgs(session, "next", "--peek")],
        ["ls", workArgs(session, "ls")],
        ["memories", workArgs(session, "memories")],
        ["core-next-focus", workArgs(session, "core", "next", "--sections", "focus")],
      ]) {
        await assertNoProbe(journal, home, projectFile, args, label);
      }

      const added = await assertNoProbe(
        journal,
        home,
        projectFile,
        workArgs(session, "add", "Probe mutation", "--json"),
        "add",
      );
      const ref = addedRef(added);
      const shown = await assertNoProbe(
        journal,
        home,
        projectFile,
        workArgs(session, "show", ref),
        "show",
      );
      assert.match(shown.stdout, new RegExp(ref));
      await assertNoProbe(
        journal,
        home,
        projectFile,
        workArgs(session, "core", "focus", ref),
        "core-focus",
      );
      await assertNoProbe(
        journal,
        home,
        projectFile,
        ["graph", "--actor-id", "probe", "--session-id", session, "save", "--stdout"],
        "graph",
      );
      await assertNoProbe(
        journal,
        home,
        projectFile,
        ["backup", "--out", join(home, "probe-backup.db")],
        "backup",
      );

      const mcpStarted = journal.cursor();
      await withMcp(mcpSession, home, projectFile, async (mcp) => {
        await mcp.handshakeAndPeek();
        assert.doesNotMatch(mcp.stderr, new RegExp(WARNING));
      });
      const mcpUntil = await journal.barrier("mcp");
      assert.equal(
        journal.probeEvents(mcpStarted, mcpUntil).length,
        0,
        `probe events during MCP: ${JSON.stringify(journal.events)}`,
      );
      assert.deepEqual(
        journal.foreignEvents(mcpStarted, mcpUntil, basename(projectFile)),
        [],
        `root writes during MCP: ${JSON.stringify(journal.events)}`,
      );
      // Apart from the journal's own barriers, the root holds only what it
      // started with.
      assert.deepEqual(
        readdirSync(projectDir).filter((name) => !name.startsWith(BARRIER_PREFIX)),
        [basename(projectFile)],
      );
    });
    });
  });

  test("a create-denied root still resolves host identity, and agent/MCP succeed, without a WARNING", async (t) => {
    await withDeniedRoot(t, "rpd-", async ({ home, projectFile, session }) => {
      // The host probe only reads an existing root entry, so a root that
      // refuses new files, like a read-only checkout, still resolves.
      const doctor = run(home, projectFile, ["doctor", "--json"]);
      assert.doesNotMatch(doctor.stderr, new RegExp(WARNING), doctor.stderr);
      assertResolvedDoctorPolicy(doctor);
      const audited = JSON.parse(doctor.stdout);
      const before = readFileSync(audited.database);
      const walPath = `${audited.database}-wal`;
      // Compare WAL bytes, treating absence as empty; changed/nonempty bytes
      // still fail. SQLite's separate read-coordination sidecar is -shm.
      const walBefore = existsSync(walPath) ? readFileSync(walPath) : Buffer.alloc(0);
      const readiness = run(home, projectFile, ["readiness", "--json"]);
      assert.equal(readiness.status, 0, readiness.stderr);
      assert.doesNotMatch(readiness.stderr, new RegExp(WARNING), readiness.stderr);
      const ready = JSON.parse(readiness.stdout);
      assert.equal(ready.ready, true);
      assert.equal(ready.scope, "readiness");
      assert.equal(ready.full_audit, "not_run");
      assert.equal(ready.mutation_enabled, false);
      assert.equal(ready.host_path_policy.status, "matched");
      assert.equal(ready.host_path_policy.resolved, audited.host_path_policy);
      assert.equal(ready.host_path_policy.stored, audited.host_path_policy);
      assert.deepEqual(readFileSync(audited.database), before);
      assert.deepEqual(existsSync(walPath) ? readFileSync(walPath) : Buffer.alloc(0), walBefore);

      const actor = "probe-denied";
      for (const args of [
        workArgs(actor, "next"),
        workArgs(actor, "next", "--peek"),
        workArgs(actor, "ls"),
        workArgs(actor, "memories"),
        workArgs(actor, "core", "next", "--sections", "focus"),
      ]) {
        const output = run(home, projectFile, args);
        assert.equal(output.status, 0, `${args.join(" ")}\n${output.stderr}`);
        assert.doesNotMatch(output.stderr, new RegExp(WARNING), output.stderr);
        assert.equal(output.stderr.includes("ENGRAM_HOST_PATH_POLICY"), false);
      }

      const added = run(home, projectFile, workArgs(actor, "add", "Denied mutation", "--json"));
      const ref = addedRef(added);
      assert.doesNotMatch(added.stderr, new RegExp(WARNING), added.stderr);
      const shown = run(home, projectFile, workArgs(actor, "show", ref));
      assert.equal(shown.status, 0, shown.stderr);
      assert.match(shown.stdout, new RegExp(ref));
      assert.doesNotMatch(shown.stderr, new RegExp(WARNING), shown.stderr);
      const focused = run(home, projectFile, workArgs(actor, "core", "focus", ref));
      assert.equal(focused.status, 0, focused.stderr);
      assert.doesNotMatch(focused.stderr, new RegExp(WARNING), focused.stderr);

      await withMcp(session, home, projectFile, async (mcp) => {
        await mcp.handshakeAndPeek();
        assert.doesNotMatch(mcp.stderr, new RegExp(WARNING), mcp.stderr);
      });
    });
  });

  test("a project file name with no ASCII letter leaves host identity unresolved with a WARNING; agent paths stay silent", async (t) => {
    const platformPolicy =
      process.platform === "win32" || process.platform === "darwin" ? "case_fold" : "case_sensitive";
    await withOwnedStore(
      t,
      "rpu-",
      async ({ home, projectDir, projectFile, session }) => {
        const doctor = run(home, projectFile, ["doctor", "--json"]);
        assert.match(doctor.stderr, new RegExp(WARNING), `host doctor must warn: ${doctor.stderr}`);
        assertResolvedDoctorPolicy(doctor);
        const readiness = run(home, projectFile, ["readiness", "--json"]);
        assert.equal(readiness.status, 0, readiness.stderr);
        assert.match(readiness.stderr, new RegExp(WARNING), readiness.stderr);
        const ready = JSON.parse(readiness.stdout);
        assert.equal(ready.host_path_policy.status, "unresolved", readiness.stdout);
        assert.equal(ready.host_path_policy.resolved, null, readiness.stdout);
        assert.match(ready.host_path_policy.stored, RESOLVED_POLICY, readiness.stdout);
        // The host control process runs the same probe at startup.
        const control = run(home, projectFile, CONTROL_ARGS);
        assert.match(control.stderr, new RegExp(WARNING), `host control must warn: ${control.stderr}`);

        // The host probe fails here, so any agent path that ran it would warn.
        const actor = "probe-unresolved";
        const silent = (output, label) => {
          assert.equal(output.status, 0, `${label}\n${output.stderr}`);
          assert.doesNotMatch(output.stderr, new RegExp(WARNING), `${label}: ${output.stderr}`);
        };
        for (const args of [
          workArgs(actor, "next"),
          workArgs(actor, "next", "--peek"),
          workArgs(actor, "ls"),
          workArgs(actor, "memories"),
          workArgs(actor, "core", "next", "--sections", "focus"),
        ]) {
          silent(run(home, projectFile, args), args.join(" "));
        }
        const added = run(home, projectFile, workArgs(actor, "add", "Unresolved mutation", "--json"));
        silent(added, "add");
        const ref = addedRef(added);
        silent(run(home, projectFile, workArgs(actor, "show", ref)), "show");
        silent(run(home, projectFile, workArgs(actor, "core", "focus", ref)), "core focus");
        silent(
          run(home, projectFile, ["graph", "--actor-id", "probe", "--session-id", actor, "save", "--stdout"]),
          "graph",
        );
        silent(
          run(home, projectFile, ["backup", "--out", join(home, "probe-backup.db")]),
          "backup",
        );
        await withMcp(session, home, projectFile, async (mcp) => {
          await mcp.handshakeAndPeek();
          assert.doesNotMatch(mcp.stderr, new RegExp(WARNING), mcp.stderr);
        });
        // Nothing the probe or the agent paths did left a file in the root.
        assert.deepEqual(readdirSync(projectDir).sort(), ["2026"]);
      },
      {
        setupOwnedStore: (prefix) =>
          setupOwnedStore(prefix, {
            projectFileName: "2026",
            // The probe cannot resolve this root, so the host supplies the
            // policy once, as it would in production, to bind the store.
            init: (home, projectFile) =>
              run(home, projectFile, ["--host-path-policy", platformPolicy, "init"]),
          }),
      },
    );
  });
}
