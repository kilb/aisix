//! Money as people type and read it: dollars with at most two decimals in,
//! exact micro-USD decimal strings out. Nothing here rounds.

use crate::error::{ApiError, Detail};

/// Micro-USD in one cent.
const MICRO_USD_PER_CENT: i64 = 10_000;

/// Micro-USD in one dollar.
const MICRO_USD_PER_USD: u64 = 1_000_000;

/// The most integer digits a typed amount may have.
const MAX_DOLLAR_DIGITS: usize = 7;

/// `s`, a dollar amount a person typed, in micro-USD: one to seven ASCII
/// digits, then optionally a point and one or two more
/// (`^\d{1,7}(\.\d{1,2})?$`). Anything else (a sign, an exponent, a
/// separator, a space, a third decimal) is `422` at `path`.
pub fn parse_usd(s: &str, path: &str) -> Result<i64, ApiError> {
    let (dollars, cents) = match s.split_once('.') {
        Some((dollars, cents)) if (1..=2).contains(&cents.len()) => (dollars, cents),
        Some(_) => return Err(not_dollars(path)),
        None => (s, ""),
    };
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if !(1..=MAX_DOLLAR_DIGITS).contains(&dollars.len()) || !digits(dollars) || !digits(cents) {
        return Err(not_dollars(path));
    }
    let value = |part: &str| part.bytes().fold(0i64, |n, b| n * 10 + i64::from(b - b'0'));
    // "5" is fifty cents, "05" five.
    let cents = match cents.len() {
        1 => value(cents) * 10,
        _ => value(cents),
    };
    Ok((value(dollars) * 100 + cents) * MICRO_USD_PER_CENT)
}

fn not_dollars(path: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: "金额应为美元，最多两位小数".to_string(),
    }])
}

/// `micro` micro-USD as an exact dollar amount with six decimals, `-` in
/// front of a negative one: `-11000` is `"-0.011000"`.
pub fn usd_string(micro: i64) -> String {
    let sign = if micro < 0 { "-" } else { "" };
    let abs = micro.unsigned_abs();
    format!(
        "{sign}{}.{:06}",
        abs / MICRO_USD_PER_USD,
        abs % MICRO_USD_PER_USD
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dollar_input_is_cents_at_most() {
        for (typed, micro) in [
            ("0.01", 10_000),
            ("12", 12_000_000),
            ("9999999.99", 9_999_999_990_000),
            ("0", 0),
            ("1.5", 1_500_000),
            ("1.05", 1_050_000),
            ("0007", 7_000_000),
        ] {
            assert_eq!(parse_usd(typed, "/amount_usd").ok(), Some(micro), "{typed}");
        }
        for typed in [
            "0.001", "-1", "1e3", "", "1,000", " 1", "1 ", "+1", "1.", ".5", "1.2.3", "10000000",
            "１", "0x10",
        ] {
            let e = parse_usd(typed, "/amount_usd").expect_err(typed);
            assert_eq!(
                (e.status.as_u16(), e.code),
                (422, "validation_failed"),
                "{typed}"
            );
            assert_eq!(
                e.details,
                vec![Detail {
                    path: "/amount_usd".to_string(),
                    message: "金额应为美元，最多两位小数".to_string(),
                }],
                "{typed}"
            );
        }
    }

    #[test]
    fn micro_usd_strings_are_exact() {
        assert_eq!(usd_string(0), "0.000000");
        assert_eq!(usd_string(1), "0.000001");
        assert_eq!(usd_string(-11_000), "-0.011000");
        assert_eq!(usd_string(12_000_000), "12.000000");
        assert_eq!(usd_string(-1_000_000), "-1.000000");
        assert_eq!(usd_string(i64::MAX), "9223372036854.775807");
        assert_eq!(usd_string(i64::MIN), "-9223372036854.775808");
    }
}
