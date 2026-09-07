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
//
// The hello carries the driver's declaration bundle (spec §6.4–§6.6): each
// verb's character from tools.js (checked into the kernel's truth table at
// spawn — an incoherent table refuses the spawn), the class's holding grade
// (`holding_rho`: the browser slot is given back exactly by `close`, so
// `inverse`), and the passthrough automaton below (enforced by the kernel on
// every call).

import { readFile } from "node:fs/promises";
import { existsSync } from "node:fs";
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
// grants introspection (description, schema, and the verb character), so a
// granted model driver needs no tool config and can tell reads from effects.
const toolsMeta = Object.fromEntries(
  [...byVerb.entries()].map(([verb, t]) => [
    verb,
    { description: t.description, schema: t.inputSchema, ...t.character },
  ]),
);

// The passthrough automaton (F6): once the window is handed to the human,
// the agent must not drive until `resume` — in the `user` state the kernel
// refuses navigate / click / type / submit before the call reaches us.
// Observations (snapshot, wait_for, screenshot) and `open` stay outside the
// automaton's scope; `close` always returns to `agent`.
const V = (s) => `browser::${s}`;
const protocol = {
  initial: "agent",
  transitions: [
    ["agent", V("navigate"), "agent"],
    ["agent", V("click"), "agent"],
    ["agent", V("type"), "agent"],
    ["agent", V("submit"), "agent"],
    ["agent", V("login_passthrough"), "user"],
    ["user", V("login_passthrough"), "user"],
    ["user", V("resume"), "agent"],
    ["agent", V("resume"), "agent"],
    ["agent", V("close"), "agent"],
    ["user", V("close"), "agent"],
  ],
};

// WP-02: once the browser is actually up, its substrate is on the ledger —
// the Chromium process (the kernel kills the exact incarnation if this plugin
// dies) and the profile's lock file when one exists. Registered lazily on the
// first successful call (navigate opens the browser without `open` since D36);
// released on `browser::close`. If the plugin dies without closing, the
// kernel's teardown reclaims both, children first.
const holdings = { process: null, lock: null };
async function ensureHoldings(client) {
  if (holdings.process || !driver.context) return;
  // Playwright exposes no process handle for a persistent context; the pid
  // comes from the browser-level CDP session (SystemInfo.getProcessInfo).
  let pid = null;
  try {
    const cdp = await driver.context.browser().newBrowserCDPSession();
    const info = await cdp.send("SystemInfo.getProcessInfo");
    pid = info.processInfo.find((p) => p.type === "browser")?.id ?? null;
    await cdp.detach();
  } catch {
    pid = null;
  }
  if (!pid) return; // no pid, no holding — the ledger stays honest
  holdings.process = await client.hold("kernel/process", `portos-browser/${pid}`, { pid });
  if (driver.userDataDir) {
    // The lock file Chromium actually wrote (headed Chrome: SingletonLock;
    // some builds: DevToolsActivePort). Headless Playwright writes neither —
    // then there is no lock substrate to register.
    for (const f of ["SingletonLock", "DevToolsActivePort"]) {
      const p = `${driver.userDataDir}/${f}`;
      if (existsSync(p)) {
        holdings.lock = await client.hold("kernel/file-lock", p, { owner_pid: pid });
        break;
      }
    }
  }
}
async function releaseHoldings(client) {
  for (const key of ["lock", "process"]) {
    const h = holdings[key];
    if (h) {
      try {
        await client.release(h.id, h.generation);
      } catch {}
      holdings[key] = null;
    }
  }
}

await servePlugin({
  name: "portos-browser",
  verbs: [...byVerb.keys()],
  tools: toolsMeta,
  holdingRho: "inverse",
  protocol,
  onCall: async (verb, args, client) => {
    clientRef.current = client;
    const tool = byVerb.get(verb);
    if (!tool) throw new Error(`unknown verb: ${verb}`);
    const out = await tool.handler(args ?? {});
    if (verb === "browser::close") {
      await releaseHoldings(client);
    } else {
      await ensureHoldings(client);
    }
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
