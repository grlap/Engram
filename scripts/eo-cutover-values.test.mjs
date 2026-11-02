import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import test, { after } from "node:test";

import { CutoverError, fingerprint, inventory, retiredColumns, retiredTables, verifyPreservation } from "./eo-cutover-values.mjs";
import { assertTempClean, fixtureHome, tempSnapshot } from "./test-temp.mjs";

const tempBefore = tempSnapshot();
after(() => assertTempClean(tempBefore));

const markerName = "work_schema_metadata";
const markerReason = "format marker; the new store keeps its own";
const marker = () => table(markerName, ["singleton", "schema_version"], [{ singleton: 1, schema_version: 1 }]);
function table(name, columns, rows = []) { return { name, columns, rows }; }
function transfer(tables = [], { sequences = {}, omissions = [], includeMarker = true } = {}) {
  const entries = includeMarker && !tables.some(entry => entry.name === markerName) ? [marker(), ...tables] : tables;
  const header = { format: "engram-json-export", exported_at: new Date().toISOString(),
    tables: entries.map(entry => ({ name: entry.name, columns: entry.columns, rows: entry.rows.length })), left_out: omissions };
  const sequenceText = typeof sequences === "string" ? sequences : JSON.stringify(sequences);
  const headerText = JSON.stringify(header);
  const lines = [`{"engram_export":{${headerText.slice(1, -1)},"sequences":${sequenceText}}}`];
  for (const entry of entries) for (const values of entry.rows) {
    const valueText = typeof values === "string" ? values : JSON.stringify(values);
    lines.push(`{"row":{"table":${JSON.stringify(entry.name)},"values":${valueText}}}`);
  }
  lines.push(JSON.stringify({ end: { rows: entries.reduce((sum, entry) => sum + entry.rows.length, 0) } }));
  return `${lines.join("\n")}\n`;
}
function report(extra = []) { return { left_out: [{ name: markerName, rows: 1, reason: markerReason }, ...extra] }; }
function fixture(t) {
  const home = fixtureHome("engram-cutover-values-", t);
  let next = 0;
  return async text => {
    const file = join(home, `transfer-${next++}.jsonl`);
    await writeFile(file, text);
    return { file, value: await inventory(file) };
  };
}
function refusal(code) {
  return error => {
    assert.ok(error instanceof CutoverError);
    assert.equal(error.stage, "compare");
    if (code) assert.equal(error.code, code);
    assert.equal(error.message, error.code);
    assert.deepEqual(error.detail, {});
    return true;
  };
}

test("fingerprint derives its reference from the complete file bytes", async t => {
  const text = transfer([table("durable", ["value"], [{ value: "fixture" }])]);
  const { file } = await fixture(t)(text);
  assert.equal(await fingerprint(file), createHash("sha256").update(text).digest("hex"));
});

test("large SQLite integers survive exactly and adjacent rounded values differ", async t => {
  const read = fixture(t);
  const text = transfer([table("durable", ["positive", "negative", "real"], [
    '{"positive":9223372036854775806,"negative":-9223372036854775807,"real":1.0}',
  ])]);
  const before = (await read(text)).value;
  assert.equal(verifyPreservation(before, (await read(text)).value, report()).preserved_rows, 1);
  for (const changed of [
    text.replace("9223372036854775806", "9223372036854775807"),
    text.replace("-9223372036854775807", "-9223372036854775808"),
    text.replace('"real":1.0', '"real":1'),
  ]) {
    const afterValue = (await read(changed)).value;
    assert.throws(() => verifyPreservation(before, afterValue, report()), refusal("surviving_values_or_order_differ"));
  }
});

test("blob bytes and SQLite value types remain distinct", async t => {
  const read = fixture(t);
  const bytes = Buffer.from([0, 255, 65]);
  const values = ["same", { text: "same" }, { text: '{ "a": 1 }' }, { hex: bytes.toString("hex") },
    { json: { nested: [null, true, "value"], "number token": "ordinary user key" } }];
  for (const value of values) {
    const text = transfer([table("durable", ["payload"], [{ payload: value }])]);
    const before = (await read(text)).value;
    assert.equal(verifyPreservation(before, (await read(text)).value, report()).preserved_rows, 1);
  }
  for (const [beforeValue, afterValue] of [
    ["same", { text: "same" }],
    [{ text: '{ "a": 1 }' }, { text: '{"a":1}' }],
    [{ hex: bytes.toString("hex") }, { hex: Buffer.from([0, 255, 66]).toString("hex") }],
    [{ json: { value: "1" } }, { json: { value: 1 } }],
  ]) {
    const before = (await read(transfer([table("durable", ["payload"], [{ payload: beforeValue }])]))).value;
    const afterValueInventory = (await read(transfer([table("durable", ["payload"], [{ payload: afterValue }])]))).value;
    assert.throws(() => verifyPreservation(before, afterValueInventory, report()), refusal("surviving_values_or_order_differ"));
  }
});

test("nested JSON blob number tokens cannot be forged by ordinary object keys", async t => {
  const read = fixture(t);
  const before = (await read(transfer([table("durable", ["payload"], [
    '{"payload":{"json":{"value":9007199254740993}}}',
  ])]))).value;
  const same = (await read(transfer([table("durable", ["payload"], [
    '{"payload":{"json":{"value":9007199254740993}}}',
  ])]))).value;
  verifyPreservation(before, same, report());
  for (const payload of [
    '{"payload":{"json":{"value":9007199254740992}}}',
    '{"payload":{"json":{"value":{"number token":"9007199254740993"}}}}',
  ]) {
    const changed = (await read(transfer([table("durable", ["payload"], [payload])]))).value;
    assert.throws(() => verifyPreservation(before, changed, report()), refusal("surviving_values_or_order_differ"));
  }
});

test("row ordering and empty durable table columns are preservation inputs", async t => {
  const read = fixture(t);
  const before = (await read(transfer([table("durable", ["value"], [{ value: "first" }, { value: "second" }]), table("empty", ["key"])]))).value;
  for (const tables of [
    [table("durable", ["value"], [{ value: "second" }, { value: "first" }]), table("empty", ["key"])],
    [table("durable", ["value"], [{ value: "first" }, { value: "second" }]), table("empty", ["changed_key"])],
  ]) {
    const changed = (await read(transfer(tables))).value;
    assert.throws(() => verifyPreservation(before, changed, report()), refusal("surviving_values_or_order_differ"));
  }
});

test("malformed JSON lines, ambiguous tags, declarations, and trailers fail safely", async t => {
  const read = fixture(t);
  const valid = transfer([table("durable", ["value"], [{ value: "private fixture body" }])]);
  const lines = valid.trimEnd().split("\n");
  const malformed = [
    "null\n",
    "{broken JSON\n",
    `${JSON.stringify({ engram_export: {}, row: {} })}\n`,
    valid.replace('"columns":["value"]', '"columns":["value","value"]'),
    valid.replace('"values":{"value":"private fixture body"}', '"values":null'),
    valid.replace('"values":{"value":"private fixture body"}', '"values":{"other":"private fixture body"}'),
    `${lines.slice(0, -1).join("\n")}\n`,
    `${valid}${lines.at(-1)}\n`,
    valid.replace('"end":{"rows":2}', '"end":{"rows":3}'),
  ];
  for (const text of malformed) await assert.rejects(read(text), refusal());
});

test("malformed blobs are refused for ordinary durable rows", async t => {
  const read = fixture(t);
  for (const payload of [[], { text: 7 }, { hex: "zz" }, { hex: "0" }, { unknown: "value" }, { json: {}, text: "extra" }]) {
    await assert.rejects(read(transfer([table("durable", ["payload"], [{ payload }])])), refusal());
  }
});

test("the final format marker must have exactly its permitted singleton row", async t => {
  const read = fixture(t);
  const before = (await read(transfer())).value;
  const variants = [
    transfer([], { includeMarker: false }),
    transfer([table(markerName, ["singleton", "schema_version"], [])]),
    transfer([table(markerName, ["singleton", "schema_version"], [{ singleton: 1, schema_version: 2 }])]),
    transfer([table(markerName, ["singleton", "schema_version"], [{ singleton: 2, schema_version: 1 }])]),
    transfer([table(markerName, ["singleton", "schema_version"], [{ singleton: 1, schema_version: 1 }, { singleton: 1, schema_version: 1 }])]),
    transfer([table(markerName, ["singleton", "schema_version", "extra"], [{ singleton: 1, schema_version: 1, extra: "value" }])]),
  ];
  for (const text of variants) {
    const changed = (await read(text)).value;
    assert.throws(() => verifyPreservation(before, changed, report()), refusal("format_marker_invalid"));
  }
});

test("sequence high-water marks remain exact and extra or retired sequences are refused", async t => {
  const read = fixture(t);
  const entries = [table("durable", ["value"], [])];
  const before = (await read(transfer(entries, { sequences: '{"durable":9223372036854775806}' }))).value;
  verifyPreservation(before, (await read(transfer(entries, { sequences: '{"durable":9223372036854775806}' }))).value, report());
  for (const sequences of ['{"durable":9223372036854775807}', '{"durable":9223372036854775806,"extra":1}', '{}']) {
    const changed = (await read(transfer(entries, { sequences }))).value;
    assert.throws(() => verifyPreservation(before, changed, report()), refusal("sequence_differs"));
  }
  const retiredBefore = (await read(transfer(entries, { sequences: { durable: 2, control_turn_results: 7 } }))).value;
  const retiredAfter = (await read(transfer(entries, { sequences: { durable: 2 } }))).value;
  verifyPreservation(retiredBefore, retiredAfter, report());
  const retainedRetired = (await read(transfer(entries, { sequences: { durable: 2, control_turn_results: 7 } }))).value;
  assert.throws(() => verifyPreservation(retiredBefore, retainedRetired, report()), refusal("sequence_differs"));
});

test("only the declared empty C1 tables may appear newly", async t => {
  const read = fixture(t);
  const before = (await read(transfer())).value;
  const additions = [table("work_task_sync_import_heads", ["project_id"]), table("work_task_sync_reconciliation_holds", ["project_id"])];
  verifyPreservation(before, (await read(transfer(additions))).value, report());
  for (const entries of [[table("unexpected", ["value"])], [table("work_task_sync_import_heads", ["project_id"], [{ project_id: "fixture" }])]]) {
    const changed = (await read(transfer(entries))).value;
    assert.throws(() => verifyPreservation(before, changed, report()), refusal("unexpected_new_rows"));
  }
});

test("retirement reports account for every retired table, column, and declared omission exactly", async t => {
  const read = fixture(t);
  const omission = { name: "object_fts", rows: 3, reason: "search index; import rebuilds it" };
  const oldPolicy = { singleton: 1, retained: "keep", required_assurance: "advisory", supported_effects_json: "[]", grant_ttl_seconds: 30 };
  const beforeTables = retiredTables.map((name, index) => table(name, ["value"], Array.from({ length: index }, (_, row) => ({ value: `row-${row}` }))));
  beforeTables.push(table("control_policy_state", Object.keys(oldPolicy), [oldPolicy]));
  const before = (await read(transfer(beforeTables, { omissions: [omission] }))).value;
  const afterValue = (await read(transfer([table("control_policy_state", ["singleton", "retained"], [{ singleton: 1, retained: "keep" }])], { omissions: [omission] }))).value;
  const expected = report([omission,
    ...retiredTables.map((name, index) => ({ name, rows: index, reason: "retired turn-admission table" })),
    ...retiredColumns.map(name => ({ name: `control_policy_state.${name}`, rows: 1, reason: "retired turn-admission column" })),
  ]);
  const result = verifyPreservation(before, afterValue, expected);
  assert.deepEqual(result.retired, Object.fromEntries(retiredTables.map((name, index) => [name, index])));
  assert.equal(result.preserved_rows, 1);
  const reports = [
    { left_out: expected.left_out.slice(1) },
    { left_out: [...expected.left_out, { name: "unexplained", rows: 0, reason: "extra" }] },
    { left_out: [...expected.left_out, expected.left_out[0]] },
    { left_out: expected.left_out.map(item => item.name === retiredTables[1] ? { ...item, rows: item.rows + 1 } : item) },
    { left_out: expected.left_out.map(item => item.name === retiredTables[0] ? { ...item, reason: "wrong reason" } : item) },
    { left_out: expected.left_out.map(item => item.name === `control_policy_state.${retiredColumns[0]}` ? { ...item, rows: 0 } : item) },
  ];
  for (const changedReport of reports) assert.throws(() => verifyPreservation(before, afterValue, changedReport), refusal());
});

test("former planning classification counts tree presence while accepting null and delta bodies", async t => {
  const read = fixture(t);
  const kind = "task_sync_import_planning";
  const objectRows = [
    { object_kind: kind, canonical_json: { json: null } },
    { object_kind: kind, canonical_json: { json: { project_id: "fixture-a", changes: [] } } },
    { object_kind: kind, canonical_json: { json: { project_id: "fixture-a", tree: [] } } },
    { object_kind: kind, canonical_json: { text: '{ "project_id": "fixture-a", "tree": null }' } },
    { object_kind: kind, canonical_json: { hex: Buffer.from('{"project_id":"fixture-b","tree":{}}').toString("hex") } },
    { object_kind: "other", canonical_json: { json: { project_id: "fixture-a", tree: {} } } },
  ];
  const result = (await read(transfer([table("objects", ["object_kind", "canonical_json"], objectRows)]))).value;
  assert.deepEqual([...result.formerPlanning], [["fixture-a", 2], ["fixture-b", 1]]);
  const invalidUtf8 = Buffer.concat([Buffer.from('{"tree":"'), Buffer.from([255]), Buffer.from('"}')]);
  await assert.rejects(read(transfer([table("objects", ["object_kind", "canonical_json"], [
    { object_kind: kind, canonical_json: { hex: invalidUtf8.toString("hex") } },
  ])])), refusal("export_blob_invalid"));
});
