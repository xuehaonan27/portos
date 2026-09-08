# 理论 ↔ 实现 ↔ 测试 对照总表（portos-rm）

用法：拿着 `design/spec.md` 的节号找模块与测试；或拿着一个失败的测试名反查它执行的是哪条理论。
本表是实现导航；文档与用户裁定优先于测试（D45）。当前开发范围遵循 D49；历史安全／授权测试不构成继续扩展这些机制的要求。

等级：〔文献✓〕被引原文核实／〔推导〕我方映射／〔设计〕自家文档决定。roadmap Phase 0-③ 所要的 `docs/correspondence.md` 即本文件。

## F1 账本 schema／世代化句柄 — spec §2.2、§6.1 — `src/ra.rs` `src/auth.rs` `src/ledger.rs` `schema.sql`

| 理论（等级） | 实现 | 测试（`tests/f1_ledger.rs`） |
|---|---|---|
| RA 四组公理〔文献✓〕 | `Ra` trait；`Ex`/`Count`/`GSet` | `ra_laws_all_algebras` |
| Auth(Option<A>) 的容量有效性，Option 补空持有单位元〔文献✓＋映射〕 | `auth::auth_valid` | `auth_validity_iff` |
| 独占不可双授／计数不可透支〔推导〕 | `Ledger::grant` → `can_mint` | `exclusive_double_grant_refused` `counting_no_overdraft` |
| 稳定指称 ⇒ 世代化句柄〔推导〕 | `Holding.generation`；`StaleGeneration` | `stale_generation_rejected` |
| release 幂等＋租约≡release；子先于父；crash-only 单路径；基底对账〔设计〕 | `release`/`sweep`/`teardown`/`reconcile` | `release_idempotent_and_sweep_equiv` `teardown_children_before_parents` `crash_only_single_path` `reconcile_detects_decay_and_untracked` |
| 全局不变量 ✓(●cap·◯fold)〔推导〕 | `Ledger::invariant` | `invariant_under_random_ops` |
| 释放碎片的 FPU 与中央 grant 容量检查分别验证〔按定义推导〕 | `ra::fpu_holds`；`auth::can_mint` | `release_is_frame_preserving`（B13：Auth 复合元素、含空帧与纯碎片帧的有限样本）`capacity_check_does_not_imply_fpu` |
| 每笔持有保留生命周期记录，聚合值可重算〔设计〕 | `holding` 表一行一笔；`authoritative.cached_outstanding` 可空缓存 | （schema 形状；由上列各测试共同前提） |
| 租约 None＝随 parent 生命期：sweep 闭包级联、子先于父〔设计〕（B14） | `Ledger::sweep` 到期集闭包 | `sweep_cascades_to_parent_bound_children` |
| 跨主体 parent 只沿实例化关系〔设计，决策 2〕 | `Ledger::declare_instantiation`；`grant` 的 `ParentAuthority` 检查 | `cross_subject_parent_requires_instantiation_authority` |
| 转授保持聚合值；parent 存在依赖保持〔设计＋推导〕 | `Ledger::transfer` | `transfer_preserves_aggregate_and_holder_checks` |

## F2 teardown 算法 — spec §6.2 — `src/teardown.rs`

| 理论（等级） | 实现 | 测试（`tests/f2_teardown.rs`） |
|---|---|---|
| 任意序撤销定理（Cordis 43），要求动作、逆与结果两两独立〔文献✓〕 | 波内种子洗牌 `[T43]`；MockWorld 满足该前提 | `t43_any_order_same_final_state` |
| 排序约束只在 ownership 边〔推导＋设计；B10：ownership 边＝持有的存在依赖，T70 的资源侧读法，非 Cordis 实例化树 π〕 | `plan_waves` 深度分层＋执行器 `[TREE]` 守卫 | `ownership_edges_never_violated` |
| ownership 边可跨主体，teardown 按闭包级联〔设计；plugin-system 裁定 6-7〕（B15） | `Ledger::live_closure`；规划器与守卫按闭包 | `teardown_cascades_across_subjects_along_ownership` |
| saga＋WAL〔文献✓〕 | `Journal` write-ahead；三崩点 | `crash_at_every_point_resumes_to_same_state` |
| 恰好一次＝至少一次＋钥匙去重〔推导〕 | `idem_key`；`MockWorld::compensate` | `compensation_exactly_once_and_key_is_load_bearing` |
| release 幂等 ⇒ 盲重放不依赖日志〔推导〕 | `[E-IDEM]`；`resume` 即再调 `teardown` | `inverse_grade_survives_journal_loss` |
| 失败隔离〔设计〕 | Failed 跳过、父项保守挡下 | `failed_branch_does_not_wedge` |
| crash-only 单路径〔文献＋设计〕 | 优雅＝崩溃后同一函数 | `crash_only_single_path_holds_with_journal` |
| ρ 纪律〔文献✓＋推导〕 | `[ρ]` match 分支按 `ClassDecl.revert_grade` | 全组共同前提 |

## F3 monitor／救济语义 — spec §1.3、§4、§6.3 — `src/monitor.rs`

| 理论（等级） | 实现 | 测试（`tests/f3_monitor.rs`） |
|---|---|---|
| precise 上限＝safety〔文献✓〕 | 无扣发件时退化为截停：(动词,目标) 范围＋预算闸 | `truncation_enforces_safety_precisely` |
| renewal 精确边界（TISSEC 3.3/3.4）〔文献✓〕 | `truncation_run` 穷举首截停点；扣发缓冲＝edit 侧 | `no_truncation_strategy_passes_transaction_witness_but_edit_does` |
| withhold＝suppression＋批准后 insertion〔推导〕 | `buffer`/`approve` | `withhold_approve_emits_exactly_once_in_order` |
| feigning acceptance 代价〔文献✓〕→ ttl〔设计〕 | `expire`；段持有走 F2 补偿 | `ttl_bounds_feigning_acceptance_expiry_compensates` |
| 定理 2.5 ⇒ ρ 纪律〔文献✓＋推导〕 | `degrade` 声明表；改写后重检 | `attenuate_only_by_declared_equivalence` |
| edit automaton 只输出合法序列 ⇒ 插入也是发射〔文献✓＋推导〕 | 管线序 confine→sink→staged→budget；缓冲不变式 | `insertion_is_still_an_emission_sink_holds_through_approval`（B3） |
| confine＝替身〔推导〕 | `standin` 通道 | `confine_redirects_to_stand_in_zero_real_effect` |
| escalate 三态〔设计〕 | `Paused`/`resume_with` | `escalate_pauses_and_resumes_exactly_with_fresh_consent` |
| 预算＝counting cap 塌缩〔设计〕、F1 正名〔推导〕 | 同意即铸池、花费即行、闸门即 `can_mint` | `budget_is_rows_not_decrement_gate_is_issuer_gate` |
| 前缀交付／前缀回滚〔设计〕 | `rollback_segment` → F2 teardown | `strict_failstop_delivers_prefix_and_rolls_back_segment_holdings` |
| 绝不静默／读效应不对称〔设计〕 | `Truncated{dropped}`＋trace | `truncate_never_silent` |
| WYSIWYS 可判定〔设计〕 | `check_quad` | `wysiwys_gate_no_consent_no_effect` |
| 事务未达 commit 即 abort；绝不悬置〔文献＋推导〕 | `abort_buffer_on_terminal`（B4） | `tests/f3_monitor_lattice.rs::deterministic_exhaustion_over_combination_lattice`（21,024 局） |
| [TTL] 封顶两种悬置形态（Paused 与 AwaitingApproval）〔设计〕（B12） | `expire` 接受 Paused；`resume_with` 拒过期原同意；拒绝入 trace | `paused_segment_is_bounded_by_original_ttl` |
| [SEG-TX] 段＝事务：commit 转授、非 commit 终态自动回滚、promote 提前 commit〔设计，决策 4〕 | `finish_commit`／`finish_abort`／`promote` | `segment_is_a_transaction_commit_promotes_abort_rolls_back` |

## F4 动词真理表 — spec §2.3、§6.4 — `src/verbs.rs`

| 理论（等级） | 实现 | 测试（`tests/f4_verbs.rs`） |
|---|---|---|
| 效应＝操作＋等式〔文献✓〕；性格分类＋一致性方向〔设计＋推导〕；交换性按动词声明〔推导〕（B11） | `Kind`；`check_coherent`（可重复⟹幂等）；`repeatable()`/`repeatable_shared()` | `coherence_lattice_exhaustive_over_kind_and_flags`（36 格，接受集 34） |
| 持有 ρ 是类属性／动作世界档是动词属性〔文献✓＋推导／设计〕 | `declare_class(ρ)`；`ConsumeGrade`/`EmitGrade` 内嵌于 `Kind` | `holding_rho_is_per_class_action_grade_is_per_verb`（B5） |
| D1 位置判据〔文献✓〕 | 键 (class, verb)；`derive_handler_policy(class)` | `d1_same_verb_two_handlers_projections_differ`（B6） |
| 消耗性读进预算〔设计＋推导〕 | `bears_budget` | `consuming_read_bears_budget_refines_read_write_binary` |
| staged 形状／可摊销／硬清单〔设计〕 | `staged_shape`／`withhold`／`amortizable` | `withhold_iff_non_amortizable_staged_shape_iff_external`（B7） |
| 降档声明住表里＋只准收窄〔推导／设计〕 | `degrade`；`check_all` 严重度序 | `degrade_declared_and_only_narrows` |
| ρ 纪律结构保证；补偿链一步闭合〔文献✓＋推导／设计〕 | `ClassNotDeclared`/`ClassAlreadyDeclared`；`check_all` | `held_requires_declared_class_rho_and_rho_is_immutable` |
| 共享真理表〔推导〕 | `derive_holding_grade`→F2；`derive_handler_policy`→F3 | `truth_table_is_shared_source_for_f2_teardown_and_f3_monitor` |

## F5 manifest requires 类型 — spec §3、§6.5 — `src/coeffect.rs`

| 理论（等级） | 实现 | 测试（`tests/f5_requires.rs`） |
|---|---|---|
| Definition 1（两幺半群＋预序＋双侧分配）〔文献✓〕；两实例〔文献✓〕 | `Scalar`；`Counting`/`Flat` | `definition1_laws_exhaustive_on_counting_flat_and_fixed_vector` |
| 结构化向量形态〔文献✓〕＋预算为多重集〔设计〕 | `Budget`（ℕ 上半模） | `budget_vector_is_a_semimodule_over_counting`；`budget_is_per_effect_class_not_a_total`（B9） |
| application 规则 ~ 缩放〔文献✓〕 | `numeral`/`scale` | `loop_scaling_is_application_rule_numeral_seq_body` |
| B̂ ≥ B〔设计〕；join 实例级〔推导〕 | `demand_sum`/`demand_paths` | `occurrence_sum_overapproximates_path_max_on_all_small_plans`（2343 计划） |
| effect row 位置∩主体〔设计〕 | `ceiling`；`admit_plan` | `effect_row_is_position_ceiling_meet_subject_ceiling` |
| ↓B 与单调性〔推导／设计〕 | `covered_by_budget` | `consent_monotonicity_downset` |
| F4 决定是否计数〔推导〕 | `Requires::from_table` | `repeatable_verbs_cost_zero_via_truth_table` |
| 自然数预算与 Count 在可表示范围内共享加法及上界检查〔推导〕 | （无专用代码） | `budget_merge_is_count_ra_op_and_downset_is_auth_valid` |
| 装载期准入＝集合包含〔设计〕 | `admit_mount` | `manifest_admission_is_set_inclusion` |

## F6 走查修复 — spec §5.2、§6.6 — `src/protocol.rs` `src/bestiary.rs` ＋ 各模块扩充

| 理论（等级） | 实现 | 测试 |
|---|---|---|
| RA 公理对区间/分数实例〔文献✓〕 | `ra::Ranges`/`ra::Frac`；`Frag::Range`/`Frag::Frac` | `f6_algebra.rs::ranges_and_frac_satisfy_ra_laws_and_inclusion_definition`；`ledger_gates_subranges_and_fraction_shares` |
| 协议＝safety（前缀闭、不可救）〔文献✓〕 | `Protocol::check_sequence` | `f6_protocol.rs::protocol_is_a_safety_property_prefix_closed_and_irremediable` |
| 静态可达集＝路径穷举〔推导〕 | `Protocol::reach`/`check_plan` | `static_reachability_equals_path_enumeration_on_all_small_plans`（5668 计划） |
| 扣发重排世界序：投影 sound、无分支精确〔推导〕 | `check_plan_world_order`；`project` | `world_order_projection_is_sound_and_exact_without_branches` |
| safety 由截停器精确执行〔文献✓〕 | `Policy.protocol` 钩子（step ③′、approve 预演） | `protocol_precisely_enforced_by_truncation_tier_in_monitor`；`withhold_reorder_is_caught_at_step_or_at_approval` |
| c-effect 象限：界内变换、逆由类 ρ 承载〔推导；四象限＋边界推进〕 | `Kind::Transforming`；`Policy.contained`；`MockWorld::restore` | `f6_bestiary.rs::workspace_segment_rollback_restores_touched_vm_exactly_once` |
| 按量计价：声明上界 vs 实际计量同一闸门〔设计〕 | `Requires::from_table_weighted`；`Budget::unit_n` | `workspace_weighted_budget_static_bound_covers_metered_spend` |
| 怪物志两条目落位〔推导〕 | `bestiary::workspace`/`bestiary::rdma` | `workspace_entry_passes_all_frozen_gates`；`rdma_entry_passes_all_frozen_gates_with_interval_and_frac`；`rdma_qp_protocol_enforced_end_to_end` |

## F8 附着演练 — `.dev/design/attachments-v0.md` v0.5 — `src/attach.rs`（feature `attach`；未冻结，2026-09-06 用户立项）

| 理论（等级） | 实现 | 测试 |
|---|---|---|
| 触发次数为独立分量 `attach::fire`（Q12）〔推导，锚 F1 每实例容量〕 | `FIRE_CLASS`；`attach` 铸池①含 fire＝n_max；`begin_with` 先过 fire 闸门 | `n_max_is_enforced_through_the_fire_component_even_for_zero_budget_plans`（回归 a） |
| seq 从行读、nonce＝H(h_attach‖seq) 可预测无妨（Q5/Q10）〔推导〕 | `next_seq`（fire 行数＋1，generation 记 seq）；`derive_nonce` | `seq_is_read_from_rows_and_nonces_are_unique_and_stable_across_restart` |
| ②③同事务；恢复规则"有②无③＝空跑"（Q14）〔设计〕 | `Scheduler::transactional`；`Crash::BetweenRows`；`recover` | `fragment_without_pool_after_crash_is_a_fired_but_empty_run`（回归 b） |
| 聚合缓存与逐笔行一致；所测序列保持容量不变量〔设计＋验证〕 | `cached_outstanding` vs `recompute_outstanding`；`invariants` | `outstanding_recomputed_from_live_fragments_equals_cache_after_any_sequence`（回归 c；2×9⁴ 序列） |
| 结账一条路径：段 teardown、③结清后容量置 0（完整账本不变量）、②留存（Q13）〔推导〕 | `settle`；`firing_pool_closed`/`firing_pool_refuses` | `settlement_is_one_path_for_every_terminal_state`（六种终态） |
| 未用余额不退；被拒触发不铸、不算失败（规则 3）〔设计〕 | ②按 B_firing 满额；`Reject::Precondition` 不铸行 | `unused_balance_is_never_refunded_and_rejected_firings_mint_nothing` |
| 触发内 `inbox::emit` 事务性：撤回只及本次触发自己投递的未消费事件（用户裁定）〔设计，锚裁定四〕 | `emit`／`consume`／`withdraw` | `withdrawal_is_bounded_to_this_firings_unconsumed_events`；`pump_failed_always_arrives_normal_path_emits_survive_later_failures` |
| 有界队列、溢出按声明、绝不静默；停机追赶一次记 missed（A4）〔设计〕 | `enqueue`；`recover` 的 Timer 追赶 | `queue_overflow_never_drops_silently_and_timer_catches_up_once_with_missed` |
| 失败预算＝连续 fail-stop 计数、成功复位（A4）〔设计〕 | `settle` 的失败计数；`resume` | `failure_budget_pauses_after_k_consecutive_failstops_and_resets_on_success` |
| detach／到期／撤销同一条 teardown 路径；显式撤销级联（A6）〔设计〕 | `retire`；`revoke` | `detach_expiry_and_revocation_end_in_the_same_ledger_shape` |
| 到期＝根租约、sweep 唯一路径；段租约 None ⇒ 保守 sweep 不等（裁定三）〔推导〕 | 每附着根类含 T；`tick` 调 `Ledger::sweep` | `firing_segments_carry_no_lease_so_expiry_never_waits_on_a_run_in_flight` |
| h_table 入签、表变重准入（Q7）〔设计〕 | `table_change`／`resign` | `table_change_reruns_admission_paused_until_resign_or_detached` |
| 每附着串行、min_interval（A4）〔设计〕 | `Reject::Serial`／`MinInterval` | `per_attachment_serial_and_min_interval_hold` |

## 接线对照：法则 → 内核对象 → 集成测试（2026-09-05，`.dev/gen/rm-wiring-status.md`；WP-01 增补 2026-09-06；WP-03 增补 2026-09-08）

| 法则 | 内核对象（`crates/portos-kernel`） | 测试 |
|---|---|---|
| F1 行式生命周期记账与中央容量闸门（§6.1） | `ledger::LedgerStore::spend/spent`（写穿 `holdings` 表）；`caps::CapStore::exercise/counts_left`（`counts` 为池容量） | `caps::counting_exercise_never_overdraws`；`ledger::spend_rows_persist_and_gate_refuses_at_capacity`；echo `counting_budget_is_ledger_rows_and_survives_kernel_reopen` |
| F1 世代化句柄、重启对账 | `kernel/plugin` 持有（世代＝spawn token）；`LedgerStore::reconcile_stale_process_rows` | `ledger::stale_plugin_rows_are_reconciled_on_open` |
| F2 crash-only、子先于父、ownership 闭包、`World` | `host::reclaim`＋`host::HostWorld`（`teardown_with`） | echo `plugin_death_is_reclaimed_crash_only_children_first` |
| ②③同事务、恢复规则（attachments §4.3 规则 5／§13.3 Q14；F1 行式记账契约的实装形态） | `LedgerStore::transaction`（单 SQLite 事务＋内存快照回滚）；`spend_many`；`hold_exclusive`/`release`/`teardown`/对账全部经其路由 | `ledger::compound_ledger_write_is_atomic_under_injected_failure` |
| F2 saga-log 持久化（§6.2 [SAGA]/[E-IDEM]：write-ahead、盲重放） | `journal` 表（`holding_id` 主键）；teardown 与墓碑同事务写穿；非 done 条目由该主体下一次 teardown 载入回放（Failed→Pending）；`open` 报告 `journal_pending`；对账落墓碑即解为 done（行即真相） | `ledger::journal_entries_survive_reopen_and_replay_once` |
| 本地订阅＝`kernel/subscription` 持有，可显式退订 | `host::Host::subscribe_local`/`unsubscribe_local`（先查 subs 锁、账本释放、再移除） | `host::unsubscribe_local_releases_the_subscription_holding` |
| 跨主体 parent 只沿实例化关系（§6.1 [AUTH-EDGE]，决策 2）；闭包级联、子先于父（§6.2） | `spawn_child` client op（`kernel:spawn` 能力门）；`LedgerStore::hold_exclusive_child`（实例化登记＋grant 同事务）；对账子先于父不动点 | echo `spawn_child_is_capability_gated_and_parent_death_reclaims_children_first`；`ledger::stale_plugin_rows_are_reconciled_on_open`（父子行） |
| 租约≡release、保守 sweep（§6.1，裁定三）；crash-only 基底对账（§6.2） | `LedgerStore::sweep_with_world`＋`Host::start_sweeper`；`hold`/`release`/`renew` ops（`kernel/process`/`kernel/port`/`kernel/file-lock`，`substrate` 表见证）；`reconcile_substrate`（pid+启动时间、bind 探测、锁属主） | `ledger::expired_parent_waits_for_child_with_its_own_lease`；`ledger::reconcile_kills_a_live_process_of_a_previous_kernel_incarnation`；`host::sweeper_thread_expires_leased_holdings_and_removes_the_lock_file`；echo `plugin_registers_a_child_process_holding_and_kill_minus_nine_reaps_it`；chat `browser_close_releases_process_and_profile_lock_holdings` |
| F4 真理表一致性、按 handler 投影 | `host::build_verb_table`（hello `tools[verb].kind`、`holding_rho`）；`grants` 的 `kind`/`budgeted` | echo `verb_kind_metadata_is_checked_at_spawn_and_exposed_in_grants` |
| F5 effect row（位置 ∩ 主体）、装载期集合包含 | `host::Slot`、`spawn_in`＋`admit_mount`、invoke 的 row 检查；chat.json plugin 条目暴露 `slot` | echo `slot_row_bounds_invoke_and_admits_requires`；chat `chat_json_slot_refusal_is_reported` |
| F5 B̂＝出现之和（§6.5，effect-plan §5.2）＝M0 准入预算 | `plan_ir::to_ast_nodes`＋`coeffect::Plan::from_ast`（WP-05 投影链；kernel feature `plans`，G1 前默认关） | `plans::plancheck_budget_equals_demand_sum_on_all_small_plans`（2343 计划；`cargo test -p portos-kernel --features plans`） |
| F6 协议＝safety，截停档精确执行 | `host::call_on` 步进（hello `protocol`） | echo `protocol_order_is_enforced_at_call` |
| F3 monitor（sink／扣发／同意／三态／段事务） | **已成（2026-09-07，D43/WP-06）**：`plans::PlanService`；同意＝按族铸纤维能力（行式预算）；`suppression_buffer`＋跨进程 `approve`；三模式＋进程内 escalate；段＝`transfer_all`/teardown | `plans::` 十条内核集成测试（镜像法则名）＋ chat `plan_run_navigate_type_submit_withheld_then_approved` |
| 能力＝持有（spec §6.1；WP-03/D48，2026-09-08） | `kernel/cap` 类；`CapStore::mint/attenuate` 同事务铸持有（租约＝`expires_at`）；`exercise`/`find_and_exercise`/`list_live` 以持行为活性真相；`revoke`＝CDT 级联＋逐能力 `LedgerStore::teardown_subtree`（释放花费行＋池归零同事务）；`Host::revoke_capability` | `caps::revoking_a_grant_releases_its_holding_and_children_first`；`caps::expired_capability_is_refused_by_the_gate_after_sweep`；`caps::caps_and_holdings_agree_after_reopen`；echo `attenuated_child_capability_dies_with_its_parent_grant` |
| F2 子树清理域（撤销级联的参照语义） | `Ledger::live_subtree`＋`teardown_subtree_with`（与 `teardown_with` 共享执行器内核 `run`；[SAGA]/[TREE]/[KEY] 纪律同一） | `f2_teardown::subtree_teardown_is_children_first_and_spares_off_tree_holdings` |

## 2026-09-08 理论修订的反例与边界

| 契约 | 验证 |
|---|---|
| FPU 必须检查空帧；Option 添加单位元 | `theory_regressions::exclusive_update_must_preserve_validity_with_no_frame`；`option_supplies_units_without_changing_base_inclusion` |
| Count 溢出非法且保持 RA 法则 | `capacity_gate_rejects_count_overflow`；`count_overflow_is_absorbing_and_obeys_ra_laws` |
| Frac 结合律、有效性下行不被整数截断破坏 | `fraction_composition_is_exact_at_large_denominators`；`invalid_fraction_cannot_become_valid_by_composition` |
| 静态预算精确计量，附着乘法溢出在建池前拒绝 | `static_budget_overflow_is_not_covered_by_a_maximum_pool`；`f8_attach::total_budget_overflow_refuses_attachment_before_creating_pools` |
| 内核先检查步数上限，避免拒绝超大计划前预算先溢出 | 内核 `plancheck::tests::oversized_nested_budget_is_refused_without_arithmetic_overflow` |
| 消费允许同一请求的幂等重试声明 | `consuming_request_can_declare_idempotent_retries`；F4 一致性表 |
| FPU 测试的 Auth 载体包含权威互斥与纯碎片帧 | `f1_ledger::auth_fixture_has_exclusive_authority_and_satisfies_ra_laws` |
| 超额拒绝的事务与重启一致；精确分数持久化 | 内核 `ledger::tests::count_overflow_refuses_atomically_and_survives_reopen`；`fragments_round_trip_through_json` |

这些测试检查具体反例与有限载体，不证明任意资源的生命周期交接或观察恢复；此类义务见 spec §2.5–§2.7、plugin-system §6.11。
