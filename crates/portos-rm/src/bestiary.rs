//! 怪物志（bestiary）— 资源模型的立案仪器（endstate §8.11），F6 起为可运行 fixture。
//!
//! 立案规则：拿四模块问卷（M 共享结构／Σ,E 签名等式／T 时间故障／W 世界关系）去安放一个
//! 真实资源；安放不顺的地方不是"再加一列"，而是记下**哪个既有数学对象缺了一个元素**。
//! 每个条目给出：类声明（F1 账本＋F4 真理表＋F5 manifest）＋走查结论（哪些直接落入冻结机制、
//! 哪些逼出了库级扩充）。法则测试（tests/f6_bestiary.rs）用它们真的驱动 F1–F5 跑通。
//!
//! 已册：socket（endstate §8.1，F4 的 recv/send/connect 一类三性格由它而来）；
//! 本文件：workspace（microVM/容器）、rdma。
//!
//! 走查战果（2026-09-05，两条目合计）：
//!   · 直接落入已冻结机制、一行不改：持有树、租约、对账、消耗读、发射、纠缠态、入向通道。
//!   · 库级扩充（申报过的待泛化项被顶成承重项）：区间/分数代数（ra.rs Ranges/Frac）；
//!     协议次序列（protocol.rs）；按量计价（coeffect.rs from_table_weighted）。
//!   · 真理表补档：c-effect（verbs.rs Kind::Transforming）——四象限有它、三分类没有。
//!   · 理论预言兑现：RDMA 硬件旁路 ⇒ 膜完备性告警（theory-spec §1.3 注记②）⇒ 中介点前移到
//!     reg_mr 的远端访问授予；ibverbs 的一个调用在我们的接口里拆成两个动词
//!     （"可交换性/可中介性是接口选择出来的"，endstate §7.1）。

use crate::coeffect::{Manifest, Mount, Requires};
use crate::ledger::{AlgebraTag, ClassDecl, Frag, Ledger, RevertGrade};
use crate::protocol::Protocol;
use crate::ra::{Count, Ex, Frac, GSet, Ranges};
use crate::verbs::{ConsumeGrade, EmitGrade, VerbEntry, VerbTable};

/// 一个怪物志条目：三张表（账本类声明、真理表、manifest）＋挂载点。
pub struct Entry {
    pub name: &'static str,
    pub ledger: Ledger,
    pub table: VerbTable,
    pub manifest: Manifest,
    pub mount: Mount,
}

fn class(l: &mut Ledger, id: &str, algebra: AlgebraTag, lease: Option<u64>, rho: RevertGrade) {
    l.register_class(ClassDecl {
        class_id: id.into(),
        algebra,
        release_idempotent: true,
        lease_secs: lease,
        revert_grade: rho,
    });
}

// ===========================================================================
// 条目一：Workspace（microVM / 容器）—— "把一块世界 reify 进界内"的教科书案例。
// ===========================================================================
//
// 四模块问卷：
//   M  VM 实例＝Ex；vCPU/内存/磁盘配额＝Count（容量池）；只读 rootfs 镜像多 VM 共享＝GSet（可复制）。
//   Σ/E spawn（Held）、exec/write_file/pause/resume/restore（**Transforming**：界内变换，
//      逆由类 ρ＝快照承载）、read_file/inspect（Repeatable）、snapshot（Held：占盘的持有）、
//      mount（Held）、net_send（Emitting External 可摊销：走 egress 代理）、copy_out（Repeatable
//      读入，taint 归数据面）。
//   T  空闲租约；pause/resume；对账＝查 firecracker API / ip link。
//   W  出网＝emission 通道（必经代理）；监听端口＝常驻入向通道；VM 内部是叠层发放方
//      （guest agent 可为界内进程的 issuer：ρ_类 ⊑ ρ_基底）。
//   ownership：enclosure ⊃ vm ⊃ {tap, rootfs-overlay, snapshot, mount, proc}。
//
// 走查结论：唯一安放不顺的是 exec/write —— 三分类里没有 c-effect。F6 补 Kind::Transforming 后
// 一切落位；段回滚＝restore 到段起点快照（F3 rollback_segment 的 [CEFF] 路径）。
pub fn workspace() -> Entry {
    let mut l = Ledger::new();
    for (id, alg, lease) in [
        ("enclosure", AlgebraTag::Exclusive, Some(3600)),
        ("vm", AlgebraTag::Exclusive, Some(600)),
        ("tap", AlgebraTag::Exclusive, Some(600)),
        ("rootfs-overlay", AlgebraTag::Exclusive, Some(600)),
        ("snapshot", AlgebraTag::Exclusive, Some(86400)),
        ("mount", AlgebraTag::Exclusive, Some(600)),
        ("proc", AlgebraTag::Exclusive, Some(60)),
    ] {
        class(&mut l, id, alg, lease, RevertGrade::Inverse);
    }
    class(&mut l, "vcpu", AlgebraTag::Counted, None, RevertGrade::Inverse);
    class(&mut l, "mem-mib", AlgebraTag::Counted, None, RevertGrade::Inverse);
    class(&mut l, "image", AlgebraTag::Set, None, RevertGrade::Inverse); // 只读镜像：可复制共享
    l.set_capacity("vcpu", "host", Frag::Count(Count(16)));
    l.set_capacity("mem-mib", "host", Frag::Count(Count(32768)));
    l.set_capacity("image", "ubuntu-24.04", Frag::Set(GSet::of(&["ro"])));
    for (c, i) in [("enclosure", "ws-1"), ("vm", "vm-1"), ("tap", "tap0"), ("rootfs-overlay", "ov-1"),
                   ("snapshot", "snap-0"), ("mount", "m-1"), ("proc", "shell")] {
        l.set_capacity(c, i, Frag::Ex(Ex::Token));
    }

    let mut t = VerbTable::new();
    for c in ["enclosure", "vm", "tap", "rootfs-overlay", "snapshot", "mount", "proc"] {
        t.declare_class(c, RevertGrade::Inverse).unwrap();
    }
    let held = || VerbEntry::consuming(ConsumeGrade::Held);
    t.register("vm", "spawn", held()).unwrap();
    t.register("vm", "exec", VerbEntry::transforming()).unwrap(); // [CEFF] 界内变换
    t.register("vm", "write_file", VerbEntry::transforming()).unwrap();
    t.register("vm", "pause", VerbEntry::transforming().with_flags(true, false)).unwrap(); // 幂等变换
    t.register("vm", "resume", VerbEntry::transforming().with_flags(true, false)).unwrap();
    t.register("vm", "restore", VerbEntry::transforming().with_flags(true, false)).unwrap(); // 幂等：恢复到同一快照
    t.register("vm", "read_file", VerbEntry::repeatable()).unwrap();
    t.register("vm", "inspect", VerbEntry::repeatable()).unwrap();
    t.register("vm", "copy_out", VerbEntry::repeatable()).unwrap(); // 读入：taint 归数据面
    t.register("snapshot", "snapshot", held()).unwrap(); // 快照是一笔占盘的持有
    t.register("mount", "mount", held()).unwrap();
    t.register("tap", "attach", held()).unwrap();
    t.register("proc", "spawn", held()).unwrap();
    // 出网：外部发射、可摊销（一次同意批量出网），必经 egress 代理（policy 的 (verb,target) 范围）。
    t.register("vm", "net_send", VerbEntry::emitting(EmitGrade::External, true)).unwrap();
    t.check_all().unwrap();

    let mut m = Manifest { driver: "workspace".into(), verbs: Default::default() };
    // 按量计价：exec 声明每次最多 5 单位 fuel；写文件每次最多 2；出网每次 1。
    m.verbs.insert("exec".into(), Requires::from_table_weighted(&t, "vm", "exec", &["ws.exec"], &[], 5).unwrap());
    m.verbs.insert("write_file".into(), Requires::from_table_weighted(&t, "vm", "write_file", &["ws.fs"], &[], 2).unwrap());
    m.verbs.insert("read_file".into(), Requires::from_table(&t, "vm", "read_file", &["ws.fs"], &[]).unwrap());
    m.verbs.insert("net_send".into(), Requires::from_table(&t, "vm", "net_send", &["net.egress"], &["egress-proxy"]).unwrap());
    m.verbs.insert("copy_out".into(), Requires::from_table(&t, "vm", "copy_out", &["ws.fs"], &["cas"]).unwrap());
    let mount = Mount {
        name: "workspace-slot".into(),
        offers: crate::coeffect::Flat::of(&["ws.exec", "ws.fs", "net.egress"]),
        provides: crate::coeffect::Flat::of(&["egress-proxy", "cas"]),
    };
    Entry { name: "workspace", ledger: l, table: t, manifest: m, mount }
}

// ===========================================================================
// 条目二：RDMA —— 硬件旁路把"中介点在哪"逼成设计决定。
// ===========================================================================
//
// 四模块问卷：
//   M  设备共享（open＝可复制）；PD/QP/CQ 各为独占持有；pinned 内存＝Count 对 memlock；
//      **MR 子区间／memory window＝Ranges**（不相交区间 RA）；**多 QP 共享读同一 MR＝Frac**。
//   Σ/E open_device（Held 可复制）、alloc_pd/reg_mr/create_qp/create_cq（Held）、
//      qp.init/rtr/rts（**Transforming**：状态迁移，逆＝modify 回 RESET）、post_recv（Transforming：
//      往本地队列放缓冲）、post_send（Emitting External 可摊销：RDMA WRITE 打到远端内存）、
//      rdma_read（Emitting External 可摊销：请求出界、数据带 taint 回流）、poll_cq（Consuming
//      External：出队即消耗，socket 之教训原样）、**grant_remote_access**（Emitting External
//      **不可摊销**：把内存暴露给远端一侧读写——这才是真正的发射点）。
//      **协议**：QP 状态机 reset→init→rtr→rts；post_send/post_recv 只在 rts 合法。
//   T  QP 掉 ERROR 靠对账；连着的 QP＝纠缠态（对端持状态），拆只能 CM disconnect；
//      进程死亡内核自动回收 ibverbs 资源，账本对账即可。
//   W  rkey 是世界侧凭证（32 位可猜——ReDMArk, USENIX Sec'21），归"秘密由 broker 持有注入"：
//      驱动持有、永不进上下文；async event channel＝入向通道；
//      **膜完备性**：grant 之后对端读写不经本机 CPU——逐操作监控在物理上不存在，
//      中介点必须前移到 grant 这一刻（D1：谁是 handler、在哪个位置决定 w-地位）。
//   ownership：device ⊃ pd ⊃ {mr ⊃ {mw…}, cq, qp}。
//   第五关切候选：NIC/NUMA 亲和（位置/QoS）——endstate §8.8 观察清单预留格的首个候选，
//   本条目只登记不建模。
//
// 走查结论：三处库级扩充被顶成承重（区间/分数代数、协议列、按量计价），一处接口拆分
// （reg_mr 与 grant_remote_access 分开），零处形状缺口。
pub fn rdma() -> Entry {
    let mut l = Ledger::new();
    class(&mut l, "device", AlgebraTag::Set, None, RevertGrade::Inverse);
    for id in ["pd", "cq", "qp"] {
        class(&mut l, id, AlgebraTag::Exclusive, Some(300), RevertGrade::Inverse);
    }
    class(&mut l, "mr", AlgebraTag::Range, Some(300), RevertGrade::Inverse); // 子区间持有
    class(&mut l, "mr-read", AlgebraTag::Frac, Some(300), RevertGrade::Inverse); // 共享读份额
    class(&mut l, "memlock-kib", AlgebraTag::Counted, None, RevertGrade::Inverse);
    l.set_capacity("device", "mlx5_0", Frag::Set(GSet::of(&["open"])));
    l.set_capacity("pd", "pd-1", Frag::Ex(Ex::Token));
    l.set_capacity("cq", "cq-1", Frag::Ex(Ex::Token));
    l.set_capacity("qp", "qp-1", Frag::Ex(Ex::Token));
    l.set_capacity("mr", "buf-A", Frag::Range(Ranges::of(&[(0, 8192)]))); // 8 KiB 缓冲区
    l.set_capacity("mr-read", "buf-A", Frag::Frac(Frac::one()));
    l.set_capacity("memlock-kib", "proc", Frag::Count(Count(65536)));

    let mut t = VerbTable::new();
    for c in ["device", "pd", "cq", "qp", "mr", "mr-read"] {
        t.declare_class(c, RevertGrade::Inverse).unwrap();
    }
    let held = || VerbEntry::consuming(ConsumeGrade::Held);
    t.register("device", "open_device", held().with_flags(false, true)).unwrap();
    t.register("pd", "alloc_pd", held()).unwrap();
    t.register("cq", "create_cq", held()).unwrap();
    t.register("cq", "poll_cq", VerbEntry::consuming(ConsumeGrade::External)).unwrap(); // 出队即消耗
    t.register("mr", "reg_mr", held()).unwrap(); // 本地注册：持有
    t.register("mr", "bind_mw", held()).unwrap(); // memory window：子区间持有
    // 真正的发射点：把内存暴露给远端。硬清单（不可摊销）——逐次同意；ibverbs 里与 reg_mr 同一
    // 调用，我们的接口拆开——中介点就在这里。
    t.register("mr", "grant_remote_access", VerbEntry::emitting(EmitGrade::External, false)).unwrap();
    t.register("qp", "create_qp", held()).unwrap();
    t.register("qp", "init", VerbEntry::transforming()).unwrap(); // 状态迁移：界内变换
    t.register("qp", "rtr", VerbEntry::transforming()).unwrap();
    t.register("qp", "rts", VerbEntry::transforming()).unwrap();
    t.register("qp", "post_recv", VerbEntry::transforming()).unwrap(); // 往本地队列放缓冲
    t.register("qp", "post_send", VerbEntry::emitting(EmitGrade::External, true)).unwrap(); // RDMA WRITE 到远端
    t.register("qp", "rdma_read", VerbEntry::emitting(EmitGrade::External, true)).unwrap(); // 请求出界，数据带 taint 回流
    t.register("qp", "query", VerbEntry::repeatable()).unwrap();
    // 协议：QP 状态机。post_send/post_recv 只在 rts 合法；query 不在辖域（任何状态可查）。
    let qp_proto = Protocol::new("reset")
        .transition("reset", "init", "init")
        .transition("init", "rtr", "rtr")
        .transition("rtr", "rts", "rts")
        .transition("rts", "post_send", "rts")
        .transition("rts", "post_recv", "rts")
        .transition("rts", "rdma_read", "rts");
    t.declare_protocol("qp", qp_proto).unwrap();
    t.check_all().unwrap();

    let mut m = Manifest { driver: "rdma".into(), verbs: Default::default() };
    // 按量：post_send 声明每次最多 64 KiB（单位 KiB）；rdma_read 同；其余按次。
    m.verbs.insert("post_send".into(), Requires::from_table_weighted(&t, "qp", "post_send", &["rdma.send"], &["pd", "cq"], 64).unwrap());
    m.verbs.insert("rdma_read".into(), Requires::from_table_weighted(&t, "qp", "rdma_read", &["rdma.read"], &["pd", "cq"], 64).unwrap());
    m.verbs.insert("post_recv".into(), Requires::from_table(&t, "qp", "post_recv", &["rdma.local"], &[]).unwrap());
    m.verbs.insert("grant_remote_access".into(), Requires::from_table(&t, "mr", "grant_remote_access", &["rdma.expose"], &["rkey-broker"]).unwrap());
    m.verbs.insert("poll_cq".into(), Requires::from_table(&t, "cq", "poll_cq", &["rdma.local"], &[]).unwrap());
    m.verbs.insert("query".into(), Requires::from_table(&t, "qp", "query", &["rdma.local"], &[]).unwrap());
    let mount = Mount {
        name: "rdma-slot".into(),
        offers: crate::coeffect::Flat::of(&["rdma.send", "rdma.read", "rdma.local", "rdma.expose"]),
        provides: crate::coeffect::Flat::of(&["pd", "cq", "rkey-broker"]),
    };
    Entry { name: "rdma", ledger: l, table: t, manifest: m, mount }
}
