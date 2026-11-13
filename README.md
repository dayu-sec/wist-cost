# wist-cost

成本域的**存储 + 入账**服务：接 `wist-harvest` 产出的分片与 manifest，按"范围替换"语义
落 PG，**金额只在 cost 算一次**。

设计依据：[`../wist-cost-设计要点.md`](../wist-cost-设计要点.md)（权威）；输入契约：
[`contracts/harvest-cost-v1/`](contracts/harvest-cost-v1/README.md)（**与消费方同仓**，改造契约先改这里）。

```
wist-harvest ──分片 + manifest──► wist-cost ──有效范围快照──► (W1) 匹配 / 日汇总 / 对账
```

## W0 范围（本版做了什么）

| 项 | 状态 |
| --- | --- |
| PG schema（`migrations/`，sqlx 迁移） | ✅ `cost_range` / `cost_range_version` / `cost_detail` / `cost_match` / `cost_daily_summary` |
| 金额口径 | ✅ 十进制精确计算，列一律 `numeric(30,12)`，无浮点；`null` / `"0"` / 负数三态保留 |
| 范围替换幂等 | ✅ staging 写入 → 校验 → 原子切换有效版本；结构性保证"金额不可能翻倍" |
| manifest 校验 | ✅ `shards[].sha256` + `rows` + `total_rows`；不符整批拒绝 |
| 对象存储 | ✅ `local`；`s3` 只有配置位与 trait，**不实现** |
| `healthz` | ✅ 回显 `objects.kind`、schema 版本、数据量（未来 HTTP `/healthz` 的同一份文档） |
| 资源匹配 / 日汇总 / 月核验 | ❌ W1/W2（表已建，逻辑未实现） |
| 查询 API / field 页面 / 授权轴 | ❌ field 页面与授权轴已明确暂缓 |
| zstd 解压 | ❌ W0 读明文 `.ndjson`（harvest 生产侧是 `.ndjson.zst`，W1 补） |

## 目录

```
wist-cost/
├── _gal/                          # gx 工作区（gx init project --path rust 生成）
├── version.txt                    # 版本唯一来源
├── Cargo.toml                     # workspace（版本 marker 在 [workspace.package]）
├── migrations/
│   ├── 0001_cost_schema.sql       # 表 + 唯一索引 + 月分区 + 元信息
│   └── 0002_cost_partitions.sql   # 建分区 / 18 个月保留期清理函数
├── contracts/harvest-cost-v1/     # 输入契约（两个 schema + 样例）；权威定义就在这
└── crates/
    ├── cost-core/                 # 领域层：金额、范围身份、契约解析、批次校验、对象存储 trait
    ├── cost-store/                # 存储层：PG 访问、迁移、入账编排、local/s3 后端
    └── cost-app/                  # bin `wist-cost`：migrate / ingest / healthz
```

## 数据模型

```
cost_range          六元组身份，唯一键就是幂等键（不含客户）
  unique (source_code, vendor, billing_account_uin, cost_basis, period_start, period_end)

cost_range_version  一次发布 = 一行；state: staging → published → superseded
  unique (range_id) where state = 'published'      ← 结构上只可能有一个有效版本
  superseded_by                                    ← 旧版本保留，可回滚

cost_detail         标准化明细，按 bill_month 分区（RANGE），保留 18 个月
  primary key (bill_month, version_id, shard_id, row_no)
  shard_id + row_no                                ← 指向对象存储里的原文
  real_cost/cash_amount/… numeric(30,12)
```

- 明细挂在 **`version_id`** 上而不是范围上：新版本写自己的行，旧行原地不动。
  所以"同一天补采两次金额翻倍"在结构上不可能发生。
- 分区由 `cost_ensure_month_partition('YYYY-MM')` 按需创建（入账写 staging 前调用）。
- 保留期由 `cost_prune_partitions(18)` 整分区 `drop table`（PG 没有 `DROP PARTITION` DDL）。
- `cost_match` / `cost_daily_summary` 只建表：匹配是**版本**不是状态，日汇总**从有效版本重算**（W1）。
- `sensitivity` 列已在明细与汇总上留位（设计 §6），策略未实现。

## 入账语义

```
键   = 七元组
语义 = 同一范围重复发布 → 整段替换
实现 = staging 写入 → 校验 → 原子切换有效版本 → 旧版本可回滚
```

一个事务内完成，任一步失败整批回滚（staging 版本与明细一起消失，上一版有效快照原样保留）：

1. **契约校验**：`schema` 常量、币种、`fetched_at` 为带时区 RFC3339、范围形状
   （`CONSUMPTION` 单日 / `CHARGE` 整月）、分片 id 唯一、`Σ rows == total_rows`、上限 30 万行；
2. **分片校验**：`sha256(字节) == shards[].sha256` → 行数 → 逐行（见下）；
3. `pg_advisory_xact_lock(range 七元组)`：同范围并发入账串行化；
4. 范围行 upsert（币种不一致直接拒绝）；
5. **重放判定**：同一 `batch_id` + 同一 manifest 内容摘要、且仍是有效版本 → `Unchanged`，不加版本；
6. 写 staging 版本 → 建分区 → 写明细；
7. **对账**：库里 `count(*)` 与五列 `(已知条数, sum())` 必须与 Rust 侧逐列一致
   （这也是"Rust 十进制求和 == PG numeric 求和"的等价性验证）；不一致整批回滚；
8. 旧有效版本 → `superseded`（`superseded_by` 指向新版本）→ 新版本 → `published`。

逐行校验：`schema` 常量、`bill_month` 形状、`cost_date ∈ 范围`、币种与 manifest 一致、
`resource_key == business_code + ':' + (resource_id | UNALLOCATED)`、金额可无损存进
`numeric(30,12)`、组件金额与汇总金额一致。

## 运行

```sh
# 环境变量（凭据只从环境变量取，不落盘）
export WIST_COST_DATABASE_URL='postgres://user:pass@host:5432/wist_cost'
export WIST_COST_OBJECT_KIND=local          # local | s3（s3 在 W0 未实现）
export WIST_COST_OBJECT_ROOT=/var/lib/wist/cost
export WIST_COST_OBJECT_KEY_TEMPLATE='cost/{vendor}/{cost_basis}/{year}/{month}/{shard_id}.ndjson'

wist-cost migrate
wist-cost ingest --manifest /path/to/manifest.json            # 入账
wist-cost ingest --manifest /path/to/manifest.json --dry-run   # 只校验，不写库
wist-cost healthz                                             # 自述（等价 HTTP /healthz）
```

对象键模板默认就是设计 §4 的布局（占位符：`{source_code}` `{vendor}`
`{cost_basis}` `{year}` `{month}` `{batch_id}` `{shard_id}`）。模板必须含 `{shard_id}`。

`healthz` 输出（`status != ok` 时退出码 1）：

```json
{
  "service": "wist-cost", "version": "0.1.0", "rule_version": "wist-cost/w0.1",
  "status": "ok",
  "objects": {"kind": "local", "root": "...", "presign": false, "implemented": true},
  "schema": {"expected": "cost-schema-v1", "version": "cost-schema-v1", "migrations": 2, "initialized": true},
  "database": {"reachable": true, "ranges": 1, "published_versions": 1, "detail_rows": 3}
}
```

## 测试

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                      # 逻辑层：47 个用例，不需要 PG
WIST_COST_TEST_DATABASE_URL=postgres://postgres:adminpw@127.0.0.1:55999/wist_cost_test \
  cargo test --workspace -- --include-ignored   # 再加 9 个 PG 入账用例
```

- **逻辑层**（`cost-core`，47 个）：金额解析/求和/落库边界（`null` vs `"0"` vs 负数）、
  七元组身份、契约解析（忽略未知字段）、sha256 / 行数 / 空行 / 组件金额一致性。
- **PG 入账**（`crates/cost-store/tests/ingest_pg.rs`，9 个，标了 `#[ignore]`）：
  直接读 `contracts/harvest-cost-v1/fixtures/` 的 `manifest-sample.json` 与
  `detail-sample.ndjson`（**不自造样例**；分片按真实对象键放到临时目录，sha256 校验保证
  字节就是契约那份）。覆盖：一次入账、同 manifest 重放两次、同范围新批次替换、sha256 不符、
  行数不符、七元组任一字段不同、空范围、分区/保留期函数、`numeric(30,12)` 列型。
- 这组用例需要一次性 PG（本机用 docker 探针库）；**没设环境变量时是 `ignored` 而不是"绿"**，
  设了但连不上会 panic。它们会 `truncate` 目标库，只能指向临时库。
- fixture 路径可用 `WIST_COST_CONTRACT_FIXTURES` 覆盖。

## 取舍（与设计/契约的差异及原因）

| 点 | 取舍 | 原因 |
| --- | --- | --- |
| 金额小数位 | 有效小数位 > 12 **直接拒绝**，不静默舍入 | 列是 `numeric(30,12)`；静默舍入会让日汇总/月核验差最后一位，比报错更难查。入库后另有一层 Rust↔PG 求和比对兜底 |
| 组件字段名 | 按交接文档 §5.2 的原文字段名（`RealCost` 等）核对；**字段缺席**视为不可核验，**字段为 null** 才强制整体 null | 契约把 `components[]` 定义成自由对象，钉内部形状会误拒 |
| 缺失金额字段 | JSON 里缺字段 == `null`（未知） | 对我们的语义完全等价，`Option<Money>` 即可，不必区分"缺席/空值" |
| 重放判定 | 同一 `batch_id` + 同一内容摘要且仍是有效版本才 `Unchanged`；内容变了就新建版本 | `batch_id` 是"一次采集运行"，偷改内容必须留下新版本 |
| 回滚 | 旧版本保留在库里，`superseded_by` 指回去；W0 不做 `rollback` 子命令 | 人工 SQL 切换即可；自动化留 W1 |
| 分区 | PG 没有 `DROP PARTITION`，用 `drop table` 子表实现整块清理 | 与设计意图等价 |
| 时间字段 | `pay_time` / `fee_*_time` 存原文 `text`，不猜时区 | 交接文档要求按 Asia/Shanghai 解释；解释口径在 W1 定，先不丢原文 |
| 对象键 | 不含客户；W0 后缀 `.ndjson`（无 zstd） | 客户由 `billing_account_uin` 映射；zstd 解压 W1 补 |
| `objects.kind` | 用 `local`（设计口径），`wist-evidence` 用的是 `file` | 跟本设计文档走；共享 crate 抽取时再统一 |
| s3 | 只有 `ObjectStoreConfig::S3` + `S3ObjectStore` 占位（`get` 明确报未实现） | 先把配置位和自述形状定下来，别让"未实现"退化成空读取 |
| 空范围 | `shards: []` + `total_rows: 0` 可以发布，成为一个 0 行的有效版本 | 与"请求失败"区分开（交接文档 §8.1） |

## TODO

- W1：匹配（`cost_match` 追加式版本）、日汇总（从有效版本重算）、`UNMATCHED`、回滚子命令。
- W1：`s3` 后端 + `:presign`、`.ndjson.zst` 解压、对象键与 `wist-evidence` 抽共享 crate。
- W2：月核验（`PROVISIONAL`/`FINAL`）、双跑比对。
- 时间字段时区解释（`Asia/Shanghai`）、`bill_month` 与 `cost_date` 归属口径的边界用例。
- 半年后核对：`cost_detail` 18 个分区是否随 `cost_prune_partitions` 正常轮转（需要外部定时任务调用）。
