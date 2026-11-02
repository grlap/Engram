// Compare whole-store transfer values without losing SQLite integers or blob bytes.
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { createInterface } from "node:readline";

export const retiredTables = ["control_sessions", "control_turn_results", "control_turn_grants", "control_turn_grant_supersessions"];
export const retiredColumns = ["required_assurance", "supported_effects_json", "grant_ttl_seconds"];
const addedTables = new Set(["work_task_sync_import_heads", "work_task_sync_reconciliation_holds"]);
const numberToken = Symbol("number token");
const sourceTokens = JSON.parse("9007199254740993", (_key, _value, context) => context?.source) === "9007199254740993";
const object = value => value !== null && typeof value === "object" && !Array.isArray(value) && !Object.hasOwn(value, numberToken);
const equal = (a, b) => JSON.stringify(stable(a)) === JSON.stringify(stable(b));
function requireValue(condition, code = "export_shape_invalid") { if (!condition) throw new CutoverError(code, "compare"); }

export class CutoverError extends Error {
  constructor(code, stage, project = undefined, detail = {}) {
    super(code);
    this.code = code;
    this.stage = stage;
    this.project = project;
    this.detail = detail;
  }
}

function parse(text) {
  if (!sourceTokens) {
    throw new CutoverError("node_number_tokens_unavailable", "preflight");
  }
  try {
    return JSON.parse(text, (_key, value, context) => typeof value === "number" ? { [numberToken]: context.source } : value);
  } catch {
    throw new CutoverError("export_json_invalid", "compare");
  }
}

function stable(value) {
  if (value && Object.hasOwn(value, numberToken)) return ["number", value[numberToken]];
  if (value === null) return ["null"];
  if (typeof value !== "object") return [typeof value, value];
  if (Array.isArray(value)) return ["array", value.map(stable)];
  return ["object", Object.keys(value).sort().map(key => [key, stable(value[key])])];
}

function integer(value) {
  const token = value?.[numberToken];
  if (!token || !/^(0|[1-9][0-9]*)$/.test(token)) throw new CutoverError("export_count_invalid", "compare");
  const count = Number(token);
  if (!Number.isSafeInteger(count)) throw new CutoverError("export_count_invalid", "compare");
  return count;
}

function blob(value) {
  requireValue(object(value) && Object.keys(value).length === 1, "export_blob_invalid");
  if (Object.hasOwn(value, "json")) return value.json;
  let text;
  if (typeof value.text === "string") text = value.text;
  else if (typeof value.hex === "string" && /^(?:[0-9a-fA-F]{2})*$/.test(value.hex)) {
    try { text = new TextDecoder("utf-8", { fatal: true }).decode(Buffer.from(value.hex, "hex")); }
    catch { throw new CutoverError("export_blob_invalid", "compare"); }
  }
  else throw new CutoverError("export_blob_invalid", "compare");
  return parse(text);
}

export async function fingerprint(file) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

export async function inventory(file) {
  const rows = new Map();
  const formerPlanning = new Map();
  let header;
  let end;
  let seen = 0;
  for await (const line of createInterface({ input: createReadStream(file), crlfDelay: Infinity })) {
    const item = parse(line);
    requireValue(object(item) && Object.keys(item).length === 1);
    if (item.engram_export) {
      if (header || seen || end !== undefined) throw new CutoverError("export_header_invalid", "compare");
      header = item.engram_export;
      requireValue(object(header) && Array.isArray(header.tables) && object(header.sequences) && Array.isArray(header.left_out));
      if (header.format !== "engram-json-export") throw new CutoverError("export_format_invalid", "compare");
      for (const table of header.tables) {
        requireValue(object(table) && typeof table.name === "string" && Array.isArray(table.columns)
          && table.columns.length > 0 && table.columns.every(column => typeof column === "string")
          && new Set(table.columns).size === table.columns.length);
        if (rows.has(table.name)) throw new CutoverError("export_duplicate_table", "compare");
        rows.set(table.name, { declared: integer(table.rows), count: 0, hash: createHash("sha256"), columns: table.columns });
      }
    } else if (item.row && header && end === undefined) {
      requireValue(object(item.row));
      const { table, values } = item.row;
      requireValue(object(item.row) && typeof table === "string" && object(values));
      const entry = rows.get(table);
      if (!entry) throw new CutoverError("export_undeclared_table", "compare");
      requireValue(equal(Object.keys(values).sort(), [...entry.columns].sort()), "export_row_columns_differ");
      for (const value of Object.values(values)) {
        requireValue(value === null || typeof value === "string" || (object(value) === false && value?.[numberToken] !== undefined)
          || (object(value) && Object.keys(value).length === 1 && ["json", "text", "hex"].some(key => Object.hasOwn(value, key))), "export_sqlite_value_invalid");
        if (object(value)) requireValue(Object.hasOwn(value, "json") || typeof value.text === "string"
          || (typeof value.hex === "string" && /^(?:[0-9a-fA-F]{2})*$/.test(value.hex)), "export_blob_invalid");
      }
      entry.count++;
      seen++;
      if (table === "objects" && values.object_kind === "task_sync_import_planning") {
        const body = blob(values.canonical_json);
        if (object(body) && Object.hasOwn(body, "tree")) {
          const project = typeof body.project_id === "string" ? body.project_id : "<missing project_id>";
          formerPlanning.set(project, (formerPlanning.get(project) ?? 0) + 1);
        }
      }
      if (table === "work_schema_metadata") entry.marker = values;
      if (table === "control_policy_state") for (const column of retiredColumns) delete values[column];
      entry.hash.update(JSON.stringify(stable(values)) + "\n");
    } else if (item.end && header && end === undefined) end = integer(item.end.rows);
    else throw new CutoverError("export_line_invalid", "compare");
  }
  if (!header || end !== seen) throw new CutoverError("export_truncated", "compare");
  for (const entry of rows.values()) {
    if (entry.declared !== entry.count) throw new CutoverError("export_count_differs", "compare");
    entry.digest = entry.hash.digest("hex");
    delete entry.hash;
  }
  return { rows, formerPlanning, sequences: header.sequences, omissions: header.left_out, seen };
}

export function verifyPreservation(before, after, report) {
  requireValue(object(report) && Array.isArray(report.left_out), "import_report_invalid");
  const omitted = new Map();
  for (const item of report.left_out) {
    requireValue(object(item) && typeof item.name === "string" && Number.isSafeInteger(item.rows) && item.rows >= 0
      && typeof item.reason === "string" && !omitted.has(item.name), "import_report_invalid");
    omitted.set(item.name, item);
  }
  const expectedOmissions = new Map(before.omissions.map(item => [item.name, { rows: integer(item.rows), reason: item.reason }]));
  let preserved = 0;
  const retired = {};
  for (const name of retiredTables) {
    const count = before.rows.get(name)?.count ?? 0;
    retired[name] = count;
    if (before.rows.has(name)) expectedOmissions.set(name, { rows: count, reason: "retired turn-admission table" });
    if (before.rows.has(name) && (omitted.get(name)?.rows !== count || after.rows.has(name))) {
      throw new CutoverError("retirement_count_differs", "compare");
    }
  }
  const policy = before.rows.get("control_policy_state");
  for (const name of retiredColumns) {
    if (policy?.columns.includes(name)) expectedOmissions.set(`control_policy_state.${name}`, { rows: policy.count, reason: "retired turn-admission column" });
    if (after.rows.get("control_policy_state")?.columns.includes(name)) throw new CutoverError("retired_column_present", "compare");
    if (policy?.columns.includes(name) && omitted.get(`control_policy_state.${name}`)?.rows !== policy.count) {
      throw new CutoverError("retirement_count_differs", "compare");
    }
  }
  const marker = after.rows.get("work_schema_metadata");
  requireValue(marker?.count === 1 && equal([...marker.columns].sort(), ["schema_version", "singleton"])
    && integer(marker.marker?.schema_version) === 1 && integer(marker.marker?.singleton) === 1, "format_marker_invalid");
  if (before.rows.has("work_schema_metadata")) expectedOmissions.set("work_schema_metadata", {
    rows: before.rows.get("work_schema_metadata").count, reason: "format marker; the new store keeps its own" });
  requireValue(omitted.size === expectedOmissions.size && [...expectedOmissions].every(([name, value]) => {
    const actual = omitted.get(name); return actual?.rows === value.rows && actual?.reason === value.reason;
  }), "retirement_report_differs");
  for (const [name, expected] of before.rows) {
    if (retiredTables.includes(name) || name === "work_schema_metadata") continue;
    const actual = after.rows.get(name);
    const columns = name === "control_policy_state" ? expected.columns.filter(column => !retiredColumns.includes(column)) : expected.columns;
    if (!actual || !equal([...columns].sort(), [...actual.columns].sort()) || actual.count !== expected.count || actual.digest !== expected.digest) throw new CutoverError("surviving_values_or_order_differ", "compare");
    preserved += expected.count;
  }
  for (const [name, actual] of after.rows) {
    if (!before.rows.has(name) && (!addedTables.has(name) || actual.count !== 0)) throw new CutoverError("unexpected_new_rows", "compare");
  }
  const expectedSequences = Object.fromEntries(Object.entries(before.sequences).filter(([name]) => !retiredTables.includes(name)));
  requireValue(equal(expectedSequences, after.sequences), "sequence_differs");
  return { preserved_rows: preserved, retired };
}
