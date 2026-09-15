#!/usr/bin/env node

// Independent current-profile round-trip oracle. This deliberately does not
// consume the migration manifest or use Engram's export/row-encoding routines.
// Run only on isolated, coherent copies, not two changing live stores.
import { DatabaseSync } from "node:sqlite";
import { createHash } from "node:crypto";
import { statSync } from "node:fs";
import { isDeepStrictEqual } from "node:util";
import { pathToFileURL } from "node:url";

const quote = (name) => `"${name.replaceAll('"', '""')}"`;
const ftsKeys = new Map([
  ["object_fts", "object_hash"],
  ["work_catalog_fts", "work_id"],
]);

function requireEqual(left, right, label) {
  // Never print private row contents, even on refusal.
  if (!isDeepStrictEqual(left, right)) throw new Error(`${label} differs`);
}

function inventory(db) {
  const schema = db.prepare(
    "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name",
  ).all();
  const tables = db.prepare("PRAGMA main.table_list").all()
    .filter((table) => table.schema === "main" && table.name !== "sqlite_schema")
    .sort((a, b) => a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
  return { schema, tables };
}

function rowQuery(db, table) {
  const columns = db.prepare(`PRAGMA main.table_xinfo(${quote(table.name)})`).all();
  const names = columns.filter((column) => column.hidden !== 1).map((column) => column.name);
  let order;
  if (table.type === "virtual") {
    const key = ftsKeys.get(table.name);
    if (!key || !names.includes(key)) throw new Error("unsupported virtual table");
    order = `${quote(key)} COLLATE BINARY`;
  } else if (table.wr) {
    order = columns.filter((column) => column.pk > 0)
      .sort((a, b) => a.pk - b.pk).map((column) => quote(column.name)).join(", ");
  } else {
    const declared = new Set(names.map((name) => name.toLowerCase()));
    const alias = ["rowid", "_rowid_", "oid"].find((name) => !declared.has(name));
    if (!alias) throw new Error("all rowid aliases are hidden");
    names.unshift(alias);
    order = quote(alias);
  }
  if (!order || !names.length) throw new Error("unsupported table ordering");
  // CAST text to BLOB before it crosses the driver, preserving invalid UTF-8
  // and NULs. A separate storage-class cell distinguishes text from BLOB.
  const fields = names.flatMap((name, index) => {
    const column = quote(name);
    return [
      `typeof(${column}) AS t${index}`,
      `CASE WHEN typeof(${column}) = 'text' THEN CAST(${column} AS BLOB) ELSE ${column} END AS v${index}`,
    ];
  });
  const statement = db.prepare(`SELECT ${fields.join(", ")} FROM ${quote(table.name)} ORDER BY ${order}`);
  statement.setReadBigInts(true);
  return { rows: statement.iterate(), cells: names.length };
}

function bytesFor(value) {
  if (value === null) return Buffer.alloc(0);
  if (typeof value === "bigint") return Buffer.from(value.toString());
  if (typeof value === "number") {
    const bytes = Buffer.alloc(8);
    bytes.writeDoubleBE(value);
    return bytes;
  }
  if (ArrayBuffer.isView(value)) return Buffer.from(value.buffer, value.byteOffset, value.byteLength);
  throw new Error("unsupported SQLite cell value");
}

function frame(digest, bytes) {
  const size = Buffer.alloc(8);
  size.writeBigUInt64BE(BigInt(bytes.length));
  digest.update(size).update(bytes);
}

function compareTable(left, right, table) {
  const l = rowQuery(left, table);
  const r = rowQuery(right, table);
  requireEqual(l.cells, r.cells, "column count");
  const digest = createHash("sha256");
  let count = 0;
  try {
    for (;;) {
      const a = l.rows.next();
      const b = r.rows.next();
      requireEqual(a.done, b.done, `table ${table.name} row count`);
      if (a.done) break;
      for (let i = 0; i < l.cells; i++) {
        requireEqual(a.value[`t${i}`], b.value[`t${i}`], `table ${table.name} row ${count} cell type`);
        const before = bytesFor(a.value[`v${i}`]);
        const after = bytesFor(b.value[`v${i}`]);
        if (!before.equals(after)) throw new Error(`table ${table.name} row ${count} cell bytes differ`);
        frame(digest, Buffer.from(a.value[`t${i}`]));
        frame(digest, before);
      }
      count++;
    }
  } finally {
    l.rows.return();
    r.rows.return();
  }
  return { table: table.name, rows: count, sha256: digest.digest("hex"),
    comparison: table.type === "virtual" ? "logical_fts" : "exact_typed_rows" };
}

export function compareStores(sourcePath, targetPath) {
  const sourceFile = statSync(sourcePath, { bigint: true });
  const targetFile = statSync(targetPath, { bigint: true });
  if (sourceFile.dev === targetFile.dev && sourceFile.ino === targetFile.ino) {
    throw new Error("source and target must be different files");
  }
  const source = new DatabaseSync(sourcePath, { readOnly: true });
  let target;
  try {
    target = new DatabaseSync(targetPath, { readOnly: true });
    for (const db of [source, target]) db.exec("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON; BEGIN;");
    const before = inventory(source);
    const after = inventory(target);
    requireEqual(before, after, "schema/table inventory");
    for (const pragma of ["encoding", "user_version", "application_id"]) {
      requireEqual(source.prepare(`PRAGMA ${pragma}`).get(), target.prepare(`PRAGMA ${pragma}`).get(), pragma);
    }
    const compared = [];
    const rebuilt = [];
    for (const table of before.tables) {
      if (table.type === "shadow") {
        if (![...ftsKeys.keys()].some((name) => table.name.startsWith(`${name}_`)
          && before.tables.some((parent) => parent.name === name && parent.type === "virtual"))) {
          throw new Error("unsupported shadow table");
        }
        rebuilt.push(table.name);
        continue;
      }
      if (!["table", "virtual"].includes(table.type)) throw new Error("unsupported table category");
      compared.push(compareTable(source, target, table));
    }
    for (const db of [source, target]) db.exec("COMMIT;");
    return { equal: true, scope: "same-profile logical store; not upgrade or activation proof",
      compared, rebuilt_fts_physical_tables: rebuilt,
      empty_tables: compared.filter((table) => table.rows === 0).map((table) => table.table) };
  } finally {
    target?.close();
    source.close();
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  if (process.argv.length !== 4) {
    console.error("Usage: node scripts/compare-migration-stores.mjs SOURCE_COPY.db IMPORTED_COPY.db");
    process.exitCode = 2;
  } else {
    try { console.log(JSON.stringify(compareStores(process.argv[2], process.argv[3]), null, 2)); }
    catch (error) { console.error(`Migration comparison refused: ${error.message}`); process.exitCode = 1; }
  }
}
