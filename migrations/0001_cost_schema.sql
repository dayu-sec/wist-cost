-- wist-cost 成本域 schema v1（W0）
--
-- 设计依据：`../wist-cost-设计要点.md` §4.1；输入契约：`../contracts/harvest-cost-v1/`。
--
-- 三条硬约束：
--   1. 金额一律 numeric(30,12)，**不用浮点**；NULL = 未知，0 = 确认为零，退款为负。
--   2. 范围身份是七元组，唯一；同一七元组重复发布 = 整段替换，不是累加。
--   3. cost_detail 按 bill_month 分区，保留 18 个月（清理用 drop table 分区，
--      PG 没有 DROP PARTITION DDL），见 0002。

-- ── 范围身份 ────────────────────────────────────────────────────────────────
-- 七元组任一字段不同就是另一个范围；同一个七元组只有一行。
create table if not exists cost_range (
  id                  bigserial primary key,
  tenant              text not null,
  source_code         text not null,
  vendor              text not null,
  billing_account_uin text not null,
  cost_basis          text not null,
  period_start        date not null,
  period_end          date not null,
  -- 一个范围只有一个币种（契约 range 层面给的是单币种）。
  currency            text not null,
  created_at          timestamptz not null default now(),
  constraint cost_range_basis_chk check (cost_basis in ('CONSUMPTION', 'CHARGE')),
  constraint cost_range_period_chk check (period_end >= period_start),
  constraint cost_range_identity_uniq unique (
    tenant, source_code, vendor, billing_account_uin,
    cost_basis, period_start, period_end
  )
);

comment on table cost_range is
  '成本范围身份（七元组）。唯一键即幂等键：同一七元组重复发布是替换。';

-- ── 范围版本 ────────────────────────────────────────────────────────────────
-- 一次"发布该范围"= 一行。有效版本永远只有一个：
--   staging → published → superseded（superseded_by 指向取代它的版本，可回滚）
create table if not exists cost_range_version (
  id            bigserial primary key,
  range_id      bigint not null references cost_range (id) on delete cascade,
  batch_id      text not null,
  state         text not null default 'staging',
  rows          bigint not null default 0,
  bytes         bigint not null default 0,
  manifest_uri  text,
  -- manifest 内容摘要：范围身份 + 币种 + 口径 + 分片 sha256/行数 的 sha256。
  sha256        text not null,
  published_at  timestamptz,
  superseded_by bigint references cost_range_version (id),
  period_source text not null,
  computed_at   timestamptz not null default now(),
  rule_version  text not null,
  fetched_at    timestamptz,
  constraint cost_range_version_state_chk
    check (state in ('staging', 'published', 'superseded')),
  constraint cost_range_version_period_source_chk
    check (period_source in ('cloud_api', 'legacy_import')),
  constraint cost_range_version_rows_chk check (rows >= 0),
  constraint cost_range_version_bytes_chk check (bytes >= 0)
);

-- 结构上保证"每个范围最多一个有效版本"：并发入账也不可能出现两版同时有效。
create unique index if not exists cost_range_version_one_published
  on cost_range_version (range_id) where state = 'published';

create index if not exists cost_range_version_range_state_idx
  on cost_range_version (range_id, state);

comment on column cost_range_version.sha256 is
  'manifest 内容摘要（manifest.content_sha256），不是单个分片的 sha256。';

-- ── 明细（按月分区）────────────────────────────────────────────────────────
-- 不存原文：原文在对象存储，这里只留标准化字段 + shard_id/row_no 指针。
create table if not exists cost_detail (
  version_id         bigint not null references cost_range_version (id) on delete cascade,
  bill_month         text not null,
  shard_id           text not null,
  row_no             integer not null,
  cost_date          date not null,
  business_code      text not null,
  business_code_name text,
  product_code       text,
  product_code_name  text,
  resource_id        text,
  resource_key       text not null,
  resource_name      text,
  payer_uin          text,
  owner_uin          text,
  pay_mode_name      text,
  action_type_name   text,
  pay_time           text,
  fee_begin_time     text,
  fee_end_time       text,
  provider_detail_id text,
  bill_id            text,
  order_id           text,
  currency           text not null,
  real_cost          numeric(30,12),
  cash_amount        numeric(30,12),
  voucher_amount     numeric(30,12),
  incentive_amount   numeric(30,12),
  transfer_amount    numeric(30,12),
  -- 设计 §6：金额终归是财务敏感数据，先留语义位，暂不实现策略。
  sensitivity        text not null default 'internal',
  -- 分区键必须在主键里。
  constraint cost_detail_pk primary key (bill_month, version_id, shard_id, row_no),
  constraint cost_detail_bill_month_chk check (bill_month ~ '^[0-9]{4}-(0[1-9]|1[0-2])$'),
  constraint cost_detail_row_no_chk check (row_no >= 1)
) partition by range (bill_month);

comment on table cost_detail is
  '标准化成本明细。按 bill_month 分区保留 18 个月；分区由 cost_ensure_month_partition() 建。';
comment on column cost_detail.pay_time is
  '保留原文（云侧无时区，按 Asia/Shanghai 解释）；时区解释口径在 W1 定，不在这里猜。';

create index if not exists cost_detail_version_idx on cost_detail (version_id);
create index if not exists cost_detail_resource_date_idx
  on cost_detail (resource_key, cost_date);
create index if not exists cost_detail_bill_month_idx on cost_detail (bill_month);

-- ── 匹配（W1 实现；这里只建表）─────────────────────────────────────────────
-- 匹配是**版本**不是状态：每次匹配追加一条，查询取每个 resource_key 最新一条。
-- asset_id 为空即 UNMATCHED（不放在 cost_detail 上）。
create table if not exists cost_match (
  id               bigserial primary key,
  range_version_id bigint not null references cost_range_version (id) on delete cascade,
  resource_key     text not null,
  asset_id         text,
  matched_at       timestamptz not null default now(),
  rule_version     text not null,
  attempt          integer not null default 1
);

create index if not exists cost_match_resource_latest_idx
  on cost_match (resource_key, matched_at desc);

-- ── 日汇总（W1 实现；这里只建表）───────────────────────────────────────────
-- 从**有效范围快照重算**，不做增量累加（设计 §3）。
create table if not exists cost_daily_summary (
  tenant             text not null,
  resource_key       text not null,
  cost_date          date not null,
  currency           text not null,
  consumed_total     numeric(30,12),
  consumed_cash      numeric(30,12),
  consumed_voucher   numeric(30,12),
  consumed_incentive numeric(30,12),
  consumed_transfer  numeric(30,12),
  charged_total      numeric(30,12),
  cash_paid          numeric(30,12),
  voucher_paid       numeric(30,12),
  incentive_paid     numeric(30,12),
  transfer_paid      numeric(30,12),
  asset_id           text,
  match_status       text,
  sensitivity        text not null default 'internal',
  computed_at        timestamptz not null default now(),
  rule_version       text not null,
  constraint cost_daily_summary_pk
    primary key (tenant, resource_key, cost_date, currency)
);

-- ── 元信息：/healthz 自述的 schema 版本 ────────────────────────────────────
create table if not exists cost_meta (
  key        text primary key,
  value      text not null,
  updated_at timestamptz not null default now()
);

insert into cost_meta (key, value) values ('schema_version', 'cost-schema-v1')
on conflict (key) do update set value = excluded.value, updated_at = now();
