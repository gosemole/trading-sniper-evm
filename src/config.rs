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
        f.write_str(if self.0.is_empty() { "[unset]" } else { "[redacted]" })
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
    /// Signal when price moves >= threshold % between consecutive blocks.
    pub threshold_pct: f64,
    /// Assumed price move (%) for the depth estimate: "how much can I buy if the
    /// price moves this much". In-range liquidity only (no tick walking).
    #[serde(default = "default_max_move")]
    pub max_move_pct: f64,
    /// Ticker -> address registry, e.g. `POOLS = "0x385b..."`. Any pool field
    /// that takes a token accepts either a ticker from here or a raw address.
    #[serde(default)]
    pub tokens: HashMap<String, String>,
    /// How often (seconds) to re-measure what an armed route's pools take on
    /// top of their stated fees - a hook charging its own cut shows up here and
    /// nowhere else. 0 turns the measurement off. Only armed routes are
    /// measured, and only in the background.
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
    /// Price an armed buy from the measured model instead of asking the router,
    /// which takes the last round trip out of the path between a drop and a
    /// signed transaction. Off by default: the router's answer is exact and
    /// doubles as a rehearsal, and giving that up trades a guarantee for
    /// latency. Falls back to asking whenever the model is not in a position to
    /// answer - see `calibrate_secs`, which is what keeps it honest.
    #[serde(default)]
    pub fast_quote: bool,
    /// Uniswap Universal Router, the contract swaps are sent to.
    #[serde(default)]
    pub universal_router: Option<String>,
    /// Permit2. Defaults to the canonical deterministic deployment.
    #[serde(default)]
    pub permit2: Option<String>,
    /// v4 PoolManager used by routes. Defaults to the address of the first
    /// v4 pool in `[[pools]]` when omitted.
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
    /// Human units of the input token, e.g. "1.0".
    pub amount_in: String,
    /// Tolerated shortfall against the quote, used for amountOutMinimum. It is
    /// applied to a quote taken from the router at the moment of the buy, not
    /// to anything measured earlier, so it caps the slippage of this pair alone.
    pub max_slippage_pct: f64,
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
    /// Sell the whole position back down this route once the pool price is
    /// this far above the average price it was bought at. Unset means never:
    /// the bot buys and holds. Measured against the pool's own price, so it is
    /// a gain against the pool's quote token, not against the dollar.
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

fn default_pool_cache() -> String {
    "pools.json".to_string()
}

fn default_inventory() -> String {
    "inventory.json".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub name: String,
    /// v3: pool contract address. v4: PoolManager address.
    pub address: String,
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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config {}", path.display()))?;
        // The environment wins over the file, so a checkout can carry a config
        // with no endpoints and no key at all.
        if let Some(v) = env_var("WS_URL") {
            cfg.ws_url = v;
        }
        if let Some(v) = env_var("HTTP_URL") {
            cfg.http_url = v;
        }
        if let Some(v) = env_var("PRIVATE_KEY") {
            cfg.private_key = Secret(v);
        }
        validate(&cfg).with_context(|| format!("in config {}", path.display()))?;
        Ok(cfg)
    }
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
    anyhow::ensure!(
        !cfg.ws_url.trim().is_empty(),
        "no ws endpoint: set the WS_URL environment variable, or ws_url in config"
    );
    anyhow::ensure!(
        !cfg.http_url.trim().is_empty(),
        "no http endpoint: set the HTTP_URL environment variable, or http_url in config"
    );
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
    let mut seen = std::collections::HashSet::new();
    for r in &cfg.routes {
        anyhow::ensure!(
            seen.insert(&r.name),
            "duplicate route name '{}'",
            r.name
        );
        anyhow::ensure!(!r.pools.is_empty(), "route '{}': no pools listed", r.name);
        if let Some(t) = &r.trigger_pool {
            crate::route::parse_pool_ref(t)
                .map_err(|e| anyhow::anyhow!("route '{}': trigger_pool: {e}", r.name))?;
        }
        // Without a gap a single dip fires one buy per block for as long as it
        // lasts, which is never what "buy the dip" is meant to mean.
        if let Some(secs) = r.exit_after_secs {
            anyhow::ensure!(
                secs > 0,
                "route '{}': exit_after_secs must be > 0",
                r.name
            );
        }
        if let Some(tp) = r.take_profit_pct {
            anyhow::ensure!(
                tp.is_finite() && tp > 0.0,
                "route '{}': take_profit_pct must be > 0",
                r.name
            );
        }
        anyhow::ensure!(
            r.max_slippage_pct.is_finite() && r.max_slippage_pct > 0.0 && r.max_slippage_pct < 100.0,
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
                anyhow::ensure!(p.fee.is_some(), "pool '{}': v4 requires pool_id or fee", p.name);
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
