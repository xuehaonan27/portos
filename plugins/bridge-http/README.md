# portos-bridge-http

把 PortOS 的**事件面**和**invoke 路径**搬过一条 HTTP socket。呈现留在另一端。

它存在的第一个理由是**证伪**：如果实现它需要改内核，那"可扩展性由 ABI 提供"就是一句空话。它没有改内核一行——`plugins/echo/tests/bridge.rs` 是这条主张的测试。

## 为什么它不是"console 插件"

初版设计把 console 写成一个东西，焊死了两层变化速度完全不同的东西：

| 层 | 语言 | 谁决定 | 变化速度 |
|---|---|---|---|
| 事件面 | `SessionEvent` JSON | PortOS | 慢 |
| **传输** | HTTP/SSE（本插件） | bridge | 慢 |
| **呈现** | HTML（`web/console.html`） | 另一端 | **快** |

`render-tty` 里传输与呈现重合（同进程 stdout + ANSI），所以看不出是两层；一上浏览器就分开了。

**所以 bridge 不说浏览器的语言，它说事件面的语言。** `web/` 下的页面只是它顺手托管的静态字节，对 PortOS 没有任何特殊性——换成 TUI、原生 app 或另一个 PortOS 节点，本文件一行不动。

## HTTP 面

PortOS 只有两种交互形状，这里就只暴露两个端点，加上呈现端真正需要的两样：

| 端点 | 对应 |
|---|---|
| `GET /events` | topic 订阅（SSE，`{topic, data}`，含最近 200 条重放） |
| `POST /invoke` | verb 调用，`{verb, args}` → `{ok}` \| `{err}` |
| `GET /grants` | 本插件可调用什么（呈现端据此自省） |
| `GET /artifact/<id>` | 数据面：按句柄取字节，**不走事件流** |
| `GET /` | 默认呈现端 `web/console.html` |

## 跑起来

`<root>/portos.json`：

```json
{
  "plugins": [
    { "bin": "node",
      "args": ["/path/to/portos/plugins/bridge-http/bridge.js"],
      "env": { "PORTOS_BRIDGE_ADDR": "127.0.0.1:7777" } }
  ],
  "grants": [
    { "subject": "plugin:portos-bridge-http",
      "resource": "driver:model", "verbs": ["start", "send", "cancel", "end"] }
  ]
}
```

开发机上：

```sh
portos run .dev/root               # 终端前端（tty）与 web console 可并存；不列 tty 就只有 web console
```

MacBook 上（**不装任何东西**，只用已有的 SSH 和浏览器）：

```sh
ssh -L 7777:127.0.0.1:7777 devbox
open http://127.0.0.1:7777
```

## 配置

- `PORTOS_BRIDGE_ADDR` —— 默认 `127.0.0.1:7777`。**默认绑回环**，外部只能经 SSH 端口转发进来。
- `PORTOS_BRIDGE_TOPICS` —— 逗号分隔，默认 `model::session::*`。
- `PORTOS_BRIDGE_PORT_FILE` —— 写下实际绑定的端口（给 `:0` 的测试用）。

## 权限：说清楚

**连上这个 socket 的任何东西，拿到的是本插件被授予的全部能力。** bridge 自己不做任何裁决——`POST /invoke` 直接转发，准与不准由内核的能力闸决定（测试 `an_unconfigured_bridge_can_invoke_nothing` 钉住了这一点）。

所以 `portos.json` 里的 `grants` 列表就是全部的访问控制。默认绑回环 + 经 `ssh -L` 访问，是让这句话成立的前提。跨信任边界用它之前需要重新想。

## 已知限制

- **一次只能有一个 turn 在飞。** `model::send` 现在接受即返回（2026-09-11 的 W0-3），所以 bridge 的 client 通道不再被整个 turn 占住——`grants`、`cancel`、另一个标签页的读操作都能穿过去。剩下的限制在 modeld：egress 流落在单一 topic 上，所以第二个 `send` 会被明确拒绝（`a turn is already running on session …`）而不是排队。解开它需要 per-turn 的 egress topic，等真有并发需求再做。
- **进程遗留：本插件已自理，别的没有。** `servePlugin` 返回后，一个还在监听的 socket 会把 node 的 event loop 永远吊住，于是父进程被信号杀掉时留下一个占着端口的孤儿。本插件现在自己关门退出。但由驱动拉起的**孙进程**（例如 browser driver 的 chromium）仍然会被遗弃——那个要靠进程组回收（W1），不是每个插件各修一遍。
- **截图还进不了 console。** `GET /artifact/<id>` 是通的，但 `tool_result` 事件目前只带 `{verb, ok}`，不带产物句柄，所以呈现端拿不到 id。要通需要让 model 族的事件携带产物句柄——一个 `model-api` 的改动，待定。
