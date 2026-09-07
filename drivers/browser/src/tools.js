// tools.js — the tool surface, transport-agnostic.
//
// These tool definitions know nothing about the kernel protocol: plugin.js
// adapts them to the PortOS plugin ABI. Each tool threads driver (seam 1) +
// policy (seam 2) + sink (seam 3), so the three seams are exercised on every
// call.
//
// Each tool also carries its `character`: the verb's row in the PortOS verb
// truth table (.dev/design/spec.md §6.4, F4) — what the verb does to the
// world, hence how the kernel budgets it and, later, whether one consent can
// cover a batch of it. One verb, one character: a verb whose character would
// depend on its arguments gets split instead (spec §6.6 ⑤ — the mediation
// point is chosen by the interface). That is why `type` never submits
// (`submit` is its own verb), `open` never navigates (`navigate` does), and
// `login_passthrough` never navigates either.
//
//   OBSERVE   repeatable_shared — an observation of the page, which is a
//             shared mutable source (it changes under us): idempotent, not
//             budgeted, does not commute with page activity.
//   CONTAINED transforming — changes the driver's own holding (the browser
//             slot: launched/closed, agent/user driving) without touching the
//             outside world; the inverse is carried by the class
//             (holding_rho = inverse in plugin.js). Idempotent by
//             construction: opening an open browser, closing a closed one,
//             re-entering the mode you are in are all no-ops.
//   EMIT      emitting/external, amortizable — the page's origin observes it
//             (a request, a click, keystrokes); one consent may cover a batch
//             within the consented origins.
//   EMIT_HARD emitting/external, not amortizable — the hard-list member:
//             every form submission is consented to on its own.

import { createDriver } from "./driver/driver.js";
import { makePolicy } from "./policy.js";
import { makeSink } from "./sink.js";

export const OBSERVE = Object.freeze({ kind: "repeatable_shared" });
export const CONTAINED = Object.freeze({ kind: "transforming", idempotent: true });
export const EMIT = Object.freeze({ kind: "emitting", world: "external", amortizable: true });
export const EMIT_HARD = Object.freeze({ kind: "emitting", world: "external", amortizable: false });

export function createWorkshop(opts = {}) {
  const driver = createDriver(opts.driver ?? {});
  const policy = opts.policy ?? makePolicy({ mode: "supervised", log: opts.log });
  const sink = opts.sink ?? makeSink({ mode: "inline" });

  const originOf = (url) => {
    try { return new URL(url).origin; } catch { return null; }
  };

  // A verb keeps its character only if its arguments cannot smuggle in
  // another one; refuse the old argument loudly rather than ignoring it.
  const refuse = (arg, instead) => {
    throw new Error(`${arg} is not accepted here; call ${instead}`);
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
      description: "启动浏览器(专用 profile);已启动则复用。不导航(导航用 browser_navigate)。返回当前页面的结构化元素表。",
      inputSchema: { type: "object", properties: {} },
      character: CONTAINED,
      async handler({ url } = {}) {
        if (url !== undefined) refuse("url", "browser_navigate");
        const g = await policy.check({ verb: "open", kind: "launch" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.open()));
      },
    },
    {
      name: "browser_navigate",
      description: "导航到 url。",
      inputSchema: { type: "object", required: ["url"], properties: { url: { type: "string" } } },
      // WP-06 sink target: the kernel extracts the origin of `url` per effect.
      character: { ...EMIT, target: { arg: "url", kind: "origin" } },
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
      character: OBSERVE,
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
      character: EMIT,
      async handler({ ref, expectName }) {
        const g = await policy.check({ verb: "click", kind: "act" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.click({ ref, expectName })));
      },
    },
    {
      name: "browser_type",
      description: "向 ref 指向的输入框填入文本。不提交(提交用 browser_submit)。可传 expectName 做 compare-and-act 校验。",
      inputSchema: {
        type: "object", required: ["ref", "text"],
        properties: {
          ref: { type: "string" }, text: { type: "string" }, expectName: { type: "string" },
        },
      },
      character: EMIT,
      async handler({ ref, text, expectName, submit } = {}) {
        if (submit !== undefined) refuse("submit", "browser_submit");
        const g = await policy.check({ verb: "type", kind: "act" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.type({ ref, text, expectName })));
      },
    },
    {
      name: "browser_submit",
      description: "在 ref 指向的表单控件上按回车提交(受控动作:每次提交单独同意)。可传 expectName 做 compare-and-act 校验。",
      inputSchema: {
        type: "object", required: ["ref"],
        properties: { ref: { type: "string" }, expectName: { type: "string" } },
      },
      character: EMIT_HARD,
      async handler({ ref, expectName }) {
        const g = await policy.check({ verb: "submit", kind: "submit" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("snapshot", shapeSnapshot(await driver.submit({ ref, expectName })));
      },
    },
    {
      name: "browser_wait_for",
      description: "等待 selector 出现 / 网络空闲 / 固定毫秒(dev-loop 同步原语)。",
      inputSchema: {
        type: "object",
        properties: { selector: { type: "string" }, ms: { type: "number" }, networkIdle: { type: "boolean" } },
      },
      character: OBSERVE,
      async handler(args = {}) {
        const g = await policy.check({ verb: "wait_for", kind: "read" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        return sink.deliver("text", await driver.waitFor(args));
      },
    },
    {
      name: "browser_screenshot",
      description: "截图。图像不进上下文:返回截图文件路径(经 PortOS 时另附 artifact 句柄)。",
      inputSchema: { type: "object", properties: {} },
      character: OBSERVE,
      async handler() {
        const g = await policy.check({ verb: "screenshot", kind: "read" });
        if (g.decision === "deny") throw new Error(`policy denied: ${g.reason}`);
        // The driver picks the file (a scratch path); callers never choose
        // where bytes land — that is what keeps this an observation.
        return await driver.screenshot();
      },
    },
    {
      name: "browser_login_passthrough",
      description: "把窗口交还给你人肉登录/验证;密码与 passkey 不经过 agent。先 browser_navigate 到目标页再交还。",
      inputSchema: { type: "object", properties: {} },
      character: CONTAINED,
      async handler({ url } = {}) {
        if (url !== undefined) refuse("url", "browser_navigate before browser_login_passthrough");
        return sink.deliver("text", await driver.passthroughBegin());
      },
    },
    {
      name: "browser_resume",
      description: "登录完成后恢复 agent 驱动。",
      inputSchema: { type: "object", properties: {} },
      character: CONTAINED,
      async handler() {
        return sink.deliver("text", await driver.passthroughEnd());
      },
    },
    {
      name: "browser_close",
      description: "关闭浏览器。",
      inputSchema: { type: "object", properties: {} },
      character: CONTAINED,
      async handler() {
        return await driver.close();
      },
    },
  ];

  return { driver, policy, sink, tools };
}
