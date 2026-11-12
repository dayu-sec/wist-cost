# Changelog

## 0.1.0 — 2026-11-12

W0 骨架：契约驱动的成本入账跑通。

- workspace 拆三层：`cost-core`（领域）/ `cost-store`（PG + 对象存储）/ `cost-app`（bin `wist-cost`）。
- PG schema（sqlx 迁移）：`cost_range`（七元组唯一）、`cost_range_version`
  （`staging → published → superseded`，`published` 上有部分唯一索引）、`cost_detail`
  （按 `bill_month` 分区，`shard_id + row_no` 指向原文）、`cost_match` /
  `cost_daily_summary`（只建表，W1 实现）。
- 金额：自研十进制 `Money`（`i128 + scale`），列一律 `numeric(30,12)`，无浮点；
  `null`（未知）、`"0"`（确认零）、负数退款三态保留；有效小数位 > 12 直接拒绝而不是静默舍入。
- 范围替换幂等：staging 写入 → 行数/金额对账 → 原子切换有效版本；旧版本保留可回滚。
  同 `batch_id` + 同内容摘要的重放返回 `unchanged`，不加版本。
- 契约校验：`shards[].sha256`、分片行数、`total_rows`、`cost_date` 落在范围内、
  `resource_key` 推导、组件金额与汇总一致；不符整批拒绝、不产生有效版本。
- 对象存储：`objects.kind = local`（读本地分片，对象键按设计 §4 布局）；
  `s3` 只有配置位与 trait 占位。
- CLI：`migrate` / `ingest`（含 `--dry-run`）/ `healthz`（回显 `objects.kind`、
  schema 版本、范围与明细量）。
- 测试：47 个逻辑层用例 + 9 个 PG 入账用例（直接读 `contracts/harvest-cost-v1/fixtures/`）。
