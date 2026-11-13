//! harvest → cost 契约 v1 的 manifest（清单）。
//!
//! 权威定义：`contracts/harvest-cost-v1/manifest.schema.json`。
//! 两条纪律：
//!
//! 1. **不 deny unknown fields**——契约 §4 要求消费方忽略未知字段（加字段不升版本）。
//! 2. **manifest 是唯一发布凭据**：只认 `shards[].sha256` 与 `rows`／`total_rows`，
//!    不接受"文件大小看起来对"。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::CostError;
use crate::range::CostRange;

/// manifest 的 `schema` 常量。
pub const MANIFEST_SCHEMA: &str = "wist.harvest.cost.manifest.v1";
/// 明细行的 `schema` 常量。
pub const DETAIL_SCHEMA: &str = "wist.harvest.cost.detail.v1";
/// 单范围明细上限（交接文档 §5.5：超限转 COS 导出，**不截断**）。
pub const MAX_RANGE_ROWS: u64 = 300_000;

/// `period_source`：区分重采与迁移数据的口径差异（设计 §4.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeriodSource {
    #[serde(rename = "cloud_api")]
    CloudApi,
    #[serde(rename = "legacy_import")]
    LegacyImport,
}

impl PeriodSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CloudApi => "cloud_api",
            Self::LegacyImport => "legacy_import",
        }
    }
}

/// 一个分片的清单条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shard {
    /// 四位序号（`0001` 起），拼进对象键。
    pub id: String,
    pub rows: u64,
    /// 分片文件字节的 sha256，小写十六进制。
    pub sha256: String,
    /// 字节数。**只做自述，不做校验依据**（契约 §2 规则 2）。
    pub bytes: u64,
}

/// 一个采集范围的清单。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub batch_id: String,
    pub range: CostRange,
    pub currency: String,
    pub period_source: PeriodSource,
    pub fetched_at: String,
    #[serde(default)]
    pub request_ids: Vec<String>,
    pub shards: Vec<Shard>,
    pub total_rows: u64,
}

impl Manifest {
    /// 契约层面的静态校验（不读分片字节）。
    pub fn validate(&self) -> Result<(), CostError> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(CostError::manifest(
                "manifest_schema",
                format!("schema 必须是 {MANIFEST_SCHEMA}，实际 {}", self.schema),
            ));
        }
        if self.batch_id.trim().is_empty() {
            return Err(CostError::manifest(
                "manifest_batch_id",
                "batch_id 不能为空",
            ));
        }
        if !is_currency_code(&self.currency) {
            return Err(CostError::manifest(
                "manifest_currency",
                format!("currency 必须是三位大写字母，实际 {}", self.currency),
            ));
        }
        self.fetched_at_utc()?;
        self.range.validate()?;

        let mut seen = std::collections::BTreeSet::new();
        let mut rows: u64 = 0;
        for shard in &self.shards {
            if !is_shard_id(&shard.id) {
                return Err(CostError::manifest(
                    "manifest_shard_id",
                    format!("分片 id 必须是四位数字，实际 {}", shard.id),
                ));
            }
            if !seen.insert(shard.id.as_str()) {
                return Err(CostError::manifest(
                    "manifest_shard_duplicate",
                    format!("分片 id {} 重复", shard.id),
                ));
            }
            if !is_sha256_hex(&shard.sha256) {
                return Err(CostError::manifest(
                    "manifest_shard_sha256",
                    format!("分片 {} 的 sha256 不是 64 位小写十六进制", shard.id),
                ));
            }
            rows = rows.saturating_add(shard.rows);
        }

        if rows != self.total_rows {
            return Err(CostError::manifest(
                "total_rows_mismatch",
                format!(
                    "分片行数合计 {rows} 与 total_rows {} 不一致",
                    self.total_rows
                ),
            ));
        }
        if self.total_rows > MAX_RANGE_ROWS {
            return Err(CostError::manifest(
                "range_rows_exceeded",
                format!(
                    "total_rows {} 超过单范围上限 {MAX_RANGE_ROWS}，应转 COS 导出而不是截断",
                    self.total_rows
                ),
            ));
        }

        Ok(())
    }

    /// `fetched_at` 的 UTC 表示（契约要求带时区的 RFC3339）。
    pub fn fetched_at_utc(&self) -> Result<DateTime<Utc>, CostError> {
        DateTime::parse_from_rfc3339(&self.fetched_at)
            .map(|at| at.with_timezone(&Utc))
            .map_err(|err| {
                CostError::manifest(
                    "manifest_fetched_at",
                    format!(
                        "fetched_at 不是带时区的 RFC3339：{} ({err})",
                        self.fetched_at
                    ),
                )
            })
    }

    /// 整个 manifest 的内容摘要（版本级完整性 + 重放判定用）。
    ///
    /// 覆盖范围身份、币种、口径、分片身份与行数——任何一项不同都会改变摘要，
    /// 因此不能用同一个 `batch_id` 偷偷改内容。
    pub fn content_sha256(&self) -> String {
        let shards: Vec<String> = self
            .shards
            .iter()
            .map(|shard| format!("{}:{}:{}", shard.id, shard.rows, shard.sha256))
            .collect();
        let canonical = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.schema,
            self.batch_id,
            self.range.identity_key(),
            self.currency,
            self.period_source.as_str(),
            self.fetched_at,
            self.total_rows,
            shards.join(","),
            self.request_ids.join(","),
            self.range.period_start,
            self.range.period_end,
        );
        let digest = Sha256::digest(canonical.as_bytes());
        format!("{digest:x}")
    }

    /// 涉及的分片总字节数（manifest 自述值，仅入报告用）。
    pub fn declared_bytes(&self) -> u64 {
        self.shards.iter().map(|shard| shard.bytes).sum()
    }
}

fn is_currency_code(value: &str) -> bool {
    value.len() == 3 && value.bytes().all(|b| b.is_ascii_uppercase())
}

fn is_shard_id(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|b| b.is_ascii_digit())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_manifest() -> serde_json::Value {
        serde_json::json!({
            "schema": MANIFEST_SCHEMA,
            "batch_id": "tencent-cloud-prod:cost-20261112T003000Z",
            "range": {
                "source_code": "tencent-cloud-prod",
                "vendor": "TENCENT_CLOUD",
                "billing_account_uin": "100000000001",
                "cost_basis": "CONSUMPTION",
                "period_start": "2026-11-10",
                "period_end": "2026-11-10"
            },
            "currency": "CNY",
            "period_source": "cloud_api",
            "fetched_at": "2026-11-12T00:31:22+08:00",
            "shards": [{
                "id": "0001",
                "rows": 3,
                "sha256": "dc837de9f371a2724c1f9611e446660ea3d20c11fca1d908f5fcca6301b8870a",
                "bytes": 3470
            }],
            "total_rows": 3
        })
    }

    fn manifest() -> Manifest {
        serde_json::from_value(raw_manifest()).expect("manifest")
    }

    #[test]
    fn parses_and_validates_contract_manifest() {
        let manifest = manifest();
        manifest.validate().unwrap();
        assert_eq!(manifest.range.period_label(), "2026-11-10");
        assert_eq!(
            manifest.fetched_at_utc().unwrap().to_rfc3339(),
            "2026-11-11T16:31:22+00:00"
        );
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let mut raw = raw_manifest();
        raw["future_field"] = serde_json::json!({"nested": true});
        raw["range"]["future_range_field"] = serde_json::json!("whatever");
        raw["shards"][0]["future_shard_field"] = serde_json::json!(1);
        let manifest: Manifest = serde_json::from_value(raw).expect("忽略未知字段");
        manifest.validate().unwrap();
    }

    #[test]
    fn total_rows_must_match_shard_sum() {
        let mut manifest = manifest();
        manifest.total_rows = 4;
        let err = manifest.validate().unwrap_err();
        assert_eq!(err.code(), "total_rows_mismatch");
    }

    #[test]
    fn duplicate_shard_ids_are_rejected() {
        let mut manifest = manifest();
        manifest.shards.push(manifest.shards[0].clone());
        manifest.total_rows = 6;
        let err = manifest.validate().unwrap_err();
        assert_eq!(err.code(), "manifest_shard_duplicate");
    }

    #[test]
    fn bad_sha_shape_is_rejected() {
        let mut manifest = manifest();
        manifest.shards[0].sha256 = "DC837DE9".to_string();
        let err = manifest.validate().unwrap_err();
        assert_eq!(err.code(), "manifest_shard_sha256");
    }

    #[test]
    fn oversize_range_is_rejected_not_truncated() {
        let mut manifest = manifest();
        manifest.total_rows = MAX_RANGE_ROWS + 1;
        manifest.shards[0].rows = MAX_RANGE_ROWS + 1;
        let err = manifest.validate().unwrap_err();
        assert_eq!(err.code(), "range_rows_exceeded");
    }

    #[test]
    fn empty_range_is_valid() {
        let mut manifest = manifest();
        manifest.shards.clear();
        manifest.total_rows = 0;
        manifest.validate().unwrap();
        assert_eq!(manifest.declared_bytes(), 0);
    }

    #[test]
    fn content_digest_tracks_content() {
        let manifest = manifest();
        let mut changed = manifest.clone();
        changed.batch_id = "tencent-cloud-prod:cost-20261113T003000Z".to_string();
        assert_ne!(manifest.content_sha256(), changed.content_sha256());
        assert_eq!(manifest.content_sha256(), manifest.clone().content_sha256());
    }
}
