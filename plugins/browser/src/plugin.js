#!/usr/bin/env node
// portos-browser: a watchable browser as `browser::*` verbs.
//
// The interface is `drivers/browser`. Its `tools.json` is read here, so what
// this plugin advertises and what the Rust side says are one document; a
// conformance test in `cli/tests/run.rs` holds the two together. The
// implementation is Playwright (`driver/`). What this file adds is the ABI:
// config in, verbs out, and the two data-plane rules every driver follows —
// a result the model might not read goes to the CAS with a preview (the JS
// twin of `portos_abi::bulk`), and a screenshot is an artifact, never bytes
// in a conversation.
//
// Config (from the launch spec), all optional:
//   {"headless": bool, "profile_dir": "…", "channel": "chrome", "no_sandbox": bool}

import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { PlaywrightDriver } from "./driver/playwright-driver.js";
import { servePlugin } from "../../../sdk/js/client.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const TOOLS = JSON.parse(
  await readFile(path.join(HERE, "../../../drivers/browser/tools.json"), "utf8"),
);
const config = JSON.parse(process.env.PORTOS_PLUGIN_CONFIG ?? "{}");

// Where the line between context and data is drawn: the constants of
// `portos_abi::bulk`, because the model was told one shape.
const INLINE_MAX = 16 * 1024;
const PREVIEW_CHARS = 2048;
/** The element table is the model's working lens; it is capped, not cut. */
const MAX_ELEMENTS = 120;

const driver = new PlaywrightDriver(config);
let client = null;

function originOf(url) {
  try {
    const origin = new URL(url).origin;
    return origin === "null" ? null : origin;
  } catch {
    return null;
  }
}

/** Provenance: an artifact should still say where it came from once it has
 *  outlived the call that made it. */
function labelsFor(url) {
  const origin = originOf(url);
  return origin ? { integ: [`web:${origin}`] } : null;
}

/** The element table the model works from: compact, capped, refs intact. */
function shape(snap, { with_bbox = false } = {}) {
  const elements = snap.elements
    .slice(0, MAX_ELEMENTS)
    .map((e) => (with_bbox ? e : { ref: e.ref, role: e.role, name: e.name, editable: e.editable }));
  return {
    snapshot_id: snap.snapshot_id,
    url: snap.url,
    title: snap.title,
    element_count: snap.elements.length,
    elements,
    ...(snap.stale_warning ? { stale_warning: snap.stale_warning } : {}),
  };
}

/** Inline if small, otherwise stored with a preview — `Bulk`, for a page. */
async function deliver(page) {
  const text = JSON.stringify(page);
  if (Buffer.byteLength(text, "utf8") <= INLINE_MAX) return page;
  const meta = await client.put(Buffer.from(text, "utf8"), "web/page-snapshot", labelsFor(page.url));
  return { handle: meta.id, size: meta.size, preview: text.slice(0, PREVIEW_CHARS) };
}

const verbs = {
  "browser::open": async ({ url } = {}) => deliver(shape(await driver.open({ url }))),
  "browser::navigate": async ({ url }) => deliver(shape(await driver.navigate({ url }))),
  "browser::snapshot": async (a = {}) => deliver(shape(await driver.snapshot(), a)),
  "browser::click": async (a) => deliver(shape(await driver.click(a))),
  "browser::type": async (a) => deliver(shape(await driver.type(a))),
  "browser::wait_for": (a = {}) => driver.waitFor(a),
  "browser::screenshot": async ({ path: where } = {}) => {
    const shot = await driver.screenshot({ path: where });
    const meta = await client.put(await readFile(shot.path), "image/png", labelsFor(driver.currentUrl()));
    return { handle: meta.id, size: meta.size, type: meta.type, path: shot.path };
  },
  "browser::login_passthrough": ({ url } = {}) => driver.passthroughBegin({ url }),
  "browser::resume": () => driver.passthroughEnd(),
  "browser::close": () => driver.close(),
};

// The interface and this implementation must name the same verbs, and a
// mismatch is a startup failure rather than a verb that quietly misses.
for (const verb of Object.keys(verbs)) {
  if (!TOOLS[verb]) throw new Error(`${verb} is not in drivers/browser/tools.json`);
}
for (const verb of Object.keys(TOOLS)) {
  if (!verbs[verb]) throw new Error(`${verb} is in the interface and not implemented here`);
}

await servePlugin({
  name: "portos-browser",
  verbs: Object.keys(verbs),
  tools: TOOLS,
  onCall: async (verb, args, c) => {
    client = c;
    return verbs[verb](args ?? {});
  },
});

// Kernel said shutdown: close the browser so Chromium never outlives us,
// then exit explicitly (open sockets would otherwise keep node alive).
try {
  await driver.close();
} catch {}
process.exit(0);
