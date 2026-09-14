# portos-browser

一个**看得见、能接管、不碰你凭证**的浏览器，作为 `browser::*` verbs。接口在 `drivers/browser`，这是它的第一个实现（Playwright）。

## 结构

- `src/plugin.js` — 插件本体：读启动配置，实现 `drivers/browser/driver.json` 里的 verbs，套用两条数据面规则——模型可能不读的结果进 CAS 只留 preview（`portos_abi::bulk` 的 JS 双胞胎），截图永远是 artifact 不是字节。
- `src/driver/playwright-driver.js` — 真正干活的：专用 profile、蒸馏元素表、按 ref 行动。
- `src/distill.js` — 注入页面的元素蒸馏脚本。
- `test/smoke.mjs` — 驱动的冒烟测试；端到端在 `cli/tests/run.rs`，其中一条把这个插件 hello 里报的工具和 Rust 接口对齐。

接口只有一份：`drivers/browser/driver.json`，Rust 用 `include_str!`，JS 用 `loadDriver`。SDK 按它的 schema 检查每次调用的参数和回复。

## 配置

从启动规格的 `config` 读，全部可选：

```json
{ "bin": "node", "args": ["plugins/browser/src/plugin.js"],
  "config": { "headless": false, "profile_dir": "~/.portos-chrome-profile",
              "channel": "chrome", "no_sandbox": false } }
```

- `headless` —— 默认"有显示器就开窗"（macOS/Windows 视为有；Linux 看 `DISPLAY`/`WAYLAND_DISPLAY`）。
- `channel` —— macOS 默认试真实 Chrome，没装则回退 Playwright 自带 Chromium；显式指定时不回退。
- `profile_dir` —— 专用、与日常浏览器隔离；你人肉登录一次，之后复用。
- `no_sandbox` —— Linux 上 Chromium 沙箱不可用时自动带 `--no-sandbox` 重试并告警；这里显式打开。

## 唯一保留的安全性质

专用 profile + 你人肉登录（`browser::login_passthrough` / `browser::resume`）+ 模型只拿到蒸馏元素表和截图、从不读 cookie。

## 跑起来

```sh
npm install
npm test
```
