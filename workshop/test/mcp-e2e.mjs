// mcp-e2e.mjs — drive a full multi-step task through the real MCP pipe:
// spawn the server, initialize, then tools/call browser_open → browser_type →
// compare-and-act browser_click → browser_wait_for the resulting state change.
// This is exactly the path an agent harness (Claude Code etc.) will use, so a
// green run here means "riding the existing harness" is proven end to end,
// browser included (mcp-smoke.mjs only proves the handshake).

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const server = join(here, "..", "src", "mcp-server.js");
const fixture = "file://" + join(here, "fixture.html");
const profileDir = `/tmp/workshop-mcpe2e-profile-${process.pid}`;

const child = spawn("node", [server], {
  env: {
    ...process.env,
    WORKSHOP_HEADLESS: "1", // deterministic regardless of local display
    WORKSHOP_PROFILE_DIR: profileDir,
  },
  stdio: ["pipe", "pipe", "inherit"],
});

let buf = "";
const pending = [];
child.stdout.on("data", (d) => {
  buf += d.toString();
  let nl;
  while ((nl = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, nl).trim();
    buf = buf.slice(nl + 1);
    if (line) pending.push(JSON.parse(line));
  }
});

const send = (obj) => child.stdin.write(JSON.stringify(obj) + "\n");
const waitFor = (id, ms = 30000) =>
  new Promise((res, rej) => {
    const t0 = Date.now();
    const iv = setInterval(() => {
      const m = pending.find((p) => p.id === id);
      if (m) { clearInterval(iv); res(m); }
      else if (Date.now() - t0 > ms) { clearInterval(iv); rej(new Error(`timeout waiting for id ${id}`)); }
    }, 20);
  });

let nextId = 0;
async function call(name, args) {
  const id = ++nextId;
  send({ jsonrpc: "2.0", id, method: "tools/call", params: { name, arguments: args } });
  const resp = await waitFor(id);
  if (resp.error) throw new Error(`${name}: rpc error ${JSON.stringify(resp.error)}`);
  if (resp.result?.isError) throw new Error(`${name}: tool error ${resp.result.content?.[0]?.text}`);
  return JSON.parse(resp.result.content[0].text);
}

let failures = 0;
const ok = (c, m) => { if (c) console.log("  ✓", m); else { failures++; console.log("  ✗", m); } };

try {
  console.log("workshop MCP end-to-end test (multi-step task over the wire)");

  const id = ++nextId;
  send({ jsonrpc: "2.0", id, method: "initialize",
    params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "e2e", version: "0" } } });
  const init = await waitFor(id);
  ok(init.result?.serverInfo?.name === "workshop-browser", "initialize handshake");
  send({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });

  const snap = await call("browser_open", { url: fixture });
  ok(snap.title?.includes("Workshop Fixture"), "browser_open: navigated to fixture");
  const user = snap.elements.find((e) => e.name === "username");
  const btn = snap.elements.find((e) => e.name === "Sign in");
  ok(!!user && !!btn, "browser_open: found username field and Sign in button by accessible name");

  const typed = await call("browser_type", { ref: user.ref, text: "PortOS", expectName: "username" });
  ok(!typed.staleWarning, "browser_type: no stale warning");

  const clicked = await call("browser_click", { ref: btn.ref, expectName: "Sign in" });
  ok(!clicked.staleWarning, "browser_click: compare-and-act clean");

  // The fixture flips its H1 to "Welcome, PortOS" on submit; observing that
  // through browser_wait_for proves the state change made the round trip.
  const waited = await call("browser_wait_for", { selector: "text=Welcome, PortOS", ms: 5000 });
  ok(waited.ok === true, "browser_wait_for: page state change observed over MCP");

  await call("browser_close", {});
  ok(true, "browser_close");
} catch (e) {
  failures++;
  console.error("  ✗ threw:", e.message);
} finally {
  child.kill();
  try { const { rmSync } = await import("node:fs"); rmSync(profileDir, { recursive: true, force: true }); } catch {}
}

console.log(failures === 0 ? "\nMCP E2E OK" : `\nMCP E2E FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);
