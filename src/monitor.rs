use crate::pool::{v3_swap_topic, v4_swap_topic, Pool};
use crate::price;
use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider, Ws};
use ethers::types::{Filter, Log, U256};
use futures_util::StreamExt;
use tracing::{info, warn};

/// Fired signal data for a big sell.
pub struct Signal {
    pub block: u64,
    pub sqrt: U256,
    pub drop_pct: f64,
}

/// Detects a large SELL of the base token: price drops by >= threshold% against
/// the previous block's close, where the whole drop happens within a single block.
pub struct BigSellMeter {
    threshold: f64, // as a fraction (e.g. 0.005)
    cur_block: Option<u64>,
    /// Reference price: close of the previous block (drop measured from here).
    reference: Option<f64>,
    /// Most recent observed price (running close of the current block).
    price: Option<f64>,
    /// Most recent observed raw sqrtPriceX96 (for the depth quote).
    sqrt: Option<U256>,
    /// Whether a signal already fired within the current block (once per block).
    fired: bool,
}

impl BigSellMeter {
    pub fn new(threshold_pct: f64) -> Self {
        Self {
            threshold: threshold_pct / 100.0,
            cur_block: None,
            reference: None,
            price: None,
            sqrt: None,
            fired: false,
        }
    }

    /// Feed a swap (block, raw sqrt, displayed price). Returns a Signal when a
    /// BIG SELL fired.
    pub fn observe(&mut self, block: u64, sqrt: U256, price: f64) -> Option<Signal> {
        match self.cur_block {
            None => {
                self.cur_block = Some(block);
                self.reference = Some(price);
                self.price = Some(price);
                self.sqrt = Some(sqrt);
                self.fired = false;
                None
            }
            Some(b) if b == block => {
                self.price = Some(price);
                self.sqrt = Some(sqrt);
                self.try_fire()
            }
            Some(b) if block > b => {
                self.reference = self.price;
                self.cur_block = Some(block);
                self.fired = false;
                self.price = Some(price);
                self.sqrt = Some(sqrt);
                self.try_fire()
            }
            Some(_b) => {
                // block went backwards (reorg / out-of-order): reset
                self.cur_block = Some(block);
                self.reference = Some(price);
                self.price = Some(price);
                self.sqrt = Some(sqrt);
                self.fired = false;
                None
            }
        }
    }

    fn try_fire(&mut self) -> Option<Signal> {
        if self.fired {
            return None;
        }
        let sig = self.check();
        if sig.is_some() {
            self.fired = true;
        }
        sig
    }

    fn check(&self) -> Option<Signal> {
        if self.fired {
            return None;
        }
        let refp = self.reference?;
        let price = self.price?;
        let sqrt = self.sqrt?;
        if refp > 0.0 {
            let change_pct = (price / refp - 1.0) * 100.0;
            if change_pct <= -self.threshold * 100.0 {
                return Some(Signal {
                    block: self.cur_block.unwrap_or(0),
                    sqrt,
                    drop_pct: -change_pct,
                });
            }
        }
        None
    }
}

/// Run the WS subscription for a single pool (or PoolManager for v4).
pub async fn run_pool(
    pool: Pool,
    http: Provider<Http>,
    threshold_pct: f64,
    max_move_pct: f64,
    ws_url: String,
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
    info!(pool = %pool.name, version = %pool.version, addr = ?pool.address, "subscribed");

    while let Some(log) = stream.next().await {
        match decode_swap(&pool, log) {
            Ok((block, sqrt, price)) => {
                if let Some(sig) = meter.observe(block, sqrt, price) {
                    let depth = match pool.quote_pay(&http, sig.sqrt, max_move_pct).await {
                        Ok(pay) => format!("pay={:.4} ETH", pay / 1e18),
                        Err(e) => {
                            warn!(pool = %pool.name, err = %e, "depth quote failed");
                            "pay=? ETH".to_string()
                        }
                    };
                    emit_signal(&pool.name, &sig, &depth);
                }
            }
            Err(e) => {
                warn!(pool = %pool.name, err = %e, "failed to decode swap log");
            }
        }
    }
    warn!(pool = %pool.name, "stream ended");
    Ok(())
}

fn emit_signal(name: &str, sig: &Signal, depth: &str) {
    info!(
        pool = %name,
        block = sig.block,
        drop_pct = format!("-{:.3}%", sig.drop_pct),
        depth = %depth,
        "BIG SELL"
    );
}

fn decode_swap(pool: &Pool, log: Log) -> Result<(u64, U256, f64)> {
    if let Some(pid) = pool.pool_id {
        let got = log.topics.get(1).context("v4 swap log missing id topic")?;
        if *got != pid {
            return Err(anyhow::anyhow!("skip: poolId mismatch"));
        }
    }
    let block = log
        .block_number
        .context("log missing block number")?
        .as_u64();
    let data = &log.data.0;
    anyhow::ensure!(
        data.len() >= 96,
        "log data too short: {} bytes",
        data.len()
    );
    let raw = &data[64..96];
    let sqrt = U256::from_big_endian(raw);
    let price = price::display_price(sqrt, pool.decimals0, pool.decimals1, pool.base_token);
    Ok((block, sqrt, price))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig_price(p: f64) -> (u64, U256, f64) {
        // sqrtPriceX96 = sqrt(p) << 96
        let sqrt = U256::from((p.sqrt() * 2f64.powi(96)) as u128);
        (0, sqrt, p)
    }

    #[test]
    fn fires_once_within_single_block() {
        let mut m = BigSellMeter::new(0.5);
        assert!(m.observe(1, sig_price(1.0).1, 1.0).is_none());
        assert!(m.observe(1, sig_price(1.0).1, 1.0).is_none());
        // -1% in a new block -> fire
        let s = m.observe(2, sig_price(0.99).1, 0.99);
        assert!(s.is_some(), "should fire on -1% within block 2");
        // no repeat in same block
        assert!(m.observe(2, sig_price(0.98).1, 0.98).is_none());
        // new block big drop fires again
        assert!(m.observe(3, sig_price(0.97).1, 0.97).is_some());
    }

    #[test]
    fn ignores_up_moves_and_small_drops() {
        let mut m = BigSellMeter::new(1.0);
        assert!(m.observe(1, sig_price(100.0).1, 100.0).is_none());
        assert!(m.observe(2, sig_price(101.0).1, 101.0).is_none()); // up
        assert!(m.observe(2, sig_price(99.5).1, 99.5).is_none()); // 0.5% < 1%
        assert!(m.observe(3, sig_price(98.0).1, 98.0).is_some()); // big drop
    }
}
