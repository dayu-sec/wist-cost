//! 存储层错误。带稳定错误码，报告与测试按码分流。

use cost_core::CostError;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Cost(#[from] CostError),

    #[error("postgres：{0}")]
    Db(#[from] sqlx::Error),

    #[error("迁移失败：{0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("范围 {range} 的币种冲突：库里 {existing}，本次 {incoming}")]
    CurrencyConflict {
        range: String,
        existing: String,
        incoming: String,
    },

    #[error("入库对账不一致（{0}）：整批回滚，不发布有效版本")]
    Reconciliation(String),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Cost(err) => err.code(),
            Self::Db(_) => "postgres_error",
            Self::Migrate(_) => "migrate_failed",
            Self::CurrencyConflict { .. } => "range_currency_conflict",
            Self::Reconciliation(_) => "reconciliation_mismatch",
        }
    }
}
