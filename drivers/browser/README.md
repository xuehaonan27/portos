# PortOS browser driver

一个**看得见、能接管、不碰你凭证**的浏览器,作为 PortOS 的第一个 driver:让模型替你在已登录的网站上做事。它已接上 PortOS 内核的 plugin 协议(`src/plugin.js`),由 `portos chat` 拉起使用。PortOS 不做任何人的 MCP server;将来通过 mcp-host **消费** MCP 生态,方向相反。

设计上游:`.dev/plans/workshopm1v0.md`(demo 优先)+ `.dev/plans/architecture-v0.md`(substrate 北极星)+ `.dev/plans/decisions-v1.md`(独立运行时方向修订)。

## 跑起来

```sh
npm install
npm test                              # 冒烟:open → navigate → 蒸馏 → type → submit(回车) → compare-and-act click,无头零配置
node src/cli-demo.js https://example.com   # 独立 demo;Mac 上默认开真实可见 Chrome,无显示器机器自动无头
```

作为 PortOS driver 运行:在 `<root>/chat.json` 里登记本插件后 `portos chat <root>`(见仓库 `.dev/gen/chat-status.md` 的配置样例);端到端测试在 `crates/portos-cli/tests/chat.rs`。

## 结构与三条缝

- `src/plugin.js` — **PortOS 适配层**(薄;只有它知道内核协议):tools.js 暴露为 `browser::*` verbs 并自述声明束——工具元数据(description/schema,供 grants 自省)、每个 verb 的性格、类的持有档 `holding_rho`、passthrough 协议(见下节);截图字节进内核 CAS 成 artifact(按页面 origin 打 `web:<origin>` taint 标签)。JS 协议客户端在共享 SDK:`sdk/js/client.js`。
- `src/tools.js` — 工具面,**传输无关**(plugin.js 之下、driver 之上)。
- `src/driver/driver.js` — **缝①驱动接口**。今天背后是 Playwright,将来换手写 CDP + 过滤代理,上层不动。
- `src/policy.js` — **缝②policy 单点**。今天近乎放行;将来 capability / effect-plan 从这里接。
- `src/sink.js` — **缝③result sink,已接数据面**:`kernel` 模式下超过 `WORKSHOP_SINK_INLINE_MAX`(默认 16KB)的 payload 进内核 CAS,模型只收 handle+元数据+预览(architecture §4.4);小 payload(蒸馏元素表这类"工作镜头")保持内联。照旧计量 context/data 字节比。

## 动词表(`browser::*`)与性格

每个 verb 在 `src/tools.js` 里带一行 `character`——PortOS 动词真理表(`.dev/design/spec.md` §6.4,F4)里它的那一行。`src/plugin.js` 把它连同 description/schema 放进 hello;内核在 spawn 时校验整张表(不一致即拒绝启动),`grants` 自省把 `kind`/`budgeted` 交给模型驱动,模型驱动再写进给 provider 的工具描述与会话事件。**一个 verb 一种性格**:性格会随参数变化的 verb 已拆开(spec §6.6 ⑤:中介点由接口选出)。

| verb | 性格 | 说明 |
|---|---|---|
| `open` | transforming(幂等) | 启动浏览器(专用 profile),已启动则复用;**不导航**(传 `url` 直接报错,用 `navigate`) |
| `navigate` | emitting / external / 可摊销 | 页面 origin 观察得到的请求;一次同意可覆盖一批 |
| `click`、`type` | emitting / external / 可摊销 | `type` **只填不提交**(传 `submit` 直接报错) |
| `submit` | emitting / external / **不可摊销(硬清单)** | 在 ref 上按回车提交;每次提交单独同意 |
| `snapshot`、`wait_for`、`screenshot` | repeatable_shared | 对页面(共享可变源)的观察:幂等、不进预算、不与页面活动交换。`screenshot` 不再接受 `path`(文件由 driver 自选,经 PortOS 时返回 artifact 句柄) |
| `login_passthrough`、`resume`、`close` | transforming(幂等) | 只改驱动自己的持有(浏览器槽:开/关、agent/人 驾驶);`login_passthrough` **不导航**(先 `navigate` 再交还) |

类级持有档 `holding_rho: inverse`:浏览器槽由 `close` 精确归还(专用 profile 里的登录态是用户自己的持久状态,不计入)。

**passthrough 自动机(F6)**:hello 里的 `protocol`——`login_passthrough` 之后处于 `user` 态,内核在调用到达本插件之前就拒绝 `navigate`/`click`/`type`/`submit`,直到 `resume`(或 `close`)回到 `agent` 态;观察类 verb 与 `open` 在自动机辖域外。以前只是注释里的"caller is expected to stop driving",现在由内核执行。

配置迁移:chat.json 的 grants 需显式给出 `submit` 才能提交表单;旧的 `open {url}` 用法改为 `navigate {url}`。已知缺口:点击一个提交按钮(`click`)仍按可摊销发射记账——DOM 层面无法在调用前区分"点按钮"与"提交表单";硬清单的提交路径是 `submit`(回车)。

## 唯一保留的安全性质

专用 Chrome profile + 你人肉登录(passthrough)+ 模型只拿到蒸馏 DOM/截图、从不读 cookie。其余 enforcement(能力、effect-plan、IFC)全部推迟,由"你看着它做"这一 supervised autonomy 顶上。

## 启动行为与环境变量

零配置即可跑:driver 按平台自动决定启动方式,环境变量只做覆盖。

- **headless**:默认"有显示器才开窗"(macOS/Windows 视为有;Linux 看 `DISPLAY`/`WAYLAND_DISPLAY`)。`WORKSHOP_HEADLESS=1` 强制无头,`=0` 强制开窗。
- **浏览器**:macOS 默认用真实 Chrome(`channel: "chrome"`),没装则自动回退 Playwright 自带 Chromium;`WORKSHOP_CHROME_CHANNEL` 显式指定时不回退。
- **沙箱**:Linux 上 Chromium 沙箱不可用(受限 user namespaces)时自动带 `--no-sandbox` 重试并在 stderr 告警;`WORKSHOP_NO_SANDBOX=1` 显式打开。
- **profile**:`WORKSHOP_PROFILE_DIR=<path>`(默认 `~/.workshop-chrome-profile`,专用、与日常浏览器隔离)。
