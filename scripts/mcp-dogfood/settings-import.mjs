import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { closeFixtureClients } from "../test-temp.mjs";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { after } from "node:test";

export function registerTaskSyncSettings(support) {
  const { test, fixtureHome, removeFixtureHomes, structured, structuredError, buildAndInit, McpClient } = support;
async function closeSettingsFixture(engramHome, writer, reader) {
  try { await closeFixtureClients(writer, reader); }
  finally { removeFixtureHomes(engramHome); }
}

for (const rejectReader of [false, true]) {
  test(`task-sync settings cleanup closes both clients after ${rejectReader ? "both close methods reject" : "writer close rejects"}`, async (t) => {
    const engramHome = fixtureHome("engram-settings-close-", t);
    const clients = [];
    const attempts = [];
    const writerError = new Error("Writer close diagnostic");
    const readerError = new Error("Reader close diagnostic");
    try {
      buildAndInit(engramHome);
      const writer = new McpClient(engramHome, "settings-close-writer");
      const closeWriter = writer.close.bind(writer);
      clients.push({ client: writer, close: closeWriter });
      const reader = new McpClient(engramHome, "settings-close-reader", undefined, "settings-close-reader", ["--read-only"]);
      const closeReader = reader.close.bind(reader);
      clients.push({ client: reader, close: closeReader });
      await writer.initialize();
      await reader.initialize();
      writer.close = async () => {
        attempts.push("writer");
        await closeWriter();
        throw writerError;
      };
      reader.close = async () => {
        attempts.push("reader");
        await closeReader();
        if (rejectReader) throw readerError;
      };
      const error = await closeSettingsFixture(engramHome, writer, reader).then(() => undefined, (failure) => failure);
      assert.deepEqual(attempts, ["writer", "reader"]);
      for (const { client } of clients) {
        assert.equal(client.closeObservation.exit?.code, 0);
        assert.equal(client.closeObservation.exit?.signal, null);
        assert.equal(client.closeObservation.close?.code, 0);
        assert.equal(client.closeObservation.close?.signal, null);
      }
      assert.equal(existsSync(engramHome), false);
      assert.ok(error instanceof AggregateError);
      assert.equal(error.errors.length, rejectReader ? 2 : 1);
      assert.equal(error.errors[0], writerError);
      if (rejectReader) assert.equal(error.errors[1], readerError);
    } finally {
      try { await closeFixtureClients(...clients.filter(({ client }) => !client.closeObservation.close)); }
      finally { removeFixtureHomes(engramHome); }
    }
  });
}

test("task-sync settings wire refuses instructions on status and disable and preserves settings", async (t) => {
  const engramHome = fixtureHome("engram-settings-instruction-", t);
  let writer, reader;
  try {
    buildAndInit(engramHome);
    writer = new McpClient(engramHome, "settings-writer");
    reader = new McpClient(engramHome, "settings-reader", undefined, "settings-reader", ["--read-only"]);
    await writer.initialize();
    await reader.initialize();
    const enabled = structured(await writer.call("task_sync_settings", { action: "enable", user_instruction: "Original enable request." }));
    assert.equal(enabled.state, "enabled");
    const project = join(engramHome, "projects", readdirSync(join(engramHome, "projects"))[0]);
    const setting = join(project, "task-sync-setting.json");
    const before = readFileSync(setting);
    const entries = readdirSync(project).sort();
    for (const [client, action, code] of [
      [writer, "status", "invalid_user_instruction"],
      [writer, "disable", "invalid_user_instruction"],
      [reader, "status", "invalid_user_instruction"],
      [reader, "disable", "mcp_read_only_refused"],
    ]) {
      const result = await client.call("task_sync_settings", { action, user_instruction: "rejected instruction sentinel" });
      structuredError(result, code);
      assert.doesNotMatch(JSON.stringify(result), /rejected instruction sentinel/u);
      assert.deepEqual(readFileSync(setting), before);
      assert.deepEqual(readdirSync(project).sort(), entries);
    }
    for (const arguments_ of [{ action: "status", user_instruction: 1 }, { action: "status", unknown: true }]) {
      const result = await writer.call("task_sync_settings", arguments_);
      assert.equal(result.isError, true);
      assert.equal(result.structuredContent, undefined);
      assert.match(result.content.map(({ text }) => text).join("\n"), /failed to deserialize parameters: field/u);
      assert.deepEqual(readFileSync(setting), before);
    }
    for (const client of [writer, reader]) {
      assert.deepEqual(structured(await client.call("task_sync_settings", { action: "status" })), enabled);
      assert.deepEqual(structured(await client.call("task_sync_settings", { action: "status", user_instruction: null })), enabled);
    }
    const disabled = structured(await writer.call("task_sync_settings", { action: "disable", user_instruction: null }));
    assert.equal(disabled.state, "disabled");
  } finally {
    await closeSettingsFixture(engramHome, writer, reader);
  }
});

}

export function registerFileIntakeAndReplay(support) {
  const { root, binary, test, fixtureHome, removeFixtureHomes, receipt, buildAndInit, cliText, cliJson, McpClient } = support;
test("file intake notifies ordinary CLI and MCP reads without steering local work", async (t) => {
  const engramHome = fixtureHome("engram-source-intake-", t);
  let client;
  try {
    buildAndInit(engramHome);
    const session = "source-reader";
    client = new McpClient(engramHome, session);
    await client.initialize();
    const held = receipt(await client.call("add", { title: "Existing local work" })).work.short_ref;
    receipt(await client.call("claim", { work_ref: held }));
    const file = join(engramHome, "intake.json");
    const input = {
      snapshot: {
        schema_version: 1, adapter_kind: "planner", canonical_ref: "plan/item-1",
        projected: { title: "Outside title", body: "Outside context", status: "closed", owner: "outside-owner" },
        captured_at: new Date().toISOString(), source_revision: "1", fingerprint: "revision-1",
        canonical_url: null, payload_hash: createHash("sha256").update("test source payload").digest("hex"),
        raw: { planner_context: ["untrusted context"] },
      },
      draft: { title: "Authored local title", outcome: "Authored local outcome" },
    };
    let importingSession = session;
    // Match McpClient's default attribution: host context must not alter this fixture.
    const importEnvironment = { ...process.env };
    delete importEnvironment.ENGRAM_ACTOR_CONTEXT;
    const invoke = (...args) => spawnSync(binary, ["--home", engramHome, "import",
      "--actor-id", importingSession, "--session-id", importingSession, ...args],
      { cwd: root, encoding: "utf8", env: importEnvironment });
    const json = (...args) => {
      const result = invoke(...args);
      assert.equal(result.status, 0, result.stderr);
      return JSON.parse(result.stdout);
    };
    writeFileSync(file, JSON.stringify(input));
    const preview = json("preview", file);
    assert.equal(preview.effect, "create");
    assert.deepEqual(preview.draft.acceptance, []);
    assert.equal(json("lookup", "planner", "plan/item-1"), null);
    const imported = json("apply", file, "--preview", preview.preview_token);
    assert.deepEqual(json("apply", file, "--preview", preview.preview_token), imported);
    const work_ref = imported.work_ref;
    const before = receipt(await client.call("show", { work_ref }));
    assert.equal(before.status.work.title, input.draft.title);
    assert.equal(before.status.work.outcome, input.draft.outcome);
    assert.deepEqual(before.status.work.acceptance, []);
    assert.equal(before.source.notice_count, 0);
    assert.equal(before.source.local_work_unchanged_by_notices, undefined);
    assert.doesNotMatch(cliText(engramHome, session, "show", work_ref), /change notices|local work not changed by notices/u);
    importingSession = "source-notifier";
    for (const revision of [2, 3]) {
      delete input.draft;
      input.snapshot.source_revision = String(revision);
      input.snapshot.projected.body = `Outside changed context ${revision}`;
      writeFileSync(file, JSON.stringify(input));
      const refresh = json("preview", file);
      assert.equal(refresh.effect, "notify");
      const result = json("apply", file, "--preview", refresh.preview_token);
      assert.equal(result.work_revision, imported.work_revision);
      assert.equal(result.cited_snapshot, imported.snapshot);
      assert.notEqual(result.snapshot, result.cited_snapshot);
    }
    const detail = json("lookup", "planner", "plan/item-1");
    assert.equal(detail.notice_count, 2);
    assert.equal(detail.notices_omitted, 1);
    assert.equal(detail.cited_source.projected.body, "Outside context");
    assert.equal(detail.latest_proposed_source.projected.body, "Outside changed context 3");
    assert.equal(detail.latest_notice.actor.session_id, importingSession);
    assert.equal(json("lookup", "Planner", "plan/item-1"), null);
    const after = receipt(await client.call("show", { work_ref }));
    const { source: beforeSource, ...beforeLocal } = before;
    const { source: afterSource, ...afterLocal } = after;
    assert.deepEqual(afterLocal, beforeLocal);
    assert.equal(afterSource.notice_count, 2);
    assert.equal(afterSource.notices_omitted, 1);
    assert.equal(afterSource.local_work_unchanged_by_notices, undefined);
    assert.match(afterSource.detail, /engram import lookup -- 'planner' 'plan\/item-1'/u);
    assert.deepEqual(cliJson(engramHome, session, "show", work_ref), after);
    const peerOrientation = receipt(await client.call("next", { peek: true }));
    assert.equal(peerOrientation.peek.delivery_advanced, false);
    const omittedChanges = peerOrientation.omissions
      ?.filter(row => row.section === "changes").reduce((sum, row) => sum + row.omitted_count, 0) ?? 0;
    assert.ok(peerOrientation.changes.length + omittedChanges >= 2);
    if (omittedChanges > 0) assert.equal(peerOrientation.peek.more_changes_available, true);
    const detailWords = peerOrientation.details.split(" ");
    assert.deepEqual(detailWords, ["engram", "work", "next", "--peek", "--verbose"]);
    const peerDetails = receipt(await client.call(detailWords[2], {
      peek: detailWords.includes("--peek"), verbose: detailWords.includes("--verbose"),
    }));
    const cliDetails = cliJson(engramHome, session, ...detailWords.slice(2));
    assert.deepEqual(peerDetails.changes_by_others, cliDetails.changes_by_others);
    const peerChanges = peerDetails.changes_by_others
      .filter((line) => line.includes("external source changed"));
    assert.equal(peerChanges.length, 2, JSON.stringify(peerOrientation));
    for (const line of peerChanges) {
      assert.match(line, /by peer-[0-9a-f]{24}/u);
      assert.ok(line.includes(work_ref));
      assert.ok(!line.includes(importingSession));
    }
    const text = cliText(engramHome, session, "show", work_ref);
    assert.ok(text.includes(`source detail: ${afterSource.detail}`));
    assert.match(text, /2 change notices \(1 older not shown\)/u);
    assert.match(text, /latest source notice: \d{2}:\d{2} UTC/u);
    assert.doesNotMatch(text, /latest source notice: .*\.\d/u);
    assert.ok(Buffer.byteLength(text) < 12288);
    assert.ok(Buffer.byteLength(JSON.stringify(after)) < 12288);
    for (const hidden of [imported.snapshot, "outside-owner", "Outside changed context", session]) {
      assert.ok(!JSON.stringify(after).includes(hidden));
      assert.ok(!text.includes(hidden));
    }
    assert.equal(json("preview", file).effect, "already_known");
    const note = receipt(await client.call("note", { text: "Bare write still targets held local work" }));
    assert.equal(note.work.short_ref, held);
    input.draft = { title: "Must not overwrite", outcome: "Must refuse" };
    writeFileSync(file, JSON.stringify(input));
    const refused = invoke("preview", file);
    assert.notEqual(refused.status, 0);
    assert.match(refused.stderr, /source refresh takes no local draft/u);
    assert.deepEqual(receipt(await client.call("show", { work_ref })).source, afterSource);
    input.snapshot.canonical_ref = "--help";
    writeFileSync(file, JSON.stringify(input));
    const dashPreview = json("preview", file);
    const dashItem = json("apply", file, "--preview", dashPreview.preview_token);
    const dashShow = receipt(await client.call("show", { work_ref: dashItem.work_ref }));
    assert.equal(dashShow.source.detail, "engram import lookup -- 'planner' '--help'");
    assert.ok(cliText(engramHome, session, "show", dashItem.work_ref).includes(`source detail: ${dashShow.source.detail}`));
    const dashLookup = json("lookup", "--", "planner", "--help");
    assert.equal(dashLookup.work_ref, dashItem.work_ref);
    assert.equal(dashLookup.source_key.canonical_ref, "--help");
  } finally {
    try { if (client) await client.close(); }
    finally { removeFixtureHomes(engramHome); }
  }
});

test("import file reader refuses input above one MiB before persistence", (t) => {
  const engramHome = fixtureHome("engram-import-input-bound-", t);
  try {
    buildAndInit(engramHome);
    const file = join(engramHome, "oversized.json");
    const before = cliJson(engramHome, "bound-reader", "ls", "--all");
    writeFileSync(file, Buffer.alloc(1024 * 1024 + 1, 32));
    const result = spawnSync(binary, ["--home", engramHome, "import", "--actor-id", "bound-reader",
      "--session-id", "bound-reader", "preview", file], { cwd: root, encoding: "utf8" });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /import input exceeds the 1048576-byte limit/u);
    assert.deepEqual(cliJson(engramHome, "bound-reader", "ls", "--all"), before);
  } finally { removeFixtureHomes(engramHome); }
});

test("printed shell session preserves import and observation replay identity", (t) => {
  const engramHome = fixtureHome("engram-printed-session-replay-", t);
  try {
    buildAndInit(engramHome);
    const environment = { ...process.env };
    delete environment.ENGRAM_SESSION_ID;
    delete environment.ENGRAM_ACTOR_CONTEXT;
    const invoke = (family, session, ...args) => {
      const result = spawnSync(binary, ["--home", engramHome, family,
        "--actor-id", "shell-author", ...(session ? ["--session-id", session] : []), ...args],
      { cwd: root, encoding: "utf8", env: environment });
      assert.equal(result.status, 0, result.stderr);
      return { value: JSON.parse(result.stdout), stderr: result.stderr };
    };
    const replay = (family, ...args) => {
      const first = invoke(family, null, ...args);
      const session = /--session-id (local-process-v1-[^\s]+)/u.exec(first.stderr)?.[1];
      assert.ok(session, first.stderr);
      const retry = invoke(family, session, ...args);
      if (family === "work") {
        const { effective_session_id, ...originalReceipt } = first.value;
        assert.equal(effective_session_id, session);
        assert.equal(retry.value.effective_session_id, undefined);
        assert.deepEqual(retry.value, originalReceipt);
      } else {
        assert.deepEqual(retry.value, first.value);
      }
      return { value: first.value, session };
    };
    const file = join(engramHome, "intake.json");
    const input = {
      snapshot: {
        schema_version: 1, adapter_kind: "planner", canonical_ref: "printed-session",
        projected: { title: "External", body: "Context", status: null, owner: null },
        captured_at: new Date().toISOString(), source_revision: "1", fingerprint: "first",
        canonical_url: null, payload_hash: createHash("sha256").update("source").digest("hex"), raw: {},
      },
      draft: { title: "Local item", outcome: "Authored outcome" },
    };
    writeFileSync(file, JSON.stringify(input));
    let preview = invoke("import", "previewer", "preview", file).value;
    const created = replay("import", "apply", file, "--preview", preview.preview_token);
    delete input.draft;
    input.snapshot.source_revision = "2";
    writeFileSync(file, JSON.stringify(input));
    preview = invoke("import", "previewer", "preview", file).value;
    const notified = replay("import", "apply", file, "--preview", preview.preview_token);
    assert.equal(notified.value.effect, "notify");
    const detail = invoke("import", "reader", "lookup", "planner", "printed-session").value;
    assert.equal(detail.notice_count, 1);
    assert.equal(detail.latest_notice.actor.session_id, notified.session);
    assert.ok(detail.latest_notice.actor.provenance_chain.some((link) =>
      link.source === "defaulted:process_session" && link.reference === "session_id"));
    const observation = replay("work", "note", created.value.work_ref, "One non-holder observation", "--json");
    const notes = invoke("work", "reader", "show", created.value.work_ref, "--notes", "--json").value;
    assert.equal(notes.notes.filter((note) => note.summary.includes("One non-holder observation")).length, 1);
    assert.equal(observation.value.work.short_ref, created.value.work_ref);
  } finally {
    removeFixtureHomes(engramHome);
  }
});

}
