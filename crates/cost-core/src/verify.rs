//! 批次校验：分片字节 → 可入账的已验证批次。
//!
//! 契约 §2 的三条不可协商规则在这里落地：
//!
//! 1. 分片不可变 —— sha256 是身份，字节不符就整批拒绝；
//! 2. manifest 是唯一发布凭据 —— 只认 `shards[].sha256` 与 `rows`／`total_rows`；
//! 3. 金额是十进制字符串 —— `null` / `"0"` / 负数各自保留语义。
//!
//! 任何一项不过：**整批拒绝，不发布有效版本**（上一版有效快照原样保留）。

use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::detail::{Amounts, DetailLine, detail_error};
use crate::error::CostError;
use crate::manifest::{Manifest, Shard};
use crate::money::{Money, MoneyError};
use crate::objects::{ObjectStore, ShardKeyLayout};

/// 一列的求和结果：`known` / `unknown` 分开计数，`sum` 只累加已知值。
///
/// 这个三元组是"Rust 十进制求和 == PG `numeric` 求和"的比对凭据：
/// 入库后按同一口径从库里重算，不一致就让整批回滚。
#[derive(Debug, Clone, Default, Serialize)]
pub struct ColumnTotal {
    pub known: u64,
    pub unknown: u64,
    pub sum: Money,
}

impl ColumnTotal {
    fn add(&mut self, value: Option<Money>) -> Result<(), MoneyError> {
        match value {
            Some(money) => {
                self.sum = self.sum.checked_add(money)?;
                self.known += 1;
            }
            None => self.unknown += 1,
        }
        Ok(())
    }
}

/// 五个金额列的求和汇总。
#[derive(Debug, Clone, Default, Serialize)]
pub struct AmountTotals {
    pub real_cost: ColumnTotal,
    pub cash_amount: ColumnTotal,
    pub voucher_amount: ColumnTotal,
    pub incentive_amount: ColumnTotal,
    pub transfer_amount: ColumnTotal,
}

impl AmountTotals {
    pub fn add(&mut self, amounts: &Amounts) -> Result<(), MoneyError> {
        for (index, (_, value)) in amounts.iter().into_iter().enumerate() {
            self.slot_mut(index).add(value)?;
        }
        Ok(())
    }

    pub fn named(&self) -> [(&'static str, &ColumnTotal); 5] {
        [
            ("real_cost", &self.real_cost),
            ("cash_amount", &self.cash_amount),
            ("voucher_amount", &self.voucher_amount),
            ("incentive_amount", &self.incentive_amount),
            ("transfer_amount", &self.transfer_amount),
        ]
    }

    fn slot_mut(&mut self, index: usize) -> &mut ColumnTotal {
        match index {
            0 => &mut self.real_cost,
            1 => &mut self.cash_amount,
            2 => &mut self.voucher_amount,
            3 => &mut self.incentive_amount,
            4 => &mut self.transfer_amount,
            other => unreachable!("金额列下标越界：{other}"),
        }
    }
}

/// 分片的实际身份（读到的字节的 sha256 与行数，而不是 manifest 自述）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShardDigest {
    pub id: String,
    pub rows: u64,
    pub bytes: u64,
    pub sha256: String,
}

/// 通过校验的一行明细。
#[derive(Debug, Clone)]
pub struct CheckedDetail {
    /// 分片内的行号（从 1 起，与文件行序一致）——指向对象存储里的原文。
    pub row_no: u64,
    pub line: DetailLine,
    pub amounts: Amounts,
}

/// 通过校验的分片。
#[derive(Debug, Clone)]
pub struct VerifiedShard {
    pub digest: ShardDigest,
    pub details: Vec<CheckedDetail>,
}

/// 通过校验的整批（一个范围版本的全部内容）。
#[derive(Debug, Clone)]
pub struct VerifiedBatch {
    pub manifest: Manifest,
    pub shards: Vec<VerifiedShard>,
    pub total_rows: u64,
    pub total_bytes: u64,
    pub months: BTreeMap<String, u64>,
    pub totals: AmountTotals,
}

impl VerifiedBatch {
    /// 涉及月份（升序），用于建分区。
    pub fn months(&self) -> Vec<(&str, u64)> {
        self.months
            .iter()
            .map(|(month, rows)| (month.as_str(), *rows))
            .collect()
    }
}

/// 读全部 shard 并校验整批。
pub async fn verify_batch(
    manifest: &Manifest,
    objects: &dyn ObjectStore,
    layout: &ShardKeyLayout,
) -> Result<VerifiedBatch, CostError> {
    manifest.validate()?;

    let mut shards = Vec::with_capacity(manifest.shards.len());
    let mut totals = AmountTotals::default();
    let mut months: BTreeMap<String, u64> = BTreeMap::new();
    let mut total_rows: u64 = 0;
    let mut total_bytes: u64 = 0;

    for shard in &manifest.shards {
        let key = layout.key_for(manifest, &shard.id);
        let bytes = objects.get(&key).await?.ok_or_else(|| {
            CostError::shard(
                &shard.id,
                "object_missing",
                format!("对象 {} 不存在（对象键模板 {}）", key, layout.template()),
            )
        })?;
        let verified = verify_shard(manifest, shard, &bytes)?;

        for detail in &verified.details {
            totals.add(&detail.amounts)?;
            *months.entry(detail.line.bill_month.clone()).or_insert(0) += 1;
        }
        total_rows += verified.digest.rows;
        total_bytes += verified.digest.bytes;
        shards.push(verified);
    }

    if total_rows != manifest.total_rows {
        return Err(CostError::manifest(
            "total_rows_mismatch",
            format!(
                "分片实际行数合计 {total_rows} 与 total_rows {} 不一致",
                manifest.total_rows
            ),
        ));
    }

    Ok(VerifiedBatch {
        manifest: manifest.clone(),
        shards,
        total_rows,
        total_bytes,
        months,
        totals,
    })
}

/// 校验单个分片：sha256 → 行数 → 每一行。
pub fn verify_shard(
    manifest: &Manifest,
    shard: &Shard,
    bytes: &[u8],
) -> Result<VerifiedShard, CostError> {
    let actual_sha = hex::encode(Sha256::digest(bytes));
    if actual_sha != shard.sha256 {
        return Err(CostError::shard(
            &shard.id,
            "sha256_mismatch",
            format!("manifest {} ≠ 实际 {actual_sha}", shard.sha256),
        ));
    }

    let text = std::str::from_utf8(bytes).map_err(|err| {
        CostError::shard(&shard.id, "invalid_utf8", format!("分片不是 UTF-8：{err}"))
    })?;

    let mut details = Vec::new();
    let mut row_no: u64 = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            return Err(CostError::shard(
                &shard.id,
                "empty_line",
                format!("第 {} 行是空行", row_no + 1),
            ));
        }
        row_no += 1;
        let detail: DetailLine = serde_json::from_str(line)
            .map_err(|err| CostError::detail(&shard.id, row_no, "invalid_json", err.to_string()))?;
        let amounts = detail
            .check(&manifest.range, &manifest.currency)
            .map_err(|violation| detail_error(&shard.id, row_no, violation))?;
        details.push(CheckedDetail {
            row_no,
            line: detail,
            amounts,
        });
    }

    if row_no != shard.rows {
        return Err(CostError::shard(
            &shard.id,
            "row_count_mismatch",
            format!("manifest 记 {} 行，实际 {row_no} 行", shard.rows),
        ));
    }

    Ok(VerifiedShard {
        digest: ShardDigest {
            id: shard.id.clone(),
            rows: row_no,
            bytes: bytes.len() as u64,
            sha256: actual_sha,
        },
        details,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{MANIFEST_SCHEMA, PeriodSource};
    use crate::objects::MemoryObjectStore;
    use crate::range::{CostBasis, CostRange, Vendor};

    const ROW: &str = r#"{"schema":"wist.harvest.cost.detail.v1","cost_date":"2026-11-10","bill_month":"2026-11","business_code":"p_cvm","resource_id":"ins-example01","resource_key":"p_cvm:ins-example01","currency":"CNY","real_cost":"10.000000000000","cash_amount":"9.000000000000","voucher_amount":"1.000000000000","incentive_amount":"0.000000000000","transfer_amount":"0.000000000000","components":[{"RealCost":"10.000000000000","CashPayAmount":"9.000000000000","VoucherPayAmount":"1.000000000000","IncentivePayAmount":"0.000000000000","TransferPayAmount":"0.000000000000"}],"raw":{}}"#;

    fn manifest_for(rows: u64, sha256: String) -> Manifest {
        Manifest {
            schema: MANIFEST_SCHEMA.to_string(),
            batch_id: "b1".to_string(),
            range: CostRange {
                source_code: "tencent-cloud-prod".to_string(),
                vendor: Vendor::TencentCloud,
                billing_account_uin: "100000000001".to_string(),
                cost_basis: CostBasis::Consumption,
                period_start: "2026-11-10".parse().unwrap(),
                period_end: "2026-11-10".parse().unwrap(),
            },
            currency: "CNY".to_string(),
            period_source: PeriodSource::CloudApi,
            fetched_at: "2026-11-12T00:31:22+08:00".to_string(),
            request_ids: Vec::new(),
            shards: vec![Shard {
                id: "0001".to_string(),
                rows,
                sha256,
                bytes: 0,
            }],
            total_rows: rows,
        }
    }

    fn sha(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    #[test]
    fn sha_mismatch_rejects_the_shard() {
        let bytes = format!("{ROW}\n").into_bytes();
        let manifest = manifest_for(1, "0".repeat(64));
        let err = verify_shard(&manifest, &manifest.shards[0], &bytes).unwrap_err();
        assert_eq!(err.code(), "sha256_mismatch");
    }

    #[test]
    fn row_count_mismatch_rejects_the_shard() {
        let bytes = format!("{ROW}\n").into_bytes();
        let manifest = manifest_for(2, sha(&bytes));
        let err = verify_shard(&manifest, &manifest.shards[0], &bytes).unwrap_err();
        assert_eq!(err.code(), "row_count_mismatch");
    }

    #[tokio::test]
    async fn missing_object_rejects_the_batch() {
        let bytes = format!("{ROW}\n").into_bytes();
        let manifest = manifest_for(1, sha(&bytes));
        let store = MemoryObjectStore::new();
        let layout = ShardKeyLayout::design_default();
        let err = verify_batch(&manifest, &store, &layout).await.unwrap_err();
        assert_eq!(err.code(), "object_missing");
    }

    #[tokio::test]
    async fn total_rows_must_match_the_files() {
        let bytes = format!("{ROW}\n").into_bytes();
        let mut manifest = manifest_for(1, sha(&bytes));
        // 把 total_rows 改大：manifest 内部自洽（分片也改大）但文件只有 1 行。
        manifest.shards[0].rows = 2;
        manifest.total_rows = 2;

        let store = MemoryObjectStore::new();
        let layout = ShardKeyLayout::design_default();
        store.put(layout.key_for(&manifest, "0001"), bytes);
        let err = verify_batch(&manifest, &store, &layout).await.unwrap_err();
        assert_eq!(err.code(), "row_count_mismatch");
    }

    #[tokio::test]
    async fn empty_range_with_no_shards_verifies() {
        let mut manifest = manifest_for(0, "0".repeat(64));
        manifest.shards.clear();
        let store = MemoryObjectStore::new();
        let layout = ShardKeyLayout::design_default();
        let batch = verify_batch(&manifest, &store, &layout).await.unwrap();
        assert_eq!(batch.total_rows, 0);
        assert_eq!(batch.totals.real_cost.known, 0);
        assert_eq!(batch.totals.real_cost.unknown, 0);
    }

    #[tokio::test]
    async fn totals_separate_known_and_unknown_amounts() {
        let unknown_row = ROW
            .replace("\"cash_amount\":\"9.000000000000\"", "\"cash_amount\":null")
            .replace(
                "\"CashPayAmount\":\"9.000000000000\"",
                "\"CashPayAmount\":null",
            )
            .replace(
                "\"voucher_amount\":\"1.000000000000\"",
                "\"voucher_amount\":null",
            )
            .replace(
                "\"VoucherPayAmount\":\"1.000000000000\"",
                "\"VoucherPayAmount\":null",
            );
        let bytes = format!("{ROW}\n{unknown_row}\n").into_bytes();
        let manifest = manifest_for(2, sha(&bytes));

        let store = MemoryObjectStore::new();
        let layout = ShardKeyLayout::design_default();
        store.put(layout.key_for(&manifest, "0001"), bytes);
        let batch = verify_batch(&manifest, &store, &layout).await.unwrap();

        assert_eq!(batch.total_rows, 2);
        assert_eq!(batch.totals.real_cost.known, 2);
        assert_eq!(batch.totals.real_cost.sum, Money::parse("20").unwrap());
        assert_eq!(batch.totals.cash_amount.known, 1);
        assert_eq!(batch.totals.cash_amount.unknown, 1);
        assert_eq!(batch.totals.cash_amount.sum, Money::parse("9").unwrap());
        assert_eq!(batch.months().len(), 1);
    }
}
