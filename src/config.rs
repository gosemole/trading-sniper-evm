use serde::Deserialize;
use std::path::Path;

fn default_max_move() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// WebSocket endpoint for live log subscriptions.
    pub ws_url: String,
    /// HTTP JSON-RPC endpoint for on-demand calls / fallback.
    pub http_url: String,
    /// Signal when price moves >= threshold % between consecutive blocks.
    pub threshold_pct: f64,
    /// Assumed price move (%) for the depth estimate: "how much can I buy if the
    /// price moves this much". In-range liquidity only (no tick walking).
    #[serde(default = "default_max_move")]
    pub max_move_pct: f64,
    /// Pools to watch.
    pub pools: Vec<PoolConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolConfig {
    pub name: String,
    /// v3: pool contract address. v4: PoolManager address.
    pub address: String,
    /// Either "v3" (Uniswap V3 pool) or "v4" (Uniswap V4 via PoolManager).
    pub version: String,
    /// v3: optional, resolved on-chain via eth_call when omitted.
    /// v4: required.
    pub token0: Option<String>,
    /// v3: optional, resolved on-chain when omitted. v4: required.
    pub token1: Option<String>,
    /// v3: optional, resolved on-chain when omitted. v4: required.
    pub decimals0: Option<u8>,
    /// v3: optional, resolved on-chain when omitted. v4: required.
    pub decimals1: Option<u8>,
    /// Index (0 or 1) of the base token used for the displayed price. Default 0.
    #[serde(default)]
    pub base_token: u8,
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
        let raw = std::fs::read_to_string(path)?;
        let cfg: Config = toml::from_str(&raw)?;
        validate(&cfg)?;
        Ok(cfg)
    }
}

fn validate(cfg: &Config) -> anyhow::Result<()> {
    anyhow::ensure!(!cfg.pools.is_empty(), "no pools configured");
    anyhow::ensure!(cfg.threshold_pct > 0.0, "threshold_pct must be > 0");
    for p in &cfg.pools {
        anyhow::ensure!(
            p.version == "v3" || p.version == "v4",
            "pool '{}': version must be 'v3' or 'v4'",
            p.name
        );
        anyhow::ensure!(
            p.base_token == 0 || p.base_token == 1,
            "pool '{}': base_token must be 0 or 1",
            p.name
        );
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
