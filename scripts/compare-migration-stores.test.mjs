import assert from "node:assert/strict";
import { copyFileSync, existsSync, linkSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import test, { after } from "node:test";
import { compareStores } from "./compare-migration-stores.mjs";
import { fixtureHome, tempSnapshot, assertTempClean } from "./test-temp.mjs";

const before = tempSnapshot();
after(() => assertTempClean(before));

function execute(path, sql) {
  const db = new DatabaseSync(path);
  try { db.exec(sql); } finally { db.close(); }
}

function pair(t) {
  const home = fixtureHome("migration-comparison-", t);
  const source = join(home, "source.db");
  const target = join(home, "target.db");
  execute(source, `
    CREATE TABLE arbitrary_empty (value BLOB);
    CREATE TABLE cells (value);
    INSERT INTO cells(rowid,value) VALUES
      (7,NULL), (9,9223372036854775807), (11,1.25),
      (13,CAST(x'ff00' AS TEXT)), (15,x'ff00'), (17,''), (19,x'');
    CREATE TABLE keyed (k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID;
    INSERT INTO keyed VALUES ('b', x'ff'), ('a', x'00');
    CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, body TEXT);
    INSERT INTO events VALUES (99, 'deleted');
    DELETE FROM events;
    CREATE TABLE migration_original_rows (body BLOB);
    INSERT INTO migration_original_rows VALUES (x'70726976617465');
    CREATE TABLE control_sessions (session TEXT, token BLOB);
    INSERT INTO control_sessions VALUES ('session', x'00ff00');
  `);
  copyFileSync(source, target);
  return { source, target };
}

test("complete comparison preserves raw types, empty tables, rowids and high water", (t) => {
  const { source, target } = pair(t);
  const original = readFileSync(source);
  const imported = readFileSync(target);
  const report = compareStores(source, target);
  assert.equal(report.equal, true);
  assert.equal(report.compared.find((table) => table.table === "cells").rows, 7);
  assert.ok(report.empty_tables.includes("arbitrary_empty"));
  assert.ok(report.empty_tables.includes("events"));
  assert.ok(report.compared.some((table) => table.table === "sqlite_sequence" && table.rows === 1));
  assert.deepEqual(readFileSync(source), original);
  assert.deepEqual(readFileSync(target), imported);
});

for (const [name, sql, reason] of [
  ["missing empty category", "DROP TABLE arbitrary_empty", /inventory/],
  ["text changed to blob", "UPDATE cells SET value=x'ff00' WHERE rowid=13", /cell type/],
  ["invalid UTF-8 bytes changed", "UPDATE cells SET value=CAST(x'fe00' AS TEXT) WHERE rowid=13", /cell bytes/],
  ["large integer changed", "UPDATE cells SET value=9223372036854775806 WHERE rowid=9", /cell bytes/],
  ["float changed", "UPDATE cells SET value=1.5 WHERE rowid=11", /cell bytes/],
  ["rowid changed", "UPDATE cells SET rowid=8 WHERE rowid=7", /cell bytes/],
  ["sequence regressed", "UPDATE sqlite_sequence SET seq=0", /cell bytes/],
  ["prior provenance lost", "DELETE FROM migration_original_rows", /row count/],
  ["session lost", "DELETE FROM control_sessions", /row count/],
  ["schema pragma changed", "PRAGMA user_version=2", /user_version/],
]) {
  test(`comparison refuses ${name}`, (t) => {
    const { source, target } = pair(t);
    execute(target, sql);
    assert.throws(() => compareStores(source, target), reason);
  });
}

test("declared FTS compares visible content, not rebuilt shadow layout or rowids", (t) => {
  const { source, target } = pair(t);
  execute(source, `CREATE VIRTUAL TABLE object_fts USING fts5(object_hash UNINDEXED,title,body);
    INSERT INTO object_fts(rowid,object_hash,title,body) VALUES (7,'hash','title','body');`);
  execute(target, `CREATE VIRTUAL TABLE object_fts USING fts5(object_hash UNINDEXED,title,body);
    INSERT INTO object_fts(rowid,object_hash,title,body) VALUES (19,'hash','title','body');`);
  const report = compareStores(source, target);
  assert.equal(report.compared.find((table) => table.table === "object_fts").comparison, "logical_fts");
  assert.ok(report.rebuilt_fts_physical_tables.length > 0);
  execute(target, "UPDATE object_fts SET body='changed'");
  assert.throws(() => compareStores(source, target), /cell bytes/);
});

test("unknown virtual tables refuse instead of being excluded", (t) => {
  const { source, target } = pair(t);
  for (const path of [source, target]) execute(path, "CREATE VIRTUAL TABLE unknown_fts USING fts5(body)");
  assert.throws(() => compareStores(source, target), /unsupported virtual table/);
});

test("absent comparison target is not initialized", (t) => {
  const { source, target } = pair(t);
  const absent = `${target}.absent`;
  assert.throws(() => compareStores(source, absent));
  assert.equal(existsSync(absent), false);
});

test("self comparison, including hard-link aliases, is refused", (t) => {
  const { source, target } = pair(t);
  assert.throws(() => compareStores(source, source), /different files/);
  const alias = `${target}.link`;
  linkSync(source, alias);
  assert.throws(() => compareStores(source, alias), /different files/);
});

test("refusal never includes private row contents", (t) => {
  const { source, target } = pair(t);
  execute(target, "UPDATE control_sessions SET token=x'707269766174652d736563726574'");
  assert.throws(() => compareStores(source, target), (error) => {
    assert.match(error.message, /cell bytes/);
    assert.ok(!error.message.includes("private-secret"));
    assert.ok(!error.message.includes("70726976617465"));
    return true;
  });
});
