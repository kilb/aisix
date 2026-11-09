//! Money as people type and read it: yuan with at most two decimals in, exact
//! micro-CNY decimal strings out. Nothing here rounds.
//!
//! Every amount a tenant owes, holds, is credited or is shown is integer
//! micro-CNY (`i64`, 10⁻⁶ CNY). An amount a person types is yuan with at most
//! two decimals, stored as fen × 10 000 µ¥. Micro-CNY rather than fen is what
//! keeps a row rounded once rather than grossly upward: 10 input tokens at
//! USD 0.15 per million × 7.2 is 11 µ¥, which in fen would round up to 0.01
//! CNY, about 900 times its price (design D14).

use crate::error::{ApiError, Detail};

/// The currency every amount in this release is in.
pub const CURRENCY: &str = "CNY";

/// Micro-CNY in one fen.
const MICRO_CNY_PER_FEN: i64 = 10_000;

/// Micro-CNY in one yuan.
const MICRO_CNY_PER_YUAN: u64 = 1_000_000;

/// The most integer digits a typed amount may have: ¥99,999,999.99. Eight
/// rather than seven because a CNY figure is about seven times the USD one
/// for the same value (design D18).
const MAX_YUAN_DIGITS: usize = 8;

/// `s`, a yuan amount a person typed, in micro-CNY: one to eight ASCII
/// digits, then optionally a point and one or two more
/// (`^\d{1,8}(\.\d{1,2})?$`). Anything else (a sign, an exponent, a
/// separator, a space, a third decimal) is `422` at `path`.
pub fn parse_cny(s: &str, path: &str) -> Result<i64, ApiError> {
    let (yuan, fen) = match s.split_once('.') {
        Some((yuan, fen)) if (1..=2).contains(&fen.len()) => (yuan, fen),
        Some(_) => return Err(not_yuan(path)),
        None => (s, ""),
    };
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if !(1..=MAX_YUAN_DIGITS).contains(&yuan.len()) || !digits(yuan) || !digits(fen) {
        return Err(not_yuan(path));
    }
    let value = |part: &str| part.bytes().fold(0i64, |n, b| n * 10 + i64::from(b - b'0'));
    // "5" is fifty fen, "05" five.
    let fen = match fen.len() {
        1 => value(fen) * 10,
        _ => value(fen),
    };
    Ok((value(yuan) * 100 + fen) * MICRO_CNY_PER_FEN)
}

fn not_yuan(path: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: path.to_string(),
        message: "金额应为人民币元，最多两位小数".to_string(),
    }])
}

/// `micro` micro-CNY as an exact yuan amount with six decimals, `-` in front
/// of a negative one: `-11000` is `"-0.011000"`.
pub fn cny_string(micro: i64) -> String {
    let sign = if micro < 0 { "-" } else { "" };
    let abs = micro.unsigned_abs();
    format!(
        "{sign}{}.{:06}",
        abs / MICRO_CNY_PER_YUAN,
        abs % MICRO_CNY_PER_YUAN
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuan_input_is_fen_at_most() {
        for (typed, micro) in [
            ("0.01", 10_000),
            ("12", 12_000_000),
            ("100.50", 100_500_000),
            ("99999999.99", 99_999_999_990_000),
            ("0", 0),
            ("1.5", 1_500_000),
            ("1.05", 1_050_000),
            ("0007", 7_000_000),
        ] {
            assert_eq!(parse_cny(typed, "/amount_cny").ok(), Some(micro), "{typed}");
        }
        for typed in [
            "0.001",
            "-1",
            "1e3",
            "",
            "1,000",
            " 1",
            "1 ",
            "+1",
            "1.",
            ".5",
            "1.2.3",
            "100000000",
            "１",
            "0x10",
        ] {
            let e = parse_cny(typed, "/amount_cny").expect_err(typed);
            assert_eq!(
                (e.status.as_u16(), e.code),
                (422, "validation_failed"),
                "{typed}"
            );
            assert_eq!(
                e.details,
                vec![Detail {
                    path: "/amount_cny".to_string(),
                    message: "金额应为人民币元，最多两位小数".to_string(),
                }],
                "{typed}"
            );
        }
    }

    #[test]
    fn cny_strings_are_exact() {
        assert_eq!(cny_string(0), "0.000000");
        assert_eq!(cny_string(1), "0.000001");
        assert_eq!(cny_string(-11_000), "-0.011000");
        assert_eq!(cny_string(100_500_000), "100.500000");
        assert_eq!(cny_string(-1_000_000), "-1.000000");
        assert_eq!(cny_string(i64::MAX), "9223372036854.775807");
        assert_eq!(cny_string(i64::MIN), "-9223372036854.775808");
    }
}
