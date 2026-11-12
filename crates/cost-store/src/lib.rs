//! `cost-store`：PG schema/迁移、范围替换入账、对象存储后端。

pub mod error;
pub mod ingest;
pub mod objects;
pub mod pg;

pub use error::StoreError;
pub use ingest::{IngestReport, Ingestor, MonthRows, PublishOutcome};
pub use objects::{LocalObjectStore, ObjectStoreConfig, ObjectsInfo, S3ObjectStore};
pub use pg::{PgStore, SCHEMA_VERSION, SchemaStatus, StoreStats};

/// 本 crate 里"有效版本"的判定口径，供查询与运维脚本复用。
pub const PUBLISHED_STATE: &str = "published";
