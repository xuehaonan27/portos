#!/usr/bin/env node
// portos-browser: a watchable browser as `browser::*` verbs.
//
// The interface is `drivers/browser/driver.json`, read here and by the Rust
// side, so what this plugin advertises and what a caller compiles against
// are one document; a conformance test in `cli/tests/run.rs` holds the two
// together, and the SDK holds every call and reply to the document's
// schemas. The implementation is Playwright (`driver/`). What this file
// adds is the ABI: config in, verbs out, and the one data-plane rule the
// document cannot state for it — a screenshot is an artifact, never bytes
// in a conversation. The other rule, that a page too big for the context
// goes to the CAS with a preview, is the document's (`bulk`) and the SDK's.
//
// Config (from the launch spec), all optional:
//   {"headless": bool, "profile_dir": "…", "channel": "chrome", "no_sandbox": bool}

import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { PlaywrightDriver } from "./driver/playwright-driver.js";
import { loadDriver, servePlugin } from "../../../sdk/js/client.js";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const DRIVER = await loadDriver(path.join(HERE, "../../../drivers/browser/driver.json"));
const config = JSON.parse(process.env.PORTOS_PLUGIN_CONFIG ?? "{}");

/** The element table is the model's working lens; it is capped, not cut. */
const MAX_ELEMENTS = 120;

const driver = new PlaywrightDriver(config);

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

await servePlugin({
  name: "portos-browser",
  driver: DRIVER,
  implement: {
    open: async ({ url }) => shape(await driver.open({ url })),
    navigate: async ({ url }) => shape(await driver.navigate({ url })),
    snapshot: async (a) => shape(await driver.snapshot(), a),
    click: async (a) => shape(await driver.click(a)),
    type: async (a) => shape(await driver.type(a)),
    wait_for: (a) => driver.waitFor(a),
    screenshot: async ({ path: where }, client) => {
      const shot = await driver.screenshot({ path: where });
      const meta = await client.put(await readFile(shot.path), "image/png", labelsFor(driver.currentUrl()));
      return { handle: meta.id, size: meta.size, type: meta.type, path: shot.path };
    },
    login_passthrough: ({ url }) => driver.passthroughBegin({ url }),
    resume: () => driver.passthroughEnd(),
    close: () => driver.close(),
  },
});

// Kernel said shutdown: close the browser so Chromium never outlives us,
// then exit explicitly (open sockets would otherwise keep node alive).
try {
  await driver.close();
} catch {}
process.exit(0);
