# portos-rm — PortOS 资源管理的法则 crate

**这是什么**：`.dev/design/spec.md` 里六个冻结决策（F1 账本／F2 teardown／F3 monitor／F4 动词真理表／F5 requires／F6 协议与怪物志）的理论列翻译成的 Rust 类型与**法则测试**——每个测试名＝它执行的定理或纪律。2026-09-05 从演练件 `.dev/design/portos-rm` 搬入 workspace（用户裁定："法则跟着语义走"）；内核 `crates/portos-kernel` 直接依赖本 crate 的账本、执行器、真理表、requires 与协议（接线状态见 `.dev/gen/rm-wiring-status.md`）。

```
cargo test -p portos-rm      # 82 条法则（F1–F6 68 ＋ F8 14），全绿；零依赖
```

## 布局

| 决策 | 规格 | 模块 | 法则测试 |
|---|---|---|---|
| F1 账本 schema／世代化句柄／转授／实例化权限 | spec §6.1 | `src/ra.rs` `src/auth.rs` `src/ledger.rs` `schema.sql` | `tests/f1_ledger.rs`（15） |
| F2 teardown 算法（`World` trait；`teardown_with` 供内核在自己的锁纪律下调用） | spec §6.2 | `src/teardown.rs` | `tests/f2_teardown.rs`（8） |
| F3 monitor／救济语义／段＝事务 | spec §6.3 | `src/monitor.rs` | `tests/f3_monitor.rs`（14）＋`tests/f3_monitor_lattice.rs`（21,024 局穷举） |
| F4 动词真理表 | spec §6.4 | `src/verbs.rs` | `tests/f4_verbs.rs`（8，含 36 格穷举） |
| F5 manifest requires 类型 | spec §6.5 | `src/coeffect.rs` | `tests/f5_requires.rs`（10，含三类穷举） |
| F6 协议列、区间/分数代数、按量计价、怪物志 | spec §6.6、§5.2 | `src/protocol.rs` `src/bestiary.rs` | `tests/f6_algebra.rs`（2）`tests/f6_protocol.rs`（5）`tests/f6_bestiary.rs`（5） |
| **F8 附着演练**（未冻结；feature `attach`）：三种行与结账、合成类 `attach::fire`、派生 nonce、队列与溢出、失败预算、租约到期、显式撤销级联、崩溃恢复、有界撤回 | `.dev/design/attachments-v0.md` v0.5（§3–§5、§11、§13.3 Q12–Q14） | `src/attach.rs` | `tests/f8_attach.rs`（14，含三条回归：零预算 n_max、②③间崩溃恢复、任意序列后 outstanding 重算——2×9⁴ 序列穷举） |

规格 § ↔ 模块 ↔ 测试 ↔ 内核对象的对照见 `docs/correspondence.md`。

## 隔离区（decisions-v1 D31）

计划语言与解释器仍在讨论中。计划形状（`coeffect::Plan`、`demand_sum`、`demand_paths`、`admit_plan`；`protocol::reach`/`check_plan`/`check_plan_world_order`/`project`/`enumerate_paths`）只在 cargo feature **`plan-shapes`** 下编译（默认开，法则测试用）。内核以 `default-features = false` 依赖本 crate：想在内核里引用 `Plan` 就编不过。 F8 附着演练（`attach.rs`）同属隔离区，在 feature **`attach`** 下编译（默认开）：附着是效应计划的另一半，接线以 D31 解禁与 F3 接线为前提。

## 读代码的约定

- 每个模块头注列出**理论标签**（`[T43]`、`[SUPPR]`、`[DEF1]`、`[SEG-TX]`…）与等级（【文献✓】被引原文核实／【推导】我方映射／【设计】自家文档决定）。
- "冻结"的机械含义：改语义不改法则表 ⇒ 红灯。欲改法则表，先证 spec 对应理论列有误（注意等级）。
- 崩溃/并发以确定性模拟表达（崩点枚举、执行序置换）；内核实装换真并发时法则组照跑。
- 十六处问题墓碑（B1–B16）见 spec §0.4；每处在对应模块头注与测试里都有墓碑。
