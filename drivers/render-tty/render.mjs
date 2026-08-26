#!/usr/bin/env node
// render-tty: the reference renderer driver (decisions-v1.md D32).
//
// Rendering in PortOS is not a special mechanism: a renderer is an ordinary
// plugin that subscribes to the event plane and presents what flows past —
// this one styles model-session events for a terminal (its stdout is
// inherited from the spawning host, so it prints into the chat terminal).
// Add several renderers and they compose; replace the builtin by setting
// `"render": "none"` in chat.json and listing your own here. The Console of
// architecture-v0.md §10 is, in these terms, just a bigger renderer.
//
// Serves no verbs; holds no capabilities; consumes `model::session::*` via a
// wildcard subscription.

import { servePlugin } from "../../sdk/js/client.js";

const DIM = "\x1b[2m";
const CYAN = "\x1b[36m";
const RESET = "\x1b[0m";

await servePlugin({
  name: "portos-render-tty",
  verbs: [],
  onReady: async (client) => {
    await client.subscribe("model::session::*");
  },
  onCall: async () => {
    throw new Error("render-tty serves no verbs");
  },
  onEvent: (_topic, data) => {
    switch (data?.kind) {
      case "delta":
        process.stdout.write(data.text ?? "");
        break;
      case "tool_call":
        process.stdout.write(`\n${CYAN}⏺ ${data.verb}${RESET}\n`);
        break;
      case "tool_result":
        process.stdout.write(`${DIM}⏺ ${data.verb} ${data.ok ? "done" : "failed"}${RESET}\n`);
        break;
      case "done":
        process.stdout.write(`\n${DIM}── turn done ──${RESET}\n`);
        break;
    }
  },
});
process.exit(0);
