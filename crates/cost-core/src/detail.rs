//! 契约 v1 的明细行，以及"这一行能不能入账"的校验。
//!
//! 权威定义：`contracts/harvest-cost-v1/detail.schema.json`。
//! 金额语义（交接文档 §5.2）：
//!
//! - `real_cost` = 各组件 `RealCost` 求和；
//! - 四类支付金额任一组件缺值 → 整体 `null`（未知，**不是 0**）；
//! - 退款为负，原样保留；
//! - `null` 与 `"0"` 不可互换。

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::CostError;
use crate::manifest::DETAIL_SCHEMA;
use crate::money::Money;
use crate::range::CostRange;

/// 一行明细的校验失败。`code` 是稳定错误码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailViolation {
    pub code: &'static str,
    pub detail: String,
}

impl DetailViolation {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// 契约 v1 的一行明细（NDJSON 的一行）。
///
/// 只保留标准化字段与原文；`components` / `raw` 按契约原样透传（对象存储里才是权威原文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetailLine {
    pub schema: String,
    pub cost_date: NaiveDate,
    pub bill_month: String,
    pub business_code: String,
    #[serde(default)]
    pub business_code_name: Option<String>,
    #[serde(default)]
    pub product_code: Option<String>,
    #[serde(default)]
    pub product_code_name: Option<String>,
    #[serde(default)]
    pub resource_id: Option<String>,
    /// `business_code + ':' + (resource_id | "UNALLOCATED")`。
    pub resource_key: String,
    #[serde(default)]
    pub resource_name: Option<String>,
    #[serde(default)]
    pub payer_uin: Option<String>,
    #[serde(default)]
    pub owner_uin: Option<String>,
    #[serde(default)]
    pub pay_mode_name: Option<String>,
    #[serde(default)]
    pub action_type_name: Option<String>,
    #[serde(default)]
    pub pay_time: Option<String>,
    #[serde(default)]
    pub fee_begin_time: Option<String>,
    #[serde(default)]
    pub fee_end_time: Option<String>,
    #[serde(default)]
    pub provider_detail_id: Option<String>,
    #[serde(default)]
    pub bill_id: Option<String>,
    #[serde(default)]
    pub order_id: Option<String>,
    pub currency: String,
    #[serde(default)]
    pub real_cost: Option<Money>,
    #[serde(default)]
    pub cash_amount: Option<Money>,
    #[serde(default)]
    pub voucher_amount: Option<Money>,
    #[serde(default)]
    pub incentive_amount: Option<Money>,
    #[serde(default)]
    pub transfer_amount: Option<Money>,
    #[serde(default)]
    pub components: Vec<Value>,
    pub raw: Value,
}

/// 一行的五个金额列。`None` = 契约里的 `null`（未知），`Some(zero)` = 确认为零。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Amounts {
    pub real_cost: Option<Money>,
    pub cash_amount: Option<Money>,
    pub voucher_amount: Option<Money>,
    pub incentive_amount: Option<Money>,
    pub transfer_amount: Option<Money>,
}

/// 金额列名（与 `cost_detail` 的列名一致）。
pub const AMOUNT_COLUMNS: [&str; 5] = [
    "real_cost",
    "cash_amount",
    "voucher_amount",
    "incentive_amount",
    "transfer_amount",
];

impl Amounts {
    pub fn iter(&self) -> [(&'static str, Option<Money>); 5] {
        [
            (AMOUNT_COLUMNS[0], self.real_cost),
            (AMOUNT_COLUMNS[1], self.cash_amount),
            (AMOUNT_COLUMNS[2], self.voucher_amount),
            (AMOUNT_COLUMNS[3], self.incentive_amount),
            (AMOUNT_COLUMNS[4], self.transfer_amount),
        ]
    }

    pub fn column(&self, column: &str) -> Option<Money> {
        self.iter()
            .into_iter()
            .find(|(name, _)| *name == column)
            .and_then(|(_, value)| value)
    }

    /// 写库用的十进制字符串，顺序与 [`AMOUNT_COLUMNS`] 一致。
    ///
    /// 不做失败返回：能走到这里说明 [`DetailLine::check`] 已经确认 `fits_db_column()`。
    /// 若绕过校验直接调用，写库后的对账（Rust 求和 vs PG `sum()`）仍会拦住被舍入的批次。
    pub fn db_strings(&self) -> [Option<String>; 5] {
        [
            self.real_cost,
            self.cash_amount,
            self.voucher_amount,
            self.incentive_amount,
            self.transfer_amount,
        ]
        .map(|value| value.map(|money| money.normalized().to_string()))
    }
}

impl DetailLine {
    /// 校验一行是否可入账，并返回入库用的金额视图。
    pub fn check(&self, range: &CostRange, currency: &str) -> Result<Amounts, DetailViolation> {
        if self.schema != DETAIL_SCHEMA {
            return Err(DetailViolation::new(
                "detail_schema",
                format!("schema 必须是 {DETAIL_SCHEMA}，实际 {}", self.schema),
            ));
        }
        if self.business_code.trim().is_empty() {
            return Err(DetailViolation::new(
                "detail_business_code",
                "business_code 不能为空",
            ));
        }
        if !is_bill_month(&self.bill_month) {
            return Err(DetailViolation::new(
                "detail_bill_month",
                format!("bill_month 必须是 YYYY-MM，实际 {}", self.bill_month),
            ));
        }
        if !range.contains_date(self.cost_date) {
            return Err(DetailViolation::new(
                "detail_cost_date_out_of_range",
                format!(
                    "cost_date {} 不在范围 {} 内",
                    self.cost_date,
                    range.period_label()
                ),
            ));
        }
        if self.currency != currency {
            return Err(DetailViolation::new(
                "detail_currency_mismatch",
                format!(
                    "明细币种 {} 与 manifest 币种 {currency} 不一致",
                    self.currency
                ),
            ));
        }
        self.check_resource_key()?;

        let amounts = Amounts {
            real_cost: self.real_cost,
            cash_amount: self.cash_amount,
            voucher_amount: self.voucher_amount,
            incentive_amount: self.incentive_amount,
            transfer_amount: self.transfer_amount,
        };
        for (name, value) in amounts.iter() {
            if let Some(money) = value
                && !money.fits_db_column()
            {
                return Err(DetailViolation::new(
                    "money_unstorable",
                    format!("{name} = {money} 无法无损存进 numeric(30,12)"),
                ));
            }
        }

        self.check_components()?;
        Ok(amounts)
    }

    /// 契约 §5.2：`resource_key = business_code + ':' + (resource_id | "UNALLOCATED")`。
    fn check_resource_key(&self) -> Result<(), DetailViolation> {
        let expected = match self.resource_id.as_deref() {
            Some(id) if !id.is_empty() => format!("{}:{}", self.business_code, id),
            _ => format!("{}:UNALLOCATED", self.business_code),
        };
        if self.resource_key != expected {
            return Err(DetailViolation::new(
                "resource_key_mismatch",
                format!("resource_key 应为 {expected}，实际 {}", self.resource_key),
            ));
        }
        Ok(())
    }

    /// 组件金额与汇总金额的一致性（交接文档 §5.2）。
    ///
    /// 取舍：组件的**内部形状**在契约里是自由对象（`items: {type: object}`），
    /// 所以这里按 §5.2 文档化的原文字段名核对；字段缺席视为"不可核验"而不是报错，
    /// 只有"字段在、值为 null"才强制整体 null 语义。
    fn check_components(&self) -> Result<(), DetailViolation> {
        if self.components.is_empty() {
            return Ok(());
        }

        let mut real_sum = Money::zero();
        let mut real_known = 0usize;
        let mut payment_sums = [Money::zero(); 4];
        let mut payment_known = [0usize; 4];
        let mut payment_unknown = [false; 4];

        for (index, component) in self.components.iter().enumerate() {
            let object = component.as_object().ok_or_else(|| {
                DetailViolation::new(
                    "component_not_object",
                    format!("第 {} 个组件不是对象", index + 1),
                )
            })?;

            match object.get("RealCost") {
                Some(Value::String(raw)) => {
                    let money = Money::parse(raw).map_err(|err| {
                        DetailViolation::new(err.code(), format!("第 {} 个组件：{err}", index + 1))
                    })?;
                    real_sum = real_sum.checked_add(money).map_err(|err| {
                        DetailViolation::new(
                            err.code(),
                            format!("第 {} 个组件求和：{err}", index + 1),
                        )
                    })?;
                    real_known += 1;
                }
                _ => {
                    return Err(DetailViolation::new(
                        "component_real_cost_missing",
                        format!("第 {} 个组件缺 RealCost，本范围失败", index + 1),
                    ));
                }
            }

            for (slot, key) in PAYMENT_KEYS.iter().enumerate() {
                match object.get(*key) {
                    Some(Value::String(raw)) => {
                        let money = Money::parse(raw).map_err(|err| {
                            DetailViolation::new(
                                err.code(),
                                format!("第 {} 个组件的 {key}：{err}", index + 1),
                            )
                        })?;
                        payment_sums[slot] =
                            payment_sums[slot].checked_add(money).map_err(|err| {
                                DetailViolation::new(
                                    err.code(),
                                    format!("第 {} 个组件的 {key} 求和：{err}", index + 1),
                                )
                            })?;
                        payment_known[slot] += 1;
                    }
                    Some(Value::Null) => payment_unknown[slot] = true,
                    Some(_) => {
                        return Err(DetailViolation::new(
                            "component_amount_not_string",
                            format!("第 {} 个组件的 {key} 不是十进制字符串", index + 1),
                        ));
                    }
                    // 字段缺席：契约没钉组件内部形状，按不可核验处理。
                    None => payment_unknown[slot] = true,
                }
            }
        }

        let real_cost = self.real_cost.ok_or_else(|| {
            DetailViolation::new("real_cost_missing", "有组件明细时必须给出 real_cost")
        })?;
        if real_known != self.components.len() || real_cost != real_sum {
            return Err(DetailViolation::new(
                "component_sum_mismatch",
                format!("real_cost {real_cost} 与组件 RealCost 求和 {real_sum} 不一致"),
            ));
        }

        let mut detail_payments = [
            self.cash_amount,
            self.voucher_amount,
            self.incentive_amount,
            self.transfer_amount,
        ];
        for (slot, key) in PAYMENT_KEYS.iter().enumerate() {
            let detail_value = detail_payments[slot].take();
            if payment_unknown[slot] || payment_known[slot] != self.components.len() {
                // §5.2：任一组件缺值则整体 null。
                if detail_value.is_some() {
                    return Err(DetailViolation::new(
                        "component_amount_null_mismatch",
                        format!("{key} 有组件缺值，明细侧必须为 null"),
                    ));
                }
            } else if let Some(value) = detail_value
                && value != payment_sums[slot]
            {
                return Err(DetailViolation::new(
                    "component_sum_mismatch",
                    format!(
                        "明细金额 {value} 与组件 {key} 求和 {} 不一致",
                        payment_sums[slot]
                    ),
                ));
            }
        }

        Ok(())
    }
}

/// 组件的支付金额字段名（交接文档 §5.2 的原文字段名）。
const PAYMENT_KEYS: [&str; 4] = [
    "CashPayAmount",
    "VoucherPayAmount",
    "IncentivePayAmount",
    "TransferPayAmount",
];

fn is_bill_month(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 7 || bytes[4] != b'-' {
        return false;
    }
    if !bytes[..4].iter().all(u8::is_ascii_digit) || !bytes[5..].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let month = (bytes[5] - b'0') * 10 + (bytes[6] - b'0');
    (1..=12).contains(&month)
}

/// 把校验失败挂上分片/行号上下文。
pub fn detail_error(shard_id: &str, row_no: u64, violation: DetailViolation) -> CostError {
    CostError::detail(shard_id, row_no, violation.code, violation.detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::{CostBasis, Vendor};

    fn range() -> CostRange {
        CostRange {
            source_code: "tencent-cloud-prod".to_string(),
            vendor: Vendor::TencentCloud,
            billing_account_uin: "100000000001".to_string(),
            cost_basis: CostBasis::Consumption,
            period_start: "2026-11-10".parse().unwrap(),
            period_end: "2026-11-10".parse().unwrap(),
        }
    }

    fn line(raw: &str) -> DetailLine {
        serde_json::from_str(raw).expect("detail line")
    }

    const ROW_CVM: &str = r#"{"schema":"wist.harvest.cost.detail.v1","cost_date":"2026-11-10","bill_month":"2026-11","business_code":"p_cvm","resource_id":"ins-example01","resource_key":"p_cvm:ins-example01","currency":"CNY","real_cost":"10.000000000000","cash_amount":"9.000000000000","voucher_amount":"1.000000000000","incentive_amount":"0.000000000000","transfer_amount":"0.000000000000","components":[{"RealCost":"6.000000000000","CashPayAmount":"5.400000000000","VoucherPayAmount":"0.600000000000","IncentivePayAmount":"0.000000000000","TransferPayAmount":"0.000000000000"},{"RealCost":"4.000000000000","CashPayAmount":"3.600000000000","VoucherPayAmount":"0.400000000000","IncentivePayAmount":"0.000000000000","TransferPayAmount":"0.000000000000"}],"raw":{}}"#;

    const ROW_UNALLOCATED: &str = r#"{"schema":"wist.harvest.cost.detail.v1","cost_date":"2026-11-10","bill_month":"2026-11","business_code":"p_cbs","resource_id":null,"resource_key":"p_cbs:UNALLOCATED","currency":"CNY","real_cost":"2.500000000000","cash_amount":null,"voucher_amount":null,"incentive_amount":null,"transfer_amount":null,"components":[{"RealCost":"2.500000000000","CashPayAmount":null,"VoucherPayAmount":null,"IncentivePayAmount":null,"TransferPayAmount":null}],"raw":{}}"#;

    const ROW_REFUND: &str = r#"{"schema":"wist.harvest.cost.detail.v1","cost_date":"2026-11-10","bill_month":"2026-11","business_code":"p_eip","resource_id":"eip-example01","resource_key":"p_eip:eip-example01","currency":"CNY","real_cost":"-3.200000000000","cash_amount":"-3.200000000000","voucher_amount":"0.000000000000","incentive_amount":"0.000000000000","transfer_amount":"0.000000000000","components":[{"RealCost":"-3.200000000000","CashPayAmount":"-3.200000000000","VoucherPayAmount":"0.000000000000","IncentivePayAmount":"0.000000000000","TransferPayAmount":"0.000000000000"}],"raw":{}}"#;

    #[test]
    fn accepts_well_formed_lines() {
        let amounts = line(ROW_CVM).check(&range(), "CNY").unwrap();
        assert_eq!(amounts.real_cost, Some(Money::parse("10").unwrap()));

        let amounts = line(ROW_UNALLOCATED).check(&range(), "CNY").unwrap();
        assert_eq!(amounts.cash_amount, None, "组件缺值必须是 null，不是 0");

        let amounts = line(ROW_REFUND).check(&range(), "CNY").unwrap();
        assert_eq!(amounts.real_cost, Some(Money::parse("-3.2").unwrap()));
        assert!(amounts.real_cost.unwrap().is_negative());
    }

    #[test]
    fn rejects_cost_date_outside_the_range() {
        let mut detail = line(ROW_CVM);
        detail.cost_date = "2026-11-09".parse().unwrap();
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "detail_cost_date_out_of_range");
    }

    #[test]
    fn rejects_currency_mismatch() {
        let violation = line(ROW_CVM).check(&range(), "USD").unwrap_err();
        assert_eq!(violation.code, "detail_currency_mismatch");
    }

    #[test]
    fn rejects_bad_resource_key() {
        let mut detail = line(ROW_CVM);
        detail.resource_key = "p_cvm:UNALLOCATED".to_string();
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "resource_key_mismatch");
    }

    #[test]
    fn rejects_component_sum_drift() {
        let mut detail = line(ROW_CVM);
        detail.real_cost = Some(Money::parse("11").unwrap());
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "component_sum_mismatch");
    }

    #[test]
    fn rejects_missing_component_amount() {
        let mut detail = line(ROW_CVM);
        detail.components[0]
            .as_object_mut()
            .unwrap()
            .remove("RealCost");
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "component_real_cost_missing");
    }

    #[test]
    fn rejects_payment_value_when_a_component_is_unknown() {
        let mut detail = line(ROW_UNALLOCATED);
        detail.cash_amount = Some(Money::parse("0").unwrap());
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "component_amount_null_mismatch");
    }

    #[test]
    fn rejects_bad_bill_month() {
        let mut detail = line(ROW_CVM);
        detail.bill_month = "2026-13".to_string();
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "detail_bill_month");
    }

    #[test]
    fn rejects_amounts_that_cannot_be_stored_exactly() {
        let mut detail = line(ROW_CVM);
        detail.components.clear();
        detail.real_cost = Some(Money::parse("1.123456789012345").unwrap());
        let violation = detail.check(&range(), "CNY").unwrap_err();
        assert_eq!(violation.code, "money_unstorable");
    }
}
