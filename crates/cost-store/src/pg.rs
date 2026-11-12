//! PG 连接、迁移与自述。

use serde::Serialize;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::error::StoreError;

/// 本 crate 期望的 schema 版本（写进 `cost_meta.schema_version`）。
pub const SCHEMA_VERSION: &str = "cost-schema-v1";

/// PG 句柄。
pub struct PgStore {
    pool: PgPool,
}

/// `/healthz` 用的 schema 自述。
#[derive(Debug, Clone, Serialize)]
pub struct SchemaStatus {
    /// 本程序期望的版本。
    pub expected: &'static str,
    /// 库里登记的版本（未迁移时为 None）。
    pub version: Option<String>,
    /// `_sqlx_migrations` 里已应用的迁移数。
    pub migrations: i64,
}

impl SchemaStatus {
    pub fn initialized(&self) -> bool {
        self.version.as_deref() == Some(self.expected)
    }
}

/// `/healthz` 用的数据量自述。
#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreStats {
    pub ranges: i64,
    pub published_versions: i64,
    pub detail_rows: i64,
}

impl PgStore {
    pub async fn connect(database_url: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 应用 `migrations/` 下的全部迁移（幂等）。
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    /// 库里登记的 schema 版本与迁移数。
    pub async fn schema_status(&self) -> Result<SchemaStatus, StoreError> {
        let version: Option<String> =
            match sqlx::query_scalar("select value from cost_meta where key = 'schema_version'")
                .fetch_optional(&self.pool)
                .await
            {
                Ok(value) => value,
                Err(err) if is_undefined_table(&err) => None,
                Err(err) => return Err(err.into()),
            };

        let migrations: i64 =
            match sqlx::query_scalar("select count(*)::bigint from _sqlx_migrations")
                .fetch_one(&self.pool)
                .await
            {
                Ok(count) => count,
                Err(err) if is_undefined_table(&err) => 0,
                Err(err) => return Err(err.into()),
            };

        Ok(SchemaStatus {
            expected: SCHEMA_VERSION,
            version,
            migrations,
        })
    }

    /// 范围 / 有效版本 / 明细行数。
    pub async fn stats(&self) -> Result<StoreStats, StoreError> {
        let row: (i64, i64, i64) = sqlx::query_as(
            "select
                 (select count(*) from cost_range)::bigint,
                 (select count(*) from cost_range_version where state = 'published')::bigint,
                 (select count(*) from cost_detail)::bigint",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(StoreStats {
            ranges: row.0,
            published_versions: row.1,
            detail_rows: row.2,
        })
    }
}

/// PG 的 `undefined_table`（42P01）：表还不存在 = 还没迁移。
fn is_undefined_table(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|db| db.code())
        .map(|code| code == "42P01")
        .unwrap_or(false)
}
