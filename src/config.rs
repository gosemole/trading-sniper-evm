use anyhow::Context;
use serde::{Deserialize, Deserializer};

/// A string that must never reach a log. `Debug` prints a placeholder, so an
/// accidental `{cfg:?}` cannot leak the key.
#[derive(Clone, Default)]
pub struct Secret(String);

impl Secret {
    /// The only way to read the value. Call it at the point of use, never store
    /// the result anywhere that gets formatted.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "[unset]"
        } else {
            "[redacted]"
        })
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Secret)
    }
}

use std::collections::HashMap;
use std::path::Path;

fn default_max_move() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// WebSocket endpoint for live log subscriptions. Prefer the WS_URL
    /// environment variable: the endpoint carries an API key, and a key in a
    /// config file also lands in backups, editor state and tool output.
    /// WS_URL overrides whatever is written here.
    #[serde(default)]
    pub ws_url: String,
    /// HTTP JSON-RPC endpoint for on-demand calls / fallback. Set via the
    /// HTTP_URL environment variable for the same reason as `ws_url`.
    #[serde(default)]
    pub http_url: String,
    /// Endpoints to broadcast signed transactions through, all at once.
    ///
    /// Submission is the only round trip a buy waits on and the one that
    /// decides whether the trade exists, so it is worth not depending on a
    /// single node's queue or a single node's uptime. Every endpoint here is
    /// handed the identical signed transaction and the first to take it wins;
    /// the rest keep going, because being in more than one mempool is the
    /// point. Reads are unaffected - they still go to `http_url`.
    ///
    /// Empty means "just use `http_url`", which is what this did before. Set
    /// via SUBMIT_URLS (comma-separated) for the same reason as `ws_url`: these
    /// carry API keys.
    #[serde(default)]
    pub submit_urls: Vec<String>,
    /// Signal when price moves >= threshold % between consecutive blocks.
    pub threshold_pct: f64,
    /// Assumed price move (%) for the depth estimate: "how much can I buy if the
    /// price moves this much". In-range liquidity only (no tick walking).
    #[serde(default = "default_max_move")]
    pub max_move_pct: f64,
    /// What the launch sniper is willing to do. Absent means the defaults
    /// below, which buy nothing until a size is given.
    #[serde(default)]
    pub snipe: SnipeConfig,
    /// Ticker -> address registry, e.g. `POOLS = "0x385b..."`. Any pool field
    /// that takes a token accepts either a ticker from here or a raw address.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
    /// How often (seconds) to re-measure what an armed route's pools take on
    /// top of their stated fees - a hook charging its own cut shows up here and
    /// nowhere else. The figure is both a term in the price and a check: a
    /// route never measured is not traded, and one found keeping more than 5%
    /// of a swap is refused (see `executor::unstated_fee_acceptable`). So 0
    /// here means no armed route is ever bought. Only armed routes are
    /// measured, and only in the background.
    ///
    /// This also paces the pool snapshot the hops a signal says nothing about
    /// are priced from, because the same pass writes it - and the snapshot is
    /// trusted for twice this interval, so raising this makes buys price off
    /// older state as well as measuring the fee less often.
    #[serde(default = "default_calibrate")]
    pub calibrate_secs: u64,
    /// Where to keep what the chain has already told us about pools and
    /// tokens - PoolKeys, decimals, symbols. All of it immutable, so the file
    /// only ever saves time; delete it and the next start is merely slow.
    #[serde(default = "default_pool_cache")]
    pub pool_cache_path: String,
    /// Where to keep what we hold and what it cost. Read at startup and
    /// written after every fill, so a restart does not forget an entry price.
    #[serde(default = "default_inventory")]
    pub inventory_path: String,
    /// Native currency that must be on hand for gas, in whole units - "0.05"
    /// is 0.05 ETH.
    ///
    /// Chiefly a GATE on buying. Below it no route buys anything, whatever it
    /// spends: a position bought with the last of the gas is a position that
    /// cannot be sold, and a bag nobody can put down is worse than a dip nobody
    /// caught. Getting in is optional; getting out is not.
    ///
    /// Nothing else: a route may not spend the native currency at all, so the
    /// balance a trade comes out of and the balance gas comes out of are never
    /// the same pot. A pool holding native ETH is traded by holding the wrapped
    /// token - see `weth`.
    ///
    /// A flat figure rather than an estimate per trade. An estimate is only as
    /// good as the last gas price seen and has to be right every time; a
    /// reserve large enough for many transactions has to be right once.
    #[serde(default = "default_gas_reserve")]
    pub gas_reserve: String,
    /// The wrapped native token, when a route pays in it for a pool that holds
    /// the native one.
    ///
    /// A v4 pool NAMES its currencies: one holding native ETH is a different
    /// pool from one holding WETH, and which to use is not a choice. Setting
    /// this lets a route hold WETH anyway - the router unwraps on the way in
    /// and wraps on the way out, in the same transaction - so the native
    /// balance is left alone for gas and the traded balance is exactly what
    /// this bot moved.
    ///
    /// Unset means a route spending the native currency spends it directly, as
    /// it always did.
    #[serde(default)]
    pub weth: Option<String>,
    /// Uniswap Universal Router, the contract swaps are sent to.
    #[serde(default)]
    pub universal_router: Option<String>,
    /// Permit2. Defaults to the canonical deterministic deployment.
    #[serde(default)]
    pub permit2: Option<String>,
    /// Multicall3, used to read a pool that has no batch read of its own - a v3
    /// one, whose state is only reachable through its own view functions.
    /// Defaults to the canonical deterministic deployment.
    ///
    /// Never required: a chain without it, or an address with nothing at it,
    /// simply means one request per read instead of one for all of them.
    #[serde(default)]
    pub multicall: Option<String>,
    /// v4 PoolManager. Every v4 pool on a chain shares one, so it belongs here
    /// beside the router rather than repeated as each pool's `address` - which
    /// is what `[[pools]]` used to require, one identical line per pool.
    ///
    /// A v4 pool with no `address` of its own uses this. Still optional, and
    /// still inferred from the first v4 pool that does name one, so a config
    /// written the old way keeps working unchanged.
    #[serde(default)]
    pub pool_manager: Option<String>,
    /// Signing key for swap execution. Prefer the PRIVATE_KEY environment
    /// variable: a key in a config file also lands in backups and editor state.
    /// Whatever is set here is overridden by PRIVATE_KEY when that is present.
    #[serde(default)]
    pub private_key: Secret,
    /// Pools to watch.
    pub pools: Vec<PoolConfig>,
    /// Swap routes available to execute.
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

/// An ordered chain of pools to swap through, written as pool ids only: the
/// full PoolKey of each is recovered on chain and verified against the id.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteConfig {
    pub name: String,
    /// Ticker or address of the token being spent.
    pub input: String,
    /// Tolerated shortfall against the quote, used for amountOutMinimum. It is
    /// applied to the quote this trade was actually priced from and to nothing
    /// measured earlier, so it caps the slippage of this pair alone - but WHICH
    /// quote that is differs by path, and so does what this number is covering:
    ///
    /// - an auto-buy is priced by the model, which never asks the router (see
    ///   `executor::model_quote`), so this covers both the market moving before
    ///   the swap lands AND the model being wrong;
    /// - `--swap`, `--sell-all` and a sale the model declines are priced by the
    ///   router itself, where only the first of those two is left to cover.
    ///
    /// Because the model shares this budget, it is also measured against it:
    /// `executor::modelled_impact_cap` allows a modelled trade to move a pool
    /// by up to a third of this, so raising the tolerance widens what the model
    /// is willing to price and lowering it narrows it. The two used to be
    /// unrelated numbers, and tightening this one silently left the modelling
    /// cap sized for the old one.
    pub max_slippage_pct: f64,
    /// Size every buy so it moves the TRIGGER pool's price by this much.
    ///
    /// There is no fixed amount and no ceiling written here: the ceiling is the
    /// tracked balance. A size is worked out per signal, and if the whole
    /// balance still cannot move the pool this far the buy is skipped rather
    /// than shrunk - a size that does not do what was asked is not a smaller
    /// version of the trade, it is a different one.
    ///
    /// Measured on the pool the drop happened in, not on the worst hop of the
    /// route: that is the pool the number is a statement about. Worked out from
    /// the signal's own log and the tick ladder already in memory, so it costs
    /// no requests and nothing waits for it.
    pub impact_pct: f64,
    /// v4 pool ids, in swap order.
    pub pools: Vec<String>,
    /// Buy this route by itself whenever its trigger pool signals a big sell.
    /// Nothing is sent unless the process was also started with --execute.
    #[serde(default)]
    pub auto_buy: bool,
    /// Pool whose drop arms this route - a v4 pool id or a v3 pool address,
    /// written exactly as in `pools`. Defaults to the LAST pool in `pools`:
    /// the buy goes through the very pool the drop was seen in, so the route
    /// is bought where the price actually moved.
    #[serde(default)]
    pub trigger_pool: Option<String>,
    /// Sell the whole position back down this route once it is worth this much
    /// more than it cost. Unset means never: the bot buys and holds.
    ///
    /// NET of the round trip. What the buy really paid - both fees, the hook's
    /// cut, the impact of our own size - is inside the recorded entry price,
    /// and the same costs are expected again on the way out, so the pool price
    /// this fires at is higher than the entry by this much PLUS the round trip.
    /// 5 here means five percent kept, not five percent of pool-price movement.
    ///
    /// Still measured against the pool's own price, so it is a gain against the
    /// pool's quote token, not against the dollar.
    #[serde(default)]
    pub take_profit_pct: Option<f64>,
    /// Sell the position regardless of price once it has been held this long,
    /// counted from the MOST RECENT buy - so averaging further into a dip
    /// restarts the clock. Unset means hold indefinitely.
    ///
    /// This is checked on a timer rather than on price updates: a position
    /// worth abandoning is often in a pool that has gone quiet, and a rule that
    /// only fires on a tick would never fire on exactly those.
    #[serde(default)]
    pub exit_after_secs: Option<u64>,
    /// Shortest gap between two automatic buys of this route. A drop usually
    /// arrives as a run of blocks, and without a gap each of those blocks buys
    /// again. Zero is allowed and means exactly that: every signal buys.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

fn default_cooldown() -> u64 {
    60
}

fn default_calibrate() -> u64 {
    300
}

fn default_gas_reserve() -> String {
    "0.05".to_string()
}

fn default_pool_cache() -> String {
    "pools.json".to_string()
}

fn default_inventory() -> String {
    "inventory.json".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub name: String,
    /// v3: the pool contract, and required - a v3 pool IS an address.
    ///
    /// v4: the PoolManager, and omitted in normal use. Every v4 pool on a chain
    /// shares one manager, so it is written once as the global `pool_manager`
    /// and left out here.
    #[serde(default)]
    pub address: Option<String>,
    /// Either "v3" (Uniswap V3 pool) or "v4" (Uniswap V4 via PoolManager).
    pub version: String,
    /// Ticker (from `[tokens]`) or raw address. v3: optional, read from the
    /// pool contract when omitted. v4: optional, but without it decimals stay
    /// unknown unless the Initialize log is reachable.
    /// For v4 the pair is sorted into canonical PoolKey order automatically,
    /// so the order written here does not matter.
    pub token0: Option<String>,
    /// See `token0`.
    pub token1: Option<String>,
    /// Optional either way: resolved on-chain when omitted, and only used for
    /// absolute prices - the %-movement detector does not need it.
    pub decimals0: Option<u8>,
    /// See `decimals0`.
    pub decimals1: Option<u8>,
    /// Index (0 or 1) into the SORTED PoolKey of the token the price is quoted
    /// FOR - a drop is a drop of this one. Usually omitted: a pool named
    /// `BASE/QUOTE` says the same thing in a form that cannot be got backwards,
    /// and the index is derived from it. Give it only when the name is not a
    /// pair, and it must then agree with the name if there is one.
    #[serde(default)]
    pub base_token: Option<u8>,
    /// Optional per-pool drop threshold (%). If unset, the global
    /// `threshold_pct` is used for this pool.
    #[serde(default)]
    pub threshold_pct: Option<f64>,
    /// Optional per-pool slippage (%, for the buy depth estimate). If unset, the
    /// global `max_move_pct` is used.
    #[serde(default)]
    pub max_move_pct: Option<f64>,
    /// v4 only: PoolId as 32-byte hex (e.g. "0x..."). If given, fee/tick_spacing/
    /// hooks/token0/token1 are not needed.
    pub pool_id: Option<String>,
    /// v4 only: fee in hundredths of a bip (e.g. 3000 = 0.3%).
    pub fee: Option<u32>,
    /// v4 only: tick spacing.
    pub tick_spacing: Option<i32>,
    /// v4 only: hooks address (0x0 if none).
    pub hooks: Option<String>,
}

/// The sniper's standing policy.
///
/// Kept here rather than in the code because every number in it is a judgement
/// about this launchpad at this moment - what a creator tax is worth paying,
/// how far a curve may have run - and those are the things that change without
/// the code changing. Only the rules stay in the code.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
pub struct SnipeConfig {
    /// The deployed `PonsSniper`, which a launch is bought THROUGH rather than
    /// from. It holds the standing approval for each quote token, so a curve
    /// that did not exist a second ago is approved and bought from in one
    /// transaction instead of two.
    ///
    /// Absent means buying straight from the curve, which a native launch can
    /// do - `buy` is payable and takes ETH from the wallet. An ERC-20 launch
    /// cannot: it needs an approval to an address that only comes into being
    /// in the launch transaction itself, and that second transaction does not
    /// fit in the window.
    #[serde(default)]
    pub contract: Option<String>,
    /// What to spend on one launch, as hundredths of a percent of the curve's
    /// phantom reserve. 100 is one percent.
    ///
    /// A fraction rather than an amount, because the same amount means
    /// different things on different pairs: 0.05 into a native curve, whose
    /// phantom reserve is 1.68 ETH, is three percent of it - and into a USDG
    /// one, whose reserve is 3236, it is nothing at all. What decides the
    /// result is the share of the curve taken, since that is what our own
    /// buying and selling moves.
    ///
    /// Zero means nothing is followed and nothing is decided.
    #[serde(default = "default_size_x100")]
    pub size_x100: u64,
    /// A ceiling on that, in the PAIR TOKEN's own units - "0.1" is 0.1 ETH on
    /// a native launch. Empty means no ceiling. For the pairs whose reserves
    /// are large enough that a percent of them is more than is wanted at risk.
    #[serde(default)]
    pub size: String,
    /// How much of the price to give away. Must stay BELOW the gap between two
    /// tax steps, or the minimum stops telling them apart: a fill one second
    /// early would satisfy it, and the point of the minimum is that it cannot.
    #[serde(default = "default_slippage_bps")]
    pub slippage_bps: u64,
    /// The most snipe tax worth paying. The launch second is not reachable
    /// through this at any value - the curve charges 99% there and the wrapper
    /// refuses it outright.
    #[serde(default = "default_max_tax_bps")]
    pub max_tax_bps: u64,
    /// A creator tax is charged on the way in AND on the way out, so it is
    /// paid twice before the price has moved at all. Zero by default: a launch
    /// that wants a cut of both legs is a launch there is no need to be in.
    #[serde(default)]
    pub max_creator_tax_bps: u64,
    /// How far above the opening price this will still buy, in hundredths.
    /// A launch whose exempt wallets bundled into its own block opens the free
    /// window at four or five times what the curve started at, and buying
    /// there is buying their exit.
    #[serde(default = "default_max_run_x100")]
    pub max_run_x100: u64,
    /// More declared exemptions than this and the launch is an arrangement
    /// rather than a market.
    #[serde(default = "default_max_exempt")]
    pub max_exempt: usize,
    /// Refuse a launch whose maker bought none of it - `launchToken` rather
    /// than `launchAndBuy`.
    ///
    /// The sharpest thing in the journals so far, and known from the feed
    /// before the block exists: of the launches made without a dev buy, four
    /// in five saw no trade at all in their first minute; of those made with
    /// one, none were dead.
    #[serde(default = "default_true")]
    pub require_dev_buy: bool,
    /// The smallest dev buy worth following, against the curve's phantom
    /// reserve, in hundredths of a percent. 500 is five percent.
    #[serde(default = "default_min_dev_buy_x100")]
    pub min_dev_buy_x100: u64,
    /// How long before a step opens the decision is made, in milliseconds. The
    /// transaction still has to be signed and sent after it.
    #[serde(default = "default_lead_ms")]
    pub lead_ms: u64,
}

fn default_slippage_bps() -> u64 {
    100
}
fn default_max_tax_bps() -> u64 {
    19
}
fn default_max_run_x100() -> u64 {
    200
}
fn default_max_exempt() -> usize {
    4
}
fn default_lead_ms() -> u64 {
    100
}
fn default_true() -> bool {
    true
}
fn default_min_dev_buy_x100() -> u64 {
    500
}
fn default_size_x100() -> u64 {
    100
}

impl Default for SnipeConfig {
    fn default() -> Self {
        Self {
            contract: None,
            size_x100: default_size_x100(),
            size: String::new(),
            slippage_bps: default_slippage_bps(),
            max_tax_bps: default_max_tax_bps(),
            max_creator_tax_bps: 0,
            max_run_x100: default_max_run_x100(),
            max_exempt: default_max_exempt(),
            require_dev_buy: true,
            min_dev_buy_x100: default_min_dev_buy_x100(),
            lead_ms: default_lead_ms(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config =
            toml::from_str(&raw).with_context(|| format!("parsing config {}", path.display()))?;
        // The environment wins over the file, so a checkout can carry a config
        // with no endpoints and no key at all.
        if let Some(v) = env_var("WS_URL") {
            cfg.ws_url = v;
        }
        if let Some(v) = env_var("HTTP_URL") {
            cfg.http_url = v;
        }
        if let Some(v) = env_var("SUBMIT_URLS") {
            cfg.submit_urls = v
                .split(',')
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
                .collect();
        }
        if let Some(v) = env_var("PRIVATE_KEY") {
            cfg.private_key = Secret(v);
        }
        validate(&cfg).with_context(|| format!("in config {}", path.display()))?;
        Ok(cfg)
    }

    /// The armed route that buys when this pool drops, if any.
    ///
    /// One place, because two of them would drift: startup arms a route
    /// against a pool here, and a reload has to find the SAME route or it would
    /// retune a pool against somebody else's numbers.
    pub fn armed_route(&self, trigger: crate::route::PoolRef) -> Option<&RouteConfig> {
        self.routes
            .iter()
            .find(|r| r.auto_buy && r.trigger() == Some(trigger))
    }
}

impl RouteConfig {
    /// The pool whose price this route reacts to: the one named, else the pool
    /// it ends in. `None` when neither parses as a pool.
    pub fn trigger(&self) -> Option<crate::route::PoolRef> {
        let raw = self
            .trigger_pool
            .clone()
            .or_else(|| self.pools.last().cloned())?;
        crate::route::parse_pool_ref(&raw).ok()
    }
}

impl PoolConfig {
    /// How the rest of the process names this pool: a v4 pool by its id, a v3
    /// pool by its own address. `None` when the file gives neither, which is a
    /// pool nothing can be matched against.
    pub fn pool_ref(&self) -> Option<crate::route::PoolRef> {
        match (&self.pool_id, &self.address) {
            (Some(id), _) => crate::route::parse_pool_ref(id).ok(),
            (None, Some(a)) => crate::route::parse_pool_ref(a).ok(),
            (None, None) => None,
        }
    }
}

/// What changed in the file that only a restart can apply.
///
/// Everything here was read once and handed to something that has been running
/// on it ever since - a websocket, a broadcaster, a wallet, a resolved route.
/// Naming them is the point: an operator who edits an endpoint and sends SIGHUP
/// must not be left believing the process took it.
pub fn restart_only(running: &Config, next: &Config) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut differs = |name: &'static str, same: bool| {
        if !same {
            out.push(name);
        }
    };
    differs("ws_url", running.ws_url == next.ws_url);
    differs("http_url", running.http_url == next.http_url);
    differs("submit_urls", running.submit_urls == next.submit_urls);
    differs(
        "private_key",
        running.private_key.expose() == next.private_key.expose(),
    );
    differs("gas_reserve", running.gas_reserve == next.gas_reserve);
    differs(
        "calibrate_secs",
        running.calibrate_secs == next.calibrate_secs,
    );
    differs("pool_manager", running.pool_manager == next.pool_manager);
    differs(
        "universal_router",
        running.universal_router == next.universal_router,
    );
    differs("permit2", running.permit2 == next.permit2);
    differs("weth", running.weth == next.weth);
    // Additions are fine - a pool added by a reload is resolved against the
    // new map. What a running process cannot follow is a ticker that MOVED:
    // every pool and route already resolved is holding the old address.
    differs(
        "tokens",
        running
            .tokens
            .iter()
            .all(|(k, v)| next.tokens.get(k) == Some(v)),
    );
    differs(
        "inventory_path",
        running.inventory_path == next.inventory_path,
    );
    differs(
        "pool_cache_path",
        running.pool_cache_path == next.pool_cache_path,
    );
    out
}

/// A non-empty environment variable, trimmed.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn validate(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(!cfg.pools.is_empty(), "no pools configured");
    // Parsed here rather than at the first buy: a reserve that turns out not to
    // be a number is a config mistake, and it should be one somebody is reading
    // a message about rather than one a trade discovers.
    crate::route::parse_units(&cfg.gas_reserve, 18)
        .with_context(|| format!("gas_reserve \"{}\" is not an amount", cfg.gas_reserve))?;
    anyhow::ensure!(
        !cfg.ws_url.trim().is_empty(),
        "no ws endpoint: set the WS_URL environment variable, or ws_url in config"
    );
    anyhow::ensure!(
        !cfg.http_url.trim().is_empty(),
        "no http endpoint: set the HTTP_URL environment variable, or http_url in config"
    );
    for (i, u) in cfg.submit_urls.iter().enumerate() {
        anyhow::ensure!(
            !u.trim().is_empty(),
            "submit_urls[{i}] is empty - remove the entry rather than leaving a blank one"
        );
    }
    if let Some(m) = &cfg.multicall {
        m.parse::<ethers::types::Address>()
            .map_err(|e| anyhow::anyhow!("multicall \"{m}\" is not an address: {e}"))?;
    }
    if let Some(w) = &cfg.weth {
        w.parse::<ethers::types::Address>()
            .map_err(|e| anyhow::anyhow!("weth \"{w}\" is not an address: {e}"))?;
    }
    for (ticker, addr) in &cfg.tokens {
        anyhow::ensure!(!ticker.is_empty(), "[tokens] has an empty ticker");
        addr.parse::<ethers::types::Address>().map_err(|e| {
            anyhow::anyhow!("[tokens] {ticker} = \"{addr}\" is not an address: {e}")
        })?;
    }
    anyhow::ensure!(
        cfg.threshold_pct.is_finite() && cfg.threshold_pct > 0.0,
        "threshold_pct must be > 0"
    );
    // A zero or negative move makes the depth estimate 0 or NaN.
    anyhow::ensure!(
        cfg.max_move_pct.is_finite() && cfg.max_move_pct > 0.0,
        "max_move_pct must be > 0"
    );
    anyhow::ensure!(
        cfg.snipe.slippage_bps < 10_000,
        "[snipe] slippage_bps is the whole trade"
    );
    // Below the gap between two tax steps, or the minimum it produces stops
    // refusing a fill at the dearer one - which is the only thing that makes a
    // buy landing a second early revert instead of paying six percent.
    if cfg.snipe.slippage_bps >= 600 {
        tracing::warn!(
            slippage_bps = cfg.snipe.slippage_bps,
            "[snipe] slippage_bps is wider than the gap between the 618 and 19 bps steps, so \
             minTokensOut no longer tells them apart"
        );
    }
    anyhow::ensure!(
        cfg.snipe.max_tax_bps < 10_000,
        "[snipe] max_tax_bps is the whole trade"
    );
    if let Some(c) = &cfg.snipe.contract {
        let addr = c
            .parse::<ethers::types::Address>()
            .map_err(|e| anyhow::anyhow!("[snipe] contract \"{c}\" is not an address: {e}"))?;
        anyhow::ensure!(
            !addr.is_zero(),
            "[snipe] contract is the zero address - remove the line rather than blanking it"
        );
    }
    anyhow::ensure!(
        cfg.snipe.size_x100 < 10_000,
        "[snipe] size_x100 is the whole curve"
    );
    if !cfg.snipe.size.trim().is_empty() {
        // Parsed against eighteen decimals only to prove it is a number; the
        // real parse happens per pair token, in that token\'s own units.
        crate::route::parse_units(cfg.snipe.size.trim(), 18)
            .with_context(|| format!("[snipe] size \"{}\" is not an amount", cfg.snipe.size))?;
    }

    let mut seen = std::collections::HashSet::new();
    for r in &cfg.routes {
        anyhow::ensure!(seen.insert(&r.name), "duplicate route name '{}'", r.name);
        anyhow::ensure!(!r.pools.is_empty(), "route '{}': no pools listed", r.name);
        if let Some(t) = &r.trigger_pool {
            crate::route::parse_pool_ref(t)
                .map_err(|e| anyhow::anyhow!("route '{}': trigger_pool: {e}", r.name))?;
        }
        // Without a gap a single dip fires one buy per block for as long as it
        // lasts, which is never what "buy the dip" is meant to mean.
        if let Some(secs) = r.exit_after_secs {
            anyhow::ensure!(secs > 0, "route '{}': exit_after_secs must be > 0", r.name);
        }
        // An impact at or past the slippage tolerance is a size the trade could
        // not survive anyway: the move it makes would eat the whole budget meant
        // for the market moving under it.
        anyhow::ensure!(
            r.impact_pct.is_finite() && r.impact_pct > 0.0 && r.impact_pct < r.max_slippage_pct,
            "route '{}': impact_pct must be > 0 and below max_slippage_pct ({})",
            r.name,
            r.max_slippage_pct
        );
        if let Some(tp) = r.take_profit_pct {
            anyhow::ensure!(
                tp.is_finite() && tp > 0.0,
                "route '{}': take_profit_pct must be > 0",
                r.name
            );
        }
        anyhow::ensure!(
            r.max_slippage_pct.is_finite()
                && r.max_slippage_pct > 0.0
                && r.max_slippage_pct < 100.0,
            "route '{}': max_slippage_pct must be in (0, 100)",
            r.name
        );
    }
    for p in &cfg.pools {
        anyhow::ensure!(
            p.version == "v3" || p.version == "v4",
            "pool '{}': version must be 'v3' or 'v4'",
            p.name
        );
        // A v3 pool is an address; a v4 pool borrows the shared manager.
        if p.version == "v3" {
            anyhow::ensure!(
                p.address.is_some(),
                "pool '{}': v3 needs its own address - that is what a v3 pool is",
                p.name
            );
        } else {
            anyhow::ensure!(
                p.address.is_some() || cfg.pool_manager.is_some(),
                "pool '{}': set pool_manager at the top of the config, or give this pool an \
                 address of its own",
                p.name
            );
        }
        if let Some(a) = &p.address {
            a.parse::<ethers::types::Address>().map_err(|e| {
                anyhow::anyhow!("pool '{}': address \"{a}\" is not an address: {e}", p.name)
            })?;
        }
        anyhow::ensure!(
            p.base_token.is_none_or(|b| b <= 1),
            "pool '{}': base_token must be 0 or 1",
            p.name
        );
        // Per-pool overrides bypassed the global checks above.
        if let Some(t) = p.threshold_pct {
            anyhow::ensure!(
                t.is_finite() && t > 0.0,
                "pool '{}': threshold_pct must be > 0",
                p.name
            );
        }
        if let Some(m) = p.max_move_pct {
            anyhow::ensure!(
                m.is_finite() && m > 0.0,
                "pool '{}': max_move_pct must be > 0",
                p.name
            );
        }
        if p.version == "v4" {
            // Either the PoolId is given directly, or it can be derived from
            // token0/token1/fee/tickSpacing/hooks. Decimals come from the
            // Initialize log (found via pool_id), or from token0/token1 in
            // config, or from decimals0/decimals1 directly.
            if p.pool_id.is_none() {
                anyhow::ensure!(
                    p.fee.is_some(),
                    "pool '{}': v4 requires pool_id or fee",
                    p.name
                );
                anyhow::ensure!(
                    p.tick_spacing.is_some(),
                    "pool '{}': v4 requires pool_id or tick_spacing",
                    p.name
                );
                anyhow::ensure!(
                    p.token0.is_some() && p.token1.is_some(),
                    "pool '{}': v4 requires pool_id or token0/token1",
                    p.name
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `validate` insists on, so a test can vary one thing at a time.
    fn with(extra: &str) -> anyhow::Result<Config> {
        let src =
            format!("ws_url = \"wss://x\"\nhttp_url = \"https://x\"\nthreshold_pct = 3\n{extra}");
        let cfg: Config = toml::from_str(&src)?;
        validate(&cfg)?;
        Ok(cfg)
    }

    const MANAGER: &str = "0x8366a39CC670B4001A1121B8F6A443A643e40951";
    const SNIPER: &str = "0x1D8F08f47b60349925fB45064e80C3e4E8AD0184";

    /// Every v4 pool on a chain shares one PoolManager, so it is written once at
    /// the top and the pools say nothing about it.
    #[test]
    fn a_v4_pool_takes_the_shared_manager() {
        let cfg = with(&format!(
            "pool_manager = \"{MANAGER}\"\n\
             [[pools]]\nname = \"A/B (v4)\"\nversion = \"v4\"\npool_id = \"0x{}\"\n",
            "11".repeat(32)
        ))
        .expect("a v4 pool needs no address of its own");
        assert!(cfg.pools[0].address.is_none());
    }

    /// ...but something has to name it. Failing here beats resolving a pool
    /// against an address nobody chose.
    #[test]
    fn a_v4_pool_with_no_manager_anywhere_is_refused() {
        let e = with(&format!(
            "[[pools]]\nname = \"A/B (v4)\"\nversion = \"v4\"\npool_id = \"0x{}\"\n",
            "11".repeat(32)
        ))
        .expect_err("no manager, no address");
        assert!(format!("{e:#}").contains("pool_manager"), "{e:#}");
    }

    /// A v3 pool IS an address; there is nothing for it to fall back on.
    #[test]
    fn a_v3_pool_must_name_its_own_contract() {
        let e = with(&format!(
            "pool_manager = \"{MANAGER}\"\n[[pools]]\nname = \"A/B\"\nversion = \"v3\"\n"
        ))
        .expect_err("a v3 pool without an address is not a pool");
        assert!(format!("{e:#}").contains("v3"), "{e:#}");

        with(&format!(
            "[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"{MANAGER}\"\n"
        ))
        .expect("with its own address it is fine, and needs no manager");
    }

    /// A config written before `pool_manager` existed keeps working: the manager
    /// is still inferred from a v4 pool that spells out its address.
    #[test]
    fn the_old_shape_still_parses() {
        let cfg = with(&format!(
            "[[pools]]\nname = \"A/B (v4)\"\nversion = \"v4\"\naddress = \"{MANAGER}\"\n\
             pool_id = \"0x{}\"\n",
            "11".repeat(32)
        ))
        .expect("an address per pool is still allowed");
        assert_eq!(cfg.pools[0].address.as_deref(), Some(MANAGER));
    }

    /// The reserve is a wallet-level amount in whole native units, and a
    /// mistyped one has to fail while somebody is reading the message rather
    /// than when a buy discovers it.
    #[test]
    fn the_gas_reserve_is_read_as_an_amount() {
        let cfg = with(&format!(
            "gas_reserve = \"0.05\"\n[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"{MANAGER}\"\n"
        ))
        .expect("0.05 is an amount");
        assert_eq!(cfg.gas_reserve, "0.05");

        // Omitted, a default stands in rather than nothing being kept back.
        let cfg = with(&format!(
            "[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"{MANAGER}\"\n"
        ))
        .expect("optional");
        assert_eq!(cfg.gas_reserve, "0.05");

        let e = with(&format!(
            "gas_reserve = \"plenty\"\n[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"{MANAGER}\"\n"
        ))
        .expect_err("not an amount");
        assert!(format!("{e:#}").contains("gas_reserve"), "{e:#}");
    }

    /// A route names the pool it reacts to the same way the pools do, or a
    /// reload would retune a pool against a route that is not the one arming
    /// it - and the numbers would come from somebody else's dip.
    #[test]
    fn an_armed_route_and_its_trigger_pool_agree_on_the_name() {
        let id = format!("0x{}", "11".repeat(32));
        let cfg = with(&format!(
            "pool_manager = \"{MANAGER}\"\n\
             [[pools]]\nname = \"A/B (v4)\"\nversion = \"v4\"\npool_id = \"{id}\"\n\
             [[routes]]\nname = \"buy A\"\ninput = \"WETH\"\nmax_slippage_pct = 3\n\
             impact_pct = 1\npools = [\"{id}\"]\nauto_buy = true\n"
        ))
        .expect("a route over the watched pool");
        let key = cfg.pools[0]
            .pool_ref()
            .expect("a v4 pool is named by its id");
        assert_eq!(cfg.routes[0].trigger(), Some(key));
        assert_eq!(cfg.armed_route(key).map(|r| r.name.as_str()), Some("buy A"));

        // A route that is not armed is not the one to take a take-profit from.
        let cfg = with(&format!(
            "pool_manager = \"{MANAGER}\"\n\
             [[pools]]\nname = \"A/B (v4)\"\nversion = \"v4\"\npool_id = \"{id}\"\n\
             [[routes]]\nname = \"buy A\"\ninput = \"WETH\"\nmax_slippage_pct = 3\n\
             impact_pct = 1\npools = [\"{id}\"]\nauto_buy = false\n"
        ))
        .expect("an unarmed route");
        assert!(cfg.armed_route(key).is_none());
    }

    /// What a reload may not touch has to be NAMED, or an operator edits an
    /// endpoint, sends SIGHUP and believes the bot took it.
    #[test]
    fn a_field_only_a_restart_applies_is_named() {
        let pool =
            format!("[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"{MANAGER}\"\n");
        let running = with(&pool).expect("valid");
        let mut next = running.clone();
        assert!(restart_only(&running, &next).is_empty(), "nothing changed");

        next.ws_url = "wss://elsewhere".into();
        next.gas_reserve = "0.5".into();
        let named = restart_only(&running, &next);
        assert!(named.contains(&"ws_url"), "{named:?}");
        assert!(named.contains(&"gas_reserve"), "{named:?}");
        assert!(!named.contains(&"http_url"), "{named:?}");
    }

    /// A misspelled address is caught while someone is reading the message, not
    /// when a pool is resolved against it.
    #[test]
    fn a_bad_address_is_refused_at_load() {
        let e = with("[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"0xnope\"\n")
            .expect_err("not an address");
        assert!(format!("{e:#}").contains("not an address"), "{e:#}");
    }

    /// The wrapper an ERC-20 launch is bought through. A blanked-out entry is
    /// refused rather than read as "no wrapper": the two mean different things
    /// and only one of them is ever meant.
    #[test]
    fn the_snipe_contract_is_read_and_checked() {
        let pools =
            format!("[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"{MANAGER}\"\n");

        let cfg = with(&format!(
            "{pools}[snipe]\ncontract = \"{SNIPER}\"\nsize = \"0.1\"\n"
        ))
        .expect("a good address");
        assert_eq!(cfg.snipe.contract.as_deref(), Some(SNIPER));

        let e =
            with(&format!("{pools}[snipe]\ncontract = \"0xnope\"\n")).expect_err("not an address");
        assert!(format!("{e:#}").contains("not an address"), "{e:#}");

        let e = with(&format!(
            "{pools}[snipe]\ncontract = \"0x0000000000000000000000000000000000000000\"\n"
        ))
        .expect_err("the zero address");
        assert!(format!("{e:#}").contains("zero address"), "{e:#}");

        // Absent is a position too: a native launch is still bought straight
        // from the curve.
        assert!(with(&pools).unwrap().snipe.contract.is_none());
    }
}
