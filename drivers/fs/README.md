# portos-fs

文件树变成 verb。五个动词，一棵树，和一条比动词本身更要紧的纪律。

## 纪律：模型可能不看的结果，不进它的上下文

一个源文件、一次全仓 grep、一个目录列表，体量从一行到一兆不等。**好用与不好用的分界，就在于让模型背哪一种。**

所有可能变大的回复都过 `portos_bulk_api::Sink`——跟 browser driver 同一个判断、同一个形状：

| 大小 | 模型收到 |
|---|---|
| ≤ 16KB | `{text}` |
| > 16KB | `{handle, size, preview}`，字节进 CAS，要全文再 `artifact::read` |

实测（本仓库，经真实运行的 PortOS）：一次 765KB 的 grep 输出，上下文里只留 2048 字符——**小 374 倍**，handle 取回的字节与原文逐字相等。

## 动词

| verb | 参数 | 回复 |
|---|---|---|
| `fs::read` | `{path, offset?, len?}` | `Bulk` |
| `fs::write` | `{path, content, create_dirs?}` | `{path, bytes}` |
| `fs::list` | `{path?, depth?}` | `{entries: [{path, kind, size}], truncated}` |
| `fs::glob` | `{pattern, max?}` | `{paths, truncated}` |
| `fs::grep` | `{pattern, glob?, max?}` | `{matches: [{path, line, text}], truncated}` |

## 遍历认 `.gitignore`

不是锦上添花：不认它的 grep 就是会去读 `target/`——本仓库里那是好几个 GB 的机器生成字节，没人问过它们。用的是 ripgrep 家的 `ignore`。

## root 是**作用域，不是安全边界**

`PORTOS_FS_ROOT` 必填，所有路径相对它解析，绝对路径和爬出树的 `..` 被拒。

安全那条线已作废（D49），所以这里不假装是围栏。它买到的是三件普通的工程好处：

- 路径短 → 工具描述短 → 上下文小。
- **一棵树一个驱动实例**，这跟多节点同构（节点 B 上的 `fs` 就是那台机器的树）。要跨多棵树就起第二个实例，不是把 root 放开。
- 模型犯迷糊时够不到 `~/.ssh`，拿到的是错误而不是文件。

**明确不假装的地方**：root 内的符号链接可以指到外面，本驱动不追。假围栏比没围栏更坏。

## 配置

- `PORTOS_FS_ROOT` —— 必填，本驱动服务的那棵树。

阈值（16KB / 2048 字符预览）是常量，没有旋钮——理由见 `.dev/plans/w1-fs-shell-v1.md` 的 C6。
