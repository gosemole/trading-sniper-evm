//! Amounts, in and out of the units a person writes them in.
//!
//! Every figure that decides a trade arrives as an integer in a token's
//! smallest unit and has to be read, printed or compared as a decimal. Doing
//! that with floats loses the tail - and the tail of a wei figure is the part
//! that says whether two numbers are the same one.

use anyhow::{Context, Result};
use ethers::types::U256;

/// Parse a decimal string into raw token units. Rejects a fraction longer than
/// the token supports rather than silently truncating value away.
pub fn parse_units(s: &str, decimals: u8) -> Result<U256> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty amount");
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    anyhow::ensure!(
        int_part.chars().all(|c| c.is_ascii_digit())
            && frac_part.chars().all(|c| c.is_ascii_digit()),
        "amount '{s}' is not a decimal number"
    );
    anyhow::ensure!(
        frac_part.len() <= decimals as usize,
        "amount '{s}' has {} decimals but the token only has {decimals}",
        frac_part.len()
    );
    let mut digits = String::from(if int_part.is_empty() { "0" } else { int_part });
    digits.push_str(frac_part);
    for _ in frac_part.len()..decimals as usize {
        digits.push('0');
    }
    U256::from_dec_str(&digits).with_context(|| format!("amount '{s}' does not fit in u256"))
}

/// Format raw units back into a human decimal string, for display only.
pub fn format_units(v: U256, decimals: u8) -> String {
    let s = v.to_string();
    let d = decimals as usize;
    if d == 0 {
        return s;
    }
    let s = format!("{:0>width$}", s, width = d + 1);
    let (i, f) = s.split_at(s.len() - d);
    let f = f.trim_end_matches('0');
    if f.is_empty() {
        i.to_string()
    } else {
        format!("{i}.{f}")
    }
}

pub fn u256_to_f64(v: U256) -> f64 {
    v.to_string().parse().unwrap_or(f64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal_amounts() {
        assert_eq!(parse_units("1", 6).unwrap(), U256::from(1_000_000u64));
        assert_eq!(parse_units("1.0", 6).unwrap(), U256::from(1_000_000u64));
        assert_eq!(parse_units("0.5", 6).unwrap(), U256::from(500_000u64));
        assert_eq!(parse_units("0.000001", 6).unwrap(), U256::one());
        assert_eq!(parse_units(" 12.34 ", 2).unwrap(), U256::from(1234u64));
        assert_eq!(parse_units("7", 0).unwrap(), U256::from(7u64));
        assert_eq!(
            parse_units("1", 18).unwrap(),
            U256::from_dec_str("1000000000000000000").unwrap()
        );
    }

    #[test]
    fn rejects_amounts_that_would_lose_value() {
        // more precision than the token has would silently truncate
        assert!(parse_units("0.0000001", 6).is_err());
        assert!(parse_units("1.5", 0).is_err());
        assert!(parse_units("", 18).is_err());
        assert!(parse_units("abc", 18).is_err());
        assert!(parse_units("-1", 18).is_err());
        assert!(parse_units("1e18", 18).is_err());
    }

    #[test]
    fn formats_back_to_human_units() {
        assert_eq!(format_units(U256::from(1_000_000u64), 6), "1");
        assert_eq!(format_units(U256::from(1_500_000u64), 6), "1.5");
        assert_eq!(format_units(U256::from(1u64), 6), "0.000001");
        assert_eq!(format_units(U256::zero(), 18), "0");
        assert_eq!(format_units(U256::from(42u64), 0), "42");
    }

    #[test]
    fn round_trips_through_raw_units() {
        for (s, d) in [("1.5", 18u8), ("0.000001", 6), ("123.456", 8), ("2", 18)] {
            let raw = parse_units(s, d).unwrap();
            assert_eq!(format_units(raw, d), s);
        }
    }
}
