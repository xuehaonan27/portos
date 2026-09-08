# portos-rm — PortOS 资源管理的法则 crate

**这是什么**：资源管理文档的可执行参照，内核使用其中的账本、回收执行器及声明设施。按 D45，确定的设计文档与用户裁定是标准，测试提供反例和回归证据，不是不可修订的定论。当前资源语义见 `.dev/design/spec.md` v1.5；D49 已冻结的安全／授权内容不构成当前开发要求。

```
cargo test -p portos-rm
```

## 布局

| 决策 | 规格 | 模块 | 法则测试 |
|---|---|---|---|
| F1 账本 schema／世代化句柄／转授／实例化权限 | spec §6.1 | `src/ra.rs` `src/auth.rs` `src/ledger.rs` `schema.sql` | `tests/f1_ledger.rs` |
| F2 teardown 算法（`World` trait；`teardown_with` 供内核在自己的锁纪律下调用） | spec §6.2 | `src/teardown.rs` | `tests/f2_teardown.rs` |
| F3 monitor／救济语义／段＝事务 | spec §6.3 | `src/monitor.rs` | `tests/f3_monitor.rs`＋`tests/f3_monitor_lattice.rs`（21,024 局穷举） |
| F4 动词真理表 | spec §6.4 | `src/verbs.rs` | `tests/f4_verbs.rs`（含 36 格穷举） |
| F5 manifest requires 类型 | spec §6.5 | `src/coeffect.rs` | `tests/f5_requires.rs`（含三类穷举） |
| F6 协议列、区间/分数代数、按量计价、怪物志 | spec §6.6、§5.2 | `src/protocol.rs` `src/bestiary.rs` | `tests/f6_algebra.rs`、`tests/f6_protocol.rs`、`tests/f6_bestiary.rs` |
| **F8 附着演练**（未冻结；feature `attach`）：三种行与结账、合成类 `attach::fire`、派生 nonce、队列与溢出、失败预算、租约到期、显式撤销级联、崩溃恢复、有界撤回 | `.dev/design/attachments-v0.md` v0.5（§3–§5、§11、§13.3 Q12–Q14） | `src/attach.rs` | `tests/f8_attach.rs`（含预算边界与三条生命周期回归：零预算 n_max、②③间崩溃恢复、任意序列后 outstanding 重算——2×9⁴ 序列穷举） |

规格 § ↔ 模块 ↔ 测试 ↔ 内核对象的对照见 `docs/correspondence.md`。

## Feature 边界

D41 已解除计划语言与解释器的 D31 隔离。`plan-shapes` 提供计划 AST 和静态分析；内核以 `default-features = false, features = ["plan-shapes"]` 接入。`attach` 提供 F8 附着演练，目前未接入内核。两个 feature 在本 crate 的默认测试中开启。

## 读代码的约定

- 每个模块头注列出**理论标签**（`[T43]`、`[SUPPR]`、`[DEF1]`、`[SEG-TX]`…）与等级（【文献✓】被引原文核实／【推导】我方映射／【设计】自家文档决定）。
- 文档修订后同步更新对应实现与测试；通过有限测试不等于证明全部法则。
- 崩溃/并发以确定性模拟表达（崩点枚举、执行序置换）；内核实装换真并发时法则组照跑。
- 十六处问题墓碑（B1–B16）见 spec §0.4；每处在对应模块头注与测试里都有墓碑。

## 数值与代数表示

`Count::Value(u64)` 表示可用计数，溢出产生 `Count::Invalid`，不能继续通过容量检查。`Frac` 通过 `num-rational`／`num-bigint` 精确组合分数，超过 1 进入吸收的非法元。Ex／Frac 的基础包含关系不人为取自反闭包；账本用 `Option<A>` 的单位元表达空持有。`auth_valid` 是容量相容性谓词，`can_mint` 检查完整账本，不宣称运行时 grant 是 Iris local update。

静态 `Counting` 使用任意精度自然数，附着总预算超出 u64 时在建池前拒绝；静态需求不能饱和成可接受的额度。

理论反例回归见 `tests/theory_regressions.rs`；跨持久化的计数边界与分数往返测试见内核账本模块。
