//! 对象存储后端：W0 只有 `local`；`s3` 留配置位与 trait，不实现。
//!
//! 设计 §4 沿用 `wist-evidence` 的形状：`objects.kind = local | s3`，没有内存档。

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use cost_core::CostError;
use cost_core::objects::{ObjectBackend, ObjectStore};
use serde::Serialize;

/// 对象存储配置（/healthz 自述与启动期校验共用）。
#[derive(Debug, Clone)]
pub enum ObjectStoreConfig {
    /// 本地目录：直接读分片文件。
    Local { root: PathBuf },
    /// COS / MinIO（W0 未实现，仅保留配置位）。
    S3 {
        bucket: String,
        endpoint: Option<String>,
        prefix: Option<String>,
    },
}

/// `/healthz` 里的 `objects` 段。
#[derive(Debug, Clone, Serialize)]
pub struct ObjectsInfo {
    /// `local` | `s3`
    pub kind: &'static str,
    pub root: Option<String>,
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    /// 该后端是否支持 `:presign`（≤60s，仅 s3）。
    pub presign: bool,
    /// W0 是否真的实现了读取。
    pub implemented: bool,
}

impl ObjectStoreConfig {
    /// 从环境变量构造。默认 `local`，根目录 `"."`。
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, CostError> {
        let kind = get("WIST_COST_OBJECT_KIND").unwrap_or_else(|| "local".to_string());
        let backend = ObjectBackend::parse(&kind).ok_or_else(|| {
            CostError::objects(format!(
                "WIST_COST_OBJECT_KIND 只接受 local|s3，实际 {kind}"
            ))
        })?;

        match backend {
            ObjectBackend::Local => Ok(Self::Local {
                root: PathBuf::from(
                    get("WIST_COST_OBJECT_ROOT").unwrap_or_else(|| ".".to_string()),
                ),
            }),
            ObjectBackend::S3 => Ok(Self::S3 {
                bucket: get("WIST_COST_OBJECT_BUCKET").unwrap_or_default(),
                endpoint: get("WIST_COST_OBJECT_ENDPOINT"),
                prefix: get("WIST_COST_OBJECT_PREFIX"),
            }),
        }
    }

    pub fn backend(&self) -> ObjectBackend {
        match self {
            Self::Local { .. } => ObjectBackend::Local,
            Self::S3 { .. } => ObjectBackend::S3,
        }
    }

    pub fn describe(&self) -> ObjectsInfo {
        match self {
            Self::Local { root } => ObjectsInfo {
                kind: "local",
                root: Some(root.display().to_string()),
                bucket: None,
                endpoint: None,
                presign: false,
                implemented: true,
            },
            Self::S3 {
                bucket,
                endpoint,
                prefix,
            } => ObjectsInfo {
                kind: "s3",
                root: prefix.clone(),
                bucket: Some(bucket.clone()),
                endpoint: endpoint.clone(),
                presign: true,
                implemented: false,
            },
        }
    }

    /// 构造后端。s3 会明确报"未实现"，而不是悄悄退化成空读取。
    pub fn build(&self) -> Result<Box<dyn ObjectStore>, CostError> {
        match self {
            Self::Local { root } => Ok(Box::new(LocalObjectStore::new(root.clone()))),
            Self::S3 {
                bucket,
                endpoint,
                prefix,
            } => Ok(Box::new(S3ObjectStore::new(
                bucket.clone(),
                endpoint.clone(),
                prefix.clone(),
            ))),
        }
    }
}

/// 本地目录后端：`root` + 对象键就是文件路径。
#[derive(Debug, Clone)]
pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[async_trait]
impl ObjectStore for LocalObjectStore {
    fn backend(&self) -> ObjectBackend {
        ObjectBackend::Local
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CostError> {
        let relative = safe_relative_key(key)?;
        let full = self.root.join(relative);
        match tokio::fs::read(&full).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(CostError::objects(format!(
                "读取 {} 失败：{err}",
                full.display()
            ))),
        }
    }

    async fn presign_get(&self, _key: &str, _ttl: Duration) -> Result<String, CostError> {
        Err(CostError::objects(
            "objects.kind = local 不支持 presign_get（设计 §4：≤60s 预签名仅 s3）",
        ))
    }
}

/// s3 占位：W0 不实现。留着让配置、自述、trait 形状先定下来。
#[derive(Debug, Clone)]
pub struct S3ObjectStore {
    bucket: String,
    endpoint: Option<String>,
    prefix: Option<String>,
}

impl S3ObjectStore {
    pub fn new(bucket: String, endpoint: Option<String>, prefix: Option<String>) -> Self {
        Self {
            bucket,
            endpoint,
            prefix,
        }
    }

    pub fn describe(&self) -> ObjectsInfo {
        ObjectsInfo {
            kind: "s3",
            root: self.prefix.clone(),
            bucket: Some(self.bucket.clone()),
            endpoint: self.endpoint.clone(),
            presign: true,
            implemented: false,
        }
    }
}

#[async_trait]
impl ObjectStore for S3ObjectStore {
    fn backend(&self) -> ObjectBackend {
        ObjectBackend::S3
    }

    async fn get(&self, _key: &str) -> Result<Option<Vec<u8>>, CostError> {
        Err(CostError::objects(format!(
            "objects.kind = s3 在 W0 未实现（bucket={}）；请用 local 后端或等 W1 接入",
            self.bucket
        )))
    }

    async fn presign_get(&self, _key: &str, _ttl: Duration) -> Result<String, CostError> {
        Err(CostError::objects(
            "objects.kind = s3 在 W0 未实现：presign_get 待接入",
        ))
    }
}

/// 拒绝逃出 root 的对象键（`..`、绝对路径、空键）。
fn safe_relative_key(key: &str) -> Result<PathBuf, CostError> {
    if key.trim().is_empty() {
        return Err(CostError::objects("对象键不能为空"));
    }
    let path = Path::new(key);
    if path.is_absolute() {
        return Err(CostError::objects(format!("对象键不能是绝对路径：{key}")));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(CostError::objects(format!(
                "对象键不能含 `.` / `..`：{key}"
            )));
        }
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_keys_that_escape_the_root() {
        assert!(safe_relative_key("cost/a/0001.ndjson").is_ok());
        assert!(safe_relative_key("../etc/passwd").is_err());
        assert!(safe_relative_key("/etc/passwd").is_err());
        assert!(safe_relative_key("./a").is_err());
        assert!(safe_relative_key("  ").is_err());
    }

    #[test]
    fn env_defaults_to_local_backend() {
        let config = ObjectStoreConfig::from_env(|_| None).unwrap();
        assert_eq!(config.backend(), ObjectBackend::Local);
        let info = config.describe();
        assert_eq!(info.kind, "local");
        assert!(info.implemented);
        assert!(!info.presign);
    }

    #[test]
    fn s3_is_configurable_but_not_implemented() {
        let config = ObjectStoreConfig::from_env(|name| match name {
            "WIST_COST_OBJECT_KIND" => Some("s3".to_string()),
            "WIST_COST_OBJECT_BUCKET" => Some("wist-cost".to_string()),
            _ => None,
        })
        .unwrap();
        let info = config.describe();
        assert_eq!(info.kind, "s3");
        assert!(!info.implemented);
        assert!(info.presign);
        assert_eq!(info.bucket.as_deref(), Some("wist-cost"));
    }

    #[test]
    fn unknown_backend_kind_is_rejected() {
        let err = ObjectStoreConfig::from_env(|name| match name {
            "WIST_COST_OBJECT_KIND" => Some("memory".to_string()),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(err.code(), "object_store_unavailable");
    }

    #[tokio::test]
    async fn local_backend_reads_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shard.ndjson");
        std::fs::write(&path, b"x").unwrap();

        let store = LocalObjectStore::new(dir.path());
        assert_eq!(
            store.get("shard.ndjson").await.unwrap(),
            Some(b"x".to_vec())
        );
        assert!(store.get("nope.ndjson").await.unwrap().is_none());
        assert!(
            store
                .presign_get("shard.ndjson", Duration::from_secs(1))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn s3_backend_fails_loudly() {
        let store = S3ObjectStore::new("b".to_string(), None, None);
        let err = store.get("k").await.unwrap_err();
        assert_eq!(err.code(), "object_store_unavailable");
    }
}
