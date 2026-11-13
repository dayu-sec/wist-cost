//! 对象存储抽象：W0 只落地 `local`，`s3` 留位。
//!
//! 沿用 `wist-evidence` 已经验证过的形状（设计 §4）：`objects.kind = local | s3`，
//! 没有"内存"档作为部署选项；读正文走代理读 / `:presign`（仅 s3）。
//!
//! 对象键沿用设计 §4 的布局：
//! `cost/{vendor}/{cost_basis}/{yyyy}/{mm}/{shard_id}.ndjson`
//! （客户由 `billing_account_uin` → 客户清单映射，与资产侧同一份关系；harvest 生产侧后缀是 `.ndjson.zst`，
//! zstd 解压在 W1 补——本层只负责"给我 key 就还我字节"）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;

use crate::error::CostError;
use crate::manifest::Manifest;

/// 对象存储后端（`/healthz` 自述用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectBackend {
    /// 本地目录，读原文即分片文件。
    Local,
    /// COS / MinIO（W0 未实现）。
    S3,
}

impl ObjectBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::S3 => "s3",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" | "file" => Some(Self::Local),
            "s3" | "cos" | "minio" => Some(Self::S3),
            _ => None,
        }
    }

    /// 该后端是否支持预签名读（设计 §4：只有 s3 支持）。
    pub fn supports_presign(self) -> bool {
        matches!(self, Self::S3)
    }
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    /// 后端类型（自述用）。
    fn backend(&self) -> ObjectBackend;

    /// 读一个对象的全部字节。对象不存在返回 `Ok(None)`（由调用方决定是不是错误）。
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CostError>;

    /// 签一个短命 GET URL。local 后端明确不支持。
    async fn presign_get(&self, key: &str, ttl: Duration) -> Result<String, CostError>;
}

/// 分片对象键模板。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardKeyLayout {
    template: String,
}

impl ShardKeyLayout {
    /// 设计 §4 的布局（W0 用 `.ndjson`，zstd 后缀在 W1 接）。
    pub const DESIGN_TEMPLATE: &str = "cost/{vendor}/{cost_basis}/{year}/{month}/{shard_id}.ndjson";

    pub fn design_default() -> Self {
        Self {
            template: Self::DESIGN_TEMPLATE.to_string(),
        }
    }

    /// 自定义模板。必须含 `{shard_id}`，否则每次查找都会落到同一个对象上。
    pub fn new(template: impl Into<String>) -> Result<Self, CostError> {
        let template = template.into();
        if template.trim().is_empty() {
            return Err(CostError::objects("对象键模板不能为空"));
        }
        if !template.contains("{shard_id}") {
            return Err(CostError::objects(format!(
                "对象键模板必须包含 {{shard_id}}，实际 {template}"
            )));
        }
        Ok(Self { template })
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    /// 按 manifest 与分片 id 展开对象键。
    ///
    /// 可用占位符：`{source_code}` `{vendor}` `{cost_basis}` `{year}`
    /// `{month}` `{batch_id}` `{shard_id}`。
    pub fn key_for(&self, manifest: &Manifest, shard_id: &str) -> String {
        let range = &manifest.range;
        let year = range.period_start.format("%Y").to_string();
        let month = range.period_start.format("%m").to_string();
        self.template
            .replace("{source_code}", &range.source_code)
            .replace("{vendor}", range.vendor.as_str())
            .replace("{cost_basis}", range.cost_basis.as_str())
            .replace("{year}", &year)
            .replace("{month}", &month)
            .replace("{batch_id}", &manifest.batch_id)
            .replace("{shard_id}", shard_id)
    }
}

/// 进程内对象存储：**只给测试**，不是部署选项（与 `wist-evidence` 同口径）。
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    objects: Mutex<HashMap<String, Vec<u8>>>,
}

impl MemoryObjectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&self, key: impl Into<String>, bytes: Vec<u8>) {
        self.objects
            .lock()
            .expect("memory object store poisoned")
            .insert(key.into(), bytes);
    }

    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .objects
            .lock()
            .expect("memory object store poisoned")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }
}

#[async_trait]
impl ObjectStore for MemoryObjectStore {
    fn backend(&self) -> ObjectBackend {
        ObjectBackend::Local
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CostError> {
        Ok(self
            .objects
            .lock()
            .expect("memory object store poisoned")
            .get(key)
            .cloned())
    }

    async fn presign_get(&self, _key: &str, _ttl: Duration) -> Result<String, CostError> {
        Err(CostError::objects("内存后端只给测试用，不支持 presign_get"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::{CostBasis, CostRange, Vendor};

    fn manifest() -> Manifest {
        Manifest {
            schema: crate::manifest::MANIFEST_SCHEMA.to_string(),
            batch_id: "tencent-cloud-prod:cost-20261112T003000Z".to_string(),
            range: CostRange {
                source_code: "tencent-cloud-prod".to_string(),
                vendor: Vendor::TencentCloud,
                billing_account_uin: "100000000001".to_string(),
                cost_basis: CostBasis::Consumption,
                period_start: "2026-11-10".parse().unwrap(),
                period_end: "2026-11-10".parse().unwrap(),
            },
            currency: "CNY".to_string(),
            period_source: crate::manifest::PeriodSource::CloudApi,
            fetched_at: "2026-11-12T00:31:22+08:00".to_string(),
            request_ids: Vec::new(),
            shards: Vec::new(),
            total_rows: 0,
        }
    }

    #[test]
    fn design_layout_uses_the_documented_key() {
        let layout = ShardKeyLayout::design_default();
        assert_eq!(
            layout.key_for(&manifest(), "0001"),
            "cost/TENCENT_CLOUD/CONSUMPTION/2026/11/0001.ndjson"
        );
    }

    #[test]
    fn custom_layout_must_contain_shard_id() {
        assert!(ShardKeyLayout::new("cost/{shard_id}.ndjson").is_ok());
        assert!(ShardKeyLayout::new("cost/all.ndjson").is_err());
        assert!(ShardKeyLayout::new("  ").is_err());
    }

    #[test]
    fn backend_names_are_stable() {
        assert_eq!(ObjectBackend::Local.as_str(), "local");
        assert_eq!(ObjectBackend::S3.as_str(), "s3");
        assert_eq!(ObjectBackend::parse("file"), Some(ObjectBackend::Local));
        assert!(!ObjectBackend::Local.supports_presign());
        assert!(ObjectBackend::S3.supports_presign());
    }

    #[tokio::test]
    async fn memory_store_round_trips_bytes() {
        let store = MemoryObjectStore::new();
        assert!(store.get("a").await.unwrap().is_none());
        store.put("a", b"x".to_vec());
        assert_eq!(store.get("a").await.unwrap(), Some(b"x".to_vec()));
        assert!(
            store
                .presign_get("a", Duration::from_secs(1))
                .await
                .is_err()
        );
    }
}
