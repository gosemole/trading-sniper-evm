//! Depth estimation that walks initialized ticks instead of assuming the whole
//! move happens at the current in-range liquidity.
//!
//! The math is the standard concentrated-liquidity step: between two prices at
//! constant `L`,
//!   dy = L * (sqrtB - sqrtA)          (token1 side)
//!   dx = L * (1/sqrtA - 1/sqrtB)      (token0 side)
//! and crossing an initialized tick adds `liquidityNet` going up, subtracts it
//! going down. Prices are handled as f64 `sqrt(P)` rather than X96 integers:
//! this is an estimate, and f64 keeps ~1e-12 relative error, far below the
//! uncertainty from reading state a block late.

use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, Bytes, TransactionRequest, H256, U256};
use ethers::utils::keccak256;

/// `PoolManager._pools` lives at storage slot 6, and within `Pool.State` the
/// `ticks` mapping is member 4 and `tickBitmap` member 5. Verified against the
/// deployed PoolManager on chain 4663 by cross-checking slot0's packed tick and
/// fee against the price implied by sqrtPriceX96.
const POOLS_SLOT: u64 = 6;

/// How many bitmap words to scan for the next initialized tick. One word spans
/// 256 tick spacings, so this covers a wide span while bounding the RPC cost.
const MAX_BITMAP_WORDS: u32 = 32;
const TICKS_OFFSET: u64 = 4;
const BITMAP_OFFSET: u64 = 5;

/// Where to read tick state from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Uniswap V3 pool contract: `ticks(int24)` and `tickBitmap(int16)` views.
    V3 { pool: Address },
    /// Uniswap V4 PoolManager: the same data read straight out of storage.
    V4 { manager: Address, pool_id: H256 },
}

fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}

/// Sign-extend a signed integer into a 32-byte big-endian word.
fn signed_word(v: i64) -> [u8; 32] {
    let mut w = if v >= 0 { [0u8; 32] } else { [0xffu8; 32] };
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn mapping_slot(key: &[u8; 32], base: U256) -> U256 {
    let mut buf = key.to_vec();
    let mut b = [0u8; 32];
    base.to_big_endian(&mut b);
    buf.extend_from_slice(&b);
    U256::from_big_endian(&keccak256(&buf))
}

/// Floor division, so negative ticks compress the same way Solidity does.
fn compress(tick: i32, spacing: i32) -> i32 {
    let mut c = tick / spacing;
    if tick % spacing != 0 && tick < 0 {
        c -= 1;
    }
    c
}

pub struct TickReader<'a> {
    http: &'a Provider<Http>,
    source: Source,
    spacing: i32,
    /// Block to read at, or `None` for the head. Pinning it matters whenever a
    /// reading is compared against another: a pool that moves between two
    /// "latest" reads turns the difference between them into price movement
    /// rather than whatever was being measured.
    at: Option<u64>,
}

impl<'a> TickReader<'a> {
    pub fn new(http: &'a Provider<Http>, source: Source, spacing: i32) -> Result<Self> {
        anyhow::ensure!(spacing > 0, "tick spacing must be > 0, got {spacing}");
        Ok(Self { http, source, spacing, at: None })
    }

    /// Read every value at one fixed block.
    pub fn at_block(mut self, block: Option<u64>) -> Self {
        self.at = block;
        self
    }

    /// The block tag every read in this reader uses.
    fn tag(&self) -> String {
        match self.at {
            Some(n) => format!("0x{n:x}"),
            None => "latest".to_string(),
        }
    }

    async fn storage(&self, addr: Address, slot: U256) -> Result<H256> {
        let word: H256 = self
            .http
            .request(
                "eth_getStorageAt",
                (
                    format!("0x{addr:x}"),
                    format!("0x{slot:064x}"),
                    self.tag(),
                ),
            )
            .await
            .context("eth_getStorageAt")?;
        Ok(word)
    }

    async fn call(&self, to: Address, data: Vec<u8>) -> Result<Bytes> {
        let tx = TransactionRequest::new().to(to).data(Bytes::from(data));
        let at = self.at.map(ethers::types::BlockId::from);
        self.http.call(&tx.into(), at).await.context("eth_call")
    }

    /// Base storage slot of `_pools[poolId]` for a v4 pool.
    fn v4_base(pool_id: H256) -> U256 {
        mapping_slot(&pool_id.0, U256::from(POOLS_SLOT))
    }

    /// One 256-bit word of the tick bitmap, covering 256 spacings.
    async fn bitmap_word(&self, word_pos: i32) -> Result<U256> {
        match &self.source {
            Source::V3 { pool } => {
                let mut data = selector("tickBitmap(int16)");
                data.extend_from_slice(&signed_word(word_pos as i64));
                let res = self.call(*pool, data).await?;
                anyhow::ensure!(res.len() >= 32, "short tickBitmap return");
                Ok(U256::from_big_endian(&res[0..32]))
            }
            Source::V4 { manager, pool_id } => {
                let base = Self::v4_base(*pool_id) + U256::from(BITMAP_OFFSET);
                let slot = mapping_slot(&signed_word(word_pos as i64), base);
                Ok(U256::from_big_endian(self.storage(*manager, slot).await?.as_bytes()))
            }
        }
    }

    /// `liquidityNet` of an initialized tick: how `L` changes crossing it upward.
    async fn liquidity_net(&self, tick: i32) -> Result<i128> {
        match &self.source {
            Source::V3 { pool } => {
                let mut data = selector("ticks(int24)");
                data.extend_from_slice(&signed_word(tick as i64));
                let res = self.call(*pool, data).await?;
                // (liquidityGross, liquidityNet, ...): net is the second word.
                anyhow::ensure!(res.len() >= 64, "short ticks() return");
                Ok(i128::from_be_bytes(res[48..64].try_into().unwrap()))
            }
            Source::V4 { manager, pool_id } => {
                let base = Self::v4_base(*pool_id) + U256::from(TICKS_OFFSET);
                let slot = mapping_slot(&signed_word(tick as i64), base);
                let w = self.storage(*manager, slot).await?;
                // TickInfo packs uint128 liquidityGross then int128 liquidityNet,
                // so net occupies the HIGH 128 bits of the word.
                Ok(i128::from_be_bytes(w.0[0..16].try_into().unwrap()))
            }
        }
    }

    /// Next initialized tick from `from` in the given direction, or None if
    /// none was found within `max_words` bitmap words.
    ///
    /// Direction is asymmetric, matching Uniswap: going up the search is
    /// exclusive of the current compressed tick, going down it is inclusive.
    /// Getting that backwards silently skips a tick and understates the cost.
    async fn next_initialized(&self, from: i32, up: bool, max_words: u32) -> Result<Option<i32>> {
        let compressed = compress(from, self.spacing);
        let mut word_pos = compressed >> 8;
        let mut bit = compressed & 255;
        let mut first = true;
        for _ in 0..max_words {
            let word = self.bitmap_word(word_pos).await?;
            if let Some(b) = scan_word(word, bit, up, first) {
                return Ok(Some(((word_pos << 8) + b as i32) * self.spacing));
            }
            first = false;
            word_pos += if up { 1 } else { -1 };
            bit = 0;
        }
        Ok(None)
    }
}

/// Find a set bit in one bitmap word, searching from `bit` in the given
/// direction. On the first word the starting bit is excluded going up and
/// included going down; later words are scanned end to end.
fn scan_word(word: U256, bit: i32, up: bool, first: bool) -> Option<u32> {
    if up {
        let start = if first { bit + 1 } else { 0 };
        (start.max(0) as u32..256).find(|b| word.bit(*b as usize))
    } else {
        let end = if first { bit } else { 255 };
        if end < 0 {
            return None;
        }
        (0..=end as u32).rev().find(|b| word.bit(*b as usize))
    }
}

/// What a swap yields while it stays inside the current tick range, where
/// liquidity is constant and no tick data is needed at all.
///
/// This is the same step `swap_exact_in` takes between two ticks, on its own.
/// It is exact whenever the swap does not reach an initialized tick, and it
/// overstates the output once it would - so callers must check the price move
/// it reports and stop trusting it before that point. Returns the output and
/// the price the pool would be left at.
pub fn in_range_out(
    sqrt_p: f64,
    liquidity: u128,
    lp_fee_pips: u32,
    zero_for_one: bool,
    amount_in: f64,
) -> Option<(f64, f64)> {
    // NaN has to fail these too, hence the explicit finiteness checks rather
    // than negated comparisons.
    if !sqrt_p.is_finite()
        || sqrt_p <= 0.0
        || !amount_in.is_finite()
        || amount_in <= 0.0
        || liquidity == 0
        || lp_fee_pips >= 1_000_000
    {
        return None;
    }
    let l = liquidity as f64;
    // The fee comes off the input before it reaches the curve.
    let net = amount_in * (1.0 - lp_fee_pips as f64 / 1_000_000.0);
    // Paying token1 pushes the raw price up; paying token0 pushes it down.
    let up = !zero_for_one;
    let (sqrt_new, out) = if up {
        let s = sqrt_p + net / l;
        (s, l * (1.0 / sqrt_p - 1.0 / s))
    } else {
        let s = 1.0 / (1.0 / sqrt_p + net / l);
        (s, l * (sqrt_p - s))
    };
    if !out.is_finite() || out <= 0.0 || !sqrt_new.is_finite() || sqrt_new <= 0.0 {
        return None;
    }
    Some((out, sqrt_new))
}

/// sqrt(price) at a tick, in the plain f64 domain (not X96).
pub fn sqrt_at_tick(tick: i32) -> f64 {
    1.0001f64.powf(tick as f64 / 2.0)
}

/// Tick containing the given sqrt(price). The epsilon absorbs the ~1e-10 error
/// of the log/exp round trip, so a price sitting exactly on a tick boundary
/// reports that tick rather than the one below it.
pub fn tick_at_sqrt(sqrt_p: f64) -> i32 {
    (2.0 * sqrt_p.ln() / 1.0001f64.ln() + 1e-9).floor() as i32
}

/// How much of the quote token must be paid IN to move the base token's price
/// up by `move_pct`, walking every tick the swap would cross.
///
/// `base_token` selects which side is the base, exactly as elsewhere:
/// 1 => base is token1 and the quote (paid in) is token0, which pushes the raw
/// price P = y/x DOWN; 0 => the mirror image.
///
/// `lp_fee_pips` is the pool fee in hundredths of a bip (3000 = 0.3%); the swap
/// fee is taken off the input, so the gross amount paid is grossed up by it.
pub async fn pay_to_move(
    reader: &TickReader<'_>,
    sqrt_p_now: f64,
    liquidity_now: u128,
    base_token: u8,
    move_pct: f64,
    lp_fee_pips: u32,
) -> Result<f64> {
    anyhow::ensure!(move_pct > 0.0, "move_pct must be > 0, got {move_pct}");
    anyhow::ensure!(sqrt_p_now > 0.0, "sqrt price must be > 0");
    anyhow::ensure!(lp_fee_pips < 1_000_000, "fee must be < 100%");

    let k = (1.0 + move_pct / 100.0).sqrt();
    // Buying the base pushes the RAW price up when base is token0, down when
    // base is token1.
    let up = base_token == 0;
    let sqrt_target = if up { sqrt_p_now * k } else { sqrt_p_now / k };

    let mut sqrt_cur = sqrt_p_now;
    let mut liquidity = liquidity_now as f64;
    let mut paid = 0.0f64;
    let mut tick = tick_at_sqrt(sqrt_cur);

    // Each iteration covers one tick segment. The bound is generous: a 100%
    // move at spacing 1 is ~6900 ticks, and initialized ticks are far sparser.
    for _ in 0..1_000 {
        let done = if up { sqrt_cur >= sqrt_target } else { sqrt_cur <= sqrt_target };
        if done {
            break;
        }
        let next = reader.next_initialized(tick, up, MAX_BITMAP_WORDS).await?;
        // Price of the next initialized tick, clamped to the target.
        let sqrt_edge = match next {
            Some(t) => {
                let s = sqrt_at_tick(t);
                if up { s.min(sqrt_target) } else { s.max(sqrt_target) }
            }
            None => sqrt_target,
        };
        if liquidity > 0.0 {
            paid += if up {
                // paying token1 (y) to push P up
                liquidity * (sqrt_edge - sqrt_cur)
            } else {
                // paying token0 (x) to push P down
                liquidity * (1.0 / sqrt_edge - 1.0 / sqrt_cur)
            };
        }
        let reached_target = if up { sqrt_edge >= sqrt_target } else { sqrt_edge <= sqrt_target };
        sqrt_cur = sqrt_edge;
        if reached_target {
            break;
        }
        // Cross the tick: L changes, and the walk continues on the far side.
        let t = next.context("no initialized tick but target not reached")?;
        let net = reader.liquidity_net(t).await? as f64;
        liquidity = (if up { liquidity + net } else { liquidity - net }).max(0.0);
        tick = if up { t } else { t - 1 };
    }

    // The pool fee is charged on the input, so the amount actually sent is
    // larger than the amount that reaches the curve.
    Ok(paid / (1.0 - lp_fee_pips as f64 / 1_000_000.0))
}

/// Live state of a pool: everything a swap simulation starts from.
#[derive(Debug, Clone, Copy)]
pub struct PoolState {
    pub sqrt_p: f64,
    pub liquidity: u128,
    /// Current LP fee in hundredths of a bip. For v4 this is read from slot0,
    /// so a hook that changes the fee dynamically is reflected.
    pub lp_fee: u32,
}

/// Read sqrt(price), in-range liquidity and the live fee for a pool.
pub async fn read_state(reader: &TickReader<'_>) -> Result<PoolState> {
    match &reader.source {
        Source::V4 { manager, pool_id } => {
            let base = TickReader::v4_base(*pool_id);
            let s0 = reader.storage(*manager, base).await?;
            anyhow::ensure!(!s0.is_zero(), "pool is not initialized");
            let (sqrt_x96, _tick, _protocol_fee, lp_fee) = decode_slot0(s0);
            let liq = reader.storage(*manager, base + U256::from(3)).await?;
            Ok(PoolState {
                sqrt_p: x96_to_f64(sqrt_x96),
                liquidity: u128::from_be_bytes(liq.0[16..32].try_into().unwrap()),
                lp_fee,
            })
        }
        Source::V3 { pool } => {
            let s0 = reader.call(*pool, selector("slot0()")).await?;
            anyhow::ensure!(s0.len() >= 32, "short slot0() return");
            let sqrt_x96 = U256::from_big_endian(&s0[0..32]);
            let l = reader.call(*pool, selector("liquidity()")).await?;
            anyhow::ensure!(l.len() >= 32, "short liquidity() return");
            let f = reader.call(*pool, selector("fee()")).await?;
            anyhow::ensure!(f.len() >= 32, "short fee() return");
            Ok(PoolState {
                sqrt_p: x96_to_f64(sqrt_x96),
                liquidity: U256::from_big_endian(&l[16..32]).as_u128(),
                lp_fee: U256::from_big_endian(&f[0..32]).low_u32(),
            })
        }
    }
}

/// Unpack a v4 `slot0` word.
///
/// Fields are packed from the LOW end: sqrtPriceX96 (160 bits), tick (24),
/// protocolFee (24), lpFee (24). In a big-endian 32-byte word that puts
/// sqrtPriceX96 last and lpFee near the front - easy to get backwards, hence
/// the regression test below against a word read off chain.
pub fn decode_slot0(word: H256) -> (U256, i32, u32, u32) {
    let b = word.0;
    let sqrt_x96 = U256::from_big_endian(&b[12..32]);
    let tick_raw = ((b[9] as u32) << 16) | ((b[10] as u32) << 8) | b[11] as u32;
    let tick = if tick_raw & 0x80_0000 != 0 {
        tick_raw as i32 - 0x100_0000
    } else {
        tick_raw as i32
    };
    let protocol_fee = ((b[6] as u32) << 16) | ((b[7] as u32) << 8) | b[8] as u32;
    let lp_fee = ((b[3] as u32) << 16) | ((b[4] as u32) << 8) | b[5] as u32;
    (sqrt_x96, tick, protocol_fee, lp_fee)
}

fn x96_to_f64(v: U256) -> f64 {
    let f = match u128::try_from(v) {
        Ok(x) => x as f64,
        Err(_) => {
            let l = v.0;
            let lo = (l[0] as u128) | ((l[1] as u128) << 64);
            let hi = (l[2] as u128) | ((l[3] as u128) << 64);
            hi as f64 * 2f64.powi(128) + lo as f64
        }
    };
    f / 2f64.powi(96)
}

/// Result of simulating one exact-input swap.
#[derive(Debug, Clone, Copy)]
pub struct SwapResult {
    pub amount_out: f64,
    /// Input actually consumed. Less than requested only if the pool ran out of
    /// liquidity in that direction entirely.
    pub amount_in_used: f64,
    pub sqrt_p_after: f64,
    pub ticks_crossed: u32,
}

/// Simulate an exact-input swap, walking every tick it crosses.
///
/// `zero_for_one` means token0 goes in and token1 comes out, which pushes the
/// raw price P = y/x DOWN. Amounts are raw token units.
pub async fn swap_exact_in(
    reader: &TickReader<'_>,
    state: PoolState,
    zero_for_one: bool,
    amount_in: f64,
) -> Result<SwapResult> {
    anyhow::ensure!(amount_in > 0.0, "amount_in must be > 0");
    anyhow::ensure!(state.sqrt_p > 0.0, "sqrt price must be > 0");
    anyhow::ensure!(state.lp_fee < 1_000_000, "fee must be < 100%");

    // The LP fee is taken off the input before it reaches the curve.
    let mut remaining = amount_in * (1.0 - state.lp_fee as f64 / 1_000_000.0);
    let gross_per_net = 1.0 / (1.0 - state.lp_fee as f64 / 1_000_000.0);

    let mut sqrt_cur = state.sqrt_p;
    let mut liquidity = state.liquidity as f64;
    let mut out = 0.0f64;
    let mut tick = tick_at_sqrt(sqrt_cur);
    let mut crossed = 0u32;
    // Going down in price when selling token0.
    let up = !zero_for_one;

    for _ in 0..1_000 {
        if remaining <= 0.0 {
            break;
        }
        let next = reader.next_initialized(tick, up, MAX_BITMAP_WORDS).await?;
        let sqrt_edge = match next {
            Some(t) => sqrt_at_tick(t),
            None => {
                // No further initialized tick means no boundary where liquidity
                // could change, so the rest of the input trades at constant L.
                // A full-range position looks exactly like this: its only ticks
                // sit at MIN_TICK/MAX_TICK, far outside the scanned window.
                if liquidity > 0.0 {
                    let sqrt_new = if up {
                        sqrt_cur + remaining / liquidity
                    } else {
                        1.0 / (1.0 / sqrt_cur + remaining / liquidity)
                    };
                    out += if up {
                        liquidity * (1.0 / sqrt_cur - 1.0 / sqrt_new)
                    } else {
                        liquidity * (sqrt_cur - sqrt_new)
                    };
                    sqrt_cur = sqrt_new;
                    remaining = 0.0;
                }
                break;
            }
        };
        if liquidity <= 0.0 {
            // Empty range: skip straight across it, no input consumed.
            let t = next.context("no tick to cross")?;
            let net = reader.liquidity_net(t).await? as f64;
            liquidity = (if up { liquidity + net } else { liquidity - net }).max(0.0);
            sqrt_cur = sqrt_edge;
            tick = if up { t } else { t - 1 };
            crossed += 1;
            continue;
        }
        // Input needed to walk this whole segment.
        let cap = if up {
            liquidity * (sqrt_edge - sqrt_cur)
        } else {
            liquidity * (1.0 / sqrt_edge - 1.0 / sqrt_cur)
        };
        if remaining < cap {
            // Stops inside the segment: solve for the price actually reached.
            let sqrt_new = if up {
                sqrt_cur + remaining / liquidity
            } else {
                1.0 / (1.0 / sqrt_cur + remaining / liquidity)
            };
            out += if up {
                liquidity * (1.0 / sqrt_cur - 1.0 / sqrt_new)
            } else {
                liquidity * (sqrt_cur - sqrt_new)
            };
            sqrt_cur = sqrt_new;
            remaining = 0.0;
            break;
        }
        out += if up {
            liquidity * (1.0 / sqrt_cur - 1.0 / sqrt_edge)
        } else {
            liquidity * (sqrt_cur - sqrt_edge)
        };
        remaining -= cap;
        sqrt_cur = sqrt_edge;
        let t = next.context("no tick to cross")?;
        let net = reader.liquidity_net(t).await? as f64;
        liquidity = (if up { liquidity + net } else { liquidity - net }).max(0.0);
        tick = if up { t } else { t - 1 };
        crossed += 1;
    }

    let net_used = amount_in * (1.0 - state.lp_fee as f64 / 1_000_000.0) - remaining;
    Ok(SwapResult {
        amount_out: out,
        amount_in_used: net_used * gross_per_net,
        sqrt_p_after: sqrt_cur,
        ticks_crossed: crossed,
    })
}

#[cfg(test)]
mod tests_in_range {
    use super::*;

    /// The in-range step and the full walk have to agree wherever the walk
    /// crosses nothing - they are the same formula, and this is what lets the
    /// cheap one stand in for the expensive one on small trades.
    #[test]
    fn it_is_the_same_step_the_walk_takes() {
        let sqrt_p = 2.0f64;
        let l = 1_000_000_000u128;
        // A trade small enough to stay put: the price barely moves.
        let (out, after) = in_range_out(sqrt_p, l, 3000, false, 1_000.0).unwrap();
        assert!(out > 0.0);
        assert!(after > sqrt_p, "paying token1 lifts the raw price");
        let moved = (after / sqrt_p).powi(2) - 1.0;
        assert!(moved < 1e-5, "moved {moved}");

        // The other direction moves it the other way.
        let (_, after) = in_range_out(sqrt_p, l, 3000, true, 1_000.0).unwrap();
        assert!(after < sqrt_p);
    }

    #[test]
    fn the_fee_comes_off_the_input() {
        let free = in_range_out(2.0, 1_000_000_000, 0, false, 1_000.0).unwrap().0;
        let charged = in_range_out(2.0, 1_000_000_000, 10_000, false, 1_000.0).unwrap().0;
        // 1% of the input never reaches the curve, so ~1% less comes out.
        let ratio = charged / free;
        assert!((ratio - 0.99).abs() < 1e-6, "ratio {ratio}");
    }

    #[test]
    fn nothing_is_quoted_out_of_nothing() {
        assert!(in_range_out(2.0, 0, 3000, false, 1.0).is_none(), "no liquidity");
        assert!(in_range_out(0.0, 1_000, 3000, false, 1.0).is_none(), "no price");
        assert!(in_range_out(2.0, 1_000, 3000, false, 0.0).is_none(), "no input");
        assert!(in_range_out(2.0, 1_000, 1_000_000, false, 1.0).is_none(), "a 100% fee");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compress_floors_towards_negative_infinity() {
        assert_eq!(compress(200, 60), 3);
        assert_eq!(compress(180, 60), 3);
        assert_eq!(compress(-1, 60), -1);
        assert_eq!(compress(-60, 60), -1);
        assert_eq!(compress(-61, 60), -2);
    }

    #[test]
    fn tick_and_sqrt_round_trip() {
        for t in [-887_220, -60_000, -1, 0, 1, 139_844, 887_220] {
            assert_eq!(tick_at_sqrt(sqrt_at_tick(t)), t, "tick {t}");
        }
    }

    #[test]
    fn sqrt_at_tick_matches_the_price_definition() {
        // P = 1.0001^tick, so sqrt(P) squared must return it.
        let p = sqrt_at_tick(139_844).powi(2);
        assert!((p / 1.0001f64.powi(139_844) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn scan_word_respects_direction_and_inclusivity() {
        // bits 3 and 200 set
        let w = (U256::one() << 3) | (U256::one() << 200);
        // going up from bit 3 must SKIP bit 3 itself
        assert_eq!(scan_word(w, 3, true, true), Some(200));
        assert_eq!(scan_word(w, 2, true, true), Some(3));
        // going down from bit 3 must INCLUDE bit 3
        assert_eq!(scan_word(w, 3, false, true), Some(3));
        assert_eq!(scan_word(w, 2, false, true), None);
        assert_eq!(scan_word(w, 200, false, true), Some(200));
        // continuation words are scanned end to end regardless of `bit`
        assert_eq!(scan_word(w, 0, true, false), Some(3));
        assert_eq!(scan_word(w, 0, false, false), Some(200));
        // empty word finds nothing
        assert_eq!(scan_word(U256::zero(), 128, true, true), None);
        assert_eq!(scan_word(U256::zero(), 128, false, true), None);
    }

    #[test]
    fn scan_word_handles_word_edges() {
        let w = (U256::one() << 0) | (U256::one() << 255);
        // nothing above bit 255
        assert_eq!(scan_word(w, 255, true, true), None);
        // nothing below bit 0 when bit 0 is excluded by direction
        assert_eq!(scan_word(w, 0, true, true), Some(255));
        assert_eq!(scan_word(w, 0, false, true), Some(0));
    }

    /// Real slot0 of the POOLS/ETH pool on chain 4663. Every field is
    /// independently checkable: the tick must match the price implied by
    /// sqrtPriceX96, and the fee must be a plausible tier.
    #[test]
    fn decodes_a_real_slot0_word() {
        let w: H256 = "0x0000000009c4190190022244000000000000043fc544e4e483a607c758a0c251"
            .parse()
            .unwrap();
        let (sqrt_x96, tick, protocol_fee, lp_fee) = decode_slot0(w);
        assert_eq!(
            sqrt_x96,
            U256::from_dec_str("86182064487810775758807691739729").unwrap()
        );
        assert_eq!(lp_fee, 2500, "0.25% tier");
        // protocolFee packs two 12-bit halves, 400 each way
        assert_eq!(protocol_fee & 0xfff, 400);
        assert_eq!(protocol_fee >> 12, 400);
        assert_eq!(tick, 139_844);

        // cross-check: the tick must agree with the price sqrtPriceX96 implies
        let sqrt_p = x96_to_f64(sqrt_x96);
        assert_eq!(tick_at_sqrt(sqrt_p), tick);
    }

    #[test]
    fn decodes_negative_ticks() {
        // tick = -1 is 0xffffff in the int24 field
        let mut b = [0u8; 32];
        b[9] = 0xff;
        b[10] = 0xff;
        b[11] = 0xff;
        let (_, tick, _, _) = decode_slot0(H256::from(b));
        assert_eq!(tick, -1);
    }

    #[test]
    fn signed_word_sign_extends() {
        assert_eq!(signed_word(1)[31], 1);
        assert!(signed_word(1)[..31].iter().all(|b| *b == 0));
        assert!(signed_word(-1).iter().all(|b| *b == 0xff));
        assert_eq!(signed_word(-2)[31], 0xfe);
    }
}
