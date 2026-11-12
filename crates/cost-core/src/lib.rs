//! `wist-cost` 的领域层：与存储无关的部分。
//!
//! 三层分工见设计 §1：harvest 只拉取与落分片，**金额只在 cost 算一次**。
//! 本 crate 负责：
//!
//! - [`money`]：十进制金额（`null` / `0` / 负数三态，`numeric(30,12)` 无损落地）；
//! - [`range`]：范围身份七元组（同一范围重复发布 = 整段替换）；
//! - [`manifest`] / [`detail`]：`contracts/harvest-cost-v1` 的解析与逐字段校验；
//! - [`verify`]：分片 sha256 / 行数 / 总额校验，产出"可入账批次"；
//! - [`objects`]：对象存储抽象（W0 落地 `local`，`s3` 留位）。
//!
//! 这一层没有任何 IO 依赖：字节由调用方给，存储由 [`objects::ObjectStore`] 抽象。
//! 因此"整批拒绝"的规则可以用内存后端直接测。

pub mod detail;
pub mod error;
pub mod manifest;
pub mod money;
pub mod objects;
pub mod range;
pub mod verify;

pub use detail::{Amounts, DetailLine, DetailViolation};
pub use error::CostError;
pub use manifest::{DETAIL_SCHEMA, MANIFEST_SCHEMA, Manifest, PeriodSource, Shard};
pub use money::Money;
pub use objects::{ObjectBackend, ObjectStore, ShardKeyLayout};
pub use range::{CostBasis, CostRange, Vendor};
pub use verify::{
    AmountTotals, CheckedDetail, ColumnTotal, ShardDigest, VerifiedBatch, VerifiedShard,
    verify_batch, verify_shard,
};

/// 入账规则的版本号，写进 `cost_range_version.rule_version`。
///
/// 口径（金额语义、校验规则）变化时必须改它，便于回溯"这一版是按哪套规则算的"。
pub const RULE_VERSION: &str = "wist-cost/w0.1";
