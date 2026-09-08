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

use std::path::Path;


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
    /// What the launch sniper is willing to do. Absent means the defaults
    /// below, which buy nothing until a size is given.
    #[serde(default)]
    pub snipe: SnipeConfig,
    /// Where to keep what the chain has already told us about pools and
    /// tokens - PoolKeys, decimals, symbols. All of it immutable, so the file
    /// only ever saves time; delete it and the next start is merely slow.
    #[serde(default = "default_token_cache")]
    pub token_cache_path: String,
    /// Signing key for swap execution. Prefer the PRIVATE_KEY environment
    /// variable: a key in a config file also lands in backups and editor state.
    /// Whatever is set here is overridden by PRIVATE_KEY when that is present.
    #[serde(default)]
    pub private_key: Secret,
}


fn default_token_cache() -> String {
    "tokens.json".to_string()
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

    // How a position ends. Measured over one night of 3473 launches, entering
    // a fixed fraction of every curve and allowing two blocks between seeing a
    // price and selling into it - see `src/exit.rs` for the table these came
    // out of.
    /// How much of its high a position may give back before it is sold, in
    /// basis points. Narrow because the delay between the price and the sale
    /// is what a wide stop pays for.
    #[serde(default = "default_trail_bps")]
    pub trail_bps: u64,
    /// A multiple of cost that closes a position outright, in hundredths. 200
    /// is twice what it cost; 0 turns it off and leaves only the stop.
    #[serde(default = "default_take_x100")]
    pub take_x100: u64,
    /// Blocks after opening at which a position is sold whatever it is worth.
    /// This chain runs 9.8 blocks to the second.
    #[serde(default = "default_hold_blocks")]
    pub hold_blocks: u64,

    /// How many of an operator's positions must have closed before their
    /// record may refuse a launch of theirs. Zero never refuses on it, which
    /// is what a fresh store amounts to anyway.
    #[serde(default = "default_operator_needs")]
    pub operator_needs: usize,
    /// The fewest exempt wallets worth following. Zero follows everything,
    /// which is what collecting wants; six is what the journals chose.
    #[serde(default)]
    pub min_exempt: usize,
    /// Which pair tokens to trade, by symbol. Empty follows every pair, which
    /// is what collecting wants.
    #[serde(default)]
    pub pairs: Vec<String>,
    /// The largest dev buy worth following, against the phantom reserve, in
    /// hundredths of a percent. 1500 is fifteen percent.
    #[serde(default = "default_max_dev_buy_x100")]
    pub max_dev_buy_x100: u64,
    /// Gas limit for a call to the wrapper. Not estimated per trade: an
    /// estimate is a round trip inside the window the whole thing is aimed at,
    /// and the call shape never changes. Generous on purpose - unused gas is
    /// refunded, a limit reached is a trade lost.
    #[serde(default = "default_gas_limit")]
    pub gas_limit: u64,
    /// How many positions may be open at once. Every one of them is money at
    /// risk in a contract, and the exit rules were measured one position at a
    /// time.
    #[serde(default = "default_max_open")]
    pub max_open: usize,
    /// The most this run may spend in total, in the pair token's own units.
    /// Reached, it stops buying and says so; it does not stop selling.
    #[serde(default)]
    pub max_spend: String,
    /// Where the operator history lives. It is the only thing this bot keeps
    /// between runs that cannot be rebuilt from the journals.
    #[serde(default = "default_operators_path")]
    pub operators: String,
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
fn default_trail_bps() -> u64 {
    500
}
fn default_take_x100() -> u64 {
    200
}
fn default_hold_blocks() -> u64 {
    588
}
fn default_operator_needs() -> usize {
    5
}
fn default_gas_limit() -> u64 {
    2_000_000
}
fn default_max_open() -> usize {
    3
}
fn default_max_dev_buy_x100() -> u64 {
    // No cap. A launch is not refused for a large dev buy unless somebody
    // says so, because the field only means anything beside the others.
    u64::MAX
}
fn default_operators_path() -> String {
    "operators.json".to_string()
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
            trail_bps: default_trail_bps(),
            take_x100: default_take_x100(),
            hold_blocks: default_hold_blocks(),
            operator_needs: default_operator_needs(),
            gas_limit: default_gas_limit(),
            max_open: default_max_open(),
            max_spend: String::new(),
            min_exempt: 0,
            pairs: Vec::new(),
            max_dev_buy_x100: default_max_dev_buy_x100(),
            operators: default_operators_path(),
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
}

/// A non-empty environment variable, trimmed.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn validate(cfg: &Config) -> anyhow::Result<()> {
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
        crate::units::parse_units(cfg.snipe.size.trim(), 18)
            .with_context(|| format!("[snipe] size \"{}\" is not an amount", cfg.snipe.size))?;
    }

    Ok(())
}
