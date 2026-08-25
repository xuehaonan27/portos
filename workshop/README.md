# Workshop — browser limb (M1, Stage A)

一个**看得见、能接管、不碰你凭证**的浏览器,让你现有的 AI agent 替你在已登录的网站上做事。这是 AI workshop 的第一条肢体;终端和编辑器由你现有的 harness 提供。

设计见 `.dev/plans/workshopm1v0.md`(demo 优先的 M1 计划);它将来长成的 substrate 见 `.dev/plans/architecture-v0.md`。

## 跑起来

```sh
npm install
npm test        # 冒烟 + MCP 握手 + MCP 端到端多步任务,全部无头、零配置

# 独立 demo:在 Mac 上默认开一个真实、可见的 Chrome 窗口;无显示器的机器上自动无头
node src/cli-demo.js https://example.com
```

## 插进你的 agent(骑现有 harness)

本仓库根目录的 `.mcp.json` 已把它注册给 Claude Code:在仓库里开会话即可用 `browser_*` 工具。其他 harness(Claude Desktop 等)手动注册:

```json
{
  "mcpServers": {
    "workshop-browser": {
      "command": "node",
      "args": ["/absolute/path/to/portos/workshop/src/mcp-server.js"]
    }
  }
}
```

工具:`browser_open / navigate / snapshot / click / type / wait_for / screenshot / login_passthrough / resume / close`。

## 三条缝(为将来的 AgentOS 留的)

- `src/driver/driver.js` — **驱动接口**。今天背后是 Playwright,将来换手写 CDP + 过滤代理,上层不动。
- `src/policy.js` — **policy 单点**。今天近乎放行;将来 capability / effect-plan 从这里接。
- `src/sink.js` — **result sink**。今天全文进上下文;将来大 payload 进 CAS、只回 handle+预览,只改这一个函数。它顺带计量 context/data 字节比。

## 唯一保留的安全性质

专用 Chrome profile + 你人肉登录(passthrough)+ agent 只拿到蒸馏 DOM/截图、从不读 cookie。其余 enforcement(能力、effect-plan、IFC)全部推迟,由"你看着它做"这一 supervised autonomy 顶上。

## 启动行为与环境变量

零配置即可跑:driver 按平台自动决定启动方式,环境变量只做覆盖。

- **headless**:默认"有显示器才开窗"(macOS/Windows 视为有;Linux 看 `DISPLAY`/`WAYLAND_DISPLAY`)。`WORKSHOP_HEADLESS=1` 强制无头,`=0` 强制开窗。
- **浏览器**:macOS 默认用真实 Chrome(`channel: "chrome"`),没装则自动回退 Playwright 自带 Chromium;`WORKSHOP_CHROME_CHANNEL` 显式指定时不回退。
- **沙箱**:Linux 上 Chromium 沙箱不可用(受限 user namespaces)时自动带 `--no-sandbox` 重试并在 stderr 告警;`WORKSHOP_NO_SANDBOX=1` 显式打开。
- **profile**:`WORKSHOP_PROFILE_DIR=<path>`(默认 `~/.workshop-chrome-profile`,专用、与日常浏览器隔离)。
