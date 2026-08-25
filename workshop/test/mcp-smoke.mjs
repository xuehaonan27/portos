// mcp-smoke.mjs — prove the harness-integration path: spawn the MCP server,
// do the JSON-RPC initialize handshake, and list tools. No browser launched.

import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const server = join(here, "..", "src", "mcp-server.js");

const child = spawn("node", [server], {
  env: { ...process.env, WORKSHOP_HEADLESS: "1", WORKSHOP_NO_SANDBOX: "1" },
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
const waitFor = (id, ms = 5000) =>
  new Promise((res, rej) => {
    const t0 = Date.now();
    const iv = setInterval(() => {
      const m = pending.find((p) => p.id === id);
      if (m) { clearInterval(iv); res(m); }
      else if (Date.now() - t0 > ms) { clearInterval(iv); rej(new Error(`timeout waiting for id ${id}`)); }
    }, 20);
  });

let failures = 0;
const ok = (c, m) => { if (c) console.log("  ✓", m); else { failures++; console.log("  ✗", m); } };

try {
  console.log("workshop MCP handshake smoke test");
  send({ jsonrpc: "2.0", id: 1, method: "initialize",
    params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "smoke", version: "0" } } });
  const init = await waitFor(1);
  ok(init.result?.serverInfo?.name === "workshop-browser", "initialize handshake");

  send({ jsonrpc: "2.0", method: "notifications/initialized", params: {} });
  send({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
  const list = await waitFor(2);
  const names = (list.result?.tools ?? []).map((t) => t.name);
  ok(names.includes("browser_open") && names.includes("browser_login_passthrough"),
    `tools/list returned ${names.length} tools`);
  console.log("  tools:", names.join(", "));
} catch (e) {
  failures++;
  console.error("  ✗", e.message);
} finally {
  child.kill();
}

console.log(failures === 0 ? "\nMCP SMOKE OK" : `\nMCP SMOKE FAILED (${failures})`);
process.exit(failures === 0 ? 0 : 1);
