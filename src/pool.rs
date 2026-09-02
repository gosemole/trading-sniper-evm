use crate::config::PoolConfig;
use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, Bytes, Filter, H256, TransactionRequest, U256, ValueOrArray};
use ethers::utils::keccak256;

/// Canonical V3 Swap signature (no `indexed`, no names) -> keccak for topic0.
const V3_SWAP_SIG: &str = "Swap(address,address,int256,int256,uint160,uint128,int24)";

/// Canonical V4 PoolManager `Swap` signature.
const V4_SWAP_SIG: &str = "Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)";

/// Canonical V4 PoolManager `Initialize` signature.
const V4_INIT_SIG: &str =
    "Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)";

/// topic0 of the given event signature.
pub fn event_topic(sig: &str) -> H256 {
    H256::from_slice(&keccak256(sig.as_bytes()))
}

pub fn v3_swap_topic() -> H256 {
    event_topic(V3_SWAP_SIG)
}

pub fn v4_swap_topic() -> H256 {
    event_topic(V4_SWAP_SIG)
}

pub fn v4_init_topic() -> H256 {
    event_topic(V4_INIT_SIG)
}

fn selector(sig: &str) -> Bytes {
    let h = keccak256(sig.as_bytes());
    Bytes::from(h[..4].to_vec())
}

/// 32-byte big-endian word of a U256.
fn word32(v: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    v.to_big_endian(&mut b);
    b
}

/// A watched pool with resolved on-chain metadata.
#[derive(Debug, Clone)]
pub struct Pool {
    pub name: String,
    pub version: String,
    pub decimals0: u8,
    pub decimals1: u8,
    pub base_token: u8,
    /// For v3: the pool contract address. For v4: the PoolManager address.
    pub address: Address,
    /// v4 only: 32-byte PoolId.
    pub pool_id: Option<H256>,
    /// v4 only: absolute storage slot of `_pools[poolId].liquidity`.
    pub liquidity_slot: Option<U256>,
}

impl Pool {
    /// Build a pool, resolving token/decimals on-chain when not given in config.
    pub async fn resolve(http: &Provider<Http>, cfg: &PoolConfig) -> Result<Self> {
        let address: Address = cfg
            .address
            .parse()
            .with_context(|| format!("invalid pool address for '{}'", cfg.name))?;

        // 1. Determine the PoolId for v4 (given directly, or derived from key).
        let pool_id = if cfg.version == "v4" {
            let pid = if let Some(pid) = &cfg.pool_id {
                pid.parse::<H256>()
                    .with_context(|| format!("invalid pool_id for '{}'", cfg.name))?
            } else {
                derive_pool_id(cfg)?
            };
            tracing::info!(pool = %cfg.name, pool_id = ?pid, "v4 pool_id");
            Some(pid)
        } else {
            None
        };

        // 2. Determine decimals (config -> on-chain). Optional: only needed for
        // human-readable absolute price; %-movement works on raw price without them.
        let (decimals0, decimals1) = match (cfg.decimals0, cfg.decimals1) {
            (Some(d0), Some(d1)) => (d0, d1),
            _ if cfg.version == "v4" => {
                let pid = pool_id.expect("v4 pool_id set");
                // Try: currencies from the Initialize log (native ETH pairs work:
                // currency0 == 0x0, decimals 18). Fall back to config tokens, else
                // raw price with decimals 0/0.
                match currencies_from_init_log(http, address, pid).await {
                    Ok((c0, c1)) => {
                        let d0 = decimals_of(http, c0).await?;
                        let d1 = decimals_of(http, c1).await?;
                        tracing::info!(
                            pool = %cfg.name, token0 = ?c0, token1 = ?c1,
                            decimals0 = d0, decimals1 = d1,
                            "resolved v4 metadata from Initialize log"
                        );
                        (d0, d1)
                    }
                    Err(e) => match (&cfg.token0, &cfg.token1) {
                        (Some(s0), Some(s1)) => {
                            let t0: Address = s0.parse()?;
                            let t1: Address = s1.parse()?;
                            let d0 = call_u8(http, t0, &selector("decimals()")).await?;
                            let d1 = call_u8(http, t1, &selector("decimals()")).await?;
                            (d0, d1)
                        }
                        _ => {
                            tracing::info!(
                                pool = %cfg.name, err = %e,
                                "no decimals available; using raw price (movement % unaffected)"
                            );
                            (0, 0)
                        }
                    },
                }
            }
            _ => {
                // v3: read token0()/token1() from the pool contract.
                let t0: Address = match &cfg.token0 {
                    Some(s) => s.parse()?,
                    None => call_address(http, address, &selector("token0()")).await?,
                };
                let t1: Address = match &cfg.token1 {
                    Some(s) => s.parse()?,
                    None => call_address(http, address, &selector("token1()")).await?,
                };
                let d0 = decimals_of(http, t0).await?;
                let d1 = decimals_of(http, t1).await?;
                tracing::info!(
                    pool = %cfg.name, token0 = ?t0, token1 = ?t1,
                    decimals0 = d0, decimals1 = d1,
                    "resolved v3 pool metadata on-chain"
                );
                (d0, d1)
            }
        };

        // 3. For v4, locate the storage slot of `_pools[poolId].liquidity` by
        // scanning for the base slot of the State struct (slot0 = its first word).
        let liquidity_slot = if cfg.version == "v4" {
            let pid = pool_id.expect("v4 pool_id set");
            match find_state_base_slot(http, address, pid).await {
                Ok(base) => {
                    let mut key = pid.as_bytes().to_vec();
                    key.extend_from_slice(&word32(U256::from(base)));
                    let abs = U256::from_big_endian(&keccak256(&key)) + U256::from(3);
                    Some(abs)
                }
                Err(e) => {
                    tracing::warn!(pool = %cfg.name, err = %e, "could not locate v4 liquidity slot");
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            name: cfg.name.clone(),
            version: cfg.version.clone(),
            decimals0,
            decimals1,
            base_token: cfg.base_token,
            address,
            pool_id,
            liquidity_slot,
        })
    }

    /// Addresses to subscribe to. v3: the single pool. v4: PoolManager.
    pub fn filter_addresses(&self) -> Vec<Address> {
        vec![self.address]
    }

    /// In-range liquidity currently active at the pool's price.
    pub async fn in_range_liquidity(&self, http: &Provider<Http>) -> Result<u128> {
        match self.version.as_str() {
            "v4" => {
                let slot = self.liquidity_slot.context("v4 liquidity slot not resolved")?;
                let word = storage_at(http, self.address, slot).await?;
                // liquidity occupies the low 128 bits of the big-endian word
                Ok(u128::from_be_bytes(word.0[16..32].try_into().unwrap()))
            }
            _ => {
                // v3: pool.liquidity() view
                let tx = TransactionRequest::new()
                    .to(self.address)
                    .data(selector("liquidity()"));
                let res: Bytes = http
                    .call(&tx.into(), None)
                    .await
                    .context("eth_call liquidity()")?;
                anyhow::ensure!(res.len() >= 32, "short return for liquidity()");
                // low 128 bits: ethers U256 is little-endian limb order
                let u = U256::from_big_endian(&res[16..32]);
                Ok(u.as_u128())
            }
        }
    }

    /// Estimate how much QUOTE token (ETH/WETH, 18 decimals) is needed to move
    /// the price by `move_pct` (in-range liquidity only).
    /// Quote side is derived from base_token: base_token=1 => quote is token0,
    /// base_token=0 => quote is token1.
    pub async fn quote_pay(
        &self,
        http: &Provider<Http>,
        sqrt_input: U256,
        move_pct: f64,
    ) -> Result<f64> {
        let l = self.in_range_liquidity(http).await? as f64;
        let sqrt_p = sqrt_to_f64(sqrt_input);
        let k = (1.0 + move_pct / 100.0).sqrt();
        // amount of QUOTE token for a `move_pct` price move (magnitude)
        let pay_raw = if self.base_token == 1 {
            // quote = token0 (x): Δx = L·(1/√P)·(1 − 1/√(1+m))
            l * (1.0 / sqrt_p) * (1.0 - 1.0 / k)
        } else {
            // quote = token1 (y): Δy = L·√P·(√(1+m) − 1)
            l * sqrt_p * (k - 1.0)
        };
        Ok(pay_raw)
    }
}

fn sqrt_to_f64(sqrt: U256) -> f64 {
    let f = if let Ok(x) = u128::try_from(sqrt) {
        x as f64
    } else {
        let l = sqrt.0;
        let lo = (l[0] as u128) | ((l[1] as u128) << 64);
        let hi = (l[2] as u128) | ((l[3] as u128) << 64);
        hi as f64 * 2f64.powi(128) + lo as f64
    };
    f / 2f64.powi(96)
}

/// Find the base storage slot of the v4 `_pools` State struct: the smallest slot
/// `s` (0..48) whose `keccak(poolId || s)` word is non-zero.
async fn find_state_base_slot(
    http: &Provider<Http>,
    manager: Address,
    pool_id: H256,
) -> Result<u64> {
    // absolute slot for mapping member s: keccak256(poolId, s) where s is a 32-byte word
    for s in 0..48u64 {
        let mut key = pool_id.as_bytes().to_vec();
        key.extend_from_slice(&word32(U256::from(s)));
        let abs = U256::from_big_endian(&keccak256(&key));
        let word = storage_at(http, manager, abs).await?;
        if !word.is_zero() {
            return Ok(s);
        }
    }
    anyhow::bail!("no non-zero storage slot found for pool")
}

/// Raw `eth_getStorageAt` returning the 32-byte word.
async fn storage_at(
    http: &Provider<Http>,
    address: Address,
    slot: U256,
) -> Result<H256> {
    let word: H256 = http
        .request(
            "eth_getStorageAt",
            (
                format!("{address:?}"),
                format!("0x{slot:064x}"),
                "latest".to_string(),
            ),
        )
        .await
        .context("eth_getStorageAt")?;
    Ok(word)
}

/// Derive poolId = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks)),
/// i.e. five 32-byte words (addresses/fee left-padded, tickSpacing sign-extended).
fn derive_pool_id(cfg: &PoolConfig) -> Result<H256> {
    let t0: Address = cfg.token0.as_deref().context("v4 missing token0")?.parse()?;
    let t1: Address = cfg.token1.as_deref().context("v4 missing token1")?.parse()?;
    let (c0, c1) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
    let fee = cfg.fee.context("v4 missing fee")?;
    let tick_spacing = cfg.tick_spacing.context("v4 missing tick_spacing")?;
    let hooks: Address = cfg
        .hooks
        .as_deref()
        .unwrap_or("0x0000000000000000000000000000000000000000")
        .parse()?;

    let word = |b: &[u8]| {
        let mut w = [0u8; 32];
        w[32 - b.len()..].copy_from_slice(b);
        w
    };
    let signed_word = |v: i32| {
        // int24 sign-extended to a 32-byte big-endian word
        let mut w = [0xffu8; 32];
        if v >= 0 {
            w = [0u8; 32];
        }
        let b = v.to_be_bytes(); // [sign, b1, b2, b3]
        w[29..].copy_from_slice(&b[1..]);
        w
    };
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(&word(c0.as_bytes()));
    buf.extend_from_slice(&word(c1.as_bytes()));
    buf.extend_from_slice(&word(&fee.to_be_bytes()[1..]));
    buf.extend_from_slice(&signed_word(tick_spacing));
    buf.extend_from_slice(&word(hooks.as_bytes()));
    Ok(H256::from_slice(&keccak256(&buf)))
}

/// Find the Initialize log for `pool_id` (id is indexed), return (currency0, currency1).
async fn currencies_from_init_log(
    http: &Provider<Http>,
    manager: Address,
    pool_id: H256,
) -> Result<(Address, Address)> {
    let filter = Filter::new()
        .address(manager)
        .topic0(ValueOrArray::Value(v4_init_topic()))
        .topic1(ValueOrArray::Value(pool_id));
    let logs = http
        .get_logs(&filter)
        .await
        .context("eth_getLogs Initialize")?;
    let log = logs
        .into_iter()
        .next()
        .context("no Initialize log for pool_id")?;
    anyhow::ensure!(log.topics.len() >= 4, "Initialize log missing currency topics");
    let c0 = Address::from_slice(&log.topics[2].as_bytes()[12..]);
    let c1 = Address::from_slice(&log.topics[3].as_bytes()[12..]);
    Ok((c0, c1))
}

/// Decimals of a currency. Native ETH is the zero address -> 18.
async fn decimals_of(http: &Provider<Http>, currency: Address) -> Result<u8> {
    if currency == Address::zero() {
        return Ok(18);
    }
    call_u8(http, currency, &selector("decimals()")).await
}

async fn call_address(provider: &Provider<Http>, to: Address, data: &Bytes) -> Result<Address> {
    let tx = TransactionRequest::new().to(to).data(data.clone());
    let res: Bytes = provider.call(&tx.into(), None).await.context("eth_call address")?;
    anyhow::ensure!(res.len() >= 32, "short return for address call");
    Ok(Address::from_slice(&res[12..32]))
}

async fn call_u8(provider: &Provider<Http>, to: Address, data: &Bytes) -> Result<u8> {
    let tx = TransactionRequest::new().to(to).data(data.clone());
    let res: Bytes = provider.call(&tx.into(), None).await.context("eth_call u8")?;
    anyhow::ensure!(!res.is_empty(), "empty return for u8 call");
    Ok(res[res.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_topics() {
        assert_eq!(
            v3_swap_topic(),
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
                .parse::<H256>()
                .unwrap()
        );
    }

    #[test]
    fn derives_pool_id() {
        // Real pool from a Robinhood-chain Initialize log:
        // c0 = 0x0 (native ETH), c1 = 0xb427..., fee 2500, tickSpacing 60, hooks 0x0.
        let cfg = PoolConfig {
            name: "t".into(),
            address: "0x0000000000000000000000000000000000000000".into(),
            version: "v4".into(),
            token0: Some("0x0000000000000000000000000000000000000000".into()),
            token1: Some("0xb427c36931e23b607cfafbcb5a93786117bad597".into()),
            decimals0: None,
            decimals1: None,
            base_token: 0,
            threshold_pct: None,
            max_move_pct: None,
            pool_id: None,
            fee: Some(2500),
            tick_spacing: Some(60),
            hooks: None,
        };
        assert_eq!(
            derive_pool_id(&cfg).unwrap(),
            "0x0277354251edc469597038bae48c9f6b7b80003999b511a7c5eba9a2de764f09"
                .parse::<H256>()
                .unwrap()
        );
    }
}
