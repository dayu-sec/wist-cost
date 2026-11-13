//! 范围替换入账：staging 写入 → 校验 → 原子切换有效版本。
//!
//! 设计 §3 的语义：
//!
//! ```text
//! 键   = (source_code, vendor, billing_account_uin, cost_basis, period_start, period_end)
//! 语义 = 同一范围重复发布 → 整段替换（不是累加）
//! 实现 = staging 写入 → 校验 → 原子切换有效版本 → 旧版本保留可回滚
//! ```
//!
//! 为什么金额**不可能翻倍**：
//!
//! - 明细行挂在 `version_id` 上，不是挂在范围上；新版本写自己的行，旧行原地不动；
//! - `cost_range_version(range_id) where state='published'` 是唯一索引，
//!   结构上不可能出现两个有效版本；
//! - 每天/每月的汇总只从"有效版本"重算（W1），不做增量累加。
//!
//! 失败语义：整批事务回滚。staging 版本与明细一起消失，上一版有效快照原样保留。

use cost_core::objects::{ObjectStore, ShardKeyLayout};
use cost_core::verify::{AmountTotals, VerifiedBatch, verify_batch};
use cost_core::{Manifest, Money};
use serde::Serialize;
use sqlx::{PgConnection, QueryBuilder};

use crate::error::StoreError;
use crate::pg::PgStore;

/// 一次入账的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishOutcome {
    /// 新版本已发布，旧版本转 `superseded`。
    Published,
    /// 同一 `batch_id` + 同一内容摘要的重复投递：不加版本、不动数据。
    Unchanged,
}

/// 入账报告（CLI 直接打印成 JSON）。
#[derive(Debug, Clone, Serialize)]
pub struct IngestReport {
    pub outcome: PublishOutcome,
    pub range_id: i64,
    pub version_id: i64,
    pub batch_id: String,
    pub range: String,
    pub period_source: String,
    pub rows: u64,
    pub bytes: u64,
    /// 本次取代的有效版本 id（可回滚指向）。
    pub superseded_version_id: Option<i64>,
    pub months: Vec<MonthRows>,
    pub totals: AmountTotals,
}

/// 每个账单月的行数。
#[derive(Debug, Clone, Serialize)]
pub struct MonthRows {
    pub bill_month: String,
    pub rows: u64,
}

/// 入账编排：读分片 → 校验 → 写 staging → 对账 → 切版本。
pub struct Ingestor<'a> {
    store: &'a PgStore,
    objects: &'a dyn ObjectStore,
    layout: ShardKeyLayout,
}

impl<'a> Ingestor<'a> {
    pub fn new(store: &'a PgStore, objects: &'a dyn ObjectStore, layout: ShardKeyLayout) -> Self {
        Self {
            store,
            objects,
            layout,
        }
    }

    pub async fn ingest(
        &self,
        manifest: &Manifest,
        manifest_uri: &str,
    ) -> Result<IngestReport, StoreError> {
        // 1) 契约校验 + 分片字节校验（不占事务，失败得快）。
        let batch = verify_batch(manifest, self.objects, &self.layout).await?;
        let content_sha256 = manifest.content_sha256();
        let months: Vec<MonthRows> = batch
            .months()
            .into_iter()
            .map(|(month, rows)| MonthRows {
                bill_month: month.to_string(),
                rows,
            })
            .collect();

        // 2) 一个事务内完成 staging → 对账 → 切换。
        let mut tx = self.store.pool().begin().await?;

        // 同一范围并发入账串行化（交接文档 §6：同范围只允许一个活动任务）。
        sqlx::query("select pg_advisory_xact_lock(hashtext($1))")
            .bind(manifest.range.identity_key())
            .execute(&mut *tx)
            .await?;

        let range_id = upsert_range(&mut tx, manifest).await?;

        // 3) 重放判定：同一批次、同一内容、仍是有效版本 → 什么都不做。
        if let Some((version_id, rows, bytes)) =
            find_identical_published(&mut tx, range_id, manifest, &content_sha256).await?
        {
            tx.commit().await?;
            return Ok(IngestReport {
                outcome: PublishOutcome::Unchanged,
                range_id,
                version_id,
                batch_id: manifest.batch_id.clone(),
                range: describe_range(manifest),
                period_source: manifest.period_source.as_str().to_string(),
                rows,
                bytes,
                superseded_version_id: None,
                months,
                totals: batch.totals.clone(),
            });
        }

        // 4) staging 版本 + 明细。
        let version_id = insert_staging_version(
            &mut tx,
            range_id,
            manifest,
            manifest_uri,
            &content_sha256,
            &batch,
        )
        .await?;
        ensure_partitions(&mut tx, &batch).await?;
        insert_details(&mut tx, version_id, &batch).await?;

        // 5) 对账：库里行数 / 金额必须与 Rust 侧逐列一致（设计 §7 的等价性验证）。
        reconcile(&mut tx, version_id, &batch).await?;

        // 6) 原子切换：先让旧版本失效，再把新版本置为 published。
        let superseded_version_id = switch_effective(&mut tx, range_id, version_id, &batch).await?;

        tx.commit().await?;

        Ok(IngestReport {
            outcome: PublishOutcome::Published,
            range_id,
            version_id,
            batch_id: manifest.batch_id.clone(),
            range: describe_range(manifest),
            period_source: manifest.period_source.as_str().to_string(),
            rows: batch.total_rows,
            bytes: batch.total_bytes,
            superseded_version_id,
            months,
            totals: batch.totals.clone(),
        })
    }
}

fn describe_range(manifest: &Manifest) -> String {
    let range = &manifest.range;
    format!(
        "{}/{}/{}/{}",
        range.source_code,
        range.billing_account_uin,
        range.cost_basis.as_str(),
        range.period_label()
    )
}

/// 幂等地拿到范围行；币种不一致直接拒绝（同范围不可能有两种币种）。
async fn upsert_range(conn: &mut PgConnection, manifest: &Manifest) -> Result<i64, StoreError> {
    let range = &manifest.range;
    sqlx::query(
        "insert into cost_range (
             source_code, vendor, billing_account_uin,
             cost_basis, period_start, period_end, currency
         ) values ($1, $2, $3, $4, $5, $6, $7)
         on conflict (
             source_code, vendor, billing_account_uin,
             cost_basis, period_start, period_end
         ) do nothing",
    )
    .bind(&range.source_code)
    .bind(range.vendor.as_str())
    .bind(&range.billing_account_uin)
    .bind(range.cost_basis.as_str())
    .bind(range.period_start)
    .bind(range.period_end)
    .bind(&manifest.currency)
    .execute(&mut *conn)
    .await?;

    let row: (i64, String) = sqlx::query_as(
        "select id, currency from cost_range
         where source_code = $1 and vendor = $2
           and billing_account_uin = $3 and cost_basis = $4
           and period_start = $5 and period_end = $6",
    )
    .bind(&range.source_code)
    .bind(range.vendor.as_str())
    .bind(&range.billing_account_uin)
    .bind(range.cost_basis.as_str())
    .bind(range.period_start)
    .bind(range.period_end)
    .fetch_one(&mut *conn)
    .await?;

    if row.1 != manifest.currency {
        return Err(StoreError::CurrencyConflict {
            range: range.identity_key(),
            existing: row.1,
            incoming: manifest.currency.clone(),
        });
    }

    Ok(row.0)
}

/// 已经是有效版本、且批次与内容都相同 → 这一次投递是重放。
async fn find_identical_published(
    conn: &mut PgConnection,
    range_id: i64,
    manifest: &Manifest,
    content_sha256: &str,
) -> Result<Option<(i64, u64, u64)>, StoreError> {
    let row: Option<(i64, i64, i64)> = sqlx::query_as(
        "select id, rows, bytes from cost_range_version
         where range_id = $1 and state = 'published'
           and batch_id = $2 and sha256 = $3",
    )
    .bind(range_id)
    .bind(&manifest.batch_id)
    .bind(content_sha256)
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row.map(|(id, rows, bytes)| (id, rows.max(0) as u64, bytes.max(0) as u64)))
}

async fn insert_staging_version(
    conn: &mut PgConnection,
    range_id: i64,
    manifest: &Manifest,
    manifest_uri: &str,
    content_sha256: &str,
    batch: &VerifiedBatch,
) -> Result<i64, StoreError> {
    let fetched_at = manifest.fetched_at_utc()?;
    let version_id: i64 = sqlx::query_scalar(
        "insert into cost_range_version (
             range_id, batch_id, state, rows, bytes, manifest_uri,
             sha256, period_source, computed_at, rule_version, fetched_at
         ) values ($1, $2, 'staging', $3, $4, $5, $6, $7, now(), $8, $9)
         returning id",
    )
    .bind(range_id)
    .bind(&manifest.batch_id)
    .bind(batch.total_rows as i64)
    .bind(batch.total_bytes as i64)
    .bind(manifest_uri)
    .bind(content_sha256)
    .bind(manifest.period_source.as_str())
    .bind(cost_core::RULE_VERSION)
    .bind(fetched_at)
    .fetch_one(&mut *conn)
    .await?;
    Ok(version_id)
}

/// 明细涉及的每个 `bill_month` 都要有分区（幂等）。
async fn ensure_partitions(
    conn: &mut PgConnection,
    batch: &VerifiedBatch,
) -> Result<(), StoreError> {
    for (bill_month, _) in batch.months() {
        let _: String = sqlx::query_scalar("select cost_ensure_month_partition($1)")
            .bind(bill_month)
            .fetch_one(&mut *conn)
            .await?;
    }
    Ok(())
}

/// 写 staging 明细。分块插入，避免单条语句参数过多。
async fn insert_details(
    conn: &mut PgConnection,
    version_id: i64,
    batch: &VerifiedBatch,
) -> Result<(), StoreError> {
    // 28 列 × 200 行 = 5600 个参数，远低于 PG 的 65535 上限。
    const CHUNK_ROWS: usize = 200;

    for shard in &batch.shards {
        for chunk in shard.details.chunks(CHUNK_ROWS) {
            let mut builder = QueryBuilder::new(
                "insert into cost_detail (
                     version_id, bill_month, shard_id, row_no, cost_date,
                     business_code, business_code_name, product_code, product_code_name,
                     resource_id, resource_key, resource_name, payer_uin, owner_uin,
                     pay_mode_name, action_type_name, pay_time, fee_begin_time, fee_end_time,
                     provider_detail_id, bill_id, order_id, currency,
                     real_cost, cash_amount, voucher_amount, incentive_amount, transfer_amount
                 ) ",
            );

            builder.push_values(chunk, |mut row, detail| {
                let line = &detail.line;
                let amounts = detail.amounts.db_strings();
                row.push_bind(version_id)
                    .push_bind(&line.bill_month)
                    .push_bind(&shard.digest.id)
                    .push_bind(detail.row_no as i32)
                    .push_bind(line.cost_date)
                    .push_bind(&line.business_code)
                    .push_bind(&line.business_code_name)
                    .push_bind(&line.product_code)
                    .push_bind(&line.product_code_name)
                    .push_bind(&line.resource_id)
                    .push_bind(&line.resource_key)
                    .push_bind(&line.resource_name)
                    .push_bind(&line.payer_uin)
                    .push_bind(&line.owner_uin)
                    .push_bind(&line.pay_mode_name)
                    .push_bind(&line.action_type_name)
                    .push_bind(&line.pay_time)
                    .push_bind(&line.fee_begin_time)
                    .push_bind(&line.fee_end_time)
                    .push_bind(&line.provider_detail_id)
                    .push_bind(&line.bill_id)
                    .push_bind(&line.order_id)
                    .push_bind(&line.currency);
                push_money(&mut row, &amounts);
            });

            builder.build().execute(&mut *conn).await?;
        }
    }

    Ok(())
}

/// 逐个金额列绑定：库里是 `numeric(30,12)`，这里显式 `::numeric` 让文本在服务端精确转换。
fn push_money(
    row: &mut sqlx::query_builder::Separated<'_, sqlx::Postgres, &'static str>,
    amounts: &[Option<String>; 5],
) {
    for value in amounts {
        row.push_bind(value.clone()).push_unseparated("::numeric");
    }
}

/// 库里重算：行数与五个金额列的 (已知条数, 求和) 必须与 Rust 侧完全一致。
async fn reconcile(
    conn: &mut PgConnection,
    version_id: i64,
    batch: &VerifiedBatch,
) -> Result<(), StoreError> {
    let row: (
        i64,
        i64,
        String,
        i64,
        String,
        i64,
        String,
        i64,
        String,
        i64,
        String,
    ) = sqlx::query_as(
        "select
                 count(*)::bigint,
                 count(real_cost)::bigint, coalesce(sum(real_cost), 0)::text,
                 count(cash_amount)::bigint, coalesce(sum(cash_amount), 0)::text,
                 count(voucher_amount)::bigint, coalesce(sum(voucher_amount), 0)::text,
                 count(incentive_amount)::bigint, coalesce(sum(incentive_amount), 0)::text,
                 count(transfer_amount)::bigint, coalesce(sum(transfer_amount), 0)::text
             from cost_detail where version_id = $1",
    )
    .bind(version_id)
    .fetch_one(&mut *conn)
    .await?;

    let (
        total,
        real_known,
        real_sum,
        cash_known,
        cash_sum,
        voucher_known,
        voucher_sum,
        incentive_known,
        incentive_sum,
        transfer_known,
        transfer_sum,
    ) = row;
    let db_columns = [
        ("real_cost", real_known, real_sum),
        ("cash_amount", cash_known, cash_sum),
        ("voucher_amount", voucher_known, voucher_sum),
        ("incentive_amount", incentive_known, incentive_sum),
        ("transfer_amount", transfer_known, transfer_sum),
    ];

    if total < 0 || total as u64 != batch.total_rows {
        return Err(StoreError::Reconciliation(format!(
            "明细行数：库 {} ≠ manifest {}",
            total, batch.total_rows
        )));
    }

    for ((name, known, sum_text), (_, expected)) in db_columns.into_iter().zip(batch.totals.named())
    {
        let known = known.max(0) as u64;
        if known != expected.known {
            return Err(StoreError::Reconciliation(format!(
                "{name} 已知条数：库 {known} ≠ Rust {}",
                expected.known
            )));
        }
        let db_sum = Money::parse(&sum_text).map_err(|err| {
            StoreError::Reconciliation(format!("{name} 求和无法解析（{sum_text}）：{err}"))
        })?;
        if db_sum != expected.sum {
            return Err(StoreError::Reconciliation(format!(
                "{name} 求和：库 {db_sum} ≠ Rust {}",
                expected.sum
            )));
        }
    }

    Ok(())
}

/// 把新版本置为有效、旧有效版本转 superseded。返回被取代的版本 id。
async fn switch_effective(
    conn: &mut PgConnection,
    range_id: i64,
    version_id: i64,
    batch: &VerifiedBatch,
) -> Result<Option<i64>, StoreError> {
    let superseded: Vec<i64> = sqlx::query_scalar(
        "update cost_range_version
            set state = 'superseded', superseded_by = $1
          where range_id = $2 and state = 'published' and id <> $1
          returning id",
    )
    .bind(version_id)
    .bind(range_id)
    .fetch_all(&mut *conn)
    .await?;

    sqlx::query(
        "update cost_range_version
            set state = 'published', published_at = now(), rows = $2, bytes = $3
          where id = $1",
    )
    .bind(version_id)
    .bind(batch.total_rows as i64)
    .bind(batch.total_bytes as i64)
    .execute(&mut *conn)
    .await?;

    Ok(superseded.into_iter().next())
}
