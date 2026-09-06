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
    /// Uniswap Universal Router, the contract swaps are sent to.
    #[serde(default)]
    pub universal_router: Option<String>,
    /// Permit2. Defaults to the canonical deterministic deployment.
    #[serde(default)]
    pub permit2: Option<String>,
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
    /// Human units of the input token, e.g. "1.0".
    pub amount_in: String,
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
    for (i, u) in cfg.submit_urls.iter().enumerate() {
        anyhow::ensure!(
            !u.trim().is_empty(),
            "submit_urls[{i}] is empty - remove the entry rather than leaving a blank one"
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything `validate` insists on, so a test can vary one thing at a time.
    fn with(extra: &str) -> anyhow::Result<Config> {
        let src = format!(
            "ws_url = \"wss://x\"\nhttp_url = \"https://x\"\nthreshold_pct = 3\n{extra}"
        );
        let cfg: Config = toml::from_str(&src)?;
        validate(&cfg)?;
        Ok(cfg)
    }

    const MANAGER: &str = "0x8366a39CC670B4001A1121B8F6A443A643e40951";

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

    /// A misspelled address is caught while someone is reading the message, not
    /// when a pool is resolved against it.
    #[test]
    fn a_bad_address_is_refused_at_load() {
        let e = with("[[pools]]\nname = \"A/B\"\nversion = \"v3\"\naddress = \"0xnope\"\n")
            .expect_err("not an address");
        assert!(format!("{e:#}").contains("not an address"), "{e:#}");
    }
}
