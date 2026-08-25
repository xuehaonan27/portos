#!/usr/bin/env node
// mcp-server.js — the MCP adapter (thin). Rides your existing agent harness:
// register this as an MCP server and your agent (Claude Code / Agent SDK) can
// drive the browser. This file knows about MCP; nothing else does.

import { createWorkshop } from "./tools.js";

async function main() {
  let Server, StdioServerTransport, schemas;
  try {
    ({ Server } = await import("@modelcontextprotocol/sdk/server/index.js"));
    ({ StdioServerTransport } = await import("@modelcontextprotocol/sdk/server/stdio.js"));
    schemas = await import("@modelcontextprotocol/sdk/types.js");
  } catch (e) {
    console.error(
      "MCP SDK not found. Install it with `npm i @modelcontextprotocol/sdk`.\n" +
      "You can still run the standalone demo without MCP: `npm run demo`.\n" +
      `(${e.message})`
    );
    process.exit(1);
  }

  // Launch behavior (headless / chrome channel / sandbox) is resolved inside
  // the driver from platform + WORKSHOP_* env vars, so a bare `node
  // src/mcp-server.js` works both on a Mac (headful real Chrome) and on a
  // displayless Linux box (headless bundled Chromium).
  const { tools } = createWorkshop({
    log: (s) => console.error(`[workshop] ${s}`),
  });
  const byName = new Map(tools.map((t) => [t.name, t]));

  const server = new Server(
    { name: "workshop-browser", version: "0.1.0" },
    { capabilities: { tools: {} } }
  );

  server.setRequestHandler(schemas.ListToolsRequestSchema, async () => ({
    tools: tools.map((t) => ({
      name: t.name, description: t.description, inputSchema: t.inputSchema,
    })),
  }));

  server.setRequestHandler(schemas.CallToolRequestSchema, async (req) => {
    const tool = byName.get(req.params.name);
    if (!tool) throw new Error(`unknown tool: ${req.params.name}`);
    try {
      const result = await tool.handler(req.params.arguments ?? {});
      return { content: [{ type: "text", text: JSON.stringify(result, null, 2) }] };
    } catch (e) {
      return { isError: true, content: [{ type: "text", text: String(e.message ?? e) }] };
    }
  });

  await server.connect(new StdioServerTransport());
  console.error("[workshop] MCP server up on stdio");
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
