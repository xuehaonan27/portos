# portos-remote

把**另一个节点**的能力，变成本节点的普通 verb。

零内核改动，零多节点协议。对端早就会把 PortOS 的两种交互形状放到一条 socket 上——那就是 `portos-bridge-http`，它是为浏览器写的，但它从不在乎连上来的是谁。本驱动就是那条 socket 的另一端。

```
节点 A                                   节点 B
portos-remote ──HTTP──> portos-bridge-http ──> B 的内核 ──> B 的 browser
   （B 的 verb 在 A 这里是普通插件）
```

## 唯一的翻译规则：族名加上节点名

| 对端 | 本地 |
|---|---|
| `browser::open` | `mac_browser::open` |
| `model::session::s1`（事件） | `mac_model::session::s1` |
| 能力资源 `driver:browser` | 能力资源 `driver:mac_browser` |

两件事因此成立：两个节点各有一个 browser 不会在平坦路由表里撞名；而且**授权读起来是诚实的**——`driver:mac_browser` 是 Mac 的浏览器，不是这台机器的。

注意 verb 段是 `[a-z][a-z0-9_]*`，**没有 `-`**，所以前缀用下划线接。

## 权限：两张表，各管一半

| 问题 | 谁答 | 在哪写 |
|---|---|---|
| **什么被暴露出来** | 对端节点 | 对端 `chat.json` 里 bridge 的 grants |
| **谁可以用它** | 本节点 | 本地 `driver:<node>_<family>` 的 grants |

两边都点头，verb 才走得通，而且谁也不能替对方点头。启动时本驱动读一次对端的 `/grants`——那就是对端关于"我愿意分享什么"的完整声明，这边不重复写一遍。

失败形状也是分开的，故意的：

- 对端没暴露 → 本地**根本没有这条路由**（"no route"）。
- 对端暴露了但本地没授权 → 本地能力闸拒绝（"no capability"）。

`crates/portos-echo/tests/remote.rs` 把这两条钉住了。

## 工具面照样能用

对端 `/grants` 回来的已经是一份工具面（verb + description + schema），因为在那个节点上同一个 join 生成了它自己模型的工具。原样带过来，远程 verb 才是**可用**而不只是**可达**——描述里再加一句 `Runs on node <node>.`，模型才知道这事发生在哪台机器上。

## 跑起来

对端（节点 B）就是一份普通的 `chat.json`，把要分享的东西授给 bridge：

```json
{ "plugins": [{ "bin": "node", "args": [".../drivers/bridge-http/bridge.js"],
                "env": { "PORTOS_BRIDGE_ADDR": "127.0.0.1:7777",
                         "PORTOS_BRIDGE_TOPICS": "browser::*" } }],
  "grants": [{ "subject": "plugin:portos-bridge-http",
               "resource": "driver:browser", "verbs": ["open", "text", "click"] }] }
```

本节点（节点 A），先把对端的端口接过来：

```sh
ssh -L 7777:127.0.0.1:7777 macmini
```

```json
{ "plugins": [{ "bin": "target/debug/portos-remote",
                "env": { "PORTOS_REMOTE_URL": "http://127.0.0.1:7777",
                         "PORTOS_REMOTE_NODE": "mac" } }],
  "grants": [{ "subject": "plugin:portos-modeld",
               "resource": "driver:mac_browser", "verbs": ["open", "text", "click"] }] }
```

模型下一轮就会看见 `mac_browser__open`。

## 配置

- `PORTOS_REMOTE_URL` —— 必填，对端 bridge 的地址。
- `PORTOS_REMOTE_NODE` —— 必填，对端在本地的代号。它会成为族名前缀，所以必须是合法 verb 段。

## 为什么不走 broker

broker 是**驱动调用外部世界**的瓶颈，允许表和凭证注入属于那里。另一个节点不是外部世界，它是这台工作站的一部分，而且它那一端是本驱动的对等物、不是第三方。把节点间链路塞进出口闸是把两层混为一谈（`extension-cases-v1.md` §4 讲的正是这个错误）。

账目也没有丢：每一个转发出去的 verb **被审计两次**——本节点记一次普通 invoke，对端记一次 bridge 发起的 invoke。粒度还比 `egress::log` 更准。

## 已知限制

- **制品句柄不跟着过来。** 回复里的 artifact id 指的是**对端 CAS** 里的字节，本驱动不去取。内容寻址意味着解法是"物化"而不是"翻译"（同样的字节在两个节点上得到同一个 id），但目前没有消费者，所以没做。对端的截图因此还看不了。
- **链路断开期间的事件会丢。** verb 不受影响（请求/响应，失败是响亮的）；事件流重连时用 `?replay=0`，不重放积压——否则一次重连会把已经收过的事件再发一遍。
- **对端节点消失时本驱动不退出。** 它继续提供那些 verb，调用会以 "node unreachable" 失败。内核不重启插件，让一次网络抖动拔掉整族 verb 不值得。
