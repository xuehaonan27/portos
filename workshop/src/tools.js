// tools.js — the tool surface, transport-agnostic.
//
// These tool definitions know nothing about MCP. mcp-server.js adapts them to
// MCP today; the same definitions can be exposed over the AgentOS data-plane
// protocol later. Each tool threads driver (seam 1) + policy (seam 2) +
// sink (seam 3), so the three seams are exercised on every call.

import { createDriver } from "./driver/driver.js";
import { makePolicy } from "./policy.js";
import { makeSink } from "./sink.js";

export function createWorkshop(opts = {}) {
  const driver = createDriver(opts.driver ?? {});
  const policy = opts.policy ?? makePolicy({ mode: "supervised", log: opts.log });
  const sink = opts.sink ?? makeSink({ mode: "inline" });

  const originOf = (url) => {
    try { return new URL(url).origin; } catch { return null; }
  };

  // Compact the snapshot for the model: drop bbox unless asked, cap element
  // count. The full element list still goes through the sink so the meter is
  // honest about what would hit context.
  const shapeSnapshot = (snap, { withBbox = false, max = 120 } = {}) => {
    const elements = snap.elements.slice(0, max).map((e) =>
      withBbox ? e : { ref: e.ref, role: e.role, name: e.name, editable: e.editable }
    );
    return {
      snapshotId: snap.snapshotId,
      url: snap.url,
      title: snap.title,
      elementCount: snap.elements.length,
      elements,
      ...(snap.staleWarning ? { staleWarning: snap.staleWarning } : {}),
    };
  };

  const tools = [
    {
      name: "browser_open",
      description: "打开浏览器(专用 profile)并可选导航到 url。返回页面结构化元素表。",
      inputSchema: { type: "object", properties: { url: { type: "string" } } },
      async handler({ url }) {
        const g = await policy.check({ verb: "open", kind: "navigate", targetOrigin: originOf(url) });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.open({ url })));
      },
    },
    {
      name: "browser_navigate",
      description: "导航到 url。",
      inputSchema: { type: "object", required: ["url"], properties: { url: { type: "string" } } },
      async handler({ url }) {
        const g = await policy.check({ verb: "navigate", kind: "navigate", targetOrigin: originOf(url) });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.navigate({ url })));
      },
    },
    {
      name: "browser_snapshot",
      description: "重新获取当前页面的结构化元素表(每个元素带一个短时 ref)。",
      inputSchema: { type: "object", properties: { withBbox: { type: "boolean" } } },
      async handler({ withBbox } = {}) {
        const g = await policy.check({ verb: "snapshot", kind: "read" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.snapshot(), { withBbox }));
      },
    },
    {
      name: "browser_click",
      description: "点击 ref 指向的元素。可传 expectName 做 compare-and-act 校验。",
      inputSchema: {
        type: "object", required: ["ref"],
        properties: { ref: { type: "string" }, expectName: { type: "string" } },
      },
      async handler({ ref, expectName }) {
        const g = await policy.check({ verb: "click", kind: "act" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.click({ ref, expectName })));
      },
    },
    {
      name: "browser_type",
      description: "向 ref 指向的输入框填入文本;submit=true 时回车提交(提交为受控动作)。",
      inputSchema: {
        type: "object", required: ["ref", "text"],
        properties: {
          ref: { type: "string" }, text: { type: "string" },
          expectName: { type: "string" }, submit: { type: "boolean" },
        },
      },
      async handler({ ref, text, expectName, submit }) {
        const g = await policy.check({ verb: "type", kind: submit ? "submit" : "act" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.type({ ref, text, expectName, submit })));
      },
    },
    {
      name: "browser_wait_for",
      description: "等待 selector 出现 / 网络空闲 / 固定毫秒(dev-loop 同步原语)。",
      inputSchema: {
        type: "object",
        properties: { selector: { type: "string" }, ms: { type: "number" }, networkIdle: { type: "boolean" } },
      },
      async handler(args = {}) {
        const g = await policy.check({ verb: "wait_for", kind: "read" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("text", await driver.waitFor(args));
      },
    },
    {
      name: "browser_screenshot",
      description: "截图,返回文件路径(不把图像塞进上下文)。",
      inputSchema: { type: "object", properties: { path: { type: "string" } } },
      async handler({ path } = {}) {
        const g = await policy.check({ verb: "screenshot", kind: "read" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        // path only — the image itself never flows through the sink into context.
        return await driver.screenshot({ path });
      },
    },
    {
      name: "browser_login_passthrough",
      description: "把窗口交还给你人肉登录/验证;密码与 passkey 不经过 agent。",
      inputSchema: { type: "object", properties: { url: { type: "string" } } },
      async handler({ url } = {}) {
        return sink.deliver("text", await driver.passthroughBegin({ url }));
      },
    },
    {
      name: "browser_resume",
      description: "登录完成后恢复 agent 驱动。",
      inputSchema: { type: "object", properties: {} },
      async handler() {
        return sink.deliver("text", await driver.passthroughEnd());
      },
    },
    {
      name: "browser_close",
      description: "关闭浏览器。",
      inputSchema: { type: "object", properties: {} },
      async handler() {
        return await driver.close();
      },
    },
  ];

  return { driver, policy, sink, tools };
}
