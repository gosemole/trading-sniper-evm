use crate::autobuy::AutoBuy;
use crate::pool::{v3_swap_topic, v4_swap_topic, Pool};
use crate::price;
use anyhow::{Context, Result};
use crate::depth;
use ethers::providers::{Http, Middleware, Provider, Ws};
use ethers::types::{Filter, Log, U256};
use futures_util::StreamExt;
use tracing::{debug, info, warn};

/// A decoded swap: everything the meter and the depth estimate need, all taken
/// from the same log so they describe the same instant.
struct Swap {
    block: u64,
    sqrt: U256,
    liquidity: u128,
    price: f64,
}

/// Fired signal data for a big sell.
pub struct Signal {
    pub block: u64,
    pub sqrt: U256,
    /// In-range liquidity as reported by the swap that triggered the signal.
    pub liquidity: u128,
    /// Price the drop is measured from (close of the previous block).
    pub reference: f64,
    pub price: f64,
    pub drop_pct: f64,
}

/// Detects a large SELL of the base token: price drops by >= threshold% against
/// the previous block's close, where the whole drop happens within a single block.
///
/// Using the last swap-bearing block's close as the reference is exact, not an
/// approximation: `sqrtPriceX96` only moves on swaps, so during a run of blocks
/// without swaps the price is unchanged and that close *is* the previous
/// block's close.
pub struct BigSellMeter {
    threshold_pct: f64,
    cur_block: Option<u64>,
    /// Reference price: close of the previous block (drop measured from here).
    reference: Option<f64>,
    /// Most recent observed price (running close of the current block).
    price: Option<f64>,
    /// Most recent observed raw sqrtPriceX96 (for the depth quote).
    sqrt: Option<U256>,
    /// In-range liquidity reported by the most recent swap.
    liquidity: u128,
    /// Whether a signal already fired within the current block (once per block).
    fired: bool,
}

impl BigSellMeter {
    pub fn new(threshold_pct: f64) -> Self {
        Self {
            threshold_pct,
            cur_block: None,
            reference: None,
            price: None,
            sqrt: None,
            liquidity: 0,
            fired: false,
        }
    }

    /// Feed a swap. Returns a Signal when a BIG SELL fired.
    fn observe(&mut self, s: &Swap) -> Option<Signal> {
        match self.cur_block {
            Some(b) if b == s.block => {
                self.record(s);
                self.try_fire()
            }
            Some(b) if s.block > b => {
                // New block: the previous block's close becomes the reference.
                self.reference = self.price;
                self.cur_block = Some(s.block);
                self.fired = false;
                self.record(s);
                self.try_fire()
            }
            // First swap, or the block went backwards (reorg / out-of-order):
            // (re)seed the reference and wait for the next swap.
            _ => {
                self.cur_block = Some(s.block);
                self.reference = Some(s.price);
                self.fired = false;
                self.record(s);
                None
            }
        }
    }

    fn record(&mut self, s: &Swap) {
        self.price = Some(s.price);
        self.sqrt = Some(s.sqrt);
        self.liquidity = s.liquidity;
    }

    fn try_fire(&mut self) -> Option<Signal> {
        if self.fired {
            return None;
        }
        let reference = self.reference?;
        let price = self.price?;
        let sqrt = self.sqrt?;
        if reference <= 0.0 {
            return None;
        }
        let change_pct = (price / reference - 1.0) * 100.0;
        if change_pct > -self.threshold_pct {
            return None;
        }
        self.fired = true;
        Some(Signal {
            block: self.cur_block.unwrap_or(0),
            sqrt,
            liquidity: self.liquidity,
            reference,
            price,
            drop_pct: -change_pct,
        })
    }
}

/// Run the WS subscription for a single pool (or PoolManager for v4).
pub async fn run_pool(
    pool: Pool,
    http: Provider<Http>,
    threshold_pct: f64,
    max_move_pct: f64,
    ws_url: String,
    auto: Option<std::sync::Arc<AutoBuy>>,
) -> Result<()> {
    let provider = Provider::<Ws>::connect(&ws_url)
        .await
        .context("connect ws")?;
    let topic = match pool.version.as_str() {
        "v4" => v4_swap_topic(),
        _ => v3_swap_topic(),
    };

    let mut filter = Filter::new().address(pool.filter_addresses()).topic0(topic);
    // v4: PoolManager emits swaps for all pools; filter by indexed poolId (topic1).
    if let Some(pid) = pool.pool_id {
        filter = filter.topic1(ethers::types::ValueOrArray::Value(pid));
    }
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe logs")?;

    let mut meter = BigSellMeter::new(threshold_pct);
    let quote_scale = 10f64.powi(pool.quote_decimals() as i32);
    let quote_unit = pool.quote_symbol.clone().unwrap_or_else(|| "quote".into());
    info!(
        pool = %pool.name, version = %pool.version, addr = ?pool.address,
        threshold_pct, max_move_pct, quote = %quote_unit,
        "subscribed"
    );

    while let Some(log) = stream.next().await {
        // A reorged-out log must not be fed to the meter as a real swap.
        if log.removed.unwrap_or(false) {
            debug!(pool = %pool.name, block = ?log.block_number, "skipping removed log");
            continue;
        }
        // Defensive: the subscription already filters on topic1 for v4.
        if let Some(pid) = pool.pool_id {
            if log.topics.get(1) != Some(&pid) {
                debug!(pool = %pool.name, "skipping log for another poolId");
                continue;
            }
        }
        let swap = match decode_swap(&pool, &log) {
            Ok(s) => s,
            Err(e) => {
                warn!(pool = %pool.name, err = %e, "failed to decode swap log");
                continue;
            }
        };
        if let Some(sig) = meter.observe(&swap) {
            let (pay, method) = estimate_depth(&pool, &http, &sig, max_move_pct).await;
            let depth = match pay {
                Some(v) => format!("{:.4} {quote_unit}", v / quote_scale),
                None => format!("? {quote_unit}"),
            };
            emit_signal(&pool, &sig, max_move_pct, &depth, method);

            // Buying blocks the stream for a few seconds. That is on purpose:
            // the logs queue up behind it, and a second drop cannot start a
            // second buy while the first is still being signed.
            if let Some(auto) = &auto {
                if let Err(e) = auto.on_drop(&pool, sig.drop_pct).await {
                    warn!(pool = %pool.name, err = %format!("{e:#}"), "auto-buy failed");
                }
            }
        }
    }
    warn!(pool = %pool.name, "stream ended");
    Ok(())
}

/// Cost to move the price, preferring the tick walk and degrading to the
/// in-range approximation when tick state cannot be read.
///
/// The two differ a lot on thin pools: the in-range figure assumes the whole
/// move happens at the current liquidity and ignores the swap fee entirely.
async fn estimate_depth(
    pool: &Pool,
    http: &Provider<Http>,
    sig: &Signal,
    max_move_pct: f64,
) -> (Option<f64>, &'static str) {
    let sqrt_p = crate::pool::sqrt_to_f64(sig.sqrt);
    if let (Some(spacing), Some(source)) = (pool.tick_spacing, pool.tick_source()) {
        match depth::TickReader::new(http, source, spacing) {
            Ok(reader) => {
                let fee = pool.lp_fee.unwrap_or(0);
                match depth::pay_to_move(
                    &reader,
                    sqrt_p,
                    sig.liquidity,
                    pool.base_token,
                    max_move_pct,
                    fee,
                )
                .await
                {
                    Ok(v) => return (Some(v), "ticks"),
                    Err(e) => warn!(
                        pool = %pool.name, err = %e,
                        "tick walk failed, falling back to in-range estimate"
                    ),
                }
            }
            Err(e) => warn!(pool = %pool.name, err = %e, "bad tick spacing"),
        }
    }
    match pool.quote_pay(sig.liquidity, sig.sqrt, max_move_pct) {
        Ok(v) => (Some(v), "in-range"),
        Err(e) => {
            warn!(pool = %pool.name, err = %e, "depth quote failed");
            (None, "none")
        }
    }
}

fn emit_signal(pool: &Pool, sig: &Signal, max_move_pct: f64, depth: &str, method: &str) {
    // Absolute prices are only meaningful once decimals are known; the drop
    // percentage is valid either way.
    let (from, to) = if pool.decimals_known {
        (format!("{:.8}", sig.reference), format!("{:.8}", sig.price))
    } else {
        ("?".to_string(), "?".to_string())
    };
    info!(
        pool = %pool.name,
        block = sig.block,
        drop_pct = format!("-{:.3}%", sig.drop_pct),
        from = %from,
        to = %to,
        liquidity = sig.liquidity,
        pay_to_move = format!("{depth} / +{max_move_pct}%"),
        method,
        "BIG SELL"
    );
}

fn decode_swap(pool: &Pool, log: &Log) -> Result<Swap> {
    let block = log
        .block_number
        .context("log missing block number")?
        .as_u64();
    let data = &log.data.0;
    // Both v3 and v4 Swap lay out the non-indexed args as
    // [amount0][amount1][sqrtPriceX96][liquidity][tick]... so the offsets below
    // hold for either version.
    anyhow::ensure!(data.len() >= 128, "log data too short: {} bytes", data.len());
    let sqrt = U256::from_big_endian(&data[64..96]);
    // liquidity is uint128: low 16 bytes of its 32-byte word.
    let liquidity = u128::from_be_bytes(data[112..128].try_into().unwrap());
    let price = price::display_price(sqrt, pool.decimals0, pool.decimals1, pool.base_token);
    anyhow::ensure!(price.is_finite() && price > 0.0, "non-finite price from sqrt");
    Ok(Swap {
        block,
        sqrt,
        liquidity,
        price,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swap(block: u64, p: f64) -> Swap {
        Swap {
            block,
            // sqrtPriceX96 = sqrt(p) << 96
            sqrt: U256::from((p.sqrt() * 2f64.powi(96)) as u128),
            liquidity: 1_000,
            price: p,
        }
    }

    #[test]
    fn fires_once_within_single_block() {
        let mut m = BigSellMeter::new(0.5);
        assert!(m.observe(&swap(1, 1.0)).is_none());
        assert!(m.observe(&swap(1, 1.0)).is_none());
        // -1% in a new block -> fire
        assert!(
            m.observe(&swap(2, 0.99)).is_some(),
            "should fire on -1% within block 2"
        );
        // no repeat in same block
        assert!(m.observe(&swap(2, 0.98)).is_none());
        // new block big drop fires again
        assert!(m.observe(&swap(3, 0.97)).is_some());
    }

    #[test]
    fn ignores_up_moves_and_small_drops() {
        let mut m = BigSellMeter::new(1.0);
        assert!(m.observe(&swap(1, 100.0)).is_none());
        assert!(m.observe(&swap(2, 101.0)).is_none()); // up
        assert!(m.observe(&swap(2, 99.5)).is_none()); // 0.5% < 1%
        assert!(m.observe(&swap(3, 98.0)).is_some()); // big drop
    }

    #[test]
    fn quiet_blocks_do_not_stack_into_a_false_signal() {
        // Price bleeds 2% per swap-bearing block with long gaps in between.
        // Each step is below the 5% threshold, so nothing should fire.
        let mut m = BigSellMeter::new(5.0);
        assert!(m.observe(&swap(100, 1.00)).is_none());
        assert!(m.observe(&swap(4_000, 0.98)).is_none());
        assert!(m.observe(&swap(9_000, 0.96)).is_none());
        assert!(m.observe(&swap(20_000, 0.94)).is_none());
        // ...but a single-block 6% drop after a long quiet stretch does fire.
        assert!(m.observe(&swap(50_000, 0.88)).is_some());
    }

    #[test]
    fn signal_carries_the_swaps_own_liquidity() {
        let mut m = BigSellMeter::new(1.0);
        m.observe(&swap(1, 100.0));
        let mut s2 = swap(2, 90.0);
        s2.liquidity = 42;
        let sig = m.observe(&s2).expect("should fire");
        assert_eq!(sig.liquidity, 42);
        assert_eq!(sig.reference, 100.0);
        assert_eq!(sig.price, 90.0);
        assert!((sig.drop_pct - 10.0).abs() < 1e-9);
    }

    #[test]
    fn reorg_backwards_reseeds_instead_of_firing() {
        let mut m = BigSellMeter::new(1.0);
        m.observe(&swap(10, 100.0));
        m.observe(&swap(11, 100.0));
        // block goes backwards: reseed, do not fire on the apparent drop
        assert!(m.observe(&swap(9, 50.0)).is_none());
    }
}
