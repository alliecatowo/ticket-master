#!/usr/bin/env node
// `pnpm gen`: writes `src/generated.ts` from the JSON Schema `tm-server` exposes at
// `GET /schema` (SPEC.md §18.1). Tries a live server first (`TM_SERVER_URL`, default
// `http://127.0.0.1:4173`); when no server answers within the timeout, falls back to the
// checked-in snapshot at `schema/snapshot.json` so `pnpm gen`/`pnpm build` never require a
// running server to succeed (and CI's drift check gets a stable baseline to diff against).
//
// `GET /schema` is itself deliberately partial today (see the doc comment on
// `tm_server::routes::get_schema`): it only covers `TicketId`, `ParticipantId`, `TicketKind`,
// `TicketState`, `CreateTicketRequest` and `TransitionRequest`. This generator renders exactly
// what the schema document contains, nothing more — the rest of the domain (Ticket, Lease,
// Decision, event payloads, ...) is hand-written in `src/domain.ts` until the server grows a
// fuller schema, at which point it belongs here instead.

import { writeFile, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));
const root = path.resolve(here, "..");
const outFile = path.join(root, "src", "generated.ts");
const snapshotFile = path.join(root, "schema", "snapshot.json");

async function fetchLiveSchema() {
  const base = process.env.TM_SERVER_URL ?? "http://127.0.0.1:4173";
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 500);
  try {
    const res = await fetch(new URL("/schema", base), { signal: controller.signal });
    if (!res.ok) return null;
    return await res.json();
  } catch {
    return null;
  } finally {
    clearTimeout(timeout);
  }
}

async function loadSchema() {
  const live = await fetchLiveSchema();
  if (live) {
    return { schema: live, source: process.env.TM_SERVER_URL ?? "http://127.0.0.1:4173" };
  }
  const raw = await readFile(snapshotFile, "utf8");
  return { schema: JSON.parse(raw), source: "schema/snapshot.json (no live server)" };
}

function toPascalCase(name) {
  return name.replace(/(^\w|_\w)/g, (m) => m.replace("_", "").toUpperCase());
}

function refName(ref) {
  const m = /^#\/definitions\/(.+)$/.exec(ref);
  if (!m) throw new Error(`unsupported $ref: ${ref}`);
  return m[1];
}

/** Render one JSON Schema node as a TS type expression (not a declaration). */
function renderType(node, definitions) {
  if (!node) return "unknown";
  if (node.$ref) return refName(node.$ref);
  if (Array.isArray(node.enum)) {
    return node.enum.map((v) => JSON.stringify(v)).join(" | ");
  }
  switch (node.type) {
    case "string":
      return "string";
    case "integer":
    case "number":
      return "number";
    case "boolean":
      return "boolean";
    case "array":
      return `Array<${renderType(node.items, definitions)}>`;
    case "object":
      if (node.properties) {
        return renderObjectLiteral(node, definitions);
      }
      return "Record<string, unknown>";
    default:
      return "unknown";
  }
}

function renderObjectLiteral(node, definitions) {
  const required = new Set(node.required ?? []);
  const props = Object.entries(node.properties ?? {}).map(([key, value]) => {
    const optional = required.has(key) ? "" : "?";
    return `  ${JSON.stringify(key)}${optional}: ${renderType(value, definitions)};`;
  });
  return `{\n${props.join("\n")}\n}`;
}

/** Render one top-level `definitions` entry as an exported TS declaration. */
function renderDefinition(name, node, definitions) {
  const typeName = toPascalCase(name);
  const lines = [];
  if (node.description) lines.push(`/** ${node.description} */`);
  if (node.pattern) lines.push(`/** Wire pattern: ${node.pattern} */`);

  if (node.type === "object" && node.properties) {
    lines.push(`export interface ${typeName} ${renderObjectLiteral(node, definitions)}`);
  } else if (node.type === "object") {
    // No `properties` in the schema (e.g. `TransitionRequest`, documented only): render as an
    // opaque record rather than inventing shape the schema doesn't actually assert.
    lines.push(`export type ${typeName} = Record<string, unknown>;`);
  } else {
    lines.push(`export type ${typeName} = ${renderType(node, definitions)};`);
  }
  return lines.join("\n");
}

async function main() {
  const { schema, source } = await loadSchema();
  const definitions = schema.definitions ?? {};
  const names = Object.keys(definitions);

  const header = `// GENERATED FILE — do not edit by hand.
//
// Produced by \`pnpm gen\` (scripts/gen.mjs) from the JSON Schema served at \`GET /schema\`
// (source for this run: ${source}). Re-run \`pnpm gen\` after the server's schema changes; CI
// regenerates and fails the build on drift, so this file and the server can never silently
// disagree (SPEC.md §18.1).
//
// \`GET /schema\` is itself partial (see the comment on this file's generator): only the
// definitions below exist there today. The rest of the Ticketmaster wire domain is hand-written
// in \`src/domain.ts\`.
`;

  const body = names.map((name) => renderDefinition(name, definitions[name], definitions)).join("\n\n");

  const out = `${header}\n${body}\n`;
  await writeFile(outFile, out, "utf8");
  console.log(`wrote ${path.relative(root, outFile)} from ${names.length} schema definitions (${source})`);
}

main().catch((err) => {
  console.error(err);
  process.exitCode = 1;
});
