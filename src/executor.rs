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
//! the send - not almost, but exactly: the price comes from memory, the gas
//! price from the block stream, and the nonce from this process's own counter -
//! filled at startup and carried forward from there, which is the whole answer
//! as long as nothing else signs with this key while the bot runs, and so the
//! FIRST buy costs no more than any later one. The buy is priced entirely from memory: the pool that dropped
//! brought its own price, liquidity and fee in the very log that raised the
//! signal, the other pools on the route come from the last calibration pass,
//! and what a hook takes on top is the correction that pass measured. See
//! `model_quote` for the arithmetic and the conditions under which it refuses
//! to answer - and `amountOutMinimum` is that price less the route's own
//! slippage tolerance, so what the trade will accept is recomputed for this
//! pair every single time.
//!
//! What the model computes is the SAME walk `--quote` runs against the chain,
//! over a ladder of ticks read in the background rather than one at a time -
//! `depth::swap_exact_in_along`, driven by `TickBook`. So a quote from memory is
//! not an approximation of the real one within some tolerance; it is the real
//! one, and a swap that walks past what was read is refused rather than
//! estimated.
//!
//! Nothing here asks the router to rehearse the trade, so nothing proves the
//! wallet can afford it except a balance check run for that purpose alone -
//! see `quote`. Allowance is dealt with once, when the route is armed: both
//! ends of it are approved without limit if they are not already, and every
//! buy afterwards relies on that rather than re-proving it. `--swap` and
//! `--sell-all`, where nobody is racing, still run the real rehearsal through
//! `execute::verify` before sending.
//!
//! Routes are resolved once at startup: recovering a PoolKey means finding the
//! one log that ever published it, which is fine before the stream opens and
//! far too slow between a drop and a buy.

use crate::config::Config;
use crate::execute;
use crate::strategy::Signal;
use crate::pool::Pool;
use crate::route::{format_units, parse_pool_ref, Hop, PoolRef, Route, Token};
use crate::swap;
use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    ///
    /// A plain `std` lock, not a `tokio` one, and deliberately: nothing is
    /// awaited while it is held, and pricing has to be callable from the tick
    /// loop - which is synchronous, and which now decides the size of a buy
    /// before committing the money for it.
    state: std::sync::Mutex<Option<RouteState>>,
    /// The same as `gas_limit`, for the sale back down this route. A separate
    /// figure because the reversed route is a different transaction: it starts
    /// on a different token and can settle a different way. Zero until one sale
    /// has been measured, and `GAS_FALLBACK` stands in until then.
    sell_gas_limit: AtomicU64,
    /// Whether the token this route buys is approved to the router without a
    /// practical ceiling, so a sale need not go and ask. Set only by an
    /// allowance big enough that no position could reach it - a partial
    /// approval is left un-cached and re-checked every time, because a cached
    /// "yes" that was only ever true for a smaller size is how a sale comes to
    /// be signed against an allowance that cannot cover it.
    ///
    /// Seeded at arming time, where the same question is already asked and
    /// answered - and, if the answer was no, acted on. See `Approver`: a route
    /// armed with `--execute` starts with this true, so the first sale is not
    /// the one that pays for the question.
    sell_approved: AtomicBool,
}

/// An allowance at least this large is treated as unlimited. `--approve` sets
/// `type(uint160).max` on Permit2 and the full `uint256` on the ERC20, and no
/// position this bot can build comes within a factor of billions of 2^96.
fn effectively_unlimited() -> U256 {
    U256::one() << 96
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

/// What the feed last saw of a pool, for pricing a trade against it right now.
///
/// A buy gets this from the log that raised its signal. A sale has no signal,
/// so it carries the same thing forward from the last tick - which matters more
/// there than on a buy, because a sale has no other fresh input at all and
/// would otherwise be priced entirely from a snapshot written on a timer.
#[derive(Clone, Copy)]
pub struct LiveState {
    pub sqrt: U256,
    pub liquidity: u128,
    /// The fee the last swap was charged, when the log said. Already the
    /// combined swap fee on v4 - see `quote`.
    pub lp_fee: Option<u32>,
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
/// What the model made of a trade: the output it expects, and how hard the
/// trade leaned on the one assumption it makes.
///
/// The impact travels with the answer rather than being recomputed or left
/// behind, because it is the number that says how much to believe the other
/// one - and it belongs in the log line of every trade, not only in the warning
/// of the trades that were refused. A distribution nobody can see is one nobody
/// can tighten `max_slippage_pct` against.
/// One hop of a route, with everything needed to walk it and nothing that has
/// to be looked up again.
struct Hopped {
    pool: PoolRef,
    zero_for_one: bool,
    state: crate::depth::PoolState,
    ladder: crate::depth::Ladder,
}

/// A whole route, gathered once so it can be walked many times.
///
/// Opaque on purpose: the strategy carries one from sizing to pricing without
/// looking inside it, which is the point - both have to describe the same
/// reading of the pools.
///
/// Gathering takes two locks and copies a ladder per hop; walking is
/// arithmetic. Sizing a trade to a target impact means walking the same route
/// at forty different amounts, and doing that against a fresh gather each time
/// would be forty times the locking for the same numbers.
pub struct Prepared {
    hops: Vec<Hopped>,
}

/// What one walk of a prepared route produced.
struct Walked {
    amount_out: f64,
    /// The price move each hop took, in route order. Kept per hop rather than
    /// reduced, because sizing asks about ONE of them - the pool the signal
    /// came from - while pricing asks about the worst.
    impacts: Vec<(PoolRef, f64)>,
    crossed: u32,
}

impl Prepared {
    /// A number in the right neighbourhood, for bracketing somebody else's
    /// search. NEVER for a minimum.
    ///
    /// Where `walk` stops at the edge of what was read, this carries on as if
    /// liquidity held - which may be wrong, and does not matter: a hint only
    /// has to be close enough to save the router's bisection from doubling its
    /// way up from two. That search costs a request per doubling, and a sale
    /// asking with nothing to start from spent fifty-odd of them at the one
    /// moment a position needed closing.
    fn estimate(&self, amount_in: f64) -> Option<f64> {
        let mut amount = amount_in;
        for hop in &self.hops {
            let walked = crate::depth::swap_exact_in_along(
                hop.state,
                hop.zero_for_one,
                amount,
                &hop.ladder.rungs,
                crate::depth::Beyond::ConstantLiquidity,
            );
            match walked {
                Ok(crate::depth::Walk::Done(r)) if r.amount_out > 0.0 => amount = r.amount_out,
                _ => return None,
            }
        }
        Some(amount)
    }

    /// Walk the route at this size. Pure: no locks, no reads, no allocation
    /// beyond the impacts it reports.
    fn walk(&self, amount_in: f64) -> Option<Walked> {
        let mut amount = amount_in;
        let mut impacts = Vec::with_capacity(self.hops.len());
        let mut crossed = 0u32;
        for hop in &self.hops {
            // `Beyond::HoldsUntil`, not `Unknown`: the scan read out to its own
            // edge, so between the last rung and that edge there is nothing to
            // cross. A pool provided across its whole range has no rungs at
            // all, and reading that as ignorance refused every trade through
            // the easiest pool there is.
            let walked = crate::depth::swap_exact_in_along(
                hop.state,
                hop.zero_for_one,
                amount,
                &hop.ladder.rungs,
                crate::depth::Beyond::HoldsUntil(hop.ladder.bound),
            );
            let r = match walked {
                Ok(crate::depth::Walk::Done(r)) if r.amount_out > 0.0 => r,
                _ => return None,
            };
            let impact = ((r.sqrt_p_after / hop.state.sqrt_p).powi(2) - 1.0).abs();
            if !impact.is_finite() {
                return None;
            }
            impacts.push((hop.pool, impact));
            crossed += r.ticks_crossed;
            amount = r.amount_out;
        }
        Some(Walked { amount_out: amount, impacts, crossed })
    }
}

struct Modelled {
    amount_out: U256,
    /// Fraction, not percent: the largest price move any single hop takes.
    impact: f64,
    /// Initialized ticks the whole route walked through. Zero is the ordinary
    /// case and is not a lesser answer - it means the swap stayed inside one
    /// stretch of constant liquidity, where the arithmetic is exact for any
    /// size at all.
    crossed: u32,
}

/// Where every armed route's pools change liquidity, read in the background so
/// that no trade ever waits for it.
///
/// This is the answer to the question `modelled_impact_cap` can only guess at.
/// The model is exact while liquidity holds across the swap, and a scanned
/// window of initialized ticks says whether it does - so a swap that crosses
/// nothing is priced however far it moves the pool, and one that crosses
/// something is refused however little it moves it. The percentage stays as the
/// fallback for a pool nothing is on file for, which is the only case left.
#[derive(Default)]
struct TickBook {
    /// `std` rather than `tokio` for the same reason as `Plan::state`: nothing
    /// awaits while it is held, and the tick loop reads it synchronously.
    inner: std::sync::Mutex<HashMap<PoolRef, Entry>>,
}

struct Entry {
    /// `None` after a read that failed, so a pool whose ticks cannot be read
    /// is remembered as such rather than retried by every signal.
    window: Option<crate::depth::TickWindow>,
    /// What the pool looked like when the window was read.
    ///
    /// The scan reads this anyway - it needs a price to centre on - and used to
    /// throw it away, leaving the calibration snapshot as the only state a hop
    /// the signal says nothing about could be priced from. That snapshot is
    /// written every `calibrate_secs` and trusted for twice as long, so two
    /// failed passes took every route off the air; this one is read every
    /// thirty seconds by a task that wants it for its own reasons.
    state: Option<crate::depth::PoolState>,
    at: Instant,
}

/// How often each armed pool's window is read again. Cheap - two batched scans
/// per pool - and the point is to be ahead of the dip rather than to notice it
/// afterwards, so this is far shorter than the calibration interval.
const TICK_WINDOW_REFRESH: Duration = Duration::from_secs(30);

/// How long to leave startup alone before the first scan. Long enough for the
/// routes, the approvals, the gas measurement and the first calibration pass to
/// be done with the endpoint.
const TICK_WINDOW_FIRST: Duration = Duration::from_secs(10);

/// Pause between pools inside one pass, so a pass is a trickle rather than a
/// burst. A request limit counts bursts.
const TICK_WINDOW_GAP: Duration = Duration::from_secs(1);

/// How old a window may be before it stops being believed. Three missed
/// refreshes.
///
/// A window does not go wrong because the price moved: it covers a span of
/// price, and for any ordinary spacing that span is the whole tick range. What
/// it can go wrong about is the pool changing shape underneath it.
///
/// TODO: a position MINTED inside a scanned window puts an initialized tick
/// where the scan saw none, and nothing here watches for that - so between two
/// refreshes a swap can be told it crosses nothing when it now crosses
/// something, which is exactly the answer that makes the model trusted outright.
/// This age limit is the only thing standing in for that, and it is a timer
/// rather than an answer: it bounds the exposure to a minute and a half, it
/// does not detect anything. The real fix is to subscribe to the pool's
/// `ModifyLiquidity` and drop the window on one, the same way the feed already
/// turns every `Swap` into a tick - then a window is invalidated by the event
/// that invalidates it rather than by the clock. Worth doing before this runs
/// on a pool whose liquidity is actively managed by someone else.
const TICK_WINDOW_STALE_AFTER: Duration = Duration::from_secs(90);

impl TickBook {
    /// Whether this swap crosses a price where the pool's liquidity changes.
    ///
    /// `None` is "nothing on file", never "no". The caller falls back to the
    /// percentage on it, and reading it as "no" would trust the model on
    /// exactly the pools nothing is known about.
    /// The ticks a move through this pool would cross, ready to be walked
    /// without touching the chain - or `None` when nothing on file reaches that
    /// far.
    fn rungs(&self, key: PoolRef, from: f64, to: f64) -> Option<Vec<crate::depth::Rung>> {
        let book = self.inner.lock().ok()?;
        let entry = book.get(&key)?;
        if entry.at.elapsed() > TICK_WINDOW_STALE_AFTER {
            return None;
        }
        entry.window.as_ref()?.rungs_towards(from, to)
    }

    /// The ladder a swap through this pool would walk, in its direction.
    fn ladder(&self, key: PoolRef, sqrt_p: f64, up: bool) -> Option<crate::depth::Ladder> {
        // A poisoned lock means a panic while holding it. Nothing here can
        // panic, and losing the ladder only costs a quote.
        let book = self.inner.lock().ok()?;
        let entry = book.get(&key)?;
        if entry.at.elapsed() > TICK_WINDOW_STALE_AFTER {
            return None;
        }
        entry.window.as_ref()?.ladder_from(sqrt_p, up)
    }

    /// File a window, saying so out loud when a pool starts or stops having one.
    ///
    /// On the transition and not on every pass: a pool whose ticks cannot be
    /// read prices every trade by the percentage instead of by its own
    /// liquidity, which is a degradation worth exactly one line - and a line
    /// repeated twice a minute for as long as it lasts is a line nobody reads.
    /// Without this the only trace was `exact=false` on a buy, which says a
    /// trade was affected but not that anything is wrong.
    /// What the last scan saw of this pool, while it is recent enough to price
    /// from. Twenty times fresher than the calibration snapshot, and read by a
    /// task that keeps running when calibration cannot.
    fn state(&self, key: PoolRef) -> Option<crate::depth::PoolState> {
        let book = self.inner.lock().ok()?;
        let entry = book.get(&key)?;
        if entry.at.elapsed() > TICK_WINDOW_STALE_AFTER {
            return None;
        }
        entry.state
    }

    /// How long ago this pool's window was last read, if it ever was.
    fn age(&self, key: PoolRef) -> Option<Duration> {
        self.inner.lock().ok()?.get(&key).map(|e| e.at.elapsed())
    }

    fn put(
        &self,
        key: PoolRef,
        window: Option<crate::depth::TickWindow>,
        state: Option<crate::depth::PoolState>,
    ) {
        let Ok(mut book) = self.inner.lock() else { return };
        let had = book.get(&key).is_some_and(|e| e.window.is_some());
        match (had, window.is_some()) {
            (true, false) => warn!(
                pool = %key,
                "TICK WINDOW LOST - trades through this pool are priced by the impact \
                 percentage again until it comes back"
            ),
            (false, true) => info!(pool = %key, "tick window available"),
            _ => {}
        }
        book.insert(key, Entry { window, state, at: Instant::now() });
    }
}

struct Quoted {
    amount_out: U256,
    /// The worst hop's price move, as `Modelled::impact`.
    impact: f64,
    /// Initialized ticks the route walked, as `Modelled::crossed`.
    crossed: u32,
    /// How old the calibration snapshot was when this was priced, in seconds,
    /// or `None` when there is no snapshot at all. Logged on every buy so that
    /// the state ageing is visible while it is still pricing trades, rather
    /// than only once it has stopped pricing them.
    state_age_s: Option<u64>,
    /// How long the asking took, so the log separates the network from
    /// everything else - which is otherwise invisible and dominates.
    took: Duration,
    /// Deferred: a gas price that is not known only matters if we send.
    fees: Option<(U256, U256)>,
}

/// What the signal itself says about the pool it came from.
///
/// The fee the swap was actually charged when the log carries one (v4, hook
/// override and all), else the pool's own fixed fee - a v3 log has no fee word
/// because a v3 fee cannot change. A v4 pool that logged nothing is left to the
/// calibration snapshot rather than priced from its PoolKey's dynamic-fee flag,
/// which is a flag and not a fee.
///
/// v4 emits the fee the swap was CHARGED, already the protocol cut and the LP
/// fee combined, so it goes in whole with no protocol fee left to add.
fn live_hop(key: PoolRef, sig: &Signal, route: &Route) -> Option<(PoolRef, HopState)> {
    let fee = sig.lp_fee.or_else(|| {
        route
            .hops
            .iter()
            .find(|h| h.pool_ref() == key)
            .map(|h| h.fee)
            .filter(|f| u64::from(*f) < PPM)
    })?;
    Some((
        key,
        HopState {
            sqrt_p: crate::pool::sqrt_to_f64(sig.sqrt),
            liquidity: sig.liquidity,
            lp_fee: fee,
            protocol_fee_0for1: 0,
            protocol_fee_1for0: 0,
        },
    ))
}

/// The largest size whose impact does not exceed `target`, never above `cap`.
///
/// Pure, and separated from `Executor::size_for` because it decides how much
/// money leaves the wallet and that is worth being able to test on its own.
///
/// `impact_at` returning `None` counts as too big. Being unable to price a size
/// is not permission to send it, and it is what a size walking past the end of
/// the read ticks looks like - which happens exactly when the ceiling is far
/// larger than the ladder reaches, and where a smaller size is still perfectly
/// answerable.
fn size_to_impact(
    cap: U256,
    target: f64,
    impact_at: impl Fn(U256) -> Option<f64>,
) -> Option<U256> {
    if cap.is_zero() {
        return None;
    }
    // The ceiling is within the target: spend it and no more. The ordinary case
    // on a deep pool, and not a refusal - it is the whole reason `amount_in`
    // stays.
    if impact_at(cap).is_some_and(|i| i <= target) {
        return Some(cap);
    }

    // Bisect. Impact rises with size, so the answer is bracketed from the
    // start: `hi` is always a size known to be too big and `lo` always one
    // known not to be, which is why `lo` is what gets returned.
    let (mut lo, mut hi) = (U256::zero(), cap);
    for _ in 0..BISECT_STEPS {
        let mid = lo + (hi - lo) / 2;
        if mid == lo || mid == hi {
            break;
        }
        match impact_at(mid) {
            Some(i) if i <= target => lo = mid,
            _ => hi = mid,
        }
    }
    (!lo.is_zero()).then_some(lo)
}

/// Bisection steps when sizing a buy to a target impact.
///
/// Each halves the bracket, so forty take a `uint256` of room down to the last
/// unit for any size this bot trades. They are pure arithmetic over a snapshot
/// gathered once, so the whole search is microseconds and no requests.
const BISECT_STEPS: u32 = 40;

/// Fixed-point scale for `yield_ppm`.
const PPM: u64 = 1_000_000;

/// The least a snapshot may be trusted for, whatever the calibration interval
/// is. Pools other than the one that just dropped move slowly enough that two
/// minutes is fine.
const STATE_STALE_MIN: Duration = Duration::from_secs(120);

/// How stale the cached pool state may be before a fast quote is refused.
///
/// This CANNOT be a constant: the snapshot is written by the calibration pass
/// and by nothing else, so a window shorter than the interval between passes
/// leaves a stretch of every cycle in which no route can be priced at all -
/// with `calibrate_secs` at its default of 300 against a fixed two minutes,
/// that was three minutes dead in every five. Two intervals, so one missed
/// pass is survivable and only a second one stops trading.
fn state_stale_after(calibrate_secs: u64) -> Duration {
    Duration::from_secs(calibrate_secs.saturating_mul(2)).max(STATE_STALE_MIN)
}

/// How far the measured yield has to move before it is worth saying out loud,
/// in parts per million. A hook that changes its cut is a change of terms, not
/// noise; measurement jitter across sizes came out at 3 ppm.
const YIELD_ALERT_PPM: u64 = 500;

/// The most a route may be measured to take beyond its pools' stated fees
/// before the bot refuses to price it at all. A hook keeping more than a
/// twentieth of every swap is not a fee to model around; it is a reason to
/// stop. Now that the measurement is a term in the price as well as a guard,
/// this is also the point past which paying it stops being worth it: a 5% cut
/// each way is 10% of a round trip, which no take-profit here is set to clear.
const MAX_UNSTATED_FEE_PPM: u64 = 50_000;

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
    /// Native currency that must be there before a buy - see `spendable`.
    gas_reserve: U256,
    /// What the wallet holds of the native currency, refreshed in the
    /// background. Read on every signal and never on the network, because the
    /// question is asked between a drop and a broadcast.
    gas_balance: std::sync::Mutex<U256>,
    wallet: LocalWallet,
    owner: Address,
    /// Sign and send, rather than only reporting what would have been sent.
    execute: bool,
    /// Trigger pool -> what to buy when it drops.
    plans: HashMap<PoolRef, Plan>,
    /// Where each pool's liquidity changes, kept current in the background so
    /// a quote can be exact instead of merely cautious. See `TickBook`.
    ticks: TickBook,
    last_fire: Mutex<HashMap<PoolRef, Instant>>,
    /// The gas price, kept current from the block stream instead of asked for
    /// on every buy.
    fees: Arc<swap::FeeWatch>,
    /// Where signed transactions go. One endpoint or several, written to at
    /// once - see `swap::Broadcaster`.
    submit: Arc<swap::Broadcaster>,
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
    /// Failing here rather than at the first drop is deliberate: a bad route or
    /// a missing key should stop the process at startup, while someone is
    /// watching, not silently do nothing at the one moment it was supposed to
    /// act. A missing approval is the one thing that does not stop it, because
    /// it is the one thing that can be fixed from here - see `Approver`.
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
        // Shared across every armed route, so two routes spending the same
        // token approve it once between them rather than once each.
        let mut approve = Approver {
            http,
            wallet: &wallet,
            owner,
            permit2,
            router,
            execute,
            done: HashMap::new(),
        };

        let mut plans = HashMap::new();
        for rc in armed {
            let weth = cfg.weth.as_deref().map(str::parse).transpose().context("weth")?;
            let route = Route::resolve(http, manager, rc, &cfg.tokens, weth)
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
                let named = match (&p.pool_id, &p.address) {
                    // A v4 pool is named by its id; a v3 pool by its own
                    // address, which it always has.
                    (Some(id), _) => parse_pool_ref(id).ok(),
                    (None, Some(a)) => parse_pool_ref(a).ok(),
                    (None, None) => None,
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

            // Also the answer to the question the first sale would otherwise
            // have to stop and ask - see `Plan::sell_approved`.
            let sell_approved = preflight(&mut approve, &route).await?;
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
                spend = %route.input.symbol,
                impact_pct = route.impact_pct,
                buy = %route.output.symbol,
                slippage_pct = route.max_slippage_pct,
                cooldown_s = rc.cooldown_secs,
                mode = if execute { "LIVE" } else { "dry run" },
                "auto-buy armed"
            );
            // Measured now, while nobody is waiting, so the hot path never has
            // to ask. A route that cannot be estimated yet still gets armed:
            // the fallback is generous and the next send re-measures.
            let probe =
                execute::pending_swap(router, &route, U256::one(), U256::one(), execute::deadline_in(600))?;
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
                    sell_gas_limit: AtomicU64::new(0),
                    sell_approved: AtomicBool::new(sell_approved),
                    state: std::sync::Mutex::new(None),
                },
            );
        }

        // Started before the first signal, so the first buy already prices off a
        // header rather than off a lookup.
        let fees = Arc::new(swap::FeeWatch::default());
        fees.watch(cfg.ws_url.clone(), http.clone());
        // And the fallback that used to run inside a buy, on a timer instead.
        fees.top_up(http.clone());
        swap::keep_warm(http.clone());

        // Broadcasting fans out; everything else stays on the one endpoint the
        // rest of the process reads through. An empty list means the two are
        // the same thing, which is what this did before there was a list.
        let submit_urls = match cfg.submit_urls.is_empty() {
            true => std::slice::from_ref(&cfg.http_url),
            false => cfg.submit_urls.as_slice(),
        };
        let submit = Arc::new(swap::Broadcaster::new(submit_urls)?);
        submit.keep_warm();
        info!(
            endpoints = ?submit.labels(),
            "transactions will be broadcast through {} endpoint(s) at once",
            submit.width()
        );

        // Taken here, after the approvals above have moved it and while nobody
        // is waiting, so the first buy does not spend its one network wait on a
        // number this process could already have known. It cost 44 ms of a
        // falling market to find that out once.
        //
        // Only when there is something to sign with. A dry run never sends, so
        // it never needs one, and asking would be a round trip for a number
        // nothing will use.
        let next_nonce = match execute {
            true => match swap::pending_nonce(http, owner).await {
                Ok(n) => {
                    info!(nonce = n, "next nonce taken at startup");
                    Some(n)
                }
                // Not fatal: the first buy asks for itself, exactly as it did
                // before this was taken in advance.
                Err(e) => {
                    warn!(err = %format!("{e:#}"), "could not read the nonce yet; the first \
                          trade will ask for it");
                    None
                }
            },
            false => None,
        };

        let me = Arc::new(Self {
            http: http.clone(),
            router,
            manager,
            permit2,
            calibrate_secs: cfg.calibrate_secs,
            gas_reserve: crate::route::parse_units(&cfg.gas_reserve, 18)
                .context("gas_reserve")?,
            // Read once here so the first signal is judged on a real figure
            // rather than on a zero that would refuse it.
            gas_balance: std::sync::Mutex::new(
                swap::balance_of(http, Address::zero(), owner).await.unwrap_or_default(),
            ),
            wallet,
            owner,
            execute,
            plans,
            ticks: TickBook::default(),
            fees,
            submit,
            last_fire: Mutex::new(HashMap::new()),
            next_nonce: Mutex::new(next_nonce),
        });
        me.calibrate();
        me.watch_ticks();
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
                // One block for the whole pass, not one per route. Cheaper by a
                // request, and better: two routes measured against different
                // blocks are two measurements of different markets, which is
                // the very confusion `measure_yield` pins a block to avoid.
                let head = crate::rpc::retrying("eth_blockNumber", || async {
                    Ok(me.http.get_block_number().await?)
                })
                .await;
                match head {
                    Ok(at) => {
                        for plan in me.plans.values() {
                            if let Err(e) = me.measure_yield(plan, at.as_u64()).await {
                                warn!(route = %plan.route.name, err = %format!("{e:#}"),
                                      "could not measure route yield");
                            }
                        }
                    }
                    Err(e) => warn!(err = %format!("{e:#}"),
                                    "could not read the block to calibrate against"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }

    /// Keep reading where each armed route's pools change liquidity.
    ///
    /// The whole point of doing it here is that it is the one question the hot
    /// path cannot afford to ask and cannot afford to guess at either. It is
    /// also why the scan covers a SPAN of price rather than the two ticks
    /// nearest the current one: by the time a dip fires, the price has moved,
    /// and a pair of neighbours read beforehand would describe where the pool
    /// used to be. See `depth::tick_window`.
    ///
    /// Separate from `calibrate` and much faster, because the two answer
    /// different questions on different clocks: what a hook charges changes
    /// when someone changes it, while where liquidity sits changes whenever
    /// anybody mints or burns.
    fn watch_ticks(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            // Startup is already the busiest moment this process has: routes
            // resolving, approvals, gas measured, the first calibration pass
            // walking every hop's ticks, balances read. Piling a full window
            // scan on top of all of it is what turns a rate limit from a
            // background annoyance into a process that cannot start. Nothing
            // trades in the first few seconds anyway.
            tokio::time::sleep(TICK_WINDOW_FIRST).await;
            loop {
                // Every hop of every armed route, deduplicated: one pool shared
                // by two routes is one window, and a route and its reverse are
                // the same pools either way.
                // Read on the same pass as the windows: it decides whether a
                // buy happens at all, and it is one request for every pool
                // rather than one each.
                if let Ok(native) = swap::balance_of(&me.http, Address::zero(), me.owner).await {
                    if let Ok(mut slot) = me.gas_balance.lock() {
                        *slot = native;
                    }
                }
                let mut done = std::collections::HashSet::new();
                for plan in me.plans.values() {
                    for hop in &plan.route.hops {
                        if !done.insert(hop.pool_ref()) {
                            continue;
                        }
                        me.refresh_window(hop).await;
                        // Spread over the interval instead of arriving as one
                        // burst. Nothing is waiting on these, and a burst is
                        // what a request limit counts.
                        tokio::time::sleep(TICK_WINDOW_GAP).await;
                    }
                }
                tokio::time::sleep(TICK_WINDOW_REFRESH).await;
            }
        });
    }

    /// Read one pool's tick window and file it.
    ///
    /// A failure is filed too, as an absence: a pool whose ticks cannot be read
    /// should fall back to the percentage once and quietly, not have every
    /// signal discover it again.
    async fn refresh_window(&self, hop: &Hop) {
        let key = hop.pool_ref();
        let reader = match crate::depth::TickReader::new(
            &self.http,
            hop.tick_source(self.manager),
            hop.tick_spacing,
        ) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(pool = %key, err = %format!("{e:#}"), "no tick reader for this pool");
                self.ticks.put(key, None, None);
                return;
            }
        };
        // Around where the pool is NOW, not where the snapshot last saw it: the
        // window is centred once and then has to cover wherever the price goes
        // next, so it is worth centring on the truth.
        let state = match crate::depth::read_state(&reader).await {
            Ok(s) => s,
            Err(e) => return self.keep_window(key, "could not read pool state", e).await,
        };
        match crate::depth::tick_window(&reader, state.sqrt_p).await {
            Ok(w) => {
                let (lo, hi) = w.span();
                let (llo, lhi) = w.ladder_span();
                tracing::debug!(
                    pool = %key,
                    edges = w.edges(),
                    // Of those, how many the scan also read the liquidity
                    // change of - which is what decides whether a depth report
                    // costs requests or costs nothing.
                    ladder = w.ladder(),
                    // True means no price move can ever put this pool back on
                    // the percentage: the scan read every tick it could have.
                    whole_range = w.whole(),
                    // As a factor on the current price, which is the form the
                    // question is actually asked in.
                    covers_down = format!("{:.1}%", ((lo / state.sqrt_p).powi(2) - 1.0) * 100.0),
                    covers_up = format!("{:.1}%", ((hi / state.sqrt_p).powi(2) - 1.0) * 100.0),
                    // And how far a swap may actually be WALKED, which is the
                    // narrower of the two and the one that decides whether a
                    // trade gets priced. On a densely provided pool the ladder
                    // runs out long before the bitmap does.
                    walkable_down = format!("{:.1}%", ((llo / state.sqrt_p).powi(2) - 1.0) * 100.0),
                    walkable_up = format!("{:.1}%", ((lhi / state.sqrt_p).powi(2) - 1.0) * 100.0),
                    "tick window read"
                );
                self.ticks.put(key, Some(w), Some(state));
            }
            Err(e) => self.keep_window(key, "could not read the tick window", e).await,
        }
    }

    /// A read failed. Leave whatever is on file alone.
    ///
    /// A failed read is not evidence that the last successful one was wrong,
    /// and discarding it on that basis is what turned an endpoint's occasional
    /// rate limit into a window flapping in and out every thirty seconds - each
    /// refusal throwing away a perfectly good scan. What bounds a window's life
    /// is its AGE, and `TICK_WINDOW_STALE_AFTER` already does that.
    ///
    /// So this only speaks up once there is genuinely nothing usable left,
    /// which is the moment worth a warning rather than every moment on the way
    /// to it.
    async fn keep_window(&self, key: PoolRef, what: &str, e: anyhow::Error) {
        match self.ticks.age(key) {
            Some(age) if age <= TICK_WINDOW_STALE_AFTER => tracing::debug!(
                pool = %key, kept_age_s = age.as_secs(), err = %format!("{e:#}"),
                "{what}; keeping the last one"
            ),
            _ => warn!(
                pool = %key, err = %format!("{e:#}"),
                "{what}, and there is no usable one left - trades through this pool are \
                 priced by the impact percentage"
            ),
        }
    }

    /// `at` is the block the whole calibration pass is pinned to. Taken at the
    /// head per reading they would land on different blocks whenever the pool
    /// is busy, and the difference between them would then be the price moving
    /// rather than a fee: that is exactly how a 1% cut first measured as 2.46%.
    async fn measure_yield(&self, plan: &Plan, at: u64) -> Result<()> {
        let route = &plan.route;
        // Measured at the size a signal would actually send, so the fee it
        // finds is the fee that size pays.
        let cap = swap::balance_of(&self.http, route.input.address, self.owner)
            .await
            .context("reading what there is to measure with")?;
        let size = self.calibration_size(plan, cap)?;
        let local = route
            .quote(&self.http, self.manager, Some(at), size)
            .await
            .context("local tick walk")?;
        let onchain = execute::verify(
            &self.http,
            self.router,
            self.owner,
            route,
            size,
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
        // A poisoned lock costs this pass's snapshot and nothing else; the
        // next one writes over it.
        if let Ok(mut slot) = plan.state.lock() {
            *slot = Some(RouteState {
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
        }

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

    /// The ticks a move through this pool crosses, from what the background
    /// scan already read - so a caller can walk them locally instead of asking
    /// the chain for what is sitting in memory.
    ///
    /// `None` means nothing usable is on file and the caller should read for
    /// itself. It never means "nothing to cross": see
    /// `depth::TickWindow::rungs_towards`, where that distinction is kept.
    pub fn rungs_towards(
        &self,
        key: PoolRef,
        from: f64,
        to: f64,
    ) -> Option<Vec<crate::depth::Rung>> {
        self.ticks.rungs(key, from, to)
    }

    /// What to spend on this signal: the route's fixed `amount_in`, or - when
    /// the route asks for one - the size that moves the TRIGGER pool by its
    /// `impact_pct`, never past `amount_in`.
    ///
    /// Synchronous and free of requests on purpose. The tick loop decides how
    /// much a buy costs before it sets the money aside, so that what is
    /// reserved is what is spent: reserving a ceiling and refunding later would
    /// hold the difference frozen through every buy, and getting the refund
    /// wrong would drift the tracked balance away from the wallet for good.
    ///
    /// The trigger pool and not the worst hop, because that is the pool the
    /// drop happened in and the one the size is a statement about. A route is
    /// still refused outright if any hop cannot be walked - see `prepare`.
    ///
    /// `None` means "do not buy": no measurement, no usable state, or a size
    /// that rounds to nothing.
    pub fn size_for(&self, key: PoolRef, sig: &Signal, cap: U256) -> Option<(U256, Prepared)> {
        let plan = self.plans.get(&key)?;
        let route = &plan.route;
        let ppm = plan.yield_ppm.load(Ordering::Relaxed);
        if !self.unstated_fee_acceptable(plan, ppm) {
            return None;
        }
        // Gathered ONCE and handed on to whoever prices the trade. Doing it
        // again there would take the same locks and copy the same ladders for
        // the same answer - and worse, would leave a window in which the
        // background scan replaces a window between the size being worked out
        // and the minimum being signed, so the money set aside and the price
        // accepted would describe two different pools.
        let prepared = self.prepare(plan, route, live_hop(key, sig, route), ppm)?;

        // The ceiling is what there is to spend, less what the transaction
        // needs to exist. On a route that spends native ETH those come out of
        // the SAME balance, so sizing to the whole of it buys a swap that
        // cannot pay for its own gas - and the tracked figure does not even
        // know about the gas, because nothing debits it.
        let cap = self.spendable(plan, cap)?;
        let target = route.impact_pct / 100.0;
        let size = {
            let impact_at = |amount: U256| -> Option<f64> {
                let walked = prepared.walk(crate::route::u256_to_f64(amount))?;
                walked.impacts.iter().find(|(p, _)| *p == key).map(|(_, i)| *i)
            };
            let sized = size_to_impact(cap, target, impact_at)?;
            // The whole balance still does not move the pool as far as asked.
            // A smaller trade is not a smaller version of this one - it is a
            // different trade nobody asked for - so it is skipped.
            let reached = impact_at(sized).is_some_and(|i| i >= target * (1.0 - 1e-6));
            if !reached {
                warn!(
                    route = %route.name,
                    want_pct = route.impact_pct,
                    have = %format_units(cap, route.input.decimals),
                    token = %route.input.symbol,
                    "not buying: the whole tracked balance cannot move this pool that far"
                );
                return None;
            }
            sized
        };
        Some((size, prepared))
    }

    /// How much of a balance a trade may actually use.
    ///
    /// How much of a balance a trade may actually use, and whether to trade at
    /// all.
    ///
    /// Whether there is enough native currency to trade at all.
    ///
    /// Not about paying for the BUY - about paying for the sale that has to
    /// follow it. A position bought with the last of the gas is a position that
    /// cannot be closed, and a bag nobody can put down is a worse outcome than
    /// a dip nobody caught. Getting in is optional; getting out is not.
    ///
    /// It has nothing to say about the size, because a route may not spend the
    /// native currency at all - `Route::resolve` refuses one that tries. A pool
    /// holding native ETH is traded by holding the wrapped token and letting
    /// the router unwrap on the way in, so the balance a trade comes out of and
    /// the balance gas comes out of are never the same pot.
    ///
    /// A flat reserve rather than an estimate. An estimate is only as good as
    /// the last gas price seen and must be right on every trade; a reserve
    /// worth many transactions must be right once, in the config.
    fn spendable(&self, plan: &Plan, balance: U256) -> Option<U256> {
        let native = self.gas_balance.lock().ok().map(|g| *g).unwrap_or_default();
        if native < self.gas_reserve {
            warn!(
                route = %plan.route.name,
                native = %format_units(native, 18),
                gas_reserve = %format_units(self.gas_reserve, 18),
                "NOT BUYING: too little native currency left to be sure of paying for the \
                 sale - a position that cannot be closed is worse than a dip not caught"
            );
            return None;
        }
        Some(balance)
    }

    /// A size to measure a route's unstated fee at.
    ///
    /// The fee a hook charges can depend on the size, so measuring at an
    /// arbitrary one measures the wrong thing. This asks the same question a
    /// signal would: what moves the trigger pool by the route's `impact_pct`,
    /// against the pool as the last snapshot saw it. There is no live state
    /// here - calibration runs on a timer, not on a drop - and that is fine,
    /// because it is measuring a ratio rather than pricing a trade.
    fn calibration_size(&self, plan: &Plan, cap: U256) -> Result<U256> {
        let route = &plan.route;
        let key = route
            .hops
            .last()
            .map(|h| h.pool_ref())
            .context("route has no hops")?;
        // No snapshot yet, which is the state every route starts in: the
        // snapshot is written by this very pass, so asking it to size the pass
        // that writes it is a circle with no way in. Measure at what there is
        // to spend instead - the ratio is what is wanted here, and the next
        // pass will have a snapshot to size itself properly from.
        //
        // Asked BEFORE `prepare` rather than by letting it fail: its refusal is
        // a warning aimed at a trade that will not happen, and on the one pass
        // that is meant to have nothing it read as a fault instead of a start.
        let cold = plan.state.lock().map(|s| s.is_none()).unwrap_or(true);
        if cold {
            tracing::debug!(
                route = %route.name,
                "no snapshot to size the measurement from yet; measuring at the balance"
            );
            return Ok(cap);
        }
        let Some(prepared) = self.prepare(plan, route, None, PPM) else {
            return Ok(cap);
        };
        let target = route.impact_pct / 100.0;
        let impact_at = |amount: U256| -> Option<f64> {
            let walked = prepared.walk(crate::route::u256_to_f64(amount))?;
            walked.impacts.iter().find(|(p, _)| *p == key).map(|(_, i)| *i)
        };
        // Falling back rather than failing: a measurement at the wrong size is
        // worth more than no measurement, and no measurement means no trading
        // at all - `unstated_fee_acceptable` refuses a route it has never seen.
        Ok(size_to_impact(cap, target, impact_at).unwrap_or(cap))
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
    /// `spend` is what the caller committed for this buy - `amount_in`, or the
    /// size `size_for` worked out for the route's `impact_pct`. It arrives from
    /// outside because the money was set aside before this was called, and
    /// spending a different number than was reserved is how a tracked balance
    /// drifts away from the wallet.
    pub async fn on_drop(
        self: &Arc<Self>,
        pool: &Pool,
        sig: &Signal,
        spend: U256,
        prepared: &Prepared,
    ) -> Result<Option<Fill>> {
        let Some((key, plan)) = self.armed_for(pool) else {
            return Ok(None);
        };
        if !self.claim_turn(key, plan, pool).await {
            return Ok(None);
        }

        let route = &plan.route;
        let started = Instant::now();
        let deadline = execute::deadline_in(120);
        let quoted = self.quote(plan, spend, prepared).await?;

        let min_out = execute::apply_slippage(quoted.amount_out, route.max_slippage_pct);
        anyhow::ensure!(
            !min_out.is_zero(),
            "route '{}': amountOutMinimum rounds to zero",
            route.name
        );
        // At the size that was priced and reserved, not at the route's ceiling:
        // `execute_calldata` spends whatever `amount_in` says, so the sizing
        // has to reach the calldata or it decides nothing at all.
        let tx = execute::pending_swap(self.router, route, spend, min_out, deadline)?;

        let amount = |v: U256, t: &crate::route::Token| {
            format!("{} {}", format_units(v, t.decimals), t.symbol)
        };
        info!(
            pool = %pool.name,
            route = %route.name,
            drop_pct = format!("-{:.3}%", sig.drop_pct),
            spend = amount(spend, &route.input),
            quoted = amount(quoted.amount_out, &route.output),
            priced_by = "model",
            // How hard this trade leaned on the in-range assumption, against
            // what it was allowed to. Logged on every buy and not only on the
            // refusals, because a cap is only tunable against a distribution
            // somebody can see.
            impact_pct = format!("{:.4}", quoted.impact * 100.0),
            // How much walking the price took. Zero means the swap stayed
            // inside one stretch of constant liquidity, where the arithmetic is
            // exact whatever the size.
            ticks_crossed = quoted.crossed,
            quote_ms = quoted.took.as_millis(),
            state_age_s = quoted.state_age_s,
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
        let hash = self.broadcast(key, plan, &tx, spend, quoted).await?;
        Ok(Some(Fill { hash, sold: spend, amount_out }))
    }

    /// Sell everything held of what a route buys, back down that same route.
    ///
    /// The size is the wallet's actual balance rather than anything remembered:
    /// what is there is what can be sold, and a position built by several buys
    /// or topped up by hand is still one balance. Priced by the model when one
    /// is armed and calibrated; unlike a buy, this is not racing anyone, so a
    /// model that cannot answer falls back to asking the router even when that
    /// costs a bisection, rather than skipping the sale.
    ///
    /// `live` is what the feed last saw of the pool being sold into, and the
    /// model is not trusted without it. A sale carries no signal of its own, so
    /// without this every hop would come from the calibration snapshot - and a
    /// snapshot written on a timer prices a falling market at the price it used
    /// to have, which is how a sale comes to sign an `amountOutMinimum` the
    /// pool can no longer pay and reverts.
    ///
    /// `retry` says this sale has already reverted once. The model does not get
    /// a second go at it: the chain has just disagreed with whatever it
    /// believed, and the router prices the next attempt at whatever is true
    /// now, however long that takes.
    pub async fn sell_all(
        self: &Arc<Self>,
        key: PoolRef,
        route: &Route,
        slippage_pct: f64,
        limit: Option<U256>,
        live: Option<LiveState>,
        retry: bool,
    ) -> Result<Option<Fill>> {
        let token = route.output.address;
        // What this bot's own books say it holds, which is a figure it already
        // has. The balance was read from the chain here until a take-profit was
        // measured spending ~130 ms of a falling market on the question - and
        // the answer was one the inventory already knew, having built it from
        // the fills themselves. Only a sale with nothing on the books asks:
        // `--sell-all` on a token no position covers, which is not racing
        // anything. What the books cannot see - a transfer in, a manual buy -
        // is not this bot's to sell anyway, so reading it would only ever
        // enlarge a sale beyond what it should be.
        let size = match limit {
            Some(l) if !l.is_zero() => l,
            Some(_) => {
                info!(token = %route.output.symbol, "nothing to sell: the position is empty");
                return Ok(None);
            }
            None => {
                let balance = swap::balance_of(&self.http, token, self.owner).await?;
                if balance.is_zero() {
                    info!(token = %route.output.symbol, "nothing to sell: the wallet is empty");
                    return Ok(None);
                }
                balance
            }
        };
        let sell = route.reversed();
        let plan = self.plans.get(&key);
        // Two `eth_call`s that answer the same way every time once `--approve`
        // has been run, and answering them here cost a sale two round trips of
        // a moving market. Asked once, cached when the answer is "unlimited",
        // and refreshed in the background after every sale - so a revoked
        // approval is noticed by the pass after the one that used it, and a
        // partial one is never cached at all.
        let cached = plan.is_some_and(|p| p.sell_approved.load(Ordering::Relaxed));
        if sell.input.address != Address::zero() && !cached {
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
            if let Some(p) = plan {
                let unlimited = erc20 >= effectively_unlimited() && p2 >= effectively_unlimited();
                p.sell_approved.store(unlimited, Ordering::Relaxed);
            }
        }

        let deadline = execute::deadline_in(300);
        // The model gets first refusal here too, when there is a plan to price
        // it from - a sale of a token no route buys has no calibration to draw
        // on and goes straight to the router. It matters more here than on a
        // buy: a reversed route that ends on v3 is quoted by bisection, which
        // is twenty-odd calls rather than one, and unlike a buy a sale is not
        // racing anyone, so the fallback stays.
        // The pool being sold into, as the feed last saw it. The fee comes from
        // the log when it carried one, else from the route's own configured
        // fee - the same order a buy uses.
        let fresh = live.and_then(|l| {
            let fee = l.lp_fee.or_else(|| {
                route
                    .hops
                    .iter()
                    .find(|h| h.pool_ref() == key)
                    .map(|h| h.fee)
                    .filter(|f| u64::from(*f) < PPM)
            })?;
            Some((
                key,
                HopState {
                    sqrt_p: crate::pool::sqrt_to_f64(l.sqrt),
                    liquidity: l.liquidity,
                    lp_fee: fee,
                    protocol_fee_0for1: 0,
                    protocol_fee_1for0: 0,
                },
            ))
        });
        if fresh.is_none() && !retry {
            info!(
                route = %sell.name,
                "no live state for this pool - asking the router rather than pricing \
                 this sale from a snapshot"
            );
        }
        // Prepared once, and used for two different things. The strict walk
        // may decline - a retry never trusts it, and a ladder may not reach -
        // but the loose one is still worth having, because the router's search
        // needs somewhere to start and starting from nothing costs it fifty
        // requests.
        let ppm = plan
            .map(|p| match p.sell_yield_ppm.load(Ordering::Relaxed) {
                0 => p.yield_ppm.load(Ordering::Relaxed),
                m => m,
            })
            .unwrap_or(0);
        let prepared = plan
            .filter(|p| self.unstated_fee_acceptable(p, ppm))
            .and_then(|p| self.prepare(p, &sell, fresh, ppm));
        let hint = prepared
            .as_ref()
            .and_then(|p| p.estimate(crate::route::u256_to_f64(size)))
            .map(|a| crate::route::f64_to_u256_pub(a * ppm.min(PPM) as f64 / PPM as f64))
            .unwrap_or_default();

        // A sale that already reverted is not re-quoted by the model, and one
        // with no live state was never going to be.
        let modelled = match !retry && fresh.is_some() {
            true => prepared.as_ref().and_then(|p| self.model_quote(&sell, p, ppm, size)),
            false => None,
        };
        // The router's answer has no impact to report: it is not a model, so
        // there is no assumption to say how hard this leaned on.
        let (amount_out, priced_by, impact) = match modelled {
            Some(m) => (m.amount_out, "model", Some(m.impact)),
            None => (
                execute::verify(
                    &self.http, self.router, self.owner, &sell, size, hint, deadline, None,
                )
                .await
                .context("quoting the sale")?
                .amount_out,
                "router",
                None,
            ),
        };
        let min_out = execute::apply_slippage(amount_out, slippage_pct);
        anyhow::ensure!(!min_out.is_zero(), "the sale's amountOutMinimum rounds to zero");
        let tx = execute::pending_swap(self.router, &sell, size, min_out, deadline)?;

        info!(
            route = %sell.name,
            sell = format!("{} {}", format_units(size, sell.input.decimals), sell.input.symbol),
            quoted = format!("{} {}", format_units(amount_out, sell.output.decimals), sell.output.symbol),
            priced_by,
            // What the router's search was started from. Zero means it had to
            // double its way up from nothing, which is fifty-odd requests.
            hint = %format_units(hint, sell.output.decimals),
            impact_pct = impact
                .map(|i| format!("{:.4}", i * 100.0))
                .unwrap_or_else(|| "-".into()),
            // Whether this was priced against the pool as the feed last saw it
            // or against a snapshot. A sale priced from a snapshot in a falling
            // market is how an amountOutMinimum gets signed that the pool can
            // no longer pay, so it belongs in the line that records the sale.
            from_live_state = fresh.is_some(),
            retry,
            min_out = format!("{} {}", format_units(min_out, sell.output.decimals), sell.output.symbol),
            slippage_pct,
            "TAKE PROFIT"
        );
        if !self.execute {
            info!(route = %sell.name, "dry run - not sent; start with --execute to sell for real");
            return Ok(None);
        }

        // Everything a buy already had, now that a sale races the same market:
        // the gas comes from the last sale down this route, the nonce from this
        // process's own counter, and the two that can still have to go and look
        // are joined rather than sequenced so the cost is the slower of them
        // rather than their sum. See `quote`, which explains each in full.
        let gas = match plan.map(|p| p.sell_gas_limit.load(Ordering::Relaxed)) {
            Some(g) if g != 0 => U256::from(g),
            _ => U256::from(GAS_FALLBACK),
        };
        // Both from memory. Nothing between deciding to sell and broadcasting
        // may wait on a request - see `swap::FeeWatch::params` and
        // `claim_nonce`, which is why neither of them can ask any more.
        let fees = self.fees.params().context(
            "no gas price known yet - the header stream has not delivered one",
        )?;
        let nonce = self.claim_nonce().await?;
        match swap::send_nowait(&self.submit, &self.wallet, &tx, nonce.into(), fees, gas).await {
            Ok(hash) => {
                info!(route = %sell.name, ?hash, nonce, %gas, "sold");
                self.refresh_sell(key, size);
                Ok(Some(Fill { hash, sold: size, amount_out }))
            }
            Err(e) => {
                *self.next_nonce.lock().await = None;
                self.refill_nonce();
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
    /// tracking it locally exists to avoid.
    ///
    /// In the steady state this asks the network for nothing at all: the gas
    /// price answers from the last block header the `newHeads` watcher stored,
    /// and the nonce answers from this process's own counter. The two are still
    /// joined rather than sequenced, because the cases where one of them does
    /// have to go and look - a stale header, an empty counter - should cost the
    /// slower of the two rather than their sum.
    async fn quote(&self, plan: &Plan, spend: U256, prepared: &Prepared) -> Result<Quoted> {
        let started = Instant::now();
        anyhow::ensure!(
            self.unstated_fee_acceptable(plan, plan.yield_ppm.load(Ordering::Relaxed)),
            "route '{}': not buying - see the fee check above",
            plan.route.name
        );
        let ppm = plan.yield_ppm.load(Ordering::Relaxed);
        let modelled = self.model_quote(&plan.route, prepared, ppm, spend);
        // The model prices every buy or none does: there is no router
        // fallback on this path. Skip the buy instead of guessing; it costs
        // nothing but this one drop, and there will be another.
        let Some(Modelled { amount_out, impact, crossed }) = modelled else {
            anyhow::bail!(
                "route '{}': the model could not price this trade - see the reason logged \
                 just above; skipping rather than guessing",
                plan.route.name
            );
        };

        // Read, never asked. This process is the only thing signing with this
        // key while it runs, so the counter IS the nonce, and the gas price
        // arrives on the header stream whether or not anybody is trading. Both
        // used to fall back to a request when they had nothing, which put two
        // round trips between a drop and a broadcast at exactly the moment the
        // whole design exists to have none.
        let fees = self.fees.params();
        // Read back rather than returned from `model_quote`, which has two
        // callers and no use for it: one uncontended lock, off the critical
        // arithmetic and before the send.
        let state_age_s = plan
            .state
            .lock()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.at.elapsed().as_secs()));

        Ok(Quoted {
            amount_out,
            impact,
            crossed,
            state_age_s,
            took: started.elapsed(),
            fees,
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
        // The size this transaction spends, so the gas it really costs can be
        // measured afterwards at that size rather than at a token amount.
        spend: U256,
        quoted: Quoted,
    ) -> Result<ethers::types::H256> {
        let name = plan.route.name.clone();
        let fees = quoted.fees.context(
            "no gas price known yet - the header stream has not delivered one",
        )?;
        let gas_limit = U256::from(plan.gas_limit.load(Ordering::Relaxed));
        let nonce = self.claim_nonce().await?;
        let started = Instant::now();
        match swap::send_nowait(
            &self.submit,
            &self.wallet,
            tx,
            nonce.into(),
            fees,
            gas_limit,
        )
        .await
        {
            Ok(hash) => {
                info!(route = %name, ?hash, nonce, %gas_limit,
                      send_ms = started.elapsed().as_millis(), "sent");
                self.remeasure_gas(key, spend);
                Ok(hash)
            }
            Err(e) => {
                // The number was taken but never used, and every later
                // transaction would queue behind the hole it leaves.
                *self.next_nonce.lock().await = None;
                self.refill_nonce();
                Err(e)
            }
        }
    }

    /// Everything a sale used from memory, checked again once the sale is out.
    ///
    /// Both facts are cheap to be wrong about for one sale and expensive to ask
    /// for during one: a gas limit that is too low costs a revert, and unused
    /// gas is refunded, so the fallback errs high until a real measurement
    /// lands here. An approval that has been revoked stops the sale after this
    /// one rather than this one - which is the same window `--approve` already
    /// leaves, and far better than paying for the question every time.
    fn refresh_sell(self: &Arc<Self>, key: PoolRef, size: U256) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let Some(plan) = me.plans.get(&key) else { return };
            let sell = plan.route.reversed();
            if sell.input.address != Address::zero() {
                match swap::check_approvals(
                    &me.http, sell.input.address, me.owner, me.permit2, me.router,
                )
                .await
                {
                    Ok((erc20, p2)) => {
                        let unlimited =
                            erc20 >= effectively_unlimited() && p2 >= effectively_unlimited();
                        // Said out loud on the way down, never on the way up: a
                        // route that stops being sellable is a position that
                        // cannot be closed, and the next sale finding out for
                        // itself is too late to be the first anyone hears.
                        if !unlimited && plan.sell_approved.swap(false, Ordering::Relaxed) {
                            warn!(
                                route = %sell.name,
                                token = %sell.input.symbol,
                                %erc20, %p2,
                                "this token is no longer approved without limit - every sale \
                                 will check again, and one bigger than the allowance will fail"
                            );
                        } else {
                            plan.sell_approved.store(unlimited, Ordering::Relaxed);
                        }
                    }
                    Err(e) => warn!(
                        route = %sell.name, err = %format!("{e:#}"),
                        "could not re-check this token's approval"
                    ),
                }
            }
            let probe = match execute::pending_swap(
                me.router,
                &sell,
                size,
                U256::one(),
                execute::deadline_in(600),
            ) {
                Ok(p) => p,
                Err(_) => return,
            };
            if let Ok(g) = swap::measure_gas(&me.http, me.owner, &probe).await {
                plan.sell_gas_limit
                    .store(g.min(U256::from(u64::MAX)).as_u64(), Ordering::Relaxed);
            }
        });
    }

    /// Re-measure a route's gas in the background, so the next buy signs with a
    /// figure that reflects the pool as it is now.
    fn remeasure_gas(self: &Arc<Self>, key: PoolRef, size: U256) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let Some(plan) = me.plans.get(&key) else { return };
            // At the size that was just sent. A one-wei probe reverts - the
            // swap returns nothing - so it measured nothing and the limit was
            // always the fallback.
            let probe = match execute::pending_swap(
                me.router,
                &plan.route,
                size,
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
        let sell = route.reversed();
        let local = match sell.quote(&self.http, self.manager, Some(at), held).await {
            Ok(q) if !q.amount_out.is_zero() => q,
            _ => return,
        };
        let onchain = match execute::verify(
            &self.http,
            self.router,
            self.owner,
            &sell,
            held,
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
    /// against what this same walk predicted, and it IS applied here as
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
    /// Returns `None` whenever anything is missing: no measurement, no state
    /// for a hop, or a ladder that does not reach as far as the swap walks. A
    /// buy is then skipped and a sale asks the router. Being slow is
    /// recoverable; signing a wrong minimum is not.
    fn prepare(
        &self,
        plan: &Plan,
        route: &Route,
        fresh: Option<(PoolRef, HopState)>,
        yield_ppm: u64,
    ) -> Option<Prepared> {
        // Never priced by a measurement that does not exist. The callers check
        // this too, and both of them refusing is the point.
        if yield_ppm == 0 {
            warn!(
                route = %route.name,
                "not priced: this direction has never been measured (see calibrate_secs)"
            );
            return None;
        }
        // Copied out and the lock let go immediately. Holding it through the
        // walk would make this function have to be async, and the tick loop -
        // which is not - has to be able to size a buy before committing money
        // for it.
        let guard = plan.state.lock().ok();
        let snapshot = guard.as_deref().and_then(|s| s.as_ref());
        // Kept rather than collapsed into the filter below, because every one
        // of the refusals in this function used to arrive as the same sentence
        // listing three possible causes, and a log that makes the reader guess
        // between three is worth about as much as no log.
        let stale_after = state_stale_after(self.calibrate_secs);
        let age = snapshot.map(|s| s.at.elapsed());
        // Keyed by pool rather than by position, so the same state serves a
        // route walked in either order. Absent or stale, it simply is not
        // there to fall back on.
        let known: Option<HashMap<PoolRef, HopState>> = snapshot
            .filter(|s| s.at.elapsed() <= stale_after && s.hops.len() == plan.route.hops.len())
            .map(|s| {
                plan.route
                    .hops
                    .iter()
                    .map(|h| h.pool_ref())
                    .zip(s.hops.iter().copied())
                    .collect()
            });
        // Everything wanted from it has been copied out, and a `MutexGuard` in
        // a binding otherwise lives to the end of the function - which here is
        // a whole route walk, with the calibration pass waiting to write.
        drop(guard);

        let mut prepared = Vec::with_capacity(route.hops.len());
        for hop in &route.hops {
            // Freshest first. The signal's own log describes the pool it came
            // from as of that very swap; the tick scan is at most half a minute
            // old and keeps running when calibration cannot; the calibration
            // snapshot is the last resort and used to be the only one.
            let scanned = self.ticks.state(hop.pool_ref()).map(|s| HopState {
                sqrt_p: s.sqrt_p,
                liquidity: s.liquidity,
                lp_fee: s.lp_fee,
                protocol_fee_0for1: s.protocol_fee_0for1,
                protocol_fee_1for0: s.protocol_fee_1for0,
            });
            let mut here = match fresh {
                Some((p, s)) if p == hop.pool_ref() => s,
                _ => match scanned.or_else(|| known.as_ref().and_then(|k| k.get(&hop.pool_ref())).copied()) {
                    Some(s) => s,
                    None => {
                        warn!(
                            route = %route.name,
                            pool = %hop.pool_ref(),
                            snapshot_age_s = age.map(|a| a.as_secs()),
                            usable_for_s = stale_after.as_secs(),
                            calibrate_secs = self.calibrate_secs,
                            "not priced: no usable state for this hop - the signal does not \
                             cover it, the tick scan has none for it, and the calibration \
                             snapshot is missing, too old, or describes a different number \
                             of hops"
                        );
                        return None;
                    }
                },
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
            // The pool as we believe it to be, and then the ticks around it.
            // A swap is exact for as long as liquidity holds, and liquidity
            // holds exactly until an initialized tick is crossed - so the walk
            // needs to know where those are, and the background scan has
            // already read them. See `TickBook`.
            let state = crate::depth::PoolState {
                sqrt_p: here.sqrt_p,
                liquidity: here.liquidity,
                lp_fee: here.lp_fee,
                protocol_fee_0for1: here.protocol_fee_0for1,
                protocol_fee_1for0: here.protocol_fee_1for0,
            };
            let up = !hop.zero_for_one();
            let Some(ladder) = self.ticks.ladder(hop.pool_ref(), here.sqrt_p, up) else {
                warn!(
                    route = %route.name,
                    pool = %hop.pool_ref(),
                    "not priced: no tick ladder covers this pool's price, so a swap through \
                     it cannot be walked from memory"
                );
                return None;
            };
            prepared.push(Hopped {
                pool: hop.pool_ref(),
                zero_for_one: hop.zero_for_one(),
                state,
                ladder,
            });
        }

        Some(Prepared { hops: prepared })
    }

    /// Price a route at one size, or say why not.
    ///
    /// `yield_ppm` is what calibration measured this direction to actually pay
    /// against what this same walk predicted, and it IS applied as a term:
    /// whatever a hook takes on top of the pools' stated fees is real money,
    /// and a quote that leaves it out is optimistic by exactly that much. It is
    /// clamped at 1.0 - a route measured to pay more than the model says is a
    /// stale snapshot or measurement noise, never a bonus to price in.
    fn model_quote(
        &self,
        route: &Route,
        prepared: &Prepared,
        yield_ppm: u64,
        amount_in: U256,
    ) -> Option<Modelled> {
        let walked = prepared.walk(crate::route::u256_to_f64(amount_in)).or_else(|| {
            warn!(
                route = %route.name,
                amount_in = %format_units(amount_in, route.input.decimals),
                "not priced: the walk returned nothing for this size - it walks past what \
                 the tick scan read, or a hop has no liquidity in this direction"
            );
            None
        })?;

        // What the pools state, less what this direction was measured to pay
        // beyond them.
        let amount = walked.amount_out * yield_ppm.min(PPM) as f64 / PPM as f64;
        let raw = crate::route::f64_to_u256_pub(amount);
        // The worst hop, not the last: a quote is only as trustworthy as the
        // pool it strained most.
        let worst = walked.impacts.iter().map(|(_, i)| *i).fold(0.0f64, f64::max);
        (!raw.is_zero()).then_some(Modelled {
            amount_out: raw,
            impact: worst,
            crossed: walked.crossed,
        })
    }

    /// Take the next nonce.
    ///
    /// `observed` is what the node said a moment ago, and it is only fetched
    /// when this process has no counter of its own - see `quote`. The higher of
    /// the two still wins where both exist: the counter can know about a
    /// transaction sent so recently that the node has not counted it yet, and
    /// signing a nonce that is already spent costs the whole buy.
    ///
    /// The premise is that nothing else signs with this key while the bot runs.
    /// If something did, its transaction would be missed until the next send
    /// failed and cleared the counter - which is what makes that reset matter.
    async fn claim_nonce(&self) -> Result<u64> {
        let mut slot = self.next_nonce.lock().await;
        let n = slot.context(
            "no nonce in hand - the counter was cleared by a failed send and is being \
             refilled; this trade is skipped rather than made to wait for it",
        )?;
        *slot = Some(n + 1);
        Ok(n)
    }

    /// Put a number back in the counter, off the path of any trade.
    ///
    /// Called when a send clears it. Asking here rather than at the next trade
    /// is the whole point: the gap a failed send leaves is exactly what nobody
    /// knows the size of, and finding out takes a round trip that a buy must
    /// not be made to wait for.
    fn refill_nonce(self: &Arc<Self>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            match swap::pending_nonce(&me.http, me.owner).await {
                Ok(n) => {
                    let mut slot = me.next_nonce.lock().await;
                    // Only if nothing has claimed one meanwhile: the node may
                    // not have counted a transaction sent a moment ago.
                    if slot.is_none() {
                        *slot = Some(n);
                        info!(nonce = n, "nonce counter refilled");
                    }
                }
                Err(e) => warn!(err = %format!("{e:#}"), "could not refill the nonce counter"),
            }
        });
    }
}

/// Everything that can be checked before the first drop: is there anything to
/// spend, and may the router spend it - approving it here when it may not.
///
/// BOTH ends of the route, not only the token a buy spends. The token a sale
/// hands back down the same pools is spent too, and an unapproved one is a
/// position that can be opened and not closed - the worse half of the pair, and
/// the half that used to be found out about at the first take-profit. Set up at
/// the same moment as the other, while nobody is waiting.
///
/// Returns whether the token this route SELLS is now approved without limit, so
/// the first sale can skip the two `eth_call`s that ask - see `Plan`'s
/// `sell_approved`, which is what the answer is kept in.
async fn preflight(approve: &mut Approver<'_>, route: &Route) -> Result<bool> {
    // No fixed size to compare against any more: what a buy costs is worked out
    // per signal and bounded by whatever is here. An empty wallet is still worth
    // saying out loud, because it means no drop can be acted on at all.
    let balance = swap::balance_of(approve.http, route.input.address, approve.owner).await?;
    if balance.is_zero() {
        warn!(
            route = %route.name,
            token = %route.input.symbol,
            "nothing to buy with; every drop will be skipped until this is topped up"
        );
    }
    approve
        .ensure_spendable(&route.input)
        .await
        .with_context(|| format!("route '{}': the token it spends", route.name))?;
    approve
        .ensure_spendable(&route.output)
        .await
        .with_context(|| format!("route '{}': the token it sells back", route.name))
}

/// Who signs, and which contracts a token has to be approved to.
///
/// One value rather than a row of arguments, for the reason `inventory::Trade`
/// is one: three addresses in a row are three addresses easy to hand over in
/// the wrong order, and here the wrong order would approve the wrong spender.
struct Approver<'a> {
    http: &'a Provider<Http>,
    wallet: &'a LocalWallet,
    owner: Address,
    permit2: Address,
    router: Address,
    /// Sign and send, rather than only printing what would have been sent.
    execute: bool,
    /// Tokens already dealt with this start, and what came of it. Two routes
    /// sharing a token settle it once between them, and the second reads the
    /// answer rather than the chain.
    done: HashMap<Address, bool>,
}

impl Approver<'_> {
    /// Make sure the router may spend `token` without a practical ceiling,
    /// setting the approval up when it may not, and say whether it now may.
    ///
    /// A missing approval is not a condition to report and stop on: it is one
    /// this bot already knows how to fix, and `--approve` fixes it with exactly
    /// these two transactions. Doing it at startup costs one round of gas once
    /// per token and removes the whole class of "armed, watching, and unable to
    /// trade" - which was previously discovered either at startup as an error
    /// telling the operator to go and run another command, or, on the selling
    /// side, at the one moment a position needed closing.
    ///
    /// The bar is `effectively_unlimited`, the same one a sale's cached
    /// approval uses and for the same reason: this bot only ever grants
    /// unlimited, so anything short of it is a partial allowance that a large
    /// enough trade runs through - and one that was only ever big enough for a
    /// smaller size is how a sale comes to be signed against an allowance that
    /// cannot cover it. Whatever is there is logged before it is replaced.
    ///
    /// Nothing is sent without `--execute`, as everywhere else: a dry run
    /// prints the transactions it would have sent, says the token is still not
    /// approved, and carries on arming - because a dry run has no trade to fail
    /// at later.
    async fn ensure_spendable(&mut self, token: &Token) -> Result<bool> {
        // Native ETH is passed as msg.value: there is nothing to approve, and
        // every caller skips the question for it anyway.
        if token.address == Address::zero() {
            return Ok(true);
        }
        if let Some(&known) = self.done.get(&token.address) {
            return Ok(known);
        }
        let unlimited = self.approve_unlimited(token).await?;
        self.done.insert(token.address, unlimited);
        Ok(unlimited)
    }

    /// The work itself. Memoised by `ensure_spendable`, which is the only
    /// caller and the only thing that should ever ask twice.
    async fn approve_unlimited(&self, token: &Token) -> Result<bool> {
        let limit = effectively_unlimited();
        let (erc20, p2) = self.allowances(token).await?;
        if erc20 >= limit && p2 >= limit {
            info!(token = %token.symbol, "approved without limit already");
            return Ok(true);
        }

        let txs = swap::build_unlimited_approval(token.address, self.permit2, self.router)?;
        warn!(
            token = %token.symbol,
            addr = ?token.address,
            erc20_to_permit2 = %erc20,
            permit2_to_router = %p2,
            "NOT approved without limit - approving it now"
        );
        if !self.execute {
            println!(
                "\nwould approve {} ({:?}), {} transaction(s):",
                token.symbol,
                token.address,
                txs.len()
            );
            for (i, tx) in txs.iter().enumerate() {
                tx.print(i);
            }
            println!("  dry run - nothing sent; start with --execute to approve for real");
            return Ok(false);
        }

        println!(
            "\napproving {} ({:?}) from {:?}, {} transaction(s)",
            token.symbol,
            token.address,
            self.owner,
            txs.len()
        );
        swap::send_all(self.http, self.wallet.clone(), &txs)
            .await
            .with_context(|| format!("approving {} for the router", token.symbol))?;

        // The receipts say the transactions succeeded. This says the allowance
        // they were for is actually there - which is not the same claim, and is
        // the only one that matters to the trade that will rely on it.
        let (erc20, p2) = self.allowances(token).await?;
        anyhow::ensure!(
            erc20 >= limit && p2 >= limit,
            "{} was approved but the allowance did not take (erc20->permit2 {erc20}, \
             permit2->router {p2}); this token may not behave like a standard ERC-20",
            token.symbol
        );
        info!(token = %token.symbol, "approved without limit");
        Ok(true)
    }

    /// What the two allowances in the chain are now: the ERC-20's to Permit2,
    /// and Permit2's to the router. Both have to be there; either alone spends
    /// nothing.
    async fn allowances(&self, token: &Token) -> Result<(U256, U256)> {
        swap::check_approvals(self.http, token.address, self.owner, self.permit2, self.router)
            .await
            .with_context(|| format!("reading {}'s allowances", token.symbol))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sizing decides how much money leaves the wallet, so what it must never
    /// do is return a size bigger than asked for. Impact rises with size, and
    /// the answer is always the low side of the bracket.
    #[test]
    fn a_size_never_overshoots_the_target() {
        // A pool where impact is simply proportional: 1000 units move it 1%.
        let linear = |a: U256| Some(a.as_u64() as f64 / 100_000.0);
        let cap = U256::from(10_000u64);

        // Target reachable well inside the ceiling.
        let got = size_to_impact(cap, 0.01, linear).expect("sizeable");
        assert!(linear(got).unwrap() <= 0.01, "must not overshoot");
        assert!(got > U256::from(900u64) && got <= U256::from(1_000u64), "{got}");

        // Target the ceiling cannot reach: spend the ceiling, which is the
        // whole reason a ceiling is kept.
        assert_eq!(size_to_impact(cap, 0.5, linear), Some(cap));

        // A target so small nothing meaningful fits.
        assert_eq!(size_to_impact(U256::from(1u64), 1e-12, linear), None);
        assert_eq!(size_to_impact(U256::zero(), 0.01, linear), None);
    }

    /// A size the walk cannot price counts as too big. Being unable to answer
    /// is not permission to send - and a ceiling far past what the tick scan
    /// read looks exactly like this, while a smaller size is still answerable.
    #[test]
    fn an_unpriceable_size_is_treated_as_too_big() {
        let cap = U256::from(10_000u64);
        // Anything past 2000 walks off the end of what was read.
        let short = |a: U256| {
            (a <= U256::from(2_000u64)).then(|| a.as_u64() as f64 / 100_000.0)
        };

        let got = size_to_impact(cap, 0.05, short).expect("the smaller sizes are priceable");
        assert!(got <= U256::from(2_000u64), "never past what could be priced: {got}");
        assert!(got > U256::from(1_900u64), "and not needlessly small either: {got}");

        // Nothing priceable at all is a refusal, not a guess.
        assert_eq!(size_to_impact(cap, 0.05, |_| None), None);
    }

    /// The bar that decides whether a token gets approved at startup has to sit
    /// above anything a position could ever reach and below what `--approve`
    /// actually grants. Too high and every start re-approves for ever; too low
    /// and an allowance that a real trade runs through passes for unlimited.
    #[test]
    fn the_unlimited_bar_sits_between_a_real_size_and_a_real_approval() {
        let bar = effectively_unlimited();

        // What the approval this bot sends actually sets.
        let permit2_max = (U256::one() << 160) - 1;
        assert!(permit2_max > bar, "a full Permit2 approval must clear the bar");
        assert!(U256::MAX > bar, "a full ERC20 approval must clear the bar");

        // A billion tokens at 18 decimals - orders of magnitude past anything
        // these routes trade, and still nowhere near the bar.
        let absurd = U256::from(10u64).pow(U256::from(27u64));
        assert!(absurd < bar, "no position this bot can build may reach the bar");

        // And an approval sized for a trade, however generously, must NOT pass
        // for unlimited: a million tokens at 18 decimals is still a ceiling,
        // and startup has to replace it rather than accept it.
        let generous = U256::from(10u64).pow(U256::from(24u64));
        assert!(generous < bar, "a trade-sized allowance must not read as unlimited");
    }
}
