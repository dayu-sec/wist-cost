//! 金额：十进制精确值。
//!
//! 设计 §4.1 与交接文档 §5.2 的要求：
//!
//! - 库里列是 `numeric(30,12)`，**不用二进制浮点**；
//! - 契约里的金额是十进制字符串，最多 18 位小数；
//! - `null`（未知）与 `"0"`（确认为零）严格区分——这一层由 `Option<Money>` 承载，
//!   `None` 就是未知，`Some(zero)` 才是零；
//! - 退款为负，原样保留。
//!
//! 与 `numeric(30,12)` 的落地关系：本类型能无损表示契约允许的全部金额；写入库前
//! [`Money::fits_db_column`] 要求**有效小数位 ≤ 12 且整数位 ≤ 18**。超过 12 位非零小数
//! 的金额在这里就被拒绝，**不做静默四舍五入**（静默舍入会让日汇总与月核验差最后一位）。

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 契约 `detail.schema.json` 允许的小数位上限。
pub const CONTRACT_MAX_SCALE: u8 = 18;
/// 库里列的小数位：`numeric(30,12)`。
pub const DB_SCALE: u8 = 12;
/// 库里列的总位数：`numeric(30,12)`。
pub const DB_PRECISION: u8 = 30;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("金额为空")]
    Empty,

    #[error("金额 `{0}` 不是合法十进制串（只接受 `-?digits(.digits)?`）")]
    Invalid(String),

    #[error("金额 `{0}` 小数位超过契约允许的 {CONTRACT_MAX_SCALE} 位")]
    TooManyFractionDigits(String),

    #[error("金额 `{0}` 超出 i128 表示范围")]
    Overflow(String),

    #[error("金额 `{0}` 无法无损存进 numeric(30,12)")]
    Unstorable(String),
}

impl MoneyError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Empty => "money_empty",
            Self::Invalid(_) => "money_invalid",
            Self::TooManyFractionDigits(_) => "money_scale_too_large",
            Self::Overflow(_) => "money_overflow",
            Self::Unstorable(_) => "money_unstorable",
        }
    }
}

/// 十进制金额：`units / 10^scale`。
///
/// 相等性按**数值**比较（`"1.10"` == `"1.1"`），这样契约侧的不同写法不会影响校验。
#[derive(Debug, Clone, Copy)]
pub struct Money {
    units: i128,
    scale: u8,
}

impl Default for Money {
    fn default() -> Self {
        Self::zero()
    }
}

impl Money {
    pub const fn zero() -> Self {
        Self { units: 0, scale: 0 }
    }

    /// 解析契约金额串。只接受 `-?digits(.digits)?`，不接受 `+`、指数、空串。
    pub fn parse(raw: &str) -> Result<Self, MoneyError> {
        let (negative, body) = match raw.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, raw),
        };
        if body.is_empty() {
            return Err(MoneyError::Empty);
        }

        let (int_part, frac_part) = match body.split_once('.') {
            Some((int_part, frac_part)) => (int_part, Some(frac_part)),
            None => (body, None),
        };

        if int_part.is_empty() || !int_part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(MoneyError::Invalid(raw.to_string()));
        }
        if let Some(frac) = frac_part
            && (frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(MoneyError::Invalid(raw.to_string()));
        }

        let frac = frac_part.unwrap_or("");
        if frac.len() > CONTRACT_MAX_SCALE as usize {
            return Err(MoneyError::TooManyFractionDigits(raw.to_string()));
        }

        let digits: String = format!("{int_part}{frac}");
        let units: i128 = digits
            .parse()
            .map_err(|_| MoneyError::Overflow(raw.to_string()))?;
        let units = if negative { -units } else { units };

        Ok(Self {
            units,
            scale: frac.len() as u8,
        })
    }

    pub fn is_zero(&self) -> bool {
        self.units == 0
    }

    pub fn is_negative(&self) -> bool {
        self.units < 0
    }

    /// 标度（小数位数）。
    pub fn scale(&self) -> u8 {
        self.scale
    }

    /// 去掉尾随零的等价表示。
    pub fn normalized(&self) -> Self {
        let mut units = self.units;
        let mut scale = self.scale;
        while scale > 0 && units % 10 == 0 {
            units /= 10;
            scale -= 1;
        }
        Self { units, scale }
    }

    pub fn checked_add(self, rhs: Self) -> Result<Self, MoneyError> {
        let scale = self.scale.max(rhs.scale);
        let lhs_units = self
            .units_at(scale)
            .ok_or_else(|| MoneyError::Overflow(format!("{self} + {rhs}")))?;
        let rhs_units = rhs
            .units_at(scale)
            .ok_or_else(|| MoneyError::Overflow(format!("{self} + {rhs}")))?;
        let units = lhs_units
            .checked_add(rhs_units)
            .ok_or_else(|| MoneyError::Overflow(format!("{self} + {rhs}")))?;
        Ok(Self { units, scale }.normalized())
    }

    /// 能否无损存进 `numeric(30,12)`。
    pub fn fits_db_column(&self) -> bool {
        let value = self.normalized();
        if value.scale > DB_SCALE {
            return false;
        }
        let integer_units = value.units.abs() / pow10(value.scale).expect("scale ≤ 18");
        digit_count(integer_units) <= u32::from(DB_PRECISION - DB_SCALE)
    }

    /// 写库用的十进制字符串；不能无损落库时报错（不替调用方做舍入决定）。
    pub fn to_db_string(&self) -> Result<String, MoneyError> {
        if !self.fits_db_column() {
            return Err(MoneyError::Unstorable(self.to_string()));
        }
        Ok(self.normalized().to_string())
    }

    /// 把标度提到 `scale` 后的整数系数。
    fn units_at(self, scale: u8) -> Option<i128> {
        if scale < self.scale {
            return None;
        }
        let factor = pow10(scale - self.scale)?;
        self.units.checked_mul(factor)
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.units < 0 { "-" } else { "" };
        let digits = self.units.unsigned_abs().to_string();
        if self.scale == 0 {
            return write!(f, "{sign}{digits}");
        }
        let scale = self.scale as usize;
        let (int_part, frac_part) = if digits.len() <= scale {
            (
                "0".to_string(),
                format!("{}{}", "0".repeat(scale - digits.len()), digits),
            )
        } else {
            let split = digits.len() - scale;
            (digits[..split].to_string(), digits[split..].to_string())
        };
        write!(f, "{sign}{int_part}.{frac_part}")
    }
}

impl PartialEq for Money {
    fn eq(&self, other: &Self) -> bool {
        let lhs = self.normalized();
        let rhs = other.normalized();
        lhs.units == rhs.units && lhs.scale == rhs.scale
    }
}

impl Eq for Money {}

impl FromStr for Money {
    type Err = MoneyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Money {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Money {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MoneyVisitor;

        impl Visitor<'_> for MoneyVisitor {
            type Value = Money;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("十进制字符串（如 \"10.000000000000\"）；不接受 JSON 数字")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Money, E> {
                Money::parse(value).map_err(|err| E::custom(err.to_string()))
            }
        }

        deserializer.deserialize_str(MoneyVisitor)
    }
}

fn pow10(exp: u8) -> Option<i128> {
    let mut value: i128 = 1;
    let mut step = 0;
    while step < exp {
        value = value.checked_mul(10)?;
        step += 1;
    }
    Some(value)
}

fn digit_count(value: i128) -> u32 {
    let mut rest = value;
    let mut digits = 1;
    while rest >= 10 {
        rest /= 10;
        digits += 1;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn money(raw: &str) -> Money {
        Money::parse(raw).expect("parse")
    }

    #[test]
    fn parses_contract_shapes() {
        assert_eq!(money("10.000000000000").to_string(), "10.000000000000");
        assert_eq!(money("0").to_string(), "0");
        assert_eq!(money("-3.200000000000").to_string(), "-3.200000000000");
        assert_eq!(money("-0.5").to_string(), "-0.5");
        assert_eq!(money("007.50").to_string(), "7.50");
    }

    #[test]
    fn rejects_non_decimal_shapes() {
        for raw in [
            "", "-", "+1", "1e5", "1.", ".5", "1.2.3", "abc", "1,5", " 1",
        ] {
            assert!(Money::parse(raw).is_err(), "应拒绝 {raw:?}");
        }
    }

    #[test]
    fn rejects_more_than_eighteen_fraction_digits() {
        let raw = format!("1.{}", "1".repeat(19));
        assert_eq!(
            Money::parse(&raw).unwrap_err().code(),
            "money_scale_too_large"
        );
    }

    #[test]
    fn equality_is_numeric_not_textual() {
        assert_eq!(money("1.10"), money("1.1"));
        assert_eq!(money("0.000"), Money::zero());
        assert_ne!(money("0.000000000001"), Money::zero());
    }

    #[test]
    fn addition_is_exact() {
        let total = money("0.1").checked_add(money("0.2")).unwrap();
        assert_eq!(total, money("0.3"));
        assert_ne!(total, money("0.300000000001"));

        let refund = money("-3.2").checked_add(money("10")).unwrap();
        assert_eq!(refund, money("6.8"));
        assert_eq!(refund.to_string(), "6.8");
    }

    #[test]
    fn twelve_decimal_columns_are_exact() {
        let sum = money("10.000000000000")
            .checked_add(money("2.500000000000"))
            .unwrap()
            .checked_add(money("-3.200000000000"))
            .unwrap();
        // 求和结果按数值相等，展示上取最短等价写法；落库时由 numeric(30,12) 补足 12 位。
        assert_eq!(sum, money("9.300000000000"));
        assert_eq!(sum.to_string(), "9.3");
        assert_eq!(sum.to_db_string().unwrap(), "9.3");
    }

    #[test]
    fn unstorable_values_are_rejected_instead_of_rounded() {
        let sub_cent = money("1.123456789012345");
        assert!(!sub_cent.fits_db_column());
        assert_eq!(
            sub_cent.to_db_string().unwrap_err().code(),
            "money_unstorable"
        );

        // 尾随零不算有效小数位，可以无损落库。
        let padded = money("1.500000000000000000");
        assert!(padded.fits_db_column());
        assert_eq!(padded.to_db_string().unwrap(), "1.5");
    }

    #[test]
    fn rejects_eighteen_digit_integer_part() {
        let too_big = money("1234567890123456789"); // 19 位整数
        assert!(!too_big.fits_db_column());
        assert!(money("123456789012345678").fits_db_column()); // 18 位
    }

    #[test]
    fn json_requires_a_string() {
        let parsed: Money = serde_json::from_str("\"1.25\"").unwrap();
        assert_eq!(parsed, money("1.25"));
        assert!(serde_json::from_str::<Money>("1.25").is_err());
        assert!(serde_json::from_str::<Money>("null").is_err());
    }

    #[test]
    fn null_and_zero_are_different_values() {
        let unknown: Option<Money> = serde_json::from_str("null").unwrap();
        let zero: Option<Money> = serde_json::from_str("\"0\"").unwrap();
        assert!(unknown.is_none());
        assert_eq!(zero, Some(Money::zero()));
        assert_ne!(unknown, zero);
    }
}
