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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
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

    /// Change what counts as a big sell, keeping everything the meter knows
    /// about the price.
    ///
    /// The reference is the previous block's close and has nothing to do with
    /// how big a drop has to be, so rebuilding the meter to retune it would
    /// throw away the reference and blind the pool until its next block.
    pub fn set_threshold(&mut self, pct: f64) {
        self.threshold_pct = pct;
    }

    /// What a drop is currently measured against, in percent.
    pub fn threshold(&self) -> f64 {
        self.threshold_pct
    }

    /// The most recent price this pool reported, which is the running close of
    /// the block being observed. The best available guess at what a sale
    /// started right now would get.
    pub fn last(&self) -> Option<f64> {
        self.price
    }

    /// Everything the last swap said about this pool, for pricing a trade
    /// against it right now. This is the same information a buy gets from the
    /// log that raised its signal - a sale has no signal, but the feed has been
    /// delivering it all along.
    pub fn last_state(&self) -> Option<crate::executor::LiveState> {
        Some(crate::executor::LiveState {
            sqrt: self.sqrt?,
            liquidity: self.liquidity,
            lp_fee: self.lp_fee,
        })
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
    /// Sell it anyway once it has been held this long since the last buy.
    exit_after: Option<Duration>,
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

/// Everything the strategy is told about trades it started.
///
/// The work of trading happens in tasks of its own so the tick loop never
/// waits on a network call - but the inventory has one owner, and this is how
/// news gets back to it.
pub enum Report {
    /// A transaction went out. Nothing is counted yet.
    Filled {
        hash: H256,
        pool: PoolRef,
        side: Side,
        token: ethers::types::Address,
        symbol: String,
        decimals: u8,
        /// Raw units the trade was quoted at; the receipt overrides it.
        raw: ethers::types::U256,
        price: f64,
        /// What a buy actually handed over, in human units of the token it
        /// spent - the other half of the price it really got. `None` for a
        /// sale. See `inventory::Trade::spent`.
        spent: Option<f64>,
        /// A sale's proceeds - which token comes back and how much - credited
        /// to tracked cash the moment this is reserved. `None` for a buy.
        credit: Option<(ethers::types::Address, ethers::types::U256)>,
        /// What `on_tick` set aside for a buy, so a rollback gives back the
        /// amount actually taken rather than the route's ceiling. `None` for a
        /// sale. See `inventory::Trade::committed`.
        committed: Option<(ethers::types::Address, ethers::types::U256)>,
    },
    /// The chain answered, with what the receipt says actually moved.
    Settled {
        hash: H256,
        pool: PoolRef,
        ok: bool,
        moved: Option<ethers::types::U256>,
        /// What the pool actually filled at, in the pool's own quote token,
        /// read from the transaction's `Swap` log. `None` for a sale, and
        /// whenever the log could not be read. See `Pool::fill_price`.
        entry_price: Option<f64>,
        /// A sale's real proceeds, if this settlement has any to reconcile.
        credit_moved: Option<ethers::types::U256>,
    },
    /// A buy was never broadcast - skipped, or failed before it went out.
    /// Carries exactly what `on_tick` committed, because with a size worked out
    /// per signal there is no fixed figure to look up afterwards.
    BuyAborted {
        token: ethers::types::Address,
        spend: ethers::types::U256,
    },
    /// A sale could not even be broadcast.
    SellFailed { pool: PoolRef },
    /// A sale was not attempted after all - a dry run, or nothing held.
    /// Not a failure, and not worth counting as one.
    SellSkipped { pool: PoolRef },
}

/// One settled trade, unpacked from its report.
struct Settled {
    hash: H256,
    pool: PoolRef,
    ok: bool,
    moved: Option<ethers::types::U256>,
    entry_price: Option<f64>,
    credit_moved: Option<ethers::types::U256>,
}

/// How many times a sale is retried before the pair is left alone. A sale that
/// fails three times in a row is not failing because of timing.
const SELL_ATTEMPTS: u32 = 3;

/// A re-read config, with its pools already resolved.
///
/// Resolving asks the chain for decimals, symbols and PoolKeys, so it happens
/// in the task that read the file - never in the loop that decides trades. A
/// pool that could not be resolved is simply absent, and the pools that could
/// be are applied without it.
pub struct Reload {
    pub cfg: crate::config::Config,
    pub pools: Vec<Pool>,
}

pub struct Strategy {
    http: Provider<Http>,
    watches: HashMap<PoolRef, Watch>,
    exec: Option<Arc<Executor>>,
    inventory: Inventory,
    reports: mpsc::Sender<Report>,
    /// The pools the feed should be subscribed to. Published only after a
    /// reload has decided what is actually watched, so a pool kept because it
    /// holds a position keeps its subscription too.
    feed: Option<tokio::sync::watch::Sender<Vec<Pool>>>,
}

impl Strategy {
    pub fn new(
        http: Provider<Http>,
        exec: Option<Arc<Executor>>,
        inventory: Inventory,
        reports: mpsc::Sender<Report>,
    ) -> Self {
        Self {
            http,
            watches: HashMap::new(),
            exec,
            inventory,
            reports,
            feed: None,
        }
    }

    /// Where to publish the watched set, so the feed can follow a pool that
    /// was added and drop one that was removed.
    pub fn publishes_pools_to(&mut self, feed: tokio::sync::watch::Sender<Vec<Pool>>) {
        self.feed = Some(feed);
    }

    /// Find out how trades that were in flight when we last stopped ended.
    ///
    /// Without this a restart leaves a reservation stranded forever: the
    /// position it belongs to would never be counted, and a half-sold token
    /// would never be sold again.
    pub async fn resolve_pending(&mut self) {
        for (hash, p) in self.inventory.unsettled() {
            let landed = crate::swap::await_receipt(&self.http, hash, "restored").await;
            let token = p.token.parse().unwrap_or_default();
            let side = if landed.outcome.happened() {
                let moved = self.moved_in(&landed, p.side, token);
                let credit_moved = self.received_of(&landed, p.credit_token());
                // Same reading as `follow` does for a live trade: the pool
                // that holds this token says what the buy filled at.
                let entry_price = match p.side {
                    Side::Buy => self
                        .watches
                        .values()
                        .find(|w| w.pool.base_currency() == Some(token))
                        .and_then(|w| w.pool.fill_price(&landed.logs)),
                    Side::Sell => None,
                };
                self.inventory
                    .settle(hash, moved, credit_moved, entry_price)
            } else {
                self.inventory.rollback(hash)
            };
            info!(
                tx = ?hash, ?side, token = %p.symbol, outcome = ?landed.outcome,
                "resolved a trade left in flight"
            );
        }
        if let Err(e) = self.inventory.save() {
            warn!(err = %format!("{e:#}"), "could not write the inventory");
        }
    }

    /// Adopt whatever the wallet already holds of the tokens we could sell.
    ///
    /// Without this the bot is blind to its own balance: a token bought before
    /// it started, or by hand, would never be sold - not on a target and not on
    /// a timeout - while a single new fill would then sell all of it anyway,
    /// against an average that only knew about the new part.
    ///
    /// The entry price has to be assumed, because the real one is unknowable.
    /// The price right now is the least wrong choice and is marked as a guess,
    /// so a target derived from it is never mistaken for one derived from fills.
    pub async fn seed_inventory(&mut self) {
        let Some(exec) = self.exec.clone() else {
            return;
        };
        let mut seeded = 0;
        for (key, watch) in &self.watches {
            let Some(token) = watch.pool.base_currency() else {
                continue;
            };
            let Some(route) = exec.route_for(*key) else {
                continue;
            };
            // The same misconfiguration `armed_for` refuses to buy on: a route
            // aimed at a pool whose base side is not what it buys. Adopting
            // here would stamp one token's balance with another's name.
            if route.output.address != token {
                warn!(
                    pool = %watch.pool.name,
                    route = %route.name,
                    buys = %route.output.symbol,
                    "route buys a different token than this pool's base - not adopting its \
                     balance; check base_token on the pool, or trigger_pool on the route"
                );
                continue;
            }
            if self.inventory.get(token).is_some() {
                continue;
            }
            let held = match crate::swap::balance_of(&self.http, token, exec.owner()).await {
                Ok(h) if !h.is_zero() => h,
                _ => continue,
            };
            let Some(price) = self.pool_price(&watch.pool).await else {
                warn!(
                    pool = %watch.pool.name,
                    "holds a balance but its price cannot be read; not adopting it"
                );
                continue;
            };
            if self.inventory.seed(
                token,
                &route.output.symbol,
                route.output.decimals,
                held,
                price,
            ) {
                seeded += 1;
                warn!(
                    token = %route.output.symbol,
                    qty = %crate::route::format_units(held, route.output.decimals),
                    assumed_entry = price,
                    "ADOPTED a balance this bot did not buy; its entry price is the price now, \
                     not what it cost"
                );
            }
        }
        if seeded > 0 {
            self.save();
        }

        // Every distinct token an armed route spends, read once each: this is
        // the one real chain read the tracked cash balance ever gets, and
        // everything after startup keeps it in step locally instead.
        let mut spend_tokens = HashSet::new();
        for key in self.watches.keys() {
            if let Some(route) = exec.route_for(*key) {
                spend_tokens.insert((route.input.address, route.input.symbol.clone()));
            }
        }
        for (token, symbol) in spend_tokens {
            match crate::swap::balance_of(&self.http, token, exec.owner()).await {
                Ok(balance) => {
                    info!(token = %symbol, %balance, "wallet balance tracked");
                    self.inventory.set_cash(token, balance);
                }
                Err(e) => warn!(
                    token = %symbol, err = %format!("{e:#}"),
                    "could not read starting balance; treating it as zero until the next \
                     successful sale credits it"
                ),
            }
        }
        self.save();
    }

    /// This pool's price of its base token, read from its own state.
    async fn pool_price(&self, pool: &Pool) -> Option<f64> {
        let reader = crate::depth::TickReader::new(
            &self.http,
            pool.tick_source()?,
            pool.tick_spacing?,
        )
        .ok()?;
        let state = crate::depth::read_state(&reader).await.ok()?;
        let price = crate::price::from_sqrt(
            state.sqrt_p,
            pool.decimals0,
            pool.decimals1,
            pool.base_token,
        );
        (price.is_finite() && price > 0.0).then_some(price)
    }

    /// Follow a transaction and bring its outcome back to the one place that
    /// may act on it.
    /// `price_from` is the pool whose own `Swap` log says what this trade
    /// actually filled at - `Some` for a buy, whose entry price has to be
    /// recorded in that pool's units, and `None` for a sale, which has no
    /// entry to record.
    fn follow(
        &self,
        hash: H256,
        pool: PoolRef,
        token: ethers::types::Address,
        credit_token: Option<ethers::types::Address>,
        price_from: Option<Pool>,
        label: String,
    ) {
        let http = self.http.clone();
        let back = self.reports.clone();
        let owner = self.exec.as_ref().map(|e| e.owner());
        tokio::spawn(async move {
            let landed = crate::swap::await_receipt(&http, hash, &label).await;
            // What the receipt says arrived, not what the quote promised. For a
            // sale nothing arrives in this token, so there is nothing to read
            // and the reserved amount stands.
            let moved = owner.and_then(|o| crate::swap::received(&landed.logs, token, o));
            // The same question about a sale's proceeds, in whichever token
            // that is - `settle` reconciles the optimistic credit against this.
            let credit_moved = match (owner, credit_token) {
                (Some(o), Some(ct)) => crate::swap::received(&landed.logs, ct, o),
                _ => None,
            };
            // What the pool charged, in the pool's own quote token, straight
            // out of its own event in this very transaction.
            let entry_price = price_from.and_then(|p| p.fill_price(&landed.logs));
            let _ = back
                .send(Report::Settled {
                    hash,
                    pool,
                    ok: landed.outcome.happened(),
                    moved,
                    entry_price,
                    credit_moved,
                })
                .await;
        });
    }

    /// What a settled trade moved in the token it is about.
    fn moved_in(
        &self,
        landed: &crate::swap::Landed,
        side: Side,
        token: ethers::types::Address,
    ) -> Option<ethers::types::U256> {
        // Only a buy delivers the token to us; a sale sends it away, and its
        // size is the amount we asked to sell.
        if side != Side::Buy {
            return None;
        }
        self.received_of(landed, Some(token))
    }

    /// What a settled trade delivered of `token`, read from the receipt - or
    /// `None` when there is nothing to ask about.
    fn received_of(
        &self,
        landed: &crate::swap::Landed,
        token: Option<ethers::types::Address>,
    ) -> Option<ethers::types::U256> {
        let owner = self.exec.as_ref()?.owner();
        crate::swap::received(&landed.logs, token?, owner)
    }

    /// Watch a pool for a drop of `threshold_pct` inside one block.
    pub fn watch(
        &mut self,
        pool: Pool,
        threshold_pct: f64,
        max_move_pct: f64,
        take_profit_pct: Option<f64>,
        exit_after_secs: Option<u64>,
    ) {
        let held = self.inventory.get(pool.base_currency().unwrap_or_default());
        info!(
            pool = %pool.name, threshold_pct, max_move_pct,
            take_profit_pct = ?take_profit_pct,
            exit_after_secs = ?exit_after_secs,
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
                exit_after: exit_after_secs.map(Duration::from_secs),
                selling: false,
                sell_attempts: 0,
                halted: false,
            },
        );
    }

    pub fn watching(&self) -> usize {
        self.watches.len()
    }

    /// Apply a re-read config to what is already running.
    ///
    /// Pool by pool and route by route, and never all or nothing: a pool that
    /// cannot be resolved, a route with no plan to flip, a pool held back
    /// because it holds a position - each is reported and skipped on its own.
    /// One edited entry must not stop the entries beside it from being applied,
    /// and must not touch what they are already doing: the pools already
    /// subscribed keep their subscriptions, the routes already armed keep
    /// buying, and a trade in flight is not looked at.
    ///
    /// What it will not apply, it says out loud. A number silently ignored is
    /// worse than one that was never editable: the operator walks away
    /// believing the bot is trading on what the file says.
    pub fn retune(&mut self, r: &Reload) {
        let cfg = &r.cfg;
        let resolved: HashMap<PoolRef, &Pool> =
            r.pools.iter().map(|p| (p.pool_ref(), p)).collect();
        let mut applied = 0usize;
        let mut set_changed = false;
        let mut seen = HashSet::new();
        for pc in &cfg.pools {
            let Some(key) = pc.pool_ref() else { continue };
            seen.insert(key);
            if !self.watches.contains_key(&key) {
                // New in the file. Watched from here on, but not armed: a route
                // is armed by resolving it, approving what it spends and
                // measuring its gas, none of which can be done from inside this
                // loop - so a new pool reports its drops and buys nothing until
                // a restart.
                let Some(pool) = resolved.get(&key) else {
                    warn!(pool = %pc.name,
                        "new in the config but could not be resolved; not watching it - the \
                         pools already watched are unaffected");
                    continue;
                };
                self.watch(
                    (*pool).clone(),
                    pc.threshold_pct.unwrap_or(cfg.threshold_pct),
                    pc.max_move_pct.unwrap_or(cfg.max_move_pct),
                    cfg.armed_route(key).and_then(|r| r.take_profit_pct),
                    cfg.armed_route(key).and_then(|r| r.exit_after_secs),
                );
                applied += 1;
                set_changed = true;
                continue;
            }
            let Some(w) = self.watches.get_mut(&key) else {
                continue;
            };
            let armed = cfg.armed_route(key);
            let threshold = pc.threshold_pct.unwrap_or(cfg.threshold_pct);
            let max_move = pc.max_move_pct.unwrap_or(cfg.max_move_pct);
            let take_profit = armed.and_then(|r| r.take_profit_pct);
            let exit_after = armed.and_then(|r| r.exit_after_secs).map(Duration::from_secs);
            // Compared field by field so the log says what actually moved. A
            // reload that printed every pool it looked at would bury the one
            // line somebody edited the file for.
            if w.meter.threshold() != threshold {
                info!(pool = %w.pool.name, from = w.meter.threshold(), to = threshold,
                      "threshold_pct");
                w.meter.set_threshold(threshold);
                applied += 1;
            }
            if w.max_move_pct != max_move {
                info!(pool = %w.pool.name, from = w.max_move_pct, to = max_move, "max_move_pct");
                w.max_move_pct = max_move;
                applied += 1;
            }
            if w.take_profit_pct != take_profit {
                info!(pool = %w.pool.name, from = ?w.take_profit_pct, to = ?take_profit,
                      "take_profit_pct");
                w.take_profit_pct = take_profit;
                applied += 1;
            }
            if w.exit_after != exit_after {
                info!(pool = %w.pool.name, from = ?w.exit_after, to = ?exit_after,
                      "exit_after_secs");
                w.exit_after = exit_after;
                applied += 1;
            }
        }
        // Dropped from the file - but not from a position. A pool holding one
        // keeps its watch and its subscription whatever the file says: the
        // take-profit and the hold timer are the only things that will ever
        // sell it, and both live here. A bag nobody is watching is how a bag
        // gets forgotten.
        let leaving: Vec<PoolRef> =
            self.watches.keys().filter(|k| !seen.contains(k)).copied().collect();
        for key in leaving {
            let Some(w) = self.watches.get(&key) else { continue };
            let held = w.pool.base_currency().and_then(|t| self.inventory.get(t)).is_some();
            if held || w.selling {
                warn!(pool = %w.pool.name, held, selling = w.selling,
                    "no longer in the config, but it holds a position - still watched and still \
                     sold; remove it after the position is closed");
                continue;
            }
            info!(pool = %w.pool.name, "no longer in the config; unwatched");
            self.watches.remove(&key);
            // Nothing feeds this pool any more, so nothing can trigger its
            // route. Said in the executor as well, so a plan cannot fire on a
            // stray tick that was already in the channel.
            if let Some(exec) = &self.exec {
                exec.set_armed(key, false);
            }
            applied += 1;
            set_changed = true;
        }
        // Only after the set is settled, and only when it moved: this is what
        // the feed subscribes to, and republishing an unchanged set would make
        // it diff for nothing.
        if set_changed {
            if let Some(feed) = &self.feed {
                let pools: Vec<Pool> = self.watches.values().map(|w| w.pool.clone()).collect();
                if feed.send(pools).is_err() {
                    warn!("the feed is not listening; the watched set was not published");
                }
            }
        }
        info!(applied, watching = self.watches.len(), "config reloaded");
    }

    /// Read the feed until it ends, and act on what comes back from the chain.
    pub async fn run(
        mut self,
        mut ticks: mpsc::Receiver<Tick>,
        mut reports: mpsc::Receiver<Report>,
        mut reloads: mpsc::Receiver<Reload>,
    ) {
        // Time passes whether or not anyone trades, so the clock gets a branch
        // of its own rather than riding on price updates.
        let mut clock = tokio::time::interval(Duration::from_secs(5));
        clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                Some(tick) = ticks.recv() => self.on_tick(tick),
                Some(r) = reports.recv() => self.on_report(r).await,
                // Applied here rather than from the signal handler's own task:
                // the watches are `&mut self` and belong to this loop, so a
                // reload lands between two ticks instead of racing one.
                Some(r) = reloads.recv() => self.retune(&r),
                _ = clock.tick() => self.check_hold_times(),
                else => break,
            }
        }
        warn!("feed ended");
    }

    /// Give up on positions that have been held too long.
    ///
    /// Held from the last buy, so averaging into a dip restarts the clock: the
    /// rule is "this has not worked out for a while", and a fresh buy is a
    /// fresh opinion.
    fn check_hold_times(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expired: Vec<(PoolRef, String, u64, Option<f64>)> = self
            .watches
            .iter()
            .filter(|(_, w)| !w.halted && !w.selling)
            .filter_map(|(key, w)| {
                let after = w.exit_after?.as_secs();
                let position = self.inventory.get(w.pool.base_currency()?)?;
                let held = position.held_for(now);
                // The last price this pool reported, which is the best guess
                // at what the sale is about to get. Absent before the first
                // tick, and then the outcome simply is not known yet.
                let net = w.meter.last().map(|p| position.net_pct(p));
                (held >= after).then(|| (*key, position.symbol.clone(), held, net))
            })
            .collect();
        for (key, symbol, held, net) in expired {
            // A timed exit sells whatever the price is - that is the whole
            // point of it, and a position held past its welcome is a position
            // to be rid of. But a sale that does not cover what it cost is a
            // loss taken deliberately, and it says so rather than passing for
            // an ordinary close in the log.
            match net {
                Some(n) if n < 0.0 => warn!(
                    token = %symbol, held_secs = held,
                    net_pct = format!("{n:+.3}%"),
                    "HOLD EXPIRED AT A LOSS - selling anyway"
                ),
                Some(n) => info!(
                    token = %symbol, held_secs = held,
                    net_pct = format!("{n:+.3}%"), "HOLD EXPIRED"
                ),
                None => info!(token = %symbol, held_secs = held, "HOLD EXPIRED"),
            }
            self.start_sale(key);
        }
    }

    fn on_tick(&mut self, tick: Tick) {
        self.take_profit(&tick);

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

        // Both of these go to tasks of their own. Awaiting either here would
        // stall every other pool behind one trade or one tick walk, and would
        // let the next signal be judged on a backlog rather than on the state
        // it was raised from.
        if let Some(exec) = self.exec.clone() {
            if let Some(route) = exec.route_for(tick.pool) {
                let spend_token = route.input.address;
                // Worked out BEFORE the money is touched, so what is set aside
                // is what is spent. `size_for` is synchronous and asks the
                // network for nothing: it walks the route over the signal's own
                // log and the tick ladder already in memory.
                // The snapshot comes back with the size, so the money set
                // aside and the minimum eventually signed are worked out from
                // one reading of the pools rather than two.
                // The ceiling is what is actually there to spend.
                let have = self.inventory.cash(spend_token);
                let Some((spend, prepared)) = exec.size_for(tick.pool, &sig, have) else {
                    warn!(
                        pool = %pool.name, route = %route.name,
                        "not buying: this signal cannot be sized - see the reason above"
                    );
                    return;
                };
                if have < spend {
                    warn!(
                        pool = %pool.name,
                        token = %route.input.symbol,
                        have = %crate::route::format_units(have, route.input.decimals),
                        need = %crate::route::format_units(spend, route.input.decimals),
                        "not buying: tracked balance is not enough"
                    );
                } else {
                    // Committed here, synchronously, so a decision made a
                    // moment later for a different route spending the same
                    // token sees the wallet as already spoken for. Released by
                    // whichever of `BuyAborted` or a failed `Settled` fits what
                    // actually happened to it. Not written to disk: this loop
                    // must not wait on a file, every outcome that can follow
                    // saves anyway, and a crash before one arrives is healed by
                    // the balance re-read at startup.
                    self.inventory.debit_cash(spend_token, spend);
                    info!(
                        pool = %pool.name,
                        route = %route.name,
                        impact_pct = route.impact_pct,
                        spend = %crate::route::format_units(spend, route.input.decimals),
                        of = %crate::route::format_units(have, route.input.decimals),
                        "sized to impact"
                    );

                    // What the route buys, decided now rather than looked up
                    // after the fact: a fill has to be recorded whatever the
                    // pool knows about its own tokens, and the route is the one
                    // thing that certainly knows what it just bought.
                    let bought = route.output.clone();
                    // Needed to turn the raw amount the fill sent into the
                    // human figure the entry price divides by, and captured
                    // here for the same reason `bought` is.
                    let spend_decimals = route.input.decimals;
                    let back = self.reports.clone();
                    let (p, s, key) = (pool.clone(), sig.clone(), tick.pool);
                    tokio::spawn(async move {
                        match exec.on_drop(&p, &s, spend, &prepared).await {
                            Ok(Some(fill)) => {
                                let _ = back
                                    .send(Report::Filled {
                                        hash: fill.hash,
                                        pool: key,
                                        side: Side::Buy,
                                        token: bought.address,
                                        symbol: bought.symbol,
                                        decimals: bought.decimals,
                                        raw: fill.amount_out,
                                        price: s.price,
                                        // What the swap actually sent, against
                                        // what actually arrived: that division
                                        // is the entry price, with every fee
                                        // and the hook already in it.
                                        spent: Some(crate::inventory::raw_to_f64(
                                            fill.sold,
                                            spend_decimals,
                                        )),
                                        credit: None,
                                        committed: Some((spend_token, spend)),
                                    })
                                    .await;
                            }
                            Ok(None) => {
                                let _ = back
                                    .send(Report::BuyAborted { token: spend_token, spend })
                                    .await;
                            }
                            Err(e) => {
                                warn!(pool = %p.name, err = %format!("{e:#}"), "auto-buy failed");
                                let _ = back
                                    .send(Report::BuyAborted { token: spend_token, spend })
                                    .await;
                            }
                        }
                    });
                }
            }
        }

        let http = self.http.clone();
        // Handed the executor so the walk can come out of the ladder the
        // background scan already read, instead of reading the same ticks again
        // per signal - which is several requests each, arriving in a burst,
        // during exactly the dip the buy is competing for the budget in.
        let exec = self.exec.clone();
        tokio::spawn(async move { report_depth(&pool, &http, exec, &sig, max_move).await });
    }

    /// The chain has answered. Everything that was set aside is now either real
    /// or was never real.
    async fn on_report(&mut self, r: Report) {
        let s = match r {
            Report::Filled {
                hash, pool, side, token, symbol, decimals, raw, price, spent, credit,
                committed,
            } => {
                let credit_token = credit.map(|(t, _)| t);
                let trade = crate::inventory::Trade {
                    side,
                    token,
                    symbol: symbol.clone(),
                    decimals,
                    raw,
                    price,
                    spent,
                    credit,
                    committed,
                };
                if self.inventory.reserve(hash, trade) {
                    self.save();
                    info!(tx = ?hash, token = %symbol, ?side, %raw, price, "reserved");
                    // A sale has no entry to price; a buy is priced from the
                    // pool it went through, which is the one being watched.
                    let price_from = match side {
                        Side::Buy => self.watches.get(&pool).map(|w| w.pool.clone()),
                        Side::Sell => None,
                    };
                    self.follow(
                        hash,
                        pool,
                        token,
                        credit_token,
                        price_from,
                        format!("{side:?} {symbol}").to_lowercase(),
                    );
                }
                return;
            }
            Report::BuyAborted { token, spend } => {
                self.inventory.credit_cash(token, spend);
                return;
            }
            Report::SellSkipped { pool } => {
                if let Some(w) = self.watches.get_mut(&pool) {
                    w.selling = false;
                }
                return;
            }
            Report::SellFailed { pool } => {
                if self.count_failure(pool) {
                    self.start_sale(pool);
                }
                return;
            }
            Report::Settled {
                hash,
                pool,
                ok,
                moved,
                entry_price,
                credit_moved,
            } => Settled {
                hash,
                pool,
                ok,
                moved,
                entry_price,
                credit_moved,
            },
        };
        // Asked before settling, which consumes the reservation. What was
        // quoted against what arrived is the only measurement of how far the
        // model and the chain drift apart on a trade that LANDED - the ones
        // that revert say so loudly, and the ones that do not used to say
        // nothing at all, so the distribution was only ever half visible.
        let quoted = self.inventory.quoted(s.hash);
        let side = if s.ok {
            self.inventory.settle(s.hash, s.moved, s.credit_moved, s.entry_price)
        } else {
            self.inventory.rollback(s.hash)
        };
        // A buy that was broadcast and then reverted or dropped never spent
        // what was set aside for it. `rollback` gives back exactly that amount,
        // recorded with the reservation - the route's `amount_in` is only a
        // ceiling now, so there is nothing to look it up from.
        self.save();

        let Some(side) = side else {
            // Not ours, or already resolved. Settling twice must not double a
            // position, and the inventory refuses to.
            return;
        };
        if let (Side::Buy, true, Some(q), Some(got)) = (side, s.ok, quoted, s.moved) {
            if !q.is_zero() {
                let ratio = crate::route::u256_to_f64(got) / crate::route::u256_to_f64(q);
                let short = (1.0 - ratio) * 100.0;
                info!(
                    tx = ?s.hash,
                    quoted = %q,
                    received = %got,
                    // Positive means the model asked for more than the chain
                    // gave. Some of it is the model, some of it is whoever got
                    // in front of us between the signal and the block - and the
                    // two only tell apart across many trades.
                    short_pct = format!("{short:+.3}%"),
                    "the fill against the quote"
                );
            }
        }
        match (side, s.ok) {
            (Side::Buy, true) => {
                if let Some(w) = self.watches.get(&s.pool) {
                    if let Some(p) = w.pool.base_currency().and_then(|t| self.inventory.get(t)) {
                        info!(
                            token = %p.symbol, qty = p.qty,
                            // Named for the currency each is in, because the
                            // two are not comparable and reading one as the
                            // other is what once put every target out of reach.
                            entry_in_pool = p.avg_price,
                            cost_in_route_token = p.avg_cost,
                            exit_ratio = p.exit_ratio,
                            target = p.target(w.take_profit_pct.unwrap_or_default()),
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
                    self.start_sale(s.pool);
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
    fn take_profit(&mut self, tick: &Tick) {
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
            entry_in_pool = position.avg_price,
            now = tick.price,
            gain_pct = format!("{:+.3}%", position.gain_pct(tick.price)),
            // What is left once the sale has paid for itself too. This is the
            // one the `pct` in the config is about; `gain_pct` beside it is
            // what the same move looked like before the way out was counted.
            net_pct = format!("{:+.3}%", position.net_pct(tick.price)),
            "TARGET REACHED"
        );
        self.start_sale(tick.pool);
    }

    /// Put the whole position up for sale, in a task of its own.
    ///
    /// The sale is claimed here rather than when it lands, because the price
    /// keeps arriving while it is in flight and every tick would otherwise
    /// start another sale of the same position.
    fn start_sale(&mut self, pool: PoolRef) {
        let Some(exec) = self.exec.clone() else {
            return;
        };
        let Some(route) = exec.route_for(pool) else {
            return;
        };
        let Some(w) = self.watches.get_mut(&pool) else {
            return;
        };
        if w.halted {
            return;
        }
        w.selling = true;
        let Some(token) = w.pool.base_currency() else {
            w.selling = false;
            return;
        };
        let symbol = route.output.symbol.clone();
        let decimals = route.output.decimals;
        // Never more than this bot bought. The wallet may hold more, and what
        // it holds beyond our own fills is not ours to sell.
        let limit = self.inventory.get(token).map(|p| p.held());
        // What the pool looks like NOW, straight off the feed. Without this a
        // sale is priced entirely from the calibration snapshot, which is
        // refreshed on a timer and can be minutes behind - and a sale is
        // exactly the moment the price is moving.
        let live = w.meter.last_state();
        // A sale that already reverted is not re-quoted by the model. Whatever
        // the model believed, the chain has just disagreed with it, and the
        // router prices the retry honestly at whatever the price now is.
        let retry = w.sell_attempts > 0;
        let back = self.reports.clone();
        tokio::spawn(async move {
            let priced = exec.sell_all(pool, &route, route.max_slippage_pct, limit, live, retry);
            let report = match priced.await {
                // Held, not closed: the position stays on the books until the
                // chain confirms it is gone.
                Ok(Some(fill)) => Report::Filled {
                    hash: fill.hash,
                    pool,
                    side: Side::Sell,
                    token,
                    symbol,
                    decimals,
                    // What the sale asked to move, so settling subtracts
                    // exactly that from the position.
                    raw: fill.sold,
                    price: 0.0,
                    spent: None,
                    // What it should bring back, credited to tracked cash the
                    // moment this reserves - see `Trade::credit`. The quote,
                    // not the receipt: precise enough for a spend gate, and
                    // available now rather than after a bisection's worth of
                    // waiting.
                    credit: Some((route.input.address, fill.amount_out)),
                    committed: None,
                },
                Ok(None) => Report::SellSkipped { pool },
                Err(e) => {
                    warn!(token = %symbol, err = %format!("{e:#}"), "sale could not be sent");
                    Report::SellFailed { pool }
                }
            };
            let _ = back.send(report).await;
        });
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
async fn report_depth(
    pool: &Pool,
    http: &Provider<Http>,
    exec: Option<Arc<Executor>>,
    sig: &Signal,
    max_move_pct: f64,
) {
    let unit = pool.quote_symbol.clone().unwrap_or_else(|| "quote".into());
    let scale = 10f64.powi(pool.quote_decimals() as i32);
    let sqrt_p = crate::pool::sqrt_to_f64(sig.sqrt);
    let fee = sig.lp_fee.or(pool.lp_fee).unwrap_or(0);

    let mut pay = None;
    let mut method = "none";

    // Everything this needs, the signal and the background scan already have
    // between them: the price, the liquidity and the fee came in the log that
    // raised the signal, and where the ticks are and what crossing them does
    // was read on a timer. So the ordinary case is arithmetic, not a request.
    if let Some(exec) = &exec {
        let target = crate::depth::move_target(sqrt_p, pool.base_token, max_move_pct);
        if let Some(rungs) = exec.rungs_towards(pool.pool_ref(), sqrt_p, target) {
            match crate::depth::pay_to_move_along(
                sqrt_p, sig.liquidity, pool.base_token, max_move_pct, fee, &rungs,
            ) {
                Ok(v) => {
                    pay = Some(v);
                    method = "ticks (cached)";
                }
                Err(e) => warn!(
                    pool = %pool.name, err = %format!("{e:#}"),
                    "cached tick walk failed, reading the chain instead"
                ),
            }
        }
    }

    if pay.is_none() {
        if let (Some(spacing), Some(source)) = (pool.tick_spacing, pool.tick_source()) {
            match crate::depth::TickReader::new(http, source, spacing) {
                Ok(reader) => {
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
                            // The whole chain, not just the outermost context:
                            // `{e}` on an anyhow error prints "eth_getStorageAt"
                            // and drops the sentence that says what went wrong
                            // with it, which is the only part worth reading.
                            pool = %pool.name, err = %format!("{e:#}"),
                            "tick walk failed, falling back to in-range estimate"
                        ),
                    }
                }
                Err(e) => warn!(pool = %pool.name, err = %format!("{e:#}"), "bad tick spacing"),
            }
        }
    }
    if pay.is_none() {
        match pool.quote_pay(sig.liquidity, sig.sqrt, max_move_pct) {
            Ok(v) => {
                pay = Some(v);
                method = "in-range";
            }
            Err(e) => warn!(pool = %pool.name, err = %format!("{e:#}"), "depth quote failed"),
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

    /// A v3 pool named by its own address, with both currencies known - enough
    /// for a watch, a position and a pool_ref.
    fn v3_pool(name: &str, byte: u8) -> Pool {
        let addr = ethers::types::Address::from([byte; 20]);
        let base = ethers::types::Address::from([byte.wrapping_add(100); 20]);
        Pool {
            name: name.to_string(),
            version: "v3".to_string(),
            decimals0: 18,
            decimals1: 18,
            decimals_known: true,
            base_token: 0,
            quote_symbol: Some("WETH".to_string()),
            base_symbol: Some(name.to_string()),
            currencies: Some((base, ethers::types::Address::from([1u8; 20]))),
            address: addr,
            pool_id: None,
            tick_spacing: None,
            lp_fee: Some(3000),
        }
    }

    fn watching(pools: &[Pool], threshold: f64) -> Strategy {
        let http = Provider::<Http>::try_from("http://127.0.0.1:1").expect("a url, not a dial");
        let (reports, _rx) = mpsc::channel(8);
        let mut s = Strategy::new(http, None, Inventory::default(), reports);
        for p in pools {
            s.watch(p.clone(), threshold, 1.0, None, None);
        }
        s
    }

    fn pool_toml(p: &Pool, threshold: Option<f64>) -> String {
        let extra = threshold.map(|t| format!("threshold_pct = {t}\n")).unwrap_or_default();
        format!(
            "[[pools]]\nname = \"{}\"\nversion = \"v3\"\naddress = \"{:?}\"\n{extra}",
            p.name, p.address
        )
    }

    fn cfg_of(body: &str) -> crate::config::Config {
        toml::from_str(&format!("threshold_pct = 3\nmax_move_pct = 1\n{body}"))
            .expect("a config")
    }

    /// One pool's edit must not decide anything about the pool beside it.
    ///
    /// The case this is about: a config where one entry cannot be applied - it
    /// would not resolve - while another was retuned. The retuned one has to
    /// land, and the untouched one has to stay exactly as it was rather than
    /// falling back to a global or being dropped.
    #[test]
    fn one_pool_that_cannot_be_applied_does_not_hold_up_the_others() {
        let (a, b, c) = (v3_pool("A", 0xa), v3_pool("B", 0xb), v3_pool("C", 0xc));
        let mut s = watching(&[a.clone(), b.clone()], 3.0);

        // C is in the file but could not be resolved, so it is absent from the
        // pools the reload carries.
        let cfg = cfg_of(&format!(
            "{}{}{}",
            pool_toml(&a, Some(8.0)),
            pool_toml(&b, None),
            pool_toml(&c, Some(2.0))
        ));
        s.retune(&Reload { cfg, pools: vec![a.clone(), b.clone()] });

        assert_eq!(s.watches[&a.pool_ref()].meter.threshold(), 8.0, "A was retuned");
        assert_eq!(s.watches[&b.pool_ref()].meter.threshold(), 3.0, "B was left alone");
        assert!(!s.watches.contains_key(&c.pool_ref()), "C could not be resolved");
        assert_eq!(s.watching(), 2);
    }

    /// A pool added to the file is watched from the next reload, on the
    /// connection the others are already using.
    #[test]
    fn a_pool_added_to_the_file_is_watched() {
        let (a, b) = (v3_pool("A", 0xa), v3_pool("B", 0xb));
        let mut s = watching(&[a.clone()], 3.0);
        let cfg = cfg_of(&format!("{}{}", pool_toml(&a, None), pool_toml(&b, Some(5.0))));
        s.retune(&Reload { cfg, pools: vec![a.clone(), b.clone()] });
        assert_eq!(s.watching(), 2);
        assert_eq!(s.watches[&b.pool_ref()].meter.threshold(), 5.0);
    }

    /// Dropping a pool from the file must not drop a position with it: the
    /// take-profit and the hold timer are the only things that ever sell it.
    #[test]
    fn a_pool_holding_a_position_is_kept_however_the_file_changed() {
        let (a, b) = (v3_pool("A", 0xa), v3_pool("B", 0xb));
        let http = Provider::<Http>::try_from("http://127.0.0.1:1").expect("a url");
        let (reports, _rx) = mpsc::channel(8);
        let mut inventory = Inventory::default();
        assert!(inventory.seed(
            b.base_currency().expect("currencies"),
            "B",
            18,
            ethers::types::U256::from(1_000u64),
            1.0,
        ));
        let mut s = Strategy::new(http, None, inventory, reports);
        s.watch(a.clone(), 3.0, 1.0, None, None);
        s.watch(b.clone(), 3.0, 1.0, None, None);

        // B is gone from the file, and A alone is left in it.
        let cfg = cfg_of(&pool_toml(&a, None));
        s.retune(&Reload { cfg, pools: vec![a.clone()] });
        assert!(s.watches.contains_key(&b.pool_ref()), "B holds a position and stays watched");

        // Once it holds nothing, the same reload drops it.
        let (a2, b2) = (a.clone(), b.clone());
        let mut s = watching(&[a2.clone(), b2.clone()], 3.0);
        s.retune(&Reload { cfg: cfg_of(&pool_toml(&a2, None)), pools: vec![a2.clone()] });
        assert!(!s.watches.contains_key(&b2.pool_ref()), "nothing held, so it is unwatched");
        assert_eq!(s.watching(), 1);
    }

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
