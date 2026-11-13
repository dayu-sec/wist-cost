//! 范围身份：六元组。
//!
//! 契约 `harvest-cost-v1/README.md` §3：
//!
//! ```text
//! (source_code, vendor, billing_account_uin, cost_basis, period_start, period_end)
//! ```
//!
//! 同一六元组重复发布 = **替换**，不是累加。七元组任一字段不同就是另一个范围，互不覆盖。

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::error::CostError;

/// 云厂商。v1 只有腾讯云（契约 `range.vendor` 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Vendor {
    #[serde(rename = "TENCENT_CLOUD")]
    TencentCloud,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TencentCloud => "TENCENT_CLOUD",
        }
    }
}

/// 成本口径。两种口径的金额**不可相加**（交接文档 §5.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CostBasis {
    /// 日消耗：`period_start == period_end`。
    #[serde(rename = "CONSUMPTION")]
    Consumption,
    /// 整月费用：整月一个范围。
    #[serde(rename = "CHARGE")]
    Charge,
}

impl CostBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Consumption => "CONSUMPTION",
            Self::Charge => "CHARGE",
        }
    }
}

/// 一个成本范围的**身份**（不含金额）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CostRange {
    pub source_code: String,
    pub vendor: Vendor,
    pub billing_account_uin: String,
    pub cost_basis: CostBasis,
    pub period_start: NaiveDate,
    pub period_end: NaiveDate,
}

impl CostRange {
    /// 契约 §3 的一致性约束。
    pub fn validate(&self) -> Result<(), CostError> {
        let fields = [
            ("source_code", self.source_code.as_str()),
            ("billing_account_uin", self.billing_account_uin.as_str()),
        ];
        for (name, value) in fields {
            if value.trim().is_empty() {
                return Err(CostError::range(format!("范围字段 {name} 不能为空")));
            }
        }

        if self.period_end < self.period_start {
            return Err(CostError::range(format!(
                "period_end {} 早于 period_start {}",
                self.period_end, self.period_start
            )));
        }

        match self.cost_basis {
            CostBasis::Consumption => {
                if self.period_start != self.period_end {
                    return Err(CostError::range(format!(
                        "CONSUMPTION 必须是单日范围，实际 {} .. {}",
                        self.period_start, self.period_end
                    )));
                }
            }
            CostBasis::Charge => {
                let first =
                    NaiveDate::from_ymd_opt(self.period_start.year(), self.period_start.month(), 1)
                        .ok_or_else(|| CostError::range("period_start 无法解析为该月首日"))?;
                let last = last_day_of_month(self.period_start);
                if self.period_start != first || self.period_end != last {
                    return Err(CostError::range(format!(
                        "CHARGE 必须是整月范围（{} .. {}），实际 {} .. {}",
                        first, last, self.period_start, self.period_end
                    )));
                }
            }
        }

        Ok(())
    }

    /// 六元组的稳定字符串：并发入账时的 advisory lock 键与日志标识。
    pub fn identity_key(&self) -> String {
        [
            self.source_code.as_str(),
            self.vendor.as_str(),
            self.billing_account_uin.as_str(),
            self.cost_basis.as_str(),
            &self.period_start.to_string(),
            &self.period_end.to_string(),
        ]
        .join("\u{1f}")
    }

    /// 人读的范围标签。
    pub fn period_label(&self) -> String {
        if self.period_start == self.period_end {
            self.period_start.to_string()
        } else {
            format!("{}..{}", self.period_start, self.period_end)
        }
    }

    /// 明细的 `cost_date` 是否落在本范围内。
    pub fn contains_date(&self, date: NaiveDate) -> bool {
        date >= self.period_start && date <= self.period_end
    }
}

fn last_day_of_month(date: NaiveDate) -> NaiveDate {
    let (year, month) = (date.year(), date.month());
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|first_of_next| first_of_next.pred_opt())
        .expect("month arithmetic stays in range")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(basis: CostBasis, start: &str, end: &str) -> CostRange {
        CostRange {
            source_code: "tencent-cloud-prod".to_string(),
            vendor: Vendor::TencentCloud,
            billing_account_uin: "100000000001".to_string(),
            cost_basis: basis,
            period_start: start.parse().unwrap(),
            period_end: end.parse().unwrap(),
        }
    }

    #[test]
    fn consumption_must_be_a_single_day() {
        assert!(
            range(CostBasis::Consumption, "2026-11-10", "2026-11-10")
                .validate()
                .is_ok()
        );
        let err = range(CostBasis::Consumption, "2026-11-10", "2026-11-11")
            .validate()
            .unwrap_err();
        assert_eq!(err.code(), "range_invalid");
    }

    #[test]
    fn charge_must_span_a_whole_month() {
        assert!(
            range(CostBasis::Charge, "2026-11-01", "2026-11-30")
                .validate()
                .is_ok()
        );
        assert!(
            range(CostBasis::Charge, "2026-02-01", "2026-02-28")
                .validate()
                .is_ok()
        );
        assert!(
            range(CostBasis::Charge, "2024-02-01", "2024-02-29")
                .validate()
                .is_ok()
        );
        assert!(
            range(CostBasis::Charge, "2026-11-01", "2026-11-29")
                .validate()
                .is_err()
        );
        assert!(
            range(CostBasis::Charge, "2026-11-02", "2026-11-30")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn identity_changes_with_any_of_the_seven_fields() {
        let base = range(CostBasis::Consumption, "2026-11-10", "2026-11-10");
        let mut other = base.clone();
        other.source_code = "tencent-cloud-dev".to_string();
        assert_ne!(base.identity_key(), other.identity_key());

        let mut other = base.clone();
        other.cost_basis = CostBasis::Charge;
        other.period_end = "2026-11-30".parse().unwrap();
        other.period_start = "2026-11-01".parse().unwrap();
        assert_ne!(base.identity_key(), other.identity_key());
    }

    #[test]
    fn contains_date_covers_the_period() {
        let month = range(CostBasis::Charge, "2026-11-01", "2026-11-30");
        assert!(month.contains_date("2026-11-10".parse().unwrap()));
        assert!(!month.contains_date("2026-12-01".parse().unwrap()));
    }
}
