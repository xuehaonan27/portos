#!/usr/bin/env node
// plugin.js — the PortOS adapter (thin). Exposes the transport-agnostic tool
// surface (tools.js) as `browser::*` verbs on the PortOS plugin ABI; the seam
// that used to hold the MCP adapter now holds this. Only this file knows the
// kernel protocol.
//
// Seam ③ cashes in here: the sink runs in "kernel" mode, so oversized
// payloads land in the kernel CAS (taint-labeled by page origin) and the
// model receives handle + preview instead of the full text. Screenshots stop
// being loose files: the bytes are ingested as an image artifact and the
// verb returns its handle (the temp path rides along for a headful human).

import { readFile } from "node:fs/promises";
import { createWorkshop } from "./tools.js";
import { makeSink } from "./sink.js";
import { servePlugin } from "../../../sdk/js/client.js";

const typeFor = (kind) =>
  kind === "snapshot" ? "web/page-snapshot" : kind === "text" ? "text/plain" : "application/json";

const labelsFor = (origin) => (origin ? { integ: [`web:${origin}`] } : null);

// The kernel client only exists once servePlugin connects; the sink binds to
// it lazily (puts can only happen inside a call, by which time it is set).
const clientRef = { current: null };

const sink = makeSink({
  mode: "kernel",
  inlineMax: Number(process.env.WORKSHOP_SINK_INLINE_MAX ?? 16 * 1024),
  previewChars: 2048,
  put: (kind, text, origin) =>
    clientRef.current.put(Buffer.from(text, "utf8"), typeFor(kind), labelsFor(origin)),
});

const { tools, driver } = createWorkshop({
  sink,
  log: (s) => console.error(`[browser] ${s}`),
});
const byVerb = new Map(tools.map((t) => [`browser::${t.name.replace(/^browser_/, "")}`, t]));

// The driver owns its tool metadata: advertised to the kernel, joined into
// grants introspection, so a granted model driver needs no tool config.
const toolsMeta = Object.fromEntries(
  [...byVerb.entries()].map(([verb, t]) => [
    verb,
    { description: t.description, schema: t.inputSchema },
  ]),
);

await servePlugin({
  name: "portos-browser",
  verbs: [...byVerb.keys()],
  tools: toolsMeta,
  onCall: async (verb, args, client) => {
    clientRef.current = client;
    const tool = byVerb.get(verb);
    if (!tool) throw new Error(`unknown verb: ${verb}`);
    const out = await tool.handler(args ?? {});
    if (verb === "browser::screenshot" && out?.path) {
      const origin = originOf(driver.currentUrl());
      const meta = await client.put(await readFile(out.path), "image/png", labelsFor(origin));
      return { handle: meta.id, size: meta.size, type: meta.type, path: out.path };
    }
    return out;
  },
});

// Kernel said shutdown: close the browser so Chromium never outlives us,
// then exit explicitly (open sockets would otherwise keep node alive).
try {
  await driver.close();
} catch {}
process.exit(0);

function originOf(url) {
  try {
    const origin = new URL(url).origin;
    return origin === "null" ? null : origin;
  } catch {
    return null;
  }
}
