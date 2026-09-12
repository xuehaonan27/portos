// Test fixture: a minimal JS plugin proving the JS protocol client speaks
// ABI v2 against the real kernel host (spawned by tests/abi_v2.rs).

import { servePlugin } from "../../../../sdk/js/client.js";

await servePlugin({
  name: "portos-jse",
  verbs: ["jse::ping", "jse::store", "jse::fetch", "jse::publish"],
  onCall: async (verb, args, client) => {
    switch (verb) {
      case "jse::ping":
        return { pong: args };
      case "jse::store": {
        const meta = await client.put(Buffer.from(String(args[0]), "utf8"), "text/plain");
        return { meta };
      }
      case "jse::fetch": {
        const buf = await client.read(String(args[0]));
        return { text: buf.toString("utf8"), bytes: buf.length };
      }
      case "jse::publish": {
        const delivered = await client.emit(String(args[0]), args[1] ?? null);
        return { delivered };
      }
      default:
        throw new Error(`unknown verb: ${verb}`);
    }
  },
});
