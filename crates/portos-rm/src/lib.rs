//! portos-rm — PortOS 资源管理的**可运行法则表**（冻结演练 F1–F6 的产物）。
//!
//! 身份（用户裁定 2026-09-04）：这是"在具有理论的前提下把理论转化为 Rust、摸索 PortOS 资源管理
//! 该怎么做"的摸索件，**不是成品**。接线（roadmap Phase C/D）时把 tests/ 里的法则搬进真 crate
//! 照跑，本 crate 退役为参照——法则跟着语义走，不是 crate 跟着走。
//!
//! 规格：design/spec.md（唯一实现参考；§6 每个决策的"理论→实现→测试"三列表指向本 crate）。
//! 对照总表：docs/correspondence.md（规格 § ↔ 模块 ↔ 测试）。
//!
//! 模块地图（按冻结决策）：
//!   F1 账本 schema／世代化句柄   ra.rs（RA 合同＋Ex/Count/GSet/Ranges/Frac）· auth.rs（●/◯ 闸门）
//!                              · ledger.rs（行式碎片、世代、租约、teardown、对账）· schema.sql
//!   F2 teardown 算法           teardown.rs（波次规划、saga-log、崩溃模拟、按钥匙去重、类 restore）
//!   F3 monitor／救济语义        monitor.rs（WYSIWYS 准入、edit automaton 扣发、三救济、三模式、协议钩子）
//!   F4 动词真理表              verbs.rs（性格四分类＋世界档、类 ρ、降档表、协议声明、按 handler 投影）
//!   F5 manifest requires 类型   coeffect.rs（scalar 合同、Flat/Counting、预算向量、计划求值、两道准入）
//!   F6 走查修复                protocol.rs（安全自动机、静态可达集、世界序投影）· bestiary.rs（workspace／rdma）
//!   F8 附着演练（feature attach） attach.rs（三种行与结账、合成类 attach::fire、派生 nonce、队列与溢出、
//!                              失败预算、租约到期、显式撤销级联、崩溃恢复、有界撤回；无计划语言）
//!
//! 测试即法则（每个测试名＝它执行的定理/纪律）：tests/f1_ledger.rs … tests/f6_bestiary.rs。
//! 改动语义而不改法则表 ⇒ 红灯——这就是"冻结"的机械含义。

#[cfg(feature = "attach")]
pub mod attach;
pub mod auth;
pub mod bestiary;
pub mod coeffect;
pub mod ledger;
pub mod monitor;
pub mod protocol;
pub mod ra;
pub mod teardown;
pub mod verbs;
