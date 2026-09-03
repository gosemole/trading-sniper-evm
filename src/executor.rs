//! Turning a decision into a transaction.
//!
//! Everything here is about *how* to trade, never *whether*: what a route is
//! worth right now, what to sign, which nonce, what gas. The decision arrives
//! from the strategy and this module carries it out.
//!
//! A route marked `auto_buy` is armed against one pool - by default the last
//! pool it swaps through, so the buy happens in the very pool the drop was
//! seen in. When that pool's price drops by its threshold inside a block, the
//! route is bought once. The size is fixed by the route's `amount_in`, so a run
//! of drops cannot escalate into a bigger and bigger position.
//!
//! A drop in a pool no armed route buys through is reported and otherwise
//! ignored: it is a signal with nothing to act on, not an error. The same goes
//! for a route pointed at a pool whose falling side is a different token than
//! the route buys - that is a misconfiguration, and refusing to buy is the only
//! safe reading of it.
//!
//! One round trip stands between a signal and a signed transaction, and it is
//! the send. The buy is priced entirely from memory: the pool that dropped
//! brought its own price, liquidity and fee in the very log that raised the
//! signal, the other pools on the route come from the last calibration pass,
//! and what a hook takes on top is the correction that pass measured. See
//! `model_quote` for the arithmetic and the conditions under which it refuses
//! to answer - and `amountOutMinimum` is that price less the route's own
//! slippage tolerance, so what the trade will accept is recomputed for this
//! pair every single time.
//!
//! Nothing here asks the router to rehearse the trade, so nothing proves the
//! wallet can afford it except a balance check run for that purpose alone -
//! see `quote`. Allowance is checked once, when the route is armed, and relies
//! on the unlimited approval this bot's own `--approve` sets up; it is not
//! re-proven per buy. `--swap` and `--sell-all`, where nobody is racing, still
//! run the real rehearsal through `execute::verify` before sending.
//!
//! Routes are resolved once at startup: recovering a PoolKey takes a couple of
//! dozen archive reads, which is fine before the stream opens and far too slow
//! between a drop and a buy.

use crate::config::Config;
use crate::execute;
use crate::strategy::Signal;
use crate::pool::Pool;
use crate::route::{format_units, parse_pool_ref, PoolRef, Route};
use crate::swap;
use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

struct Plan {
    route: Route,
    cooldown: Duration,
    /// Gas limit to sign with, measured once when the route is armed and
    /// refreshed after each send. Keeping it here is what takes `estimate_gas`
    /// out of the path between a drop and a broadcast.
    gas_limit: AtomicU64,
    /// What the route actually pays as a fraction of what the local tick walk
    /// says it should, in parts per million. 1_000_000 is "the model is right";
    /// anything under that is being taken by something the model cannot see -
    /// in practice a hook charging its own fee on top of the pool's. Zero means
    /// it has not been measured yet.
    yield_ppm: AtomicU64,
    /// The same, measured in the selling direction. A hook is free to charge
    /// differently each way, so this is measured rather than assumed - but it
    /// can only be measured while the token is actually held, so it falls back
    /// to the buying figure until then.
    sell_yield_ppm: AtomicU64,
    /// Last known price and liquidity of every pool on the route, so a quote
    /// can be worked out without asking anyone.
    state: Mutex<Option<RouteState>>,
}

/// Everything the model needs to price the route, and when it was true.
struct RouteState {
    at: Instant,
    hops: Vec<HopState>,
}

#[derive(Clone, Copy)]
struct HopState {
    sqrt_p: f64,
    liquidity: u128,
    lp_fee: u32,
    /// Both halves of the pool's protocol fee, in hundredths of a bip, kept
    /// unresolved because one snapshot has to serve both directions: the same
    /// state prices the buy and the sale that undoes it.
    protocol_fee_0for1: u32,
    protocol_fee_1for0: u32,
}

impl HopState {
    /// The whole fee this hop's input pays, protocol cut included. See
    /// `depth::PoolState::swap_fee` - the same arithmetic, on a snapshot.
    fn swap_fee(&self, zero_for_one: bool) -> u32 {
        let pf = match zero_for_one {
            true => self.protocol_fee_0for1,
            false => self.protocol_fee_1for0,
        } as u64;
        let lp = self.lp_fee as u64;
        (pf + lp - pf * lp / PPM).min(PPM) as u32
    }
}

/// A transaction that actually went out, and what it was expected to produce.
pub struct Fill {
    pub hash: ethers::types::H256,
    /// Raw units the trade was asked to move on the way in. For a sale this is
    /// exactly what leaves the position.
    pub sold: U256,
    /// Quoted output in raw units. What lands can differ by less than the
    /// slippage tolerance, and never by more - `amountOutMinimum` sees to that.
    pub amount_out: U256,
}

/// What one buy had to go and find out, gathered in one place so the send does
/// not have to reach back for any of it.
struct Quoted {
    amount_out: U256,
    /// How long the asking took, so the log separates the network from
    /// everything else - which is otherwise invisible and dominates.
    took: Duration,
    /// Deferred: a gas price that could not be read only matters if we send.
    fees: Result<(U256, U256)>,
    /// The nonce the node reported while the quote was in flight, if asked.
    nonce_seen: Option<u64>,
}

/// Fixed-point scale for `yield_ppm`.
const PPM: u64 = 1_000_000;

/// How stale the cached pool state may be before a fast quote is refused.
/// Pools other than the one that just dropped move slowly enough that a few
/// minutes is fine; past that, asking the router is cheaper than being wrong.
const STATE_STALE_AFTER: Duration = Duration::from_secs(120);

/// How far a fast-quoted swap may move a pool's own price before the model is
/// no longer trusted. Inside a tick range the arithmetic is exact; past one it
/// silently overstates the output, and this is what keeps it from getting
/// there. Measured impact on the configured sizes is under 0.01%.
const MAX_MODELLED_IMPACT: f64 = 0.005;

/// How far the measured yield has to move before it is worth saying out loud,
/// in parts per million. A hook that changes its cut is a change of terms, not
/// noise; measurement jitter across sizes came out at 3 ppm.
const YIELD_ALERT_PPM: u64 = 500;

/// The most a route may be measured to take beyond its pools' stated fees
/// before the bot refuses to price it at all. A hook that suddenly keeps a
/// quarter of every swap is not a fee to model around; it is a reason to stop.
const MAX_UNSTATED_FEE_PPM: u64 = 250_000;

/// What to sign with when the route has never been measured - enough for a
/// two-leg swap, and unused gas is refunded either way.
const GAS_FALLBACK: u64 = 1_200_000;

pub struct Executor {
    http: Provider<Http>,
    router: Address,
    manager: Address,
    permit2: Address,
    /// Seconds between yield measurements; 0 turns them off.
    calibrate_secs: u64,
    wallet: LocalWallet,
    owner: Address,
    /// Sign and send, rather than only reporting what would have been sent.
    execute: bool,
    /// Trigger pool -> what to buy when it drops.
    plans: HashMap<PoolRef, Plan>,
    last_fire: Mutex<HashMap<PoolRef, Instant>>,
    /// The gas price, kept current from the block stream instead of asked for
    /// on every buy.
    fees: Arc<swap::FeeWatch>,
    /// Next nonce to hand out. Held only long enough to take a number, never
    /// across a network call: two buys may be in flight at once, and asking the
    /// node for a nonce while the first is unmined would return the same one.
    /// `None` means "ask the node", which is also how a failed broadcast is
    /// recovered from - the gap is refilled rather than left to stall the queue.
    next_nonce: Mutex<Option<u64>>,
}

impl Executor {
    /// Resolve every armed route, or `None` when none is armed.
    ///
    /// Failing here rather than at the first drop is deliberate: a bad route,
    /// a missing key or a missing approval should stop the process at startup,
    /// while someone is watching, not silently do nothing at the one moment it
    /// was supposed to act.
    pub async fn build(
        http: &Provider<Http>,
        cfg: &Config,
        manager: Address,
        chain_id: u64,
        execute: bool,
    ) -> Result<Option<Arc<Self>>> {
        let armed: Vec<_> = cfg.routes.iter().filter(|r| r.auto_buy).collect();
        if armed.is_empty() {
            return Ok(None);
        }
        let router = swap::resolve_addr(&cfg.universal_router, None, "universal_router")?;
        let permit2 = swap::resolve_addr(&cfg.permit2, Some(swap::PERMIT2_DEFAULT), "permit2")?;
        let wallet = swap::load_wallet(cfg, chain_id).context("auto_buy needs a signing key")?;
        let owner = wallet.address();

        let mut plans = HashMap::new();
        for rc in armed {
            let route = Route::resolve(http, manager, rc, &cfg.tokens)
                .await
                .with_context(|| format!("auto_buy route '{}'", rc.name))?;
            // Default to the pool the route ends in: that is the one whose
            // price the buy is reacting to.
            let raw = rc
                .trigger_pool
                .clone()
                .unwrap_or_else(|| rc.pools[rc.pools.len() - 1].clone());
            let trigger = parse_pool_ref(&raw)
                .with_context(|| format!("route '{}': trigger pool", rc.name))?;
            anyhow::ensure!(
                !plans.contains_key(&trigger),
                "two auto_buy routes are armed against pool {trigger}; only one can fire"
            );

            // A trigger nobody subscribes to is a route that can never fire.
            // Not fatal - the pool may be about to be added - but silent
            // failure is exactly what this is otherwise.
            let watched = cfg.pools.iter().any(|p| {
                let named = match &p.pool_id {
                    Some(id) => parse_pool_ref(id).ok(),
                    None => parse_pool_ref(&p.address).ok(),
                };
                named == Some(trigger)
            });
            if !watched {
                warn!(
                    route = %rc.name, trigger = ?trigger,
                    "trigger pool is not in [[pools]], so nothing watches it - this route \
                     will never fire"
                );
            }

            preflight(http, &route, owner, permit2, router).await?;
            if rc.cooldown_secs == 0 {
                warn!(
                    route = %rc.name,
                    "cooldown_secs = 0: every signal buys, and a dip lasting ten blocks buys \
                     ten times"
                );
            }
            info!(
                route = %rc.name,
                trigger = %trigger,
                spend = format!("{} {}", format_units(route.amount_in, route.input.decimals), route.input.symbol),
                buy = %route.output.symbol,
                slippage_pct = route.max_slippage_pct,
                cooldown_s = rc.cooldown_secs,
                mode = if execute { "LIVE" } else { "dry run" },
                "auto-buy armed"
            );
            // Measured now, while nobody is waiting, so the hot path never has
            // to ask. A route that cannot be estimated yet still gets armed:
            // the fallback is generous and the next send re-measures.
            let probe = execute::pending_swap(router, &route, U256::one(), execute::deadline_in(600))?;
            let gas_limit = match swap::measure_gas(http, owner, &probe).await {
                Ok(g) => g.min(U256::from(u64::MAX)).as_u64(),
                Err(e) => {
                    warn!(route = %rc.name, err = %format!("{e:#}"), gas = GAS_FALLBACK,
                          "could not measure gas yet; using the fallback");
                    GAS_FALLBACK
                }
            };
            info!(route = %rc.name, gas_limit, "gas measured");

            plans.insert(
                trigger,
                Plan {
                    route,
                    cooldown: Duration::from_secs(rc.cooldown_secs),
                    gas_limit: AtomicU64::new(gas_limit),
                    yield_ppm: AtomicU64::new(0),
                    sell_yield_ppm: AtomicU64::new(0),
                    state: Mutex::new(None),
                },
            );
        }

        // Started before the first signal, so the first buy already prices off a
        // header rather than off a lookup.
        let fees = Arc::new(swap::FeeWatch::default());
        fees.watch(cfg.ws_url.clone(), http.clone());
        swap::keep_warm(http.clone());

        let me = Arc::new(Self {
            http: http.clone(),
            router,
            manager,
            permit2,
            calibrate_secs: cfg.calibrate_secs,
            wallet,
            owner,
            execute,
            plans,
            fees,
            last_fire: Mutex::new(HashMap::new()),
            next_nonce: Mutex::new(None),
        });
        me.calibrate();
        Ok(Some(me))
    }

    /// Keep measuring what each armed route really pays against what the local
    /// model expects.
    ///
    /// The two numbers should agree: the tick walk knows every pool's stated
    /// fee and crosses the same ticks. Where they do not, the difference is a
    /// hook taking a cut the PoolKey does not mention - and that is worth
    /// watching, because it is a term of the trade that can change without any
    /// visible transaction. It runs in the background and touches nothing in
    /// the path of a buy.
    fn calibrate(self: &Arc<Self>) {
        if self.calibrate_secs == 0 {
            return;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let every = Duration::from_secs(me.calibrate_secs);
            loop {
                for plan in me.plans.values() {
                    if let Err(e) = me.measure_yield(plan).await {
                        warn!(route = %plan.route.name, err = %format!("{e:#}"),
                              "could not measure route yield");
                    }
                }
                tokio::time::sleep(every).await;
            }
        });
    }

    async fn measure_yield(&self, plan: &Plan) -> Result<()> {
        let route = &plan.route;
        // Both readings are pinned to one block. Taken at the head they would
        // land on different ones whenever the pool is busy, and the difference
        // between them would then be the price moving rather than a fee: that
        // is exactly how a 1% cut first measured as 2.46%.
        let at = self
            .http
            .get_block_number()
            .await
            .context("eth_blockNumber")?
            .as_u64();
        let local = route
            .quote(&self.http, self.manager, Some(at))
            .await
            .context("local tick walk")?;
        let onchain = execute::verify(
            &self.http,
            self.router,
            self.owner,
            route,
            local.amount_out,
            execute::deadline_in(600),
            Some(at),
        )
        .await
        .context("router quote")?;

        let expected = local.amount_out;
        anyhow::ensure!(!expected.is_zero(), "the local quote is zero");
        // Both sides are raw token units of the same token, so the ratio is
        // exact in integer arithmetic - no float round trip in the number that
        // would later be used to price a trade.
        let ppm = (onchain.amount_out * U256::from(PPM) / expected)
            .min(U256::from(u64::MAX))
            .as_u64();

        // The same pass records what every pool looked like at that block, so
        // a fast quote has something to price the hops the signal says nothing
        // about. The pool that drops brings its own state with the signal.
        *plan.state.lock().await = Some(RouteState {
            at: Instant::now(),
            hops: local
                .hops
                .iter()
                .map(|h| HopState {
                    sqrt_p: h.sqrt_p,
                    liquidity: h.liquidity,
                    lp_fee: h.lp_fee,
                    protocol_fee_0for1: h.protocol_fee_0for1,
                    protocol_fee_1for0: h.protocol_fee_1for0,
                })
                .collect(),
        });

        // The same measurement the other way round, while we hold enough of
        // the token to ask. A hook may charge differently by direction, and the
        // only way to know is to look; until then a sale borrows the buying
        // figure, which is an assumption rather than a measurement.
        self.measure_sell_yield(plan, at).await;

        let previous = plan.yield_ppm.swap(ppm, Ordering::Relaxed);
        let taken_pct = (PPM.saturating_sub(ppm.min(PPM))) as f64 / 10_000.0;
        let moved = previous != 0 && previous.abs_diff(ppm) > YIELD_ALERT_PPM;
        if moved {
            warn!(
                route = %route.name,
                was_pct = (PPM.saturating_sub(previous.min(PPM))) as f64 / 10_000.0,
                now_pct = taken_pct,
                "the route's unstated fee CHANGED"
            );
        } else {
            info!(
                route = %route.name,
                unstated_fee_pct = format!("{taken_pct:.4}"),
                yield_ppm = ppm,
                "route yield measured"
            );
        }
        Ok(())
    }

    /// The address everything is signed and settled from.
    pub fn owner(&self) -> Address {
        self.owner
    }

    /// The route armed against this pool, for a caller that needs to trade it
    /// in the other direction.
    pub fn route_for(&self, key: PoolRef) -> Option<&Route> {
        self.plans.get(&key).map(|p| &p.route)
    }

    /// React to one big-sell signal: decide, price, sign, send.
    pub async fn on_drop(self: &Arc<Self>, pool: &Pool, sig: &Signal) -> Result<Option<Fill>> {
        let Some((key, plan)) = self.armed_for(pool) else {
            return Ok(None);
        };
        if !self.claim_turn(key, plan, pool).await {
            return Ok(None);
        }

        let route = &plan.route;
        let started = Instant::now();
        let deadline = execute::deadline_in(120);
        let quoted = self.quote(plan, key, sig).await?;

        let min_out = execute::apply_slippage(quoted.amount_out, route.max_slippage_pct);
        anyhow::ensure!(
            !min_out.is_zero(),
            "route '{}': amountOutMinimum rounds to zero",
            route.name
        );
        let tx = execute::pending_swap(self.router, route, min_out, deadline)?;

        let amount = |v: U256, t: &crate::route::Token| {
            format!("{} {}", format_units(v, t.decimals), t.symbol)
        };
        info!(
            pool = %pool.name,
            route = %route.name,
            drop_pct = format!("-{:.3}%", sig.drop_pct),
            spend = amount(route.amount_in, &route.input),
            quoted = amount(quoted.amount_out, &route.output),
            priced_by = "model",
            quote_ms = quoted.took.as_millis(),
            min_out = amount(min_out, &route.output),
            slippage_pct = route.max_slippage_pct,
            took_ms = started.elapsed().as_millis(),
            "BUY THE DIP"
        );

        if !self.execute {
            info!(route = %route.name, "dry run - not sent; start with --execute to buy for real");
            return Ok(None);
        }
        let amount_out = quoted.amount_out;
        let hash = self.broadcast(key, plan, &tx, quoted).await?;
        Ok(Some(Fill { hash, sold: route.amount_in, amount_out }))
    }

    /// Sell everything held of what a route buys, back down that same route.
    ///
    /// The size is the wallet's actual balance rather than anything remembered:
    /// what is there is what can be sold, and a position built by several buys
    /// or topped up by hand is still one balance. Priced by the model when one
    /// is armed and calibrated; unlike a buy, this is not racing anyone, so a
    /// model that cannot answer falls back to asking the router even when that
    /// costs a bisection, rather than skipping the sale.
    pub async fn sell_all(
        self: &Arc<Self>,
        key: PoolRef,
        route: &Route,
        slippage_pct: f64,
        limit: Option<U256>,
    ) -> Result<Option<Fill>> {
        let token = route.output.address;
        let balance = swap::balance_of(&self.http, token, self.owner).await?;
        // The lesser of what we hold and what we bought. The balance alone
        // would sell tokens that arrived some other way - a manual buy, a
        // transfer - which are not this bot's to sell. What we bought alone is
        // a quote rather than a measurement, so it can exceed what actually
        // landed and would simply fail.
        let size = limit.map_or(balance, |l| l.min(balance));
        if size.is_zero() {
            info!(
                token = %route.output.symbol,
                %balance,
                limit = ?limit,
                "nothing to sell"
            );
            return Ok(None);
        }
        if size < balance {
            info!(
                token = %route.output.symbol,
                selling = %format_units(size, route.output.decimals),
                held = %format_units(balance, route.output.decimals),
                "selling only what this bot bought"
            );
        }
        let sell = route.reversed(size);
        if sell.input.address != Address::zero() {
            let (erc20, p2) =
                swap::check_approvals(&self.http, sell.input.address, self.owner, self.permit2, self.router)
                    .await?;
            anyhow::ensure!(
                erc20 >= size && p2 >= size,
                "{} is not approved for the router (erc20->permit2 {erc20}, permit2->router {p2}); \
                 run --approve {} --execute first",
                sell.input.symbol,
                sell.input.symbol
            );
        }

        let deadline = execute::deadline_in(300);
        // The model gets first refusal here too, when there is a plan to price
        // it from - a sale of a token no route buys has no calibration to draw
        // on and goes straight to the router. It matters more here than on a
        // buy: a reversed route that ends on v3 is quoted by bisection, which
        // is twenty-odd calls rather than one, and unlike a buy a sale is not
        // racing anyone, so the fallback stays.
        let modelled = match self.plans.get(&key) {
            Some(plan) => {
                // The fee check uses the figure measured in this direction when
                // the token was held long enough to measure it; otherwise the
                // buying figure, which assumes the hook charges the same both
                // ways. Failing it does not stop the sale - the router prices
                // it instead, and prices it honestly.
                let ppm = match plan.sell_yield_ppm.load(Ordering::Relaxed) {
                    0 => plan.yield_ppm.load(Ordering::Relaxed),
                    m => m,
                };
                match self.unstated_fee_acceptable(plan, ppm) {
                    true => self.model_quote(plan, &sell, None, ppm).await,
                    false => None,
                }
            }
            None => None,
        };
        let (amount_out, priced_by) = match modelled {
            Some(a) => (a, "model"),
            None => (
                execute::verify(
                    &self.http, self.router, self.owner, &sell, U256::zero(), deadline, None,
                )
                .await
                .context("quoting the sale")?
                .amount_out,
                "router",
            ),
        };
        let min_out = execute::apply_slippage(amount_out, slippage_pct);
        anyhow::ensure!(!min_out.is_zero(), "the sale's amountOutMinimum rounds to zero");
        let tx = execute::pending_swap(self.router, &sell, min_out, deadline)?;

        info!(
            route = %sell.name,
            sell = format!("{} {}", format_units(size, sell.input.decimals), sell.input.symbol),
            quoted = format!("{} {}", format_units(amount_out, sell.output.decimals), sell.output.symbol),
            priced_by,
            min_out = format!("{} {}", format_units(min_out, sell.output.decimals), sell.output.symbol),
            slippage_pct,
            "TAKE PROFIT"
        );
        if !self.execute {
            info!(route = %sell.name, "dry run - not sent; start with --execute to sell for real");
            return Ok(None);
        }

        let fees = self.fees.params(&self.http).await.context("reading the gas price")?;
        let gas = swap::measure_gas(&self.http, self.owner, &tx)
            .await
            .unwrap_or_else(|_| U256::from(GAS_FALLBACK));
        let seen = swap::pending_nonce(&self.http, self.owner).await.ok();
        let nonce = self.claim_nonce(seen).await?;
        match swap::send_nowait(&self.http, &self.wallet, &tx, nonce.into(), fees, gas).await {
            Ok(hash) => {
                info!(route = %sell.name, ?hash, nonce, "sold");
                Ok(Some(Fill { hash, sold: size, amount_out }))
            }
            Err(e) => {
                *self.next_nonce.lock().await = None;
                Err(e)
            }
        }
    }

    /// The route armed against this pool, if it is one this signal should act
    /// on at all. Everything it turns down, it says why.
    fn armed_for(&self, pool: &Pool) -> Option<(PoolRef, &Plan)> {
        let fell = pool.base_symbol.as_deref().unwrap_or("?");
        let key = pool.pool_ref();
        let Some(plan) = self.plans.get(&key) else {
            warn!(
                pool = %pool.name, token = %fell, id = %key,
                "big sell, but no auto_buy route buys through this pool - not buying"
            );
            return None;
        };
        // The route has to buy what actually fell. A route armed against a pool
        // whose base side is the other token would buy on someone else's dip.
        if let Some(base) = pool.base_currency() {
            if base != plan.route.output.address {
                warn!(
                    pool = %pool.name,
                    route = %plan.route.name,
                    buys = %plan.route.output.symbol,
                    dropped = %fell,
                    "route buys a different token than the one that fell here - not buying; \
                     check base_token on the pool, or trigger_pool on the route"
                );
                return None;
            }
        }
        Some((key, plan))
    }

    /// Claim this route's turn to buy, or report how long is left.
    ///
    /// The turn is claimed before any work and stays claimed even if the buy
    /// then fails: a route that reverts every block should not retry every
    /// block.
    async fn claim_turn(&self, key: PoolRef, plan: &Plan, pool: &Pool) -> bool {
        let mut last = self.last_fire.lock().await;
        if let Some(waited) = last.get(&key).map(Instant::elapsed) {
            if waited < plan.cooldown {
                info!(
                    pool = %pool.name,
                    route = %plan.route.name,
                    again_in_s = (plan.cooldown - waited).as_secs(),
                    "drop seen, still cooling down"
                );
                return false;
            }
        }
        last.insert(key, Instant::now());
        true
    }

    /// Everything a buy needs that could not be worked out from memory alone.
    ///
    /// The router is never asked here: the model prices the trade from the
    /// dropping pool's own log plus the last calibration pass, or the buy is
    /// skipped - see `model_quote`. That is what keeps a buy to one round trip
    /// (the send) instead of two, at the cost of the router's rehearsal - which
    /// is why nothing here reads a balance either: the caller already checked
    /// the wallet can cover this against its own tracked figure before this was
    /// ever called, and a second read here would be the very round trip that
    /// tracking it locally exists to avoid. Fees and nonce are asked for
    /// together, so the slower of the two sets the pace rather than their sum -
    /// and the gas price usually answers from the last block header without
    /// going anywhere at all.
    async fn quote(
        &self,
        plan: &Plan,
        key: PoolRef,
        sig: &Signal,
    ) -> Result<Quoted> {
        let started = Instant::now();
        anyhow::ensure!(
            self.unstated_fee_acceptable(plan, plan.yield_ppm.load(Ordering::Relaxed)),
            "route '{}': not buying - see the fee check above",
            plan.route.name
        );
        // The fee the swap was actually charged when the log carries one (v4,
        // hook override and all), else the pool's own fixed fee (v3 logs have
        // no fee word because the fee cannot change). A v4 pool that logged
        // nothing is left to the calibration snapshot rather than priced with
        // its PoolKey's dynamic-fee flag, which is a flag and not a fee.
        let fee = sig.lp_fee.or_else(|| {
            plan.route
                .hops
                .iter()
                .find(|h| h.pool_ref() == key)
                .map(|h| h.fee)
                .filter(|f| u64::from(*f) < PPM)
        });
        // v4 emits the fee the swap was CHARGED, which is already the protocol
        // cut and the LP fee combined (`Pool.swap`: `swapFee = protocolFee == 0
        // ? lpFee : calculateSwapFee(protocolFee, lpFee)`), so it goes in whole
        // with no protocol fee left to add. A v3 log carries no fee word at all
        // and falls back to the pool's fixed `fee()`, where the protocol's cut
        // comes out of the LPs' share and the swapper pays no more either way.
        // `model_quote` checks this reading against the calibration snapshot
        // and says so if the chain disagrees.
        let fresh = fee.map(|charged| {
            (
                key,
                HopState {
                    sqrt_p: crate::pool::sqrt_to_f64(sig.sqrt),
                    liquidity: sig.liquidity,
                    lp_fee: charged,
                    protocol_fee_0for1: 0,
                    protocol_fee_1for0: 0,
                },
            )
        });
        let ppm = plan.yield_ppm.load(Ordering::Relaxed);
        let modelled = self.model_quote(plan, &plan.route, fresh, ppm).await;
        // The model prices every buy or none does: there is no router
        // fallback on this path. Skip the buy instead of guessing; it costs
        // nothing but this one drop, and there will be another.
        let Some(amount_out) = modelled else {
            anyhow::bail!(
                "route '{}': the model could not price this trade right now (state too \
                 stale, a hop's pool changed count, or the move too large to trust the \
                 in-range formula) - skipping rather than guessing",
                plan.route.name
            );
        };

        // A nonce is only spent by a real send, so a dry run does not ask.
        let want_nonce = self.execute;
        let (fees, nonce_seen) = tokio::join!(
            self.fees.params(&self.http),
            async {
                match want_nonce {
                    true => swap::pending_nonce(&self.http, self.owner).await.ok(),
                    false => None,
                }
            },
        );
        Ok(Quoted {
            amount_out,
            took: started.elapsed(),
            fees,
            nonce_seen,
        })
    }

    /// The calibration's job as a check, alongside its job as a term.
    ///
    /// What a route is measured to take beyond its stated fees is priced in by
    /// `model_quote`, but only within reason: past `MAX_UNSTATED_FEE_PPM` the
    /// figure stops being a fee worth paying and starts being a sign that the
    /// route is not what it was measured to be. So a route never measured is
    /// not traded, and one keeping more than the limit is refused and said so.
    /// Neither is a number to quietly fold into a price.
    fn unstated_fee_acceptable(&self, plan: &Plan, yield_ppm: u64) -> bool {
        if yield_ppm == 0 {
            warn!(
                route = %plan.route.name,
                "not priced: the route's fee has not been measured yet (see calibrate_secs)"
            );
            return false;
        }
        let unstated = PPM.saturating_sub(yield_ppm);
        if unstated > MAX_UNSTATED_FEE_PPM {
            warn!(
                route = %plan.route.name,
                unstated_fee_pct = unstated as f64 / 10_000.0,
                limit_pct = MAX_UNSTATED_FEE_PPM as f64 / 10_000.0,
                "REFUSED: the route keeps far more than its pools state - not trading it"
            );
            return false;
        }
        true
    }

    /// Sign and broadcast, then let go: the receipt and the next gas figure are
    /// both reports about a transaction that is already on its way.
    async fn broadcast(
        self: &Arc<Self>,
        key: PoolRef,
        plan: &Plan,
        tx: &crate::swap::PendingTx,
        quoted: Quoted,
    ) -> Result<ethers::types::H256> {
        let name = plan.route.name.clone();
        let fees = quoted.fees.context("reading the gas price")?;
        let gas_limit = U256::from(plan.gas_limit.load(Ordering::Relaxed));
        let nonce = self.claim_nonce(quoted.nonce_seen).await?;
        let started = Instant::now();
        match swap::send_nowait(&self.http, &self.wallet, tx, nonce.into(), fees, gas_limit).await {
            Ok(hash) => {
                info!(route = %name, ?hash, nonce, %gas_limit,
                      send_ms = started.elapsed().as_millis(), "sent");
                self.remeasure_gas(key);
                Ok(hash)
            }
            Err(e) => {
                // The number was taken but never used, and every later
                // transaction would queue behind the hole it leaves.
                *self.next_nonce.lock().await = None;
                Err(e)
            }
        }
    }

    /// Re-measure a route's gas in the background, so the next buy signs with a
    /// figure that reflects the pool as it is now.
    fn remeasure_gas(self: &Arc<Self>, key: PoolRef) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let Some(plan) = me.plans.get(&key) else { return };
            let probe = match execute::pending_swap(
                me.router,
                &plan.route,
                U256::one(),
                execute::deadline_in(600),
            ) {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Ok(g) = swap::measure_gas(&me.http, me.owner, &probe).await {
                plan.gas_limit
                    .store(g.min(U256::from(u64::MAX)).as_u64(), Ordering::Relaxed);
            }
        });
    }

    /// What the route yields sold back down, against what the model expects.
    ///
    /// Only possible while the token is held: quoting a sale means settling it
    /// through Permit2, and a balance of nothing settles nothing.
    async fn measure_sell_yield(&self, plan: &Plan, at: u64) {
        let route = &plan.route;
        let held = match swap::balance_of(&self.http, route.output.address, self.owner).await {
            Ok(h) if !h.is_zero() => h,
            _ => return,
        };
        let sell = route.reversed(held);
        let local = match sell.quote(&self.http, self.manager, Some(at)).await {
            Ok(q) if !q.amount_out.is_zero() => q,
            _ => return,
        };
        let onchain = match execute::verify(
            &self.http,
            self.router,
            self.owner,
            &sell,
            local.amount_out,
            execute::deadline_in(600),
            Some(at),
        )
        .await
        {
            Ok(q) => q,
            Err(e) => {
                tracing::debug!(route = %route.name, err = %format!("{e:#}"),
                                "could not measure the selling direction");
                return;
            }
        };
        let ppm = (onchain.amount_out * U256::from(PPM) / local.amount_out)
            .min(U256::from(u64::MAX))
            .as_u64();
        let previous = plan.sell_yield_ppm.swap(ppm, Ordering::Relaxed);
        let taken = (PPM.saturating_sub(ppm.min(PPM))) as f64 / 10_000.0;
        if previous != 0 && previous.abs_diff(ppm) > YIELD_ALERT_PPM {
            warn!(
                route = %route.name,
                was_pct = (PPM.saturating_sub(previous.min(PPM))) as f64 / 10_000.0,
                now_pct = taken,
                "the SELLING side's unstated fee CHANGED"
            );
        } else {
            info!(
                route = %route.name,
                unstated_fee_pct = format!("{taken:.4}"),
                yield_ppm = ppm,
                "sell yield measured"
            );
        }
    }

    /// Price a route without asking anyone, or say why not.
    ///
    /// The pool that just moved needs no lookup: its price, its liquidity and
    /// the fee it actually charged all arrive in the log that raised the
    /// signal, and they describe the pool as of that very swap. The others come
    /// from the last calibration pass, which is fair because they are not the
    /// ones that moved.
    ///
    /// `yield_ppm` is what calibration measured this direction to actually pay
    /// against what this same arithmetic predicted, and it IS applied here as
    /// a term: whatever a hook takes on top of the pools' stated fees is real
    /// money, and a quote that leaves it out is optimistic by exactly that
    /// much. It is clamped at 1.0 - a route measured to pay more than the
    /// model says is a stale snapshot or measurement noise, never a bonus to
    /// price in. It stays a guard as well: the caller checks
    /// `unstated_fee_acceptable` first and does not get here at all when the
    /// unstated fee is out of hand.
    ///
    /// Works in either direction: the cached state describes a pool, not a
    /// direction, so a reversed route reads the same numbers and only the
    /// `zeroForOne` of each hop differs.
    ///
    /// Each hop is priced from the freshest state there is for its pool: the
    /// signal's own log where this is the pool that dropped, else the last
    /// calibration snapshot - which is only consulted for hops the signal says
    /// nothing about, so a single-hop route never waits on a snapshot at all.
    ///
    /// Returns `None` whenever anything is missing or out of range; a buy is
    /// then skipped and a sale asks the router. Being slow is recoverable;
    /// signing a wrong minimum is not.
    async fn model_quote(
        &self,
        plan: &Plan,
        route: &Route,
        fresh: Option<(PoolRef, HopState)>,
        yield_ppm: u64,
    ) -> Option<U256> {
        // Never priced by a measurement that does not exist. The callers check
        // this too, and both of them refusing is the point.
        if yield_ppm == 0 {
            return None;
        }
        let guard = plan.state.lock().await;
        // Keyed by pool rather than by position, so the same state serves a
        // route walked in either order. Absent or stale, it simply is not
        // there to fall back on.
        let known: Option<HashMap<PoolRef, HopState>> = guard
            .as_ref()
            .filter(|s| {
                s.at.elapsed() <= STATE_STALE_AFTER && s.hops.len() == plan.route.hops.len()
            })
            .map(|s| {
                plan.route
                    .hops
                    .iter()
                    .map(|h| h.pool_ref())
                    .zip(s.hops.iter().copied())
                    .collect()
            });

        let mut amount = crate::route::u256_to_f64(route.amount_in);
        for hop in &route.hops {
            let mut here = match fresh {
                Some((p, s)) if p == hop.pool_ref() => s,
                _ => *known.as_ref()?.get(&hop.pool_ref())?,
            };
            // The one thing about the log the chain gets to overrule. A log fee
            // landing exactly on the pool's stored LP fee while slot0 says a
            // protocol fee is charged cannot be a combined figure: combining a
            // non-zero protocol fee always lands strictly above the LP fee. So
            // that log is reporting the LP fee alone, and pricing it as the
            // total would undercharge every buy by the protocol's cut. Take the
            // snapshot's reading instead, and say so rather than silently
            // disagreeing with the log.
            if let Some(snap) = known.as_ref().and_then(|k| k.get(&hop.pool_ref())) {
                let from_log = matches!(fresh, Some((p, _)) if p == hop.pool_ref());
                let charges_protocol = snap.swap_fee(hop.zero_for_one()) != snap.lp_fee;
                if from_log && charges_protocol && here.lp_fee == snap.lp_fee {
                    warn!(
                        route = %route.name,
                        pool = %hop.pool_ref(),
                        log_fee = here.lp_fee,
                        "the swap log reports the LP fee alone, not the combined swap fee - \
                         pricing this hop from slot0's protocol fee instead"
                    );
                    here.protocol_fee_0for1 = snap.protocol_fee_0for1;
                    here.protocol_fee_1for0 = snap.protocol_fee_1for0;
                }
            }
            let (out, after) = crate::depth::in_range_out(
                here.sqrt_p,
                here.liquidity,
                here.swap_fee(hop.zero_for_one()),
                hop.zero_for_one(),
                amount,
            )?;
            // Past a tick boundary the arithmetic stops being exact and starts
            // being optimistic, so it is not used there.
            let impact = ((after / here.sqrt_p).powi(2) - 1.0).abs();
            if !impact.is_finite() || impact > MAX_MODELLED_IMPACT {
                return None;
            }
            amount = out;
        }

        // What the pools state, less what this direction was measured to pay
        // beyond them.
        let amount = amount * yield_ppm.min(PPM) as f64 / PPM as f64;

        let raw = crate::route::f64_to_u256_pub(amount);
        (!raw.is_zero()).then_some(raw)
    }

    /// Take the next nonce.
    ///
    /// `observed` is what the node said a moment ago, fetched beside the quote.
    /// The higher of the two wins: the node knows about transactions this
    /// process did not send, and the counter knows about one it sent so
    /// recently that the node may not have counted it yet. Either alone is
    /// wrong, and being wrong here costs the whole buy.
    async fn claim_nonce(&self, observed: Option<u64>) -> Result<u64> {
        let mut slot = self.next_nonce.lock().await;
        let n = match (*slot, observed) {
            (Some(local), Some(seen)) => local.max(seen),
            (Some(local), None) => local,
            (None, Some(seen)) => seen,
            (None, None) => swap::pending_nonce(&self.http, self.owner).await?,
        };
        *slot = Some(n + 1);
        Ok(n)
    }
}

/// Everything that can be checked before the first drop: is there anything to
/// spend, and may the router spend it.
async fn preflight(
    http: &Provider<Http>,
    route: &Route,
    owner: Address,
    permit2: Address,
    router: Address,
) -> Result<()> {
    let balance = swap::balance_of(http, route.input.address, owner).await?;
    if balance < route.amount_in {
        warn!(
            route = %route.name,
            have = %format_units(balance, route.input.decimals),
            need = %format_units(route.amount_in, route.input.decimals),
            token = %route.input.symbol,
            "not enough to buy with; the first drop will fail unless this is topped up"
        );
    }
    if route.input.address != Address::zero() {
        let (erc20, p2) =
            swap::check_approvals(http, route.input.address, owner, permit2, router).await?;
        anyhow::ensure!(
            erc20 >= route.amount_in && p2 >= route.amount_in,
            "route '{}': {} is not approved for the router (erc20->permit2 {erc20}, \
             permit2->router {p2}); run --approve {} --execute first",
            route.name,
            route.input.symbol,
            route.input.symbol
        );
    }
    Ok(())
}
