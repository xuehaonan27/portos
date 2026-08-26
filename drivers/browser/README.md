# PortOS browser driver

一个**看得见、能接管、不碰你凭证**的浏览器,作为 PortOS 的第一个 driver:让模型替你在已登录的网站上做事。它已接上 PortOS 内核的 plugin 协议(`src/plugin.js`),由 `portos chat` 拉起使用。PortOS 不做任何人的 MCP server;将来通过 mcp-host **消费** MCP 生态,方向相反。

设计上游:`.dev/plans/workshopm1v0.md`(demo 优先)+ `.dev/plans/architecture-v0.md`(substrate 北极星)+ `.dev/plans/decisions-v1.md`(独立运行时方向修订)。

## 跑起来

```sh
npm install
npm test                              # 冒烟:navigate → 蒸馏 → type → compare-and-act click,无头零配置
node src/cli-demo.js https://example.com   # 独立 demo;Mac 上默认开真实可见 Chrome,无显示器机器自动无头
```

作为 PortOS driver 运行:在 `<root>/chat.json` 里登记本插件后 `portos chat <root>`(见仓库 `.dev/gen/chat-status.md` 的配置样例);端到端测试在 `crates/portos-cli/tests/chat.rs`。

## 结构与三条缝

- `src/plugin.js` — **PortOS 适配层**(薄;只有它知道内核协议):tools.js 暴露为 `browser::*` verbs 并自述工具元数据(description/schema,供 grants 自省);截图字节进内核 CAS 成 artifact(按页面 origin 打 `web:<origin>` taint 标签)。JS 协议客户端在共享 SDK:`sdk/js/client.js`。
- `src/tools.js` — 工具面,**传输无关**(plugin.js 之下、driver 之上)。
- `src/driver/driver.js` — **缝①驱动接口**。今天背后是 Playwright,将来换手写 CDP + 过滤代理,上层不动。
- `src/policy.js` — **缝②policy 单点**。今天近乎放行;将来 capability / effect-plan 从这里接。
- `src/sink.js` — **缝③result sink,已接数据面**:`kernel` 模式下超过 `WORKSHOP_SINK_INLINE_MAX`(默认 16KB)的 payload 进内核 CAS,模型只收 handle+元数据+预览(architecture §4.4);小 payload(蒸馏元素表这类"工作镜头")保持内联。照旧计量 context/data 字节比。

## 唯一保留的安全性质

专用 Chrome profile + 你人肉登录(passthrough)+ 模型只拿到蒸馏 DOM/截图、从不读 cookie。其余 enforcement(能力、effect-plan、IFC)全部推迟,由"你看着它做"这一 supervised autonomy 顶上。

## 启动行为与环境变量

零配置即可跑:driver 按平台自动决定启动方式,环境变量只做覆盖。

- **headless**:默认"有显示器才开窗"(macOS/Windows 视为有;Linux 看 `DISPLAY`/`WAYLAND_DISPLAY`)。`WORKSHOP_HEADLESS=1` 强制无头,`=0` 强制开窗。
- **浏览器**:macOS 默认用真实 Chrome(`channel: "chrome"`),没装则自动回退 Playwright 自带 Chromium;`WORKSHOP_CHROME_CHANNEL` 显式指定时不回退。
- **沙箱**:Linux 上 Chromium 沙箱不可用(受限 user namespaces)时自动带 `--no-sandbox` 重试并在 stderr 告警;`WORKSHOP_NO_SANDBOX=1` 显式打开。
- **profile**:`WORKSHOP_PROFILE_DIR=<path>`(默认 `~/.workshop-chrome-profile`,专用、与日常浏览器隔离)。
