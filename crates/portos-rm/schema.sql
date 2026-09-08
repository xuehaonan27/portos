-- F1 演练的历史 DDL 参照，不由内核执行。
-- 当前 SQLite 定义与迁移见 portos-kernel/src/db.rs 和 ledger/recovery.rs。
-- 设计后果 1（无消去性）：碎片逐行为真相，合成值只可重算/缓存对账，绝不原地扣减。
-- 设计后果 2（发放方闸门）：grant 必须对照 authoritative 行做全量合成检查。

CREATE TABLE resource_class (
  class_id            TEXT PRIMARY KEY,   -- "tcp-port" | "proc-tree" | "artifact-ttl" | ...
  algebra             TEXT NOT NULL,      -- M: "exclusive" | "counted" | "set"
  sigma               TEXT NOT NULL,      -- Σ: JSON 动词签名（acquire/release/renew/...）
  laws                TEXT NOT NULL,      -- E: JSON {release_idempotent, commutes_across_instances, protocol}
  temporal            TEXT NOT NULL,      -- T: JSON {lease_secs, reconcile: "substrate"|"wal"|"none", parent_class}
  world               TEXT NOT NULL       -- W: JSON {revert_grade: "inverse"|"compensable"|"external", observable, inbound}
);

CREATE TABLE authoritative (
  class_id            TEXT NOT NULL REFERENCES resource_class(class_id),
  instance            TEXT NOT NULL,      -- 具名实例（端口号）或 "pool"（可替代类容量池）
  capacity            TEXT NOT NULL,      -- ● 容量元素（序列化）
  cached_outstanding  TEXT,               -- ◯ 合成缓存（可空；对账时以 holding 折叠重算校验）
  PRIMARY KEY (class_id, instance)
);

CREATE TABLE holding (
  holding_id          INTEGER PRIMARY KEY AUTOINCREMENT,
  subject             TEXT NOT NULL,      -- fiber 实例 id
  class_id            TEXT NOT NULL REFERENCES resource_class(class_id),
  instance            TEXT NOT NULL,
  fragment            TEXT NOT NULL,      -- ◯ 碎片（序列化；一行=一笔）
  generation          TEXT NOT NULL,      -- 世代见证（pid 启动时间 / CDP target nonce / fd inode）
  parent              INTEGER REFERENCES holding(holding_id),  -- ownership 树边
  lease_expires_at    INTEGER,            -- unix 秒；历史 NULL：有 parent 随父，无 parent 无期限；运行中用 Lease 枚举区分
  acquired_at         INTEGER NOT NULL,
  released_at         INTEGER             -- NULL = live；墓碑仅为回放便利，权威审计在哈希链
);

-- F2 反向日志（saga-log）：清理进度的 write-ahead 记录。
-- 有逆档不依赖本表内容（幂等盲重放）；可补偿档依赖 idem_key 做恰好一次。
CREATE TABLE teardown_journal (
  journal_id          INTEGER PRIMARY KEY AUTOINCREMENT,
  subject             TEXT NOT NULL,
  holding_id          INTEGER NOT NULL REFERENCES holding(holding_id),
  grade               TEXT NOT NULL,      -- "inverse" | "compensable"
  idem_key            TEXT NOT NULL UNIQUE, -- 跨崩溃稳定的去重钥匙
  state               TEXT NOT NULL,      -- "pending" | "in_flight" | "done" | "failed"
  updated_at          INTEGER NOT NULL
);
CREATE INDEX journal_open ON teardown_journal(subject) WHERE state IN ('pending','in_flight','failed');

-- F3 同意四元组（WYSIWYS）。预算池不在此表：同意即铸造 ——
-- authoritative 落一行 (class_id='budget', instance=nonce, capacity=Count(B))，
-- 花费即 holding 行（一行一笔，按生命周期契约结算）；本表只承载四元组与生命周期。
CREATE TABLE consent (
  nonce               TEXT PRIMARY KEY,   -- 一次性；重放 = 主键冲突（StaleNonce 的落库形态）
  plan_hash           TEXT NOT NULL,      -- 计划字节的 CAS id（WYSIWYS：展示=执行）
  budget              INTEGER NOT NULL,
  ttl_expires_at      INTEGER NOT NULL,   -- [TTL] 悬置段的封顶：到期即废，绝不无限悬置
  issued_at           INTEGER NOT NULL,
  kind                TEXT NOT NULL,      -- 'initial' | 'incremental'(escalate) | 'approval'(commit)
  status              TEXT NOT NULL       -- 'active' | 'consumed' | 'expired'
);

-- F3 扣发缓冲的耐久影子。不变式（演练第二只 bug 的墓碑）：入此表者除同意外已全
-- 合法（sink 复检先于压制）——批准后放出无需重检。崩溃丢行 = 退化 abort：
-- 压制的失败方向是"不发射"，对 w-effect 恰是安全侧。
CREATE TABLE suppression_buffer (
  seq                 INTEGER PRIMARY KEY AUTOINCREMENT,  -- 放出按此序（原序恰好一次）
  subject             TEXT NOT NULL,
  verb                TEXT NOT NULL,
  target              TEXT NOT NULL,
  payload_hash        TEXT NOT NULL,      -- payload 永不入表，CAS 引用
  cost                INTEGER NOT NULL,
  staged_under        TEXT NOT NULL REFERENCES consent(nonce),
  state               TEXT NOT NULL       -- 'held' | 'inserted' | 'aborted'
);
CREATE INDEX buffer_held ON suppression_buffer(subject) WHERE state = 'held';

-- F3 发射日志：w-effect 的影子（内核只能治理影子——治理=准入这张表的写入）。
CREATE TABLE emission_log (
  emission_id         INTEGER PRIMARY KEY AUTOINCREMENT,
  subject             TEXT NOT NULL,
  verb                TEXT NOT NULL,      -- attenuate 后记降档动词，原动词入 disposition 备注
  target              TEXT NOT NULL,
  payload_hash        TEXT NOT NULL,
  cost                INTEGER NOT NULL,
  consent_nonce       TEXT NOT NULL REFERENCES consent(nonce),  -- 每笔发射都指认其同意
  disposition         TEXT NOT NULL,      -- 'emitted' | 'attenuated:<原动词>' | 'confined'(替身)
  emitted_at          INTEGER NOT NULL
);

CREATE INDEX holding_live      ON holding(class_id, instance) WHERE released_at IS NULL;
CREATE INDEX holding_subject   ON holding(subject)            WHERE released_at IS NULL;
CREATE INDEX holding_lease_due ON holding(lease_expires_at)   WHERE released_at IS NULL;
CREATE INDEX holding_parent    ON holding(parent)             WHERE released_at IS NULL;
