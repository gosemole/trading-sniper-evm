use ethers::types::U256;

/// Convert a U256 (expected uint160 value) to f64.
fn to_f64(v: U256) -> f64 {
    if let Ok(x) = u128::try_from(v) {
        return x as f64;
    }
    // Coarse fallback for very large uint160: value = hi*2^128 + lo.
    let l = v.0; // [u64; 4]
    let lo = (l[0] as u128) | ((l[1] as u128) << 64);
    let hi = (l[2] as u128) | ((l[3] as u128) << 64);
    (hi as f64) * 2f64.powi(128) + lo as f64
}

/// Raw price of token1 per token0 (no decimal adjustment): (sqrtPriceX96/2^96)^2.
pub fn raw_price(sqrt: U256) -> f64 {
    let s = to_f64(sqrt);
    let p = s / 2f64.powi(96);
    p * p
}

/// Displayed price of the configured base token.
/// `raw` is token1-per-token0; real quote = raw * 10^(decimals0-decimals1).
pub fn display_price(sqrt: U256, decimals0: u8, decimals1: u8, base_token: u8) -> f64 {
    from_raw(raw_price(sqrt), decimals0, decimals1, base_token)
}

/// The same, from a price already out of the X96 domain - as the tick walk
/// keeps it. One function so the two paths cannot drift apart.
pub fn from_sqrt(sqrt_p: f64, decimals0: u8, decimals1: u8, base_token: u8) -> f64 {
    from_raw(sqrt_p * sqrt_p, decimals0, decimals1, base_token)
}

fn from_raw(raw: f64, decimals0: u8, decimals1: u8, base_token: u8) -> f64 {
    let p = raw * 10f64.powi(decimals0 as i32 - decimals1 as i32);
    match base_token {
        1 => 1.0 / p,
        _ => p,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_price() {
        // sqrtPriceX96 = 2^96 => raw price = 1.0
        let s = U256::from(2u8).pow(96.into());
        let raw = raw_price(s);
        assert!((raw - 1.0).abs() < 1e-9);

        // decimals 18/6 -> token1/token0 = 1 * 10^12
        let d = display_price(s, 18, 6, 0);
        let rel = (d - 1e12).abs() / 1e12;
        assert!(rel < 1e-9, "got {d}");

        // base token is token1 -> inverse
        let d1 = display_price(s, 18, 6, 1);
        let rel1 = (d1 - 1e-12).abs() / 1e-12;
        assert!(rel1 < 1e-9, "got {d1}");
    }

    #[test]
    fn both_domains_agree() {
        // The X96 path and the plain-sqrt path must give the same price, or a
        // seeded entry price would not compare with the prices that follow it.
        let s = U256::from(2u8).pow(96.into()) * 3u64;
        for base in [0u8, 1] {
            let from_x96 = display_price(s, 18, 6, base);
            let plain = from_sqrt(3.0, 18, 6, base);
            let rel = (from_x96 - plain).abs() / from_x96;
            assert!(rel < 1e-12, "base {base}: {from_x96} vs {plain}");
        }
    }
}
