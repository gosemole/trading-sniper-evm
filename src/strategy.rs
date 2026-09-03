//! Deciding what a price movement means, and what to do about it.
//!
//! The feed says what happened; this says whether it matters. Keeping the two
//! apart is what lets a fall and a rise be read from the same stream of ticks -
//! the feed has no opinion about which is interesting, and the rules live in
//! one place instead of being spread through the subscription loop.

use crate::executor::Executor;
use crate::feed::Tick;
use crate::inventory::{Inventory, Side};
use crate::pool::Pool;
use crate::route::PoolRef;
use ethers::providers::{Http, Provider};
use ethers::types::{H256, U256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// What a big sell looked like, once one has been recognised.
#[derive(Clone)]
pub struct Signal {
    pub block: u64,
    pub sqrt: U256,
    /// In-range liquidity as reported by the swap that triggered the signal.
    pub liquidity: u128,
    /// Fee charged by the swap that triggered the signal, when the log says.
    pub lp_fee: Option<u32>,
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
    /// Fee reported by the most recent swap.
    lp_fee: Option<u32>,
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
            lp_fee: None,
            fired: false,
        }
    }

    /// Feed a tick. Returns a Signal when a BIG SELL fired.
    pub fn observe(&mut self, t: &Tick) -> Option<Signal> {
        match self.cur_block {
            Some(b) if b == t.block => {
                self.record(t);
                self.try_fire()
            }
            Some(b) if t.block > b => {
                // New block: the previous block's close becomes the reference.
                self.reference = self.price;
                self.cur_block = Some(t.block);
                self.fired = false;
                self.record(t);
                self.try_fire()
            }
            // First tick, or the block went backwards (reorg / out-of-order):
            // (re)seed the reference and wait for the next one.
            _ => {
                self.cur_block = Some(t.block);
                self.reference = Some(t.price);
                self.fired = false;
                self.record(t);
                None
            }
        }
    }

    fn record(&mut self, t: &Tick) {
        self.price = Some(t.price);
        self.sqrt = Some(t.sqrt);
        self.liquidity = t.liquidity;
        self.lp_fee = t.lp_fee;
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
            lp_fee: self.lp_fee,
            reference,
            price,
            drop_pct: -change_pct,
        })
    }
}

/// One watched pool and the rules applied to it.
struct Watch {
    pool: Pool,
    meter: BigSellMeter,
    max_move_pct: f64,
    /// Sell the position once the price is this far above what it cost.
    take_profit_pct: Option<f64>,
    /// A sale is on its way. Held until the chain answers, because the price
    /// keeps arriving in the meantime and every tick would otherwise start
    /// another sale of the same position.
    selling: bool,
    /// Consecutive failed sales. Reset by one that lands.
    sell_attempts: u32,
    /// Stopped trading this pair. Set when selling has failed enough times
    /// that something is wrong with the pair rather than with the moment.
    halted: bool,
}

/// How a broadcast trade ended, on its way back to the strategy.
pub struct Settled {
    pub hash: H256,
    pub pool: PoolRef,
    pub ok: bool,
}

/// How many times a sale is retried before the pair is left alone. A sale that
/// fails three times in a row is not failing because of timing.
const SELL_ATTEMPTS: u32 = 3;

pub struct Strategy {
    http: Provider<Http>,
    watches: HashMap<PoolRef, Watch>,
    exec: Option<Arc<Executor>>,
    inventory: Inventory,
    settled_tx: mpsc::Sender<Settled>,
}

impl Strategy {
    pub fn new(
        http: Provider<Http>,
        exec: Option<Arc<Executor>>,
        inventory: Inventory,
        settled_tx: mpsc::Sender<Settled>,
    ) -> Self {
        Self {
            http,
            watches: HashMap::new(),
            exec,
            inventory,
            settled_tx,
        }
    }

    /// Find out how trades that were in flight when we last stopped ended.
    ///
    /// Without this a restart leaves a reservation stranded forever: the
    /// position it belongs to would never be counted, and a half-sold token
    /// would never be sold again.
    pub async fn resolve_pending(&mut self) {
        for (hash, p) in self.inventory.unsettled() {
            let outcome = crate::swap::await_receipt(&self.http, hash, "restored").await;
            let side = if outcome.happened() {
                self.inventory.settle(hash)
            } else {
                self.inventory.rollback(hash)
            };
            info!(
                tx = ?hash, ?side, token = %p.symbol, outcome = ?outcome,
                "resolved a trade left in flight"
            );
        }
        if let Err(e) = self.inventory.save() {
            warn!(err = %format!("{e:#}"), "could not write the inventory");
        }
    }

    /// Follow a transaction and bring its outcome back to the one place that
    /// may act on it.
    fn follow(&self, hash: H256, pool: PoolRef, label: String) {
        let http = self.http.clone();
        let back = self.settled_tx.clone();
        tokio::spawn(async move {
            let outcome = crate::swap::await_receipt(&http, hash, &label).await;
            let _ = back
                .send(Settled {
                    hash,
                    pool,
                    ok: outcome.happened(),
                })
                .await;
        });
    }

    /// Watch a pool for a drop of `threshold_pct` inside one block.
    pub fn watch(
        &mut self,
        pool: Pool,
        threshold_pct: f64,
        max_move_pct: f64,
        take_profit_pct: Option<f64>,
    ) {
        let held = self.inventory.get(pool.base_currency().unwrap_or_default());
        info!(
            pool = %pool.name, threshold_pct, max_move_pct,
            take_profit_pct = ?take_profit_pct,
            quote = %pool.quote_symbol.clone().unwrap_or_else(|| "quote".into()),
            holding = ?held.map(|p| format!("{:.6} at {:.10}", p.qty, p.avg_price)),
            "watching"
        );
        self.watches.insert(
            pool.pool_ref(),
            Watch {
                pool,
                meter: BigSellMeter::new(threshold_pct),
                max_move_pct,
                take_profit_pct,
                selling: false,
                sell_attempts: 0,
                halted: false,
            },
        );
    }

    pub fn watching(&self) -> usize {
        self.watches.len()
    }

    /// Read the feed until it ends, and act on what comes back from the chain.
    pub async fn run(mut self, mut ticks: mpsc::Receiver<Tick>, mut settled: mpsc::Receiver<Settled>) {
        loop {
            tokio::select! {
                Some(tick) = ticks.recv() => self.on_tick(tick).await,
                Some(s) = settled.recv() => self.on_settled(s).await,
                else => break,
            }
        }
        warn!("feed ended");
    }

    async fn on_tick(&mut self, tick: Tick) {
        self.take_profit(&tick).await;

        // The meter is the only thing here that needs to mutate, so the borrow
        // ends with it.
        let observed = {
            let Some(w) = self.watches.get_mut(&tick.pool) else {
                return;
            };
            if w.halted {
                return;
            }
            w.meter.observe(&tick)
        };
        let Some(sig) = observed else {
            return;
        };
        let (pool, max_move) = {
            let w = &self.watches[&tick.pool];
            (w.pool.clone(), w.max_move_pct)
        };

        // Said before anything is measured, so the signal keeps its place in
        // the log. Everything in this line came out of the tick itself.
        emit_signal(&pool, &sig);

        if let Some(exec) = self.exec.clone() {
            match exec.on_drop(&pool, &sig).await {
                // A transaction went out. Nothing is counted yet: it is set
                // aside until the chain says whether it happened.
                Ok(Some(fill)) => {
                    if let Some(token) = pool.base_currency() {
                        let (qty, symbol) = match exec.route_for(tick.pool) {
                            Some(r) => (
                                crate::route::u256_to_f64(fill.amount_out)
                                    / 10f64.powi(r.output.decimals as i32),
                                r.output.symbol.clone(),
                            ),
                            None => (0.0, "?".to_string()),
                        };
                        self.inventory
                            .reserve(fill.hash, Side::Buy, token, &symbol, qty, sig.price);
                        self.save();
                        info!(tx = ?fill.hash, token = %symbol, qty, price = sig.price, "reserved");
                        self.follow(fill.hash, tick.pool, format!("buy {symbol}"));
                    }
                }
                Ok(None) => {}
                Err(e) => warn!(pool = %pool.name, err = %format!("{e:#}"), "auto-buy failed"),
            }
        }

        // This one costs a walk over the tick data, which is why it comes last
        // and on its own line rather than holding up the two above it.
        report_depth(&pool, &self.http, &sig, max_move).await;
    }

    /// The chain has answered. Everything that was set aside is now either real
    /// or was never real.
    async fn on_settled(&mut self, s: Settled) {
        let side = if s.ok {
            self.inventory.settle(s.hash)
        } else {
            self.inventory.rollback(s.hash)
        };
        self.save();

        let Some(side) = side else {
            // Not ours, or already resolved. Settling twice must not double a
            // position, and the inventory refuses to.
            return;
        };
        match (side, s.ok) {
            (Side::Buy, true) => {
                if let Some(w) = self.watches.get(&s.pool) {
                    if let Some(p) = w.pool.base_currency().and_then(|t| self.inventory.get(t)) {
                        info!(
                            token = %p.symbol, qty = p.qty, avg_price = p.avg_price,
                            buys = p.buys, "position"
                        );
                    }
                }
            }
            (Side::Buy, false) => {
                warn!(tx = ?s.hash, "the buy did not happen; the average is unchanged");
            }
            (Side::Sell, true) => {
                if let Some(w) = self.watches.get_mut(&s.pool) {
                    w.selling = false;
                    w.sell_attempts = 0;
                }
                info!(tx = ?s.hash, "position closed");
            }
            (Side::Sell, false) => {
                // Straight away rather than on the next tick: the price that
                // justified the sale is the one we still want.
                if self.count_failure(s.pool) {
                    self.sell_now(s.pool).await;
                }
            }
        }
    }

    /// A sale did not land. Count it, and say whether it is worth trying again.
    ///
    /// Three failures in a row is not bad timing, it is something wrong with
    /// the pair - a hook that refuses this direction, an allowance that was
    /// revoked, a pool with nothing on the other side. Retrying past that point
    /// only burns gas, so the pair is left alone and said so, loudly. The
    /// position stays held and stays recorded: halting stops trading, it does
    /// not forget anything.
    fn count_failure(&mut self, pool: PoolRef) -> bool {
        let Some(w) = self.watches.get_mut(&pool) else {
            return false;
        };
        w.selling = false;
        w.sell_attempts += 1;
        let attempts = w.sell_attempts;
        let name = w.pool.name.clone();
        if attempts >= SELL_ATTEMPTS {
            w.halted = true;
            warn!(
                pool = %name, attempts, limit = SELL_ATTEMPTS,
                "SELLING FAILED - trading halted for this pair; the position is still held \
                 and still recorded"
            );
            return false;
        }
        warn!(pool = %name, attempt = attempts, of = SELL_ATTEMPTS, "sale failed, retrying");
        true
    }

    fn save(&self) {
        if let Err(e) = self.inventory.save() {
            warn!(err = %format!("{e:#}"), "could not write the inventory");
        }
    }
}

impl Strategy {
    /// Sell the whole position when the price has risen far enough above what
    /// it cost.
    ///
    /// This is why the feed reports every swap rather than only the alarming
    /// ones: a rise is as much a signal as a fall, and the same stream serves
    /// both. The gain is measured against the average entry, so buying further
    /// into a dip lowers the bar rather than raising it.
    async fn take_profit(&mut self, tick: &Tick) {
        let Some(w) = self.watches.get(&tick.pool) else {
            return;
        };
        // Halted, no rule, already selling, or nothing to sell.
        if w.halted || w.selling || w.take_profit_pct.is_none() {
            return;
        }
        let pct = w.take_profit_pct.unwrap_or_default();
        let name = w.pool.name.clone();
        let Some(token) = w.pool.base_currency() else {
            return;
        };
        let Some(position) = self.inventory.get(token) else {
            return;
        };
        if tick.price < position.target(pct) {
            return;
        }
        info!(
            pool = %name,
            token = %position.symbol,
            entry = position.avg_price,
            now = tick.price,
            gain_pct = format!("{:+.3}%", position.gain_pct(tick.price)),
            "TARGET REACHED"
        );
        self.sell_now(tick.pool).await;
    }

    /// Put the whole position up for sale, retrying a broadcast that never got
    /// off the ground. A sale that *was* broadcast is judged by its receipt
    /// instead, which arrives later.
    async fn sell_now(&mut self, pool: PoolRef) {
        let Some(exec) = self.exec.clone() else {
            return;
        };
        let Some(route) = exec.route_for(pool).cloned() else {
            return;
        };
        let token = match self.watches.get(&pool).and_then(|w| w.pool.base_currency()) {
            Some(t) => t,
            None => return,
        };
        let symbol = route.output.symbol.clone();

        loop {
            // Claimed before the sale so the ticks that keep arriving while it
            // is in flight do not each start one of their own.
            if let Some(w) = self.watches.get_mut(&pool) {
                if w.halted {
                    return;
                }
                w.selling = true;
            }
            match exec.sell_all(&route, route.max_slippage_pct).await {
                Ok(Some(fill)) => {
                    // Held, not closed: the position stays on the books until
                    // the chain confirms it is gone.
                    self.inventory
                        .reserve(fill.hash, Side::Sell, token, &symbol, 0.0, 0.0);
                    self.save();
                    info!(tx = ?fill.hash, token = %symbol, "sale reserved");
                    self.follow(fill.hash, pool, format!("sell {symbol}"));
                    return;
                }
                // Nothing was sent - a dry run, or nothing held. Neither is a
                // failure and neither is worth retrying.
                Ok(None) => {
                    if let Some(w) = self.watches.get_mut(&pool) {
                        w.selling = false;
                    }
                    return;
                }
                Err(e) => {
                    warn!(token = %symbol, err = %format!("{e:#}"), "sale could not be sent");
                    if !self.count_failure(pool) {
                        return;
                    }
                }
            }
        }
    }
}

fn emit_signal(pool: &Pool, sig: &Signal) {
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
        "BIG SELL"
    );
}

/// Cost to move the price, preferring the tick walk and degrading to the
/// in-range approximation when tick state cannot be read.
///
/// The two differ a lot on thin pools: the in-range figure assumes the whole
/// move happens at the current liquidity and ignores the swap fee entirely.
async fn report_depth(pool: &Pool, http: &Provider<Http>, sig: &Signal, max_move_pct: f64) {
    let unit = pool.quote_symbol.clone().unwrap_or_else(|| "quote".into());
    let scale = 10f64.powi(pool.quote_decimals() as i32);
    let sqrt_p = crate::pool::sqrt_to_f64(sig.sqrt);

    let mut pay = None;
    let mut method = "none";
    if let (Some(spacing), Some(source)) = (pool.tick_spacing, pool.tick_source()) {
        match crate::depth::TickReader::new(http, source, spacing) {
            Ok(reader) => {
                let fee = sig.lp_fee.or(pool.lp_fee).unwrap_or(0);
                match crate::depth::pay_to_move(
                    &reader, sqrt_p, sig.liquidity, pool.base_token, max_move_pct, fee,
                )
                .await
                {
                    Ok(v) => {
                        pay = Some(v);
                        method = "ticks";
                    }
                    Err(e) => warn!(
                        pool = %pool.name, err = %e,
                        "tick walk failed, falling back to in-range estimate"
                    ),
                }
            }
            Err(e) => warn!(pool = %pool.name, err = %e, "bad tick spacing"),
        }
    }
    if pay.is_none() {
        match pool.quote_pay(sig.liquidity, sig.sqrt, max_move_pct) {
            Ok(v) => {
                pay = Some(v);
                method = "in-range";
            }
            Err(e) => warn!(pool = %pool.name, err = %e, "depth quote failed"),
        }
    }
    let depth = match pay {
        Some(v) => format!("{:.4} {unit}", v / scale),
        None => format!("? {unit}"),
    };
    info!(
        pool = %pool.name, block = sig.block,
        pay_to_move = format!("{depth} / +{max_move_pct}%"), method,
        "depth"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(block: u64, p: f64) -> Tick {
        Tick {
            pool: PoolRef::V3(ethers::types::Address::zero()),
            block,
            // sqrtPriceX96 = sqrt(p) << 96
            sqrt: U256::from((p.sqrt() * 2f64.powi(96)) as u128),
            liquidity: 1_000,
            lp_fee: Some(3000),
            price: p,
        }
    }

    #[test]
    fn fires_once_within_single_block() {
        let mut m = BigSellMeter::new(0.5);
        assert!(m.observe(&tick(1, 1.0)).is_none());
        assert!(m.observe(&tick(1, 1.0)).is_none());
        assert!(m.observe(&tick(2, 0.99)).is_some(), "-1% in a new block fires");
        assert!(m.observe(&tick(2, 0.98)).is_none(), "but only once per block");
        assert!(m.observe(&tick(3, 0.97)).is_some());
    }

    #[test]
    fn ignores_up_moves_and_small_drops() {
        let mut m = BigSellMeter::new(1.0);
        assert!(m.observe(&tick(1, 100.0)).is_none());
        assert!(m.observe(&tick(2, 101.0)).is_none(), "up");
        assert!(m.observe(&tick(2, 99.5)).is_none(), "0.5% < 1%");
        assert!(m.observe(&tick(3, 98.0)).is_some());
    }

    #[test]
    fn quiet_blocks_do_not_stack_into_a_false_signal() {
        // Price bleeds 2% per swap-bearing block with long gaps in between.
        // Each step is below the 5% threshold, so nothing should fire.
        let mut m = BigSellMeter::new(5.0);
        assert!(m.observe(&tick(100, 1.00)).is_none());
        assert!(m.observe(&tick(4_000, 0.98)).is_none());
        assert!(m.observe(&tick(9_000, 0.96)).is_none());
        assert!(m.observe(&tick(20_000, 0.94)).is_none());
        // ...but a single-block 6% drop after a long quiet stretch does fire.
        assert!(m.observe(&tick(50_000, 0.88)).is_some());
    }

    #[test]
    fn signal_carries_the_ticks_own_state() {
        let mut m = BigSellMeter::new(1.0);
        m.observe(&tick(1, 100.0));
        let mut t2 = tick(2, 90.0);
        t2.liquidity = 42;
        t2.lp_fee = Some(500);
        let sig = m.observe(&t2).expect("should fire");
        assert_eq!(sig.liquidity, 42);
        assert_eq!(sig.lp_fee, Some(500));
        assert_eq!(sig.reference, 100.0);
        assert_eq!(sig.price, 90.0);
        assert!((sig.drop_pct - 10.0).abs() < 1e-9);
    }

    #[test]
    fn reorg_backwards_reseeds_instead_of_firing() {
        let mut m = BigSellMeter::new(1.0);
        m.observe(&tick(10, 100.0));
        m.observe(&tick(11, 100.0));
        // block goes backwards: reseed, do not fire on the apparent drop
        assert!(m.observe(&tick(9, 50.0)).is_none());
    }
}
