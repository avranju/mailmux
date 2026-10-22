use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Deserializer, de::Error};

/// Keep JSON's original decimal spelling; never route money through f64.
pub fn deserialize_optional<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Decimal>, D::Error> {
    let raw = Option::<Box<serde_json::value::RawValue>>::deserialize(deserializer)?;
    raw.map(|raw| {
        let value = raw.get();
        if !value.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
            return Err(D::Error::custom("amount must be a JSON number"));
        }
        Decimal::from_str_exact(value)
            .or_else(|_| Decimal::from_scientific(value))
            .map_err(|_| D::Error::custom("amount is outside the supported decimal range"))
    })
    .transpose()
}

pub fn minor_units(amount: Decimal) -> anyhow::Result<i64> {
    let scaled = amount
        .abs()
        .checked_mul(Decimal::from(100))
        .ok_or_else(|| anyhow::anyhow!("amount is outside the supported minor-unit range"))?;
    if !scaled.fract().is_zero() {
        anyhow::bail!("amount must have at most two fractional decimal digits");
    }
    scaled
        .to_i64()
        .ok_or_else(|| anyhow::anyhow!("amount is outside the supported minor-unit range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Deserialize)]
    struct Amount {
        #[serde(default, deserialize_with = "deserialize_optional")]
        amount: Option<Decimal>,
    }

    #[test]
    fn decimal_json_and_minor_units_are_exact_and_checked() {
        for (raw, units) in [
            ("0.29", 29),
            ("-1.01", 101),
            ("1e-2", 1),
            ("90071992547409.91", 9_007_199_254_740_991),
        ] {
            let value: Amount = serde_json::from_str(&format!("{{\"amount\":{raw}}}")).unwrap();
            assert_eq!(minor_units(value.amount.unwrap()).unwrap(), units);
        }
        assert!(serde_json::from_str::<Amount>(r#"{"amount":"0.29"}"#).is_err());
        assert!(minor_units(Decimal::from_str_exact("0.001").unwrap()).is_err());
        assert!(minor_units(Decimal::MAX).is_err());
    }
}
