//! 统一错误类型。
//!
//! 每个失败都带一个**稳定错误码**（[`CostError::code`]），报告、日志与测试都断言码而不是
//! 断言中文文案——文案会改，码是契约。

use crate::money::MoneyError;

#[derive(Debug, thiserror::Error)]
pub enum CostError {
    #[error("manifest 不合法（{code}）：{detail}")]
    Manifest { code: &'static str, detail: String },

    #[error("范围身份不合法：{0}")]
    Range(String),

    #[error("分片 {shard} 校验失败（{code}）：{detail}")]
    Shard {
        shard: String,
        code: &'static str,
        detail: String,
    },

    #[error("分片 {shard} 第 {row} 行不合法（{code}）：{detail}")]
    Detail {
        shard: String,
        row: u64,
        code: &'static str,
        detail: String,
    },

    #[error(transparent)]
    Money(#[from] MoneyError),

    #[error("对象存储不可用：{0}")]
    Objects(String),

    #[error("JSON 解析失败：{0}")]
    Json(#[from] serde_json::Error),
}

impl CostError {
    pub fn manifest(code: &'static str, detail: impl Into<String>) -> Self {
        Self::Manifest {
            code,
            detail: detail.into(),
        }
    }

    pub fn range(detail: impl Into<String>) -> Self {
        Self::Range(detail.into())
    }

    pub fn shard(shard: &str, code: &'static str, detail: impl Into<String>) -> Self {
        Self::Shard {
            shard: shard.to_string(),
            code,
            detail: detail.into(),
        }
    }

    pub fn detail(shard: &str, row: u64, code: &'static str, detail: impl Into<String>) -> Self {
        Self::Detail {
            shard: shard.to_string(),
            row,
            code,
            detail: detail.into(),
        }
    }

    pub fn objects(detail: impl Into<String>) -> Self {
        Self::Objects(detail.into())
    }

    /// 稳定错误码。报告与测试按它分流，不依赖文案。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Manifest { code, .. } | Self::Shard { code, .. } | Self::Detail { code, .. } => {
                code
            }
            Self::Range(_) => "range_invalid",
            Self::Money(err) => err.code(),
            Self::Objects(_) => "object_store_unavailable",
            Self::Json(_) => "json_invalid",
        }
    }
}
