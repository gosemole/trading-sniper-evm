use crate::config::PoolConfig;
use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, Bytes, Filter, Log, TransactionRequest, H256, U256, ValueOrArray};
use ethers::utils::keccak256;
use std::collections::HashMap;

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

/// The magnitude of a two's-complement signed word, as an f64.
///
/// Swap events report amounts signed by direction, and only the size matters
/// here. Both protocols keep the real magnitude far inside 128 bits, so nothing
/// is lost going through f64 for a ratio.
fn abs_signed_word(b: &[u8]) -> f64 {
    let v = U256::from_big_endian(b);
    let magnitude = match b[0] & 0x80 != 0 {
        true => (!v).overflowing_add(U256::one()).0,
        false => v,
    };
    crate::route::u256_to_f64(magnitude)
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

/// Resolve a token reference: a ticker from the `[tokens]` registry (matched
/// case-insensitively) or a raw `0x…` address.
pub fn resolve_token(tokens: &HashMap<String, String>, s: &str) -> Result<Address> {
    let listed = tokens.get(s).or_else(|| {
        tokens
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(s))
            .map(|(_, v)| v)
    });
    match listed {
        Some(addr) => addr
            .parse()
            .with_context(|| format!("ticker '{s}' maps to invalid address '{addr}'")),
        None => s.parse().with_context(|| {
            format!("'{s}' is neither a ticker in [tokens] nor a 0x address")
        }),
    }
}

/// v4 PoolKey currencies are always stored sorted by address, so a pair given
/// in any order is normalised here and the caller cannot get it backwards.
fn sorted(a: Address, b: Address) -> (Address, Address) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn selector(sig: &str) -> Bytes {
    let h = keccak256(sig.as_bytes());
    Bytes::from(h[..4].to_vec())
}

/// A watched pool with resolved on-chain metadata.
#[derive(Debug, Clone)]
pub struct Pool {
    pub name: String,
    pub version: String,
    pub decimals0: u8,
    pub decimals1: u8,
    /// False when decimals could not be resolved and 0/0 is being used as a
    /// stand-in. Relative moves are unaffected; absolute prices are not shown.
    pub decimals_known: bool,
    pub base_token: u8,
    /// Symbol of the quote side, for log output. None when the quote token
    /// address could not be resolved.
    pub quote_symbol: Option<String>,
    /// Symbol of the base side - the token a drop here is a drop *of*.
    pub base_symbol: Option<String>,
    /// PoolKey currencies in sorted order. None when they could not be
    /// resolved, which leaves relative price moves usable and everything that
    /// names a token unusable.
    pub currencies: Option<(Address, Address)>,
    /// For v3: the pool contract address. For v4: the PoolManager address.
    pub address: Address,
    /// v4 only: 32-byte PoolId.
    pub pool_id: Option<H256>,
    /// PoolKey tick spacing. Required to walk the tick bitmap; v4 does not
    /// store it, so it comes from config or the Initialize log.
    pub tick_spacing: Option<i32>,
    /// Pool fee in hundredths of a bip (3000 = 0.3%), charged on the swap input.
    pub lp_fee: Option<u32>,
}

impl Pool {
    /// Build a pool, resolving token/decimals on-chain when not given in config.
    pub async fn resolve(
        http: &Provider<Http>,
        cfg: &PoolConfig,
        tokens: &HashMap<String, String>,
    ) -> Result<Self> {
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
                derive_pool_id(cfg, tokens)?
            };
            tracing::info!(pool = %cfg.name, pool_id = ?pid, "v4 pool_id");
            Some(pid)
        } else {
            None
        };

        // 2. PoolKey metadata. `fee` and `tickSpacing` are needed to walk the
        // tick bitmap; v4 keeps them only in the PoolKey, never in storage.
        let mut tick_spacing = cfg.tick_spacing;
        let mut lp_fee = cfg.fee;
        // Config tokens win: they cost no requests and let the operator override.
        let mut currencies: Option<(Address, Address)> = match (&cfg.token0, &cfg.token1) {
            (Some(s0), Some(s1)) => Some(sorted(
                resolve_token(tokens, s0)?,
                resolve_token(tokens, s1)?,
            )),
            _ => None,
        };
        let mut key_source = "config";

        if cfg.version == "v4" {
            if currencies.is_none() || tick_spacing.is_none() || lp_fee.is_none() {
                let pid = pool_id.expect("v4 pool_id set");
                match v4_pool_key(http, address, pid).await {
                    Ok((c0, c1, fee, ts, _hooks)) => {
                        if currencies.is_none() {
                            key_source = "Initialize log";
                            currencies = Some((c0, c1));
                        }
                        tick_spacing.get_or_insert(ts);
                        lp_fee.get_or_insert(fee);
                    }
                    Err(e) => tracing::warn!(
                        pool = %cfg.name, err = %e,
                        "could not recover v4 PoolKey; set token0/token1/fee/tick_spacing \
                         in config or depth stays in-range only"
                    ),
                }
            }
        } else {
            // v3 publishes all of it as plain views on the pool contract.
            if currencies.is_none() {
                key_source = "pool contract";
                currencies = Some((
                    call_address(http, address, &selector("token0()")).await?,
                    call_address(http, address, &selector("token1()")).await?,
                ));
            }
            if tick_spacing.is_none() {
                tick_spacing = call_uint(http, address, &selector("tickSpacing()"))
                    .await
                    .ok()
                    .map(|v| v as i32);
            }
            if lp_fee.is_none() {
                lp_fee = call_uint(http, address, &selector("fee()")).await.ok();
            }
        }

        // 3. Decimals: only needed for human-readable absolute prices; the
        // %-movement detector works on the raw price without them.
        let (decimals0, decimals1, decimals_known) = match (cfg.decimals0, cfg.decimals1) {
            (Some(d0), Some(d1)) => (d0, d1, true),
            _ => match currencies {
                // decimals_of, not a bare decimals() call: native ETH is the
                // zero address and has no contract to ask.
                Some((c0, c1)) => (
                    decimals_of(http, c0).await?,
                    decimals_of(http, c1).await?,
                    true,
                ),
                None => (0, 0, false),
            },
        };

        // 4. Symbols of both sides. They name the token that fell when a signal
        // fires, and they are what decides which side is the base.
        let (sym0, sym1) = match currencies {
            Some((t0, t1)) => (
                symbol_of(http, t0).await.ok(),
                symbol_of(http, t1).await.ok(),
            ),
            None => (None, None),
        };
        let base_token = resolve_base_token(
            &cfg.name,
            cfg.base_token,
            sym0.as_deref(),
            sym1.as_deref(),
        )
        .with_context(|| format!("pool '{}'", cfg.name))?;
        let (base_symbol, quote_symbol) = if base_token == 1 {
            (sym1.clone(), sym0.clone())
        } else {
            (sym0.clone(), sym1.clone())
        };

        match currencies {
            Some((c0, c1)) => tracing::info!(
                pool = %cfg.name, currency0 = ?c0, currency1 = ?c1,
                decimals0, decimals1, tick_spacing = ?tick_spacing, lp_fee = ?lp_fee,
                base_token,
                base = base_symbol.as_deref().unwrap_or("?"),
                quote = quote_symbol.as_deref().unwrap_or("?"),
                from = if cfg.base_token.is_some() { "config" } else { "name" },
                source = key_source,
                "resolved pool metadata (sorted PoolKey order)"
            ),
            None => tracing::info!(
                pool = %cfg.name,
                "no currencies resolved; raw price only (movement % unaffected)"
            ),
        }


        Ok(Self {
            name: cfg.name.clone(),
            version: cfg.version.clone(),
            decimals0,
            decimals1,
            decimals_known,
            base_token,
            quote_symbol,
            base_symbol,
            currencies,
            address,
            pool_id,
            tick_spacing,
            lp_fee,
        })
    }

    /// How this pool is named where routes and triggers refer to it.
    pub fn pool_ref(&self) -> crate::route::PoolRef {
        match self.pool_id {
            Some(id) => crate::route::PoolRef::V4(id),
            None => crate::route::PoolRef::V3(self.address),
        }
    }

    /// The token this pool's price is quoted *for*: a drop in the reported
    /// price is a drop of this token against the other one.
    pub fn base_currency(&self) -> Option<Address> {
        self.currencies
            .map(|(c0, c1)| if self.base_token == 1 { c1 } else { c0 })
    }

    /// Addresses to subscribe to. v3: the single pool. v4: PoolManager.
    pub fn filter_addresses(&self) -> Vec<Address> {
        vec![self.address]
    }

    /// The price this pool actually filled at, read out of a transaction's own
    /// `Swap` log.
    ///
    /// Both amounts come from the same event, so their ratio is the price the
    /// swap really got: the LP fee, the protocol fee, the hook's cut and the
    /// impact of the size are all already inside those two numbers, and none of
    /// them has to be modelled. It is in this pool's own quote token, which is
    /// the unit every other price here is measured in - notably the one a
    /// take-profit target is compared against.
    ///
    /// Signs are ignored on purpose. Which side is negative depends on the
    /// direction and on whose balance the event describes, and the ratio of the
    /// magnitudes is the price either way. More than one swap through the same
    /// pool in one transaction is summed rather than the first one taken.
    ///
    /// `None` when this pool did not swap in this transaction, or when its
    /// decimals were never resolved - a price scaled by the wrong power of ten
    /// is worse than no price, because it would be believed.
    pub fn fill_price(&self, logs: &[Log]) -> Option<f64> {
        if !self.decimals_known {
            return None;
        }
        let topic = match self.version.as_str() {
            "v4" => v4_swap_topic(),
            _ => v3_swap_topic(),
        };
        let (mut a0, mut a1) = (0.0f64, 0.0f64);
        for log in logs {
            if log.address != self.address || log.topics.first() != Some(&topic) {
                continue;
            }
            // v4 puts the PoolId in the first indexed slot, and one PoolManager
            // emits for every pool it holds - so without this every other pool
            // that traded in the same transaction would be counted as ours.
            if let Some(pid) = self.pool_id {
                if log.topics.get(1) != Some(&pid) {
                    continue;
                }
            }
            // Both protocols lay the two amounts out first, one word each.
            if log.data.0.len() < 64 {
                continue;
            }
            a0 += abs_signed_word(&log.data.0[0..32]);
            a1 += abs_signed_word(&log.data.0[32..64]);
        }
        // Both sides have to be real and positive; a NaN reaching a price a
        // target is built from would compare false against everything forever.
        if !a0.is_finite() || !a1.is_finite() || a0 <= 0.0 || a1 <= 0.0 {
            return None;
        }
        let (base, quote, base_dec, quote_dec) = match self.base_token {
            1 => (a1, a0, self.decimals1, self.decimals0),
            _ => (a0, a1, self.decimals0, self.decimals1),
        };
        let price = (quote / 10f64.powi(quote_dec as i32)) / (base / 10f64.powi(base_dec as i32));
        (price.is_finite() && price > 0.0).then_some(price)
    }

    /// Decimals of the quote side, used to scale `quote_pay` into human units.
    /// base_token=1 => quote is token0; base_token=0 => quote is token1.
    /// Falls back to 18 (ETH/WETH) when decimals could not be resolved.
    pub fn quote_decimals(&self) -> u8 {
        if !self.decimals_known {
            return 18;
        }
        if self.base_token == 1 {
            self.decimals0
        } else {
            self.decimals1
        }
    }

    /// Where tick state for this pool lives, for the tick-walking estimate.
    pub fn tick_source(&self) -> Option<crate::depth::Source> {
        match (self.version.as_str(), self.pool_id) {
            ("v4", Some(pool_id)) => Some(crate::depth::Source::V4 {
                manager: self.address,
                pool_id,
            }),
            ("v4", None) => None,
            _ => Some(crate::depth::Source::V3 { pool: self.address }),
        }
    }

    /// Estimate how much QUOTE token (raw units) must be paid in to move the
    /// price of the base token UP by `move_pct`.
    ///
    /// `liquidity` and `sqrt_input` must come from the same Swap log, so both
    /// describe the same instant. In-range liquidity only (no tick walking):
    /// the true cost is higher once the move crosses an initialized tick.
    pub fn quote_pay(&self, liquidity: u128, sqrt_input: U256, move_pct: f64) -> Result<f64> {
        anyhow::ensure!(move_pct > 0.0, "move_pct must be > 0, got {move_pct}");
        let l = liquidity as f64;
        let sqrt_p = sqrt_to_f64(sqrt_input);
        anyhow::ensure!(sqrt_p > 0.0, "sqrtPriceX96 is zero");
        let k = (1.0 + move_pct / 100.0).sqrt();
        Ok(if self.base_token == 1 {
            // Base is token1: buying it with token0 (x) pushes P = y/x DOWN,
            // so sqrt(P') = sqrt(P)/k and dx_in = L*(1/sqrt(P))*(k - 1).
            l * (1.0 / sqrt_p) * (k - 1.0)
        } else {
            // Base is token0: buying it with token1 (y) pushes P UP,
            // so sqrt(P') = sqrt(P)*k and dy_in = L*sqrt(P)*(k - 1).
            l * sqrt_p * (k - 1.0)
        })
    }
}

/// sqrtPriceX96 as a plain f64 sqrt(price).
pub fn sqrt_to_f64(sqrt: U256) -> f64 {
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

/// Derive poolId = keccak256(abi.encode(currency0, currency1, fee, tickSpacing, hooks)),
/// i.e. five 32-byte words (addresses/fee left-padded, tickSpacing sign-extended).
pub fn pool_id_from_key(
    c0: Address,
    c1: Address,
    fee: u32,
    tick_spacing: i32,
    hooks: Address,
) -> H256 {
    let (c0, c1) = sorted(c0, c1);
    let word = |b: &[u8]| {
        let mut w = [0u8; 32];
        w[32 - b.len()..].copy_from_slice(b);
        w
    };
    let signed_word = |v: i32| {
        let mut w = if v >= 0 { [0u8; 32] } else { [0xffu8; 32] };
        w[29..].copy_from_slice(&v.to_be_bytes()[1..]);
        w
    };
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(&word(c0.as_bytes()));
    buf.extend_from_slice(&word(c1.as_bytes()));
    buf.extend_from_slice(&word(&fee.to_be_bytes()[1..]));
    buf.extend_from_slice(&signed_word(tick_spacing));
    buf.extend_from_slice(&word(hooks.as_bytes()));
    H256::from_slice(&keccak256(&buf))
}

fn derive_pool_id(cfg: &PoolConfig, tokens: &HashMap<String, String>) -> Result<H256> {
    let t0 = resolve_token(tokens, cfg.token0.as_deref().context("v4 missing token0")?)?;
    let t1 = resolve_token(tokens, cfg.token1.as_deref().context("v4 missing token1")?)?;
    let hooks: Address = cfg
        .hooks
        .as_deref()
        .unwrap_or("0x0000000000000000000000000000000000000000")
        .parse()?;
    Ok(pool_id_from_key(
        t0,
        t1,
        cfg.fee.context("v4 missing fee")?,
        cfg.tick_spacing.context("v4 missing tick_spacing")?,
        hooks,
    ))
}

/// How far back one `eth_getLogs` reaches. Measured on chain 4663: a window of
/// ten million blocks is answered, fifty million is not - and the refusal there
/// is `fullnode unavailable`, which is the backend's retention rather than a
/// limit on the request. Ten million blocks is about eleven days at this
/// chain's block time, so a pool created in the last week and a half is found
/// by the very first query and the whole chain by six.
const LOG_WINDOW: u64 = 10_000_000;

/// Where halving stops. A provider that refuses even this is one the walk
/// cannot finish against in any sensible number of requests, and saying so
/// beats making thousands of them.
const MIN_LOG_WINDOW: u64 = 10_000;

/// Hard cap on queries, so no combination of a small window and a long chain
/// can turn one pool's resolution into an unbounded loop.
const MAX_LOG_QUERIES: u32 = 64;

/// Why an `eth_getLogs` was refused, which decides what to do about it.
enum Refused {
    /// The range was too wide for this provider. Ask for less.
    TooWide,
    /// The node does not keep logs this far back. Asking for less will not
    /// help; asking again for the same thing will not either.
    TooOld,
}

/// Read a provider's complaint. Anything not recognised is a real error and is
/// returned as one - guessing that an unfamiliar failure means "narrow the
/// window" would turn one broken endpoint into sixty-four requests.
fn classify(msg: &str) -> Option<Refused> {
    let m = msg.to_ascii_lowercase();
    if m.contains("too large")
        || m.contains("too wide")
        || m.contains("exceed")
        || m.contains("range is too")
        // "query returned more than N results" - the same refusal counted the
        // other way round, and answered the same way.
        || (m.contains("more than") && m.contains("results"))
    {
        return Some(Refused::TooWide);
    }
    if m.contains("unavailable") || m.contains("not available") || m.contains("pruned")
        || m.contains("missing trie") || m.contains("too old")
    {
        return Some(Refused::TooOld);
    }
    None
}

/// The pool's `Initialize` log, found by walking back from the head in windows.
///
/// This used to bisect `eth_getStorageAt` over the whole chain to pin the block
/// down to one, and then ask for that single block's logs - about 26 reads of
/// HISTORICAL STATE. That is the one thing an ordinary node does not keep: the
/// bisection failed outright against a non-archive endpoint, with
/// `historical state is not available`, which is a message that tells the
/// operator nothing about what to do.
///
/// Old *logs*, unlike old *state*, are kept almost everywhere - they are two
/// unrelated capabilities, and the bisection was spending the rare one to save
/// the common one. So the search asks for logs directly, newest window first,
/// and stops at the first hit. A pool created recently - which is every pool
/// worth trading here - costs one request. No archive, at any point.
///
/// The window narrows itself against a stricter provider and gives up cleanly
/// against one whose history simply does not reach, saying how far it got.
async fn find_init_log(http: &Provider<Http>, manager: Address, pool_id: H256) -> Result<Log> {
    let head = http.get_block_number().await?.as_u64();
    let mut window = LOG_WINDOW;
    let mut to = head;
    let mut queries = 0u32;

    loop {
        anyhow::ensure!(
            queries < MAX_LOG_QUERIES,
            "gave up looking for the Initialize log after {queries} queries; searched back \
             to block {to} of {head} in windows of {window}"
        );
        // Inclusive on both ends, so consecutive windows neither overlap nor
        // skip the block between them.
        let from = to.saturating_sub(window.saturating_sub(1));
        queries += 1;
        let filter = Filter::new()
            .address(manager)
            .topic0(ValueOrArray::Value(v4_init_topic()))
            .topic1(ValueOrArray::Value(pool_id))
            .from_block(from)
            .to_block(to);
        match http.get_logs(&filter).await {
            Ok(logs) => {
                if let Some(log) = logs.into_iter().next() {
                    tracing::debug!(?pool_id, from, to, queries, "found the Initialize log");
                    return Ok(log);
                }
                anyhow::ensure!(
                    from > 0,
                    "no Initialize log for this pool anywhere in the chain's logs - the id is \
                     wrong, or the pool belongs to a different PoolManager than {manager:?}"
                );
                to = from - 1;
            }
            Err(e) => match classify(&e.to_string()) {
                Some(Refused::TooWide) => {
                    let narrower = window / 2;
                    anyhow::ensure!(
                        narrower >= MIN_LOG_WINDOW,
                        "this endpoint refuses a log range of even {window} blocks, so the \
                         Initialize log cannot be reached from here: {e}"
                    );
                    tracing::debug!(window, narrower, "log range refused as too wide, narrowing");
                    window = narrower;
                }
                // The floor of what this endpoint keeps. Nothing about asking
                // differently gets underneath it.
                Some(Refused::TooOld) => anyhow::bail!(
                    "this endpoint's log history stops above block {from}, and the pool was \
                     initialized below it - searched back from {head} without finding it. \
                     Write the pool's token0/token1/fee/tick_spacing/hooks into [[pools]], or \
                     point HTTP_URL at an endpoint that keeps more history: {e}"
                ),
                None => return Err(anyhow::anyhow!("{e}")).context("eth_getLogs Initialize"),
            },
        }
    }
}

/// Recover a v4 PoolKey: the five fields the pool id is the hash of.
///
/// A pool id cannot be reversed, but the key was published once in the pool's
/// `Initialize` log - see `find_init_log` for how that log is reached. The
/// answer is cached, and a cached one is only used when it still hashes to the
/// id it was filed under, which makes a wrong cache impossible to act on. What
/// comes off the chain is held to exactly the same test before it is returned
/// or written: a matching hash proves all five fields at once, and it is the
/// only thing that can catch a misread of the log's own layout.
pub async fn v4_pool_key(
    http: &Provider<Http>,
    manager: Address,
    pool_id: H256,
) -> Result<(Address, Address, u32, i32, Address)> {
    if let Some(k) = crate::cache::v4_pool(pool_id, |k| {
        pool_id_from_key(
            k.currency0,
            k.currency1,
            k.fee,
            k.tick_spacing,
            k.hooks.unwrap_or_default(),
        )
    }) {
        return Ok((
            k.currency0,
            k.currency1,
            k.fee,
            k.tick_spacing,
            k.hooks.unwrap_or_default(),
        ));
    }
    let log = find_init_log(http, manager, pool_id).await?;
    let block = log.block_number.map(|b| b.as_u64()).unwrap_or_default();
    anyhow::ensure!(log.topics.len() >= 4, "Initialize log missing currency topics");
    let c0 = Address::from_slice(&log.topics[2].as_bytes()[12..]);
    let c1 = Address::from_slice(&log.topics[3].as_bytes()[12..]);
    let d = &log.data.0;
    anyhow::ensure!(d.len() >= 96, "Initialize log data too short");
    let fee = U256::from_big_endian(&d[0..32]).low_u32() & 0xff_ffff;
    let ts_raw = U256::from_big_endian(&d[32..64]).low_u32() & 0xff_ffff;
    let tick_spacing = if ts_raw & 0x80_0000 != 0 {
        ts_raw as i32 - 0x100_0000
    } else {
        ts_raw as i32
    };
    let hooks = Address::from_slice(&d[76..96]);
    // The filter already guarantees this log belongs to this pool, so what
    // this catches is not the wrong log but the right one read wrongly - a
    // field taken from the wrong offset, or a tick spacing whose sign was
    // rebuilt incorrectly. Nothing downstream would notice either.
    let rederived = pool_id_from_key(c0, c1, fee, tick_spacing, hooks);
    anyhow::ensure!(
        rederived == pool_id,
        "the Initialize log at block {block} decodes to a PoolKey hashing to {rederived:?}, \
         not to {pool_id:?} - refusing to use it"
    );
    tracing::info!(
        block, ?c0, ?c1, fee, tick_spacing, ?hooks,
        "recovered v4 PoolKey from Initialize log"
    );
    crate::cache::put_v4_pool(
        pool_id,
        crate::cache::PoolKey {
            currency0: c0,
            currency1: c1,
            fee,
            tick_spacing,
            hooks: Some(hooks),
        },
    );
    Ok((c0, c1, fee, tick_spacing, hooks))
}

/// Decimals of a currency. Native ETH is the zero address -> 18.
/// The `BASE/QUOTE` pair a pool's name claims, if it is written that way.
///
/// Anything after a space is dropped, so "ROBLOXIANS/RBLX (v4)" reads as
/// ("ROBLOXIANS", "RBLX"). A name that is not one slash between two non-empty
/// words is not a claim about anything and is left alone.
fn named_pair(name: &str) -> Option<(&str, &str)> {
    let head = name.split_whitespace().next()?;
    let (base, quote) = head.split_once('/')?;
    if base.is_empty() || quote.is_empty() || quote.contains('/') {
        return None;
    }
    Some((base.trim(), quote.trim()))
}

/// Which side of the pair the price is quoted FOR, as an index into the sorted
/// PoolKey.
///
/// A name written `BASE/QUOTE` is a statement a human made about a pool whose
/// currency order they did not choose, so where both symbols are known it
/// decides - and `base_token` stops being something to get right. An explicit
/// `base_token` still wins, but it has to agree with the name, because one of
/// the two being wrong is silent: the monitor would report the other token's
/// moves and an armed route would refuse to buy on its own dip.
fn resolve_base_token(
    name: &str,
    configured: Option<u8>,
    sym0: Option<&str>,
    sym1: Option<&str>,
) -> Result<u8> {
    let from_name = match (named_pair(name), sym0, sym1) {
        (Some((b, q)), Some(s0), Some(s1)) => {
            if b.eq_ignore_ascii_case(s0) && q.eq_ignore_ascii_case(s1) {
                Some(0)
            } else if b.eq_ignore_ascii_case(s1) && q.eq_ignore_ascii_case(s0) {
                Some(1)
            } else {
                // Neither way round fits, so the name is about a different
                // pool than the id or address points at.
                anyhow::bail!(
                    "is named {b}/{q} but holds {s0}/{s1}; the name and the pool do not match"
                );
            }
        }
        _ => None,
    };
    match (configured, from_name) {
        (Some(c), Some(n)) => {
            anyhow::ensure!(
                c == n,
                "is named {}/{}, which makes base_token {n}, but base_token = {c} was set; \
                 base_token indexes the SORTED PoolKey, not the order in the name",
                named_pair(name).map(|p| p.0).unwrap_or("?"),
                named_pair(name).map(|p| p.1).unwrap_or("?")
            );
            Ok(c)
        }
        (Some(c), None) => Ok(c),
        (None, Some(n)) => Ok(n),
        // Nothing said anything: token0 is the base, as it always was.
        (None, None) => Ok(0),
    }
}

/// token0/token1/fee/tickSpacing straight off a v3 pool contract. Unlike v4,
/// where the PoolKey has to be recovered from a log and re-hashed, v3 publishes
/// all of it as plain views on the pool itself.
pub async fn v3_pool_key(
    http: &Provider<Http>,
    pool: Address,
) -> Result<(Address, Address, u32, i32)> {
    if let Some(k) = crate::cache::v3_pool(pool) {
        return Ok((k.currency0, k.currency1, k.fee, k.tick_spacing));
    }
    let code = http.get_code(pool, None).await.context("eth_getCode")?;
    anyhow::ensure!(!code.0.is_empty(), "{pool:?} has no code; not a v3 pool");
    let t0 = call_address(http, pool, &selector("token0()"))
        .await
        .with_context(|| format!("{pool:?}: token0()"))?;
    let t1 = call_address(http, pool, &selector("token1()"))
        .await
        .with_context(|| format!("{pool:?}: token1()"))?;
    let fee = call_uint(http, pool, &selector("fee()"))
        .await
        .with_context(|| format!("{pool:?}: fee()"))?;
    let spacing = call_uint(http, pool, &selector("tickSpacing()"))
        .await
        .with_context(|| format!("{pool:?}: tickSpacing()"))? as i32;
    crate::cache::put_v3_pool(
        pool,
        crate::cache::PoolKey {
            currency0: t0,
            currency1: t1,
            fee,
            tick_spacing: spacing,
            hooks: None,
        },
    );
    Ok((t0, t1, fee, spacing))
}

pub async fn decimals_of(http: &Provider<Http>, currency: Address) -> Result<u8> {
    if currency == Address::zero() {
        return Ok(18);
    }
    if let Some(t) = crate::cache::token(currency) {
        return Ok(t.decimals);
    }
    let decimals = call_u8(http, currency, &selector("decimals()")).await?;
    crate::cache::put_token(
        currency,
        crate::cache::TokenInfo {
            decimals,
            symbol: None,
        },
    );
    Ok(decimals)
}

/// `symbol()` of a currency. Native ETH (zero address) is "ETH". Handles both
/// the modern `string` return and the legacy `bytes32` one.
pub async fn symbol_of(http: &Provider<Http>, currency: Address) -> Result<String> {
    if currency == Address::zero() {
        return Ok("ETH".to_string());
    }
    if let Some(s) = crate::cache::token(currency).and_then(|t| t.symbol) {
        return Ok(s);
    }
    let tx = TransactionRequest::new()
        .to(currency)
        .data(selector("symbol()"));
    let res: Bytes = http.call(&tx.into(), None).await.context("eth_call symbol()")?;
    anyhow::ensure!(res.len() >= 32, "short return for symbol()");
    // ABI string: [offset][len][bytes...]
    if res.len() >= 64 {
        let off = U256::from_big_endian(&res[0..32]).as_usize();
        if off == 32 && res.len() >= 64 {
            let len = U256::from_big_endian(&res[32..64]).as_usize();
            if len > 0 && len <= 64 && res.len() >= 64 + len {
                let sym = String::from_utf8_lossy(&res[64..64 + len]).into_owned();
                remember_symbol(http, currency, &sym).await;
                return Ok(sym);
            }
        }
    }
    // legacy bytes32: right-padded with zeros
    let trimmed: Vec<u8> = res[0..32].iter().copied().take_while(|b| *b != 0).collect();
    anyhow::ensure!(!trimmed.is_empty(), "empty symbol()");
    let sym = String::from_utf8_lossy(&trimmed).into_owned();
    remember_symbol(http, currency, &sym).await;
    Ok(sym)
}

/// File a symbol against the decimals we know, or go and learn them.
async fn remember_symbol(http: &Provider<Http>, currency: Address, symbol: &str) {
    let decimals = match crate::cache::token(currency) {
        Some(t) => t.decimals,
        None => match decimals_of(http, currency).await {
            Ok(d) => d,
            Err(_) => return,
        },
    };
    crate::cache::put_token(
        currency,
        crate::cache::TokenInfo {
            decimals,
            symbol: Some(symbol.to_string()),
        },
    );
}

/// Read a small unsigned integer return value (uint24/int24/uint8...).
async fn call_uint(provider: &Provider<Http>, to: Address, data: &Bytes) -> Result<u32> {
    let tx = TransactionRequest::new().to(to).data(data.clone());
    let res: Bytes = provider.call(&tx.into(), None).await.context("eth_call uint")?;
    anyhow::ensure!(res.len() >= 32, "short return for uint call");
    Ok(U256::from_big_endian(&res[0..32]).low_u32())
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

    /// The two refusals mean opposite things and the walk answers them in
    /// opposite ways: a range that is too wide is retried narrower, and history
    /// the node does not keep is not retried at all. Reading one as the other
    /// either abandons a pool that was findable, or halves the window sixty
    /// times against a wall.
    ///
    /// Both strings below are verbatim from the endpoint this runs against.
    #[test]
    fn a_refusal_is_read_for_which_kind_it_is() {
        assert!(matches!(
            classify("Block range is too large"),
            Some(Refused::TooWide)
        ));
        assert!(matches!(
            classify("(code: -32000, message: fullnode unavailable, data: None)"),
            Some(Refused::TooOld)
        ));
        // Counted the other way round by some providers, same meaning.
        assert!(matches!(
            classify("query returned more than 10000 results"),
            Some(Refused::TooWide)
        ));

        // Anything unrecognised must stay unrecognised: guessing "narrow the
        // window" at a broken endpoint would spend MAX_LOG_QUERIES finding out.
        assert!(classify("connection reset by peer").is_none());
        assert!(classify("invalid api key").is_none());
        assert!(classify("").is_none());
    }

    /// The walk has to be able to cover the chain it runs on in a sane number
    /// of requests, or it is a different kind of failure than the bisection it
    /// replaced rather than a fix for it.
    #[test]
    fn the_window_covers_the_chain_in_few_enough_queries() {
        // Chain 4663 was 55.4M blocks deep when this was written.
        let chain = 55_429_928u64;
        let windows = chain.div_ceil(LOG_WINDOW);
        assert!(windows <= 6, "{windows} windows to cover the whole chain");
        assert!(u64::from(MAX_LOG_QUERIES) > windows);
    }

    fn pool(base_token: u8, decimals: (u8, u8)) -> Pool {
        Pool {
            name: "t".into(),
            version: "v4".into(),
            decimals0: decimals.0,
            decimals1: decimals.1,
            decimals_known: true,
            base_token,
            quote_symbol: None,
            base_symbol: None,
            currencies: None,
            address: Address::zero(),
            pool_id: None,
            tick_spacing: Some(60),
            lp_fee: Some(3000),
        }
    }

    #[test]
    fn a_name_written_as_a_pair_is_read_base_first() {
        assert_eq!(named_pair("ROBLOXIANS/RBLX (v4)"), Some(("ROBLOXIANS", "RBLX")));
        assert_eq!(named_pair("PONS/WETH (v3)"), Some(("PONS", "WETH")));
        assert_eq!(named_pair("CAMELTOE/LULU"), Some(("CAMELTOE", "LULU")));
        // Not a pair, so not a claim: these must not be checked against.
        assert_eq!(named_pair("the deep pool"), None);
        assert_eq!(named_pair("A/B/C"), None);
        assert_eq!(named_pair("/RBLX"), None);
        assert_eq!(named_pair("RBLX/"), None);
        assert_eq!(named_pair(""), None);
    }

    #[test]
    fn the_name_decides_which_side_is_the_base() {
        // Sorted order is (RBLX, ROBLOXIANS), and the name says ROBLOXIANS is
        // the base - so base_token is 1, and nobody had to work that out.
        let t = |n, c| resolve_base_token(n, c, Some("RBLX"), Some("ROBLOXIANS"));
        assert_eq!(t("ROBLOXIANS/RBLX (v4)", None).unwrap(), 1);
        assert_eq!(t("RBLX/ROBLOXIANS (v4)", None).unwrap(), 0);
        // Case is not the point.
        assert_eq!(t("robloxians/rblx", None).unwrap(), 1);
        // An explicit index that agrees is redundant but fine.
        assert_eq!(t("ROBLOXIANS/RBLX", Some(1)).unwrap(), 1);
        // One that disagrees is the mistake this exists to catch.
        assert!(t("ROBLOXIANS/RBLX", Some(0)).is_err());
        // A name about tokens this pool does not hold points at the wrong pool.
        assert!(t("PONS/WETH", None).is_err());
    }

    #[test]
    fn without_a_pair_name_nothing_is_inferred() {
        // No claim, no symbols, or only one symbol: fall back to what was
        // configured, and to token0 when that is absent too.
        assert_eq!(resolve_base_token("the deep pool", None, Some("A"), Some("B")).unwrap(), 0);
        assert_eq!(resolve_base_token("the deep pool", Some(1), Some("A"), Some("B")).unwrap(), 1);
        assert_eq!(resolve_base_token("A/B", None, None, Some("B")).unwrap(), 0);
        assert_eq!(resolve_base_token("A/B", Some(1), None, None).unwrap(), 1);
    }

    #[test]
    fn the_base_side_is_the_token_a_drop_is_a_drop_of() {
        let c0 = Address::from([1u8; 20]);
        let c1 = Address::from([2u8; 20]);
        let mut p = pool(0, (18, 18));
        p.currencies = Some((c0, c1));
        assert_eq!(p.base_currency(), Some(c0));
        p.base_token = 1;
        assert_eq!(p.base_currency(), Some(c1));
        // Unresolved currencies must not guess: relative moves still work,
        // but nothing that names a token may act on them.
        p.currencies = None;
        assert_eq!(p.base_currency(), None);
    }

    fn registry() -> HashMap<String, String> {
        [
            ("POOLS", "0x385b36ff682ab4c76e7c37a66b96aabc466471d5"),
            ("LULU", "0x4e62068525ab11fe768e29dfd00ef909b9803016"),
            ("CAMELTOE", "0xc32b91fe216af1b834db02f33326e983ad8cf201"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
    }

    #[test]
    fn resolves_tickers_and_raw_addresses() {
        let t = registry();
        let pools: Address = "0x385b36ff682ab4c76e7c37a66b96aabc466471d5".parse().unwrap();
        assert_eq!(resolve_token(&t, "POOLS").unwrap(), pools);
        // tickers are matched case-insensitively
        assert_eq!(resolve_token(&t, "pools").unwrap(), pools);
        // a raw address still works, registry or not
        assert_eq!(
            resolve_token(&t, "0x385b36ff682ab4c76e7c37a66b96aabc466471d5").unwrap(),
            pools
        );
        assert_eq!(
            resolve_token(&HashMap::new(), "0x0000000000000000000000000000000000000000").unwrap(),
            Address::zero()
        );
        // an unknown ticker is an error, not a silent zero address
        assert!(resolve_token(&t, "NOPE").is_err());
    }

    /// The pair must land in canonical PoolKey order however it was written,
    /// otherwise decimals and the quote side get swapped.
    #[test]
    fn pair_order_is_normalised() {
        let t = registry();
        let lulu = resolve_token(&t, "LULU").unwrap();
        let cameltoe = resolve_token(&t, "CAMELTOE").unwrap();
        // on chain: currency0 = LULU (0x4e62…), currency1 = CAMELTOE (0xc32b…)
        assert_eq!(sorted(lulu, cameltoe), (lulu, cameltoe));
        assert_eq!(sorted(cameltoe, lulu), (lulu, cameltoe));
    }

    #[test]
    fn derived_pool_id_is_order_independent() {
        let t = registry();
        let mut a = PoolConfig {
            name: "t".into(),
            address: "0x0000000000000000000000000000000000000000".into(),
            version: "v4".into(),
            token0: Some("LULU".into()),
            token1: Some("CAMELTOE".into()),
            decimals0: None,
            decimals1: None,
            base_token: None,
            threshold_pct: None,
            max_move_pct: None,
            pool_id: None,
            fee: Some(2500),
            tick_spacing: Some(60),
            hooks: None,
        };
        let forward = derive_pool_id(&a, &t).unwrap();
        std::mem::swap(&mut a.token0, &mut a.token1);
        assert_eq!(derive_pool_id(&a, &t).unwrap(), forward);
    }

    #[test]
    fn known_topics() {
        // All three verified against logs emitted on chain 4663.
        assert_eq!(
            v3_swap_topic(),
            "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
                .parse::<H256>()
                .unwrap()
        );
        assert_eq!(
            v4_swap_topic(),
            "0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f"
                .parse::<H256>()
                .unwrap()
        );
        assert_eq!(
            v4_init_topic(),
            "0xdd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438"
                .parse::<H256>()
                .unwrap()
        );
    }

    /// Anchored on the live POOLS/ETH pool: L and sqrtPriceX96 read from chain
    /// 4663, quote side is native ETH (token0, 18 decimals).
    #[test]
    fn quote_pay_matches_hand_computation() {
        let p = pool(1, (18, 18));
        let sqrt = U256::from_dec_str("86182064487810775758807691739729").unwrap();
        let l: u128 = 85_149_968_048_777_632_622_077;
        let eth = p.quote_pay(l, sqrt, 1.0).unwrap() / 1e18;
        // dx_in = L*(1/sqrt(P))*(sqrt(1.01) - 1)
        assert!((eth - 0.390_423_092).abs() < 1e-8, "got {eth}");
        // The direction matters: the amount RECEIVED when selling the same
        // distance is smaller by a factor of sqrt(1.01), and is not what we want.
        let wrong_direction = eth / 1.01f64.sqrt();
        assert!((wrong_direction - 0.388_485_496).abs() < 1e-8);
    }

    /// The two branches are the same formula viewed from either side, so for a
    /// shared (L, sqrtP) their ratio must be exactly the raw price P = y/x.
    #[test]
    fn quote_pay_branches_are_symmetric() {
        let sqrt = U256::from_dec_str("86182064487810775758807691739729").unwrap();
        let l: u128 = 85_149_968_048_777_632_622_077;
        let pay_token0 = pool(1, (18, 18)).quote_pay(l, sqrt, 2.5).unwrap();
        let pay_token1 = pool(0, (18, 18)).quote_pay(l, sqrt, 2.5).unwrap();
        let raw_p = crate::price::raw_price(sqrt);
        let rel = ((pay_token1 / pay_token0) - raw_p).abs() / raw_p;
        assert!(rel < 1e-9, "ratio {} vs P {raw_p}", pay_token1 / pay_token0);
    }

    #[test]
    fn quote_pay_rejects_degenerate_inputs() {
        let p = pool(1, (18, 18));
        let sqrt = U256::from_dec_str("86182064487810775758807691739729").unwrap();
        assert!(p.quote_pay(1, sqrt, 0.0).is_err());
        assert!(p.quote_pay(1, sqrt, -1.0).is_err());
        assert!(p.quote_pay(1, U256::zero(), 1.0).is_err());
    }

    /// One v4 `Swap` word layout, with the amounts signed the way the chain
    /// signs them: the side paid in is negative from the swapper's view.
    fn swap_log(pool: &Pool, amount0: i128, amount1: i128, id: Option<H256>) -> Log {
        // Sign-extended two's complement, as the ABI encodes a negative int.
        let signed_word = |v: i128| -> [u8; 32] {
            let mut w = [if v < 0 { 0xffu8 } else { 0x00 }; 32];
            w[16..].copy_from_slice(&v.to_be_bytes());
            w
        };
        let mut data = vec![0u8; 192];
        data[0..32].copy_from_slice(&signed_word(amount0));
        data[32..64].copy_from_slice(&signed_word(amount1));
        let mut topics = vec![v4_swap_topic()];
        if let Some(id) = id {
            topics.push(id);
        }
        Log {
            address: pool.address,
            topics,
            data: data.into(),
            ..Default::default()
        }
    }

    /// The price a fill really got, out of the swap's own event. The ratio has
    /// to survive the signs - which side is negative depends on the direction,
    /// and the magnitudes are the price either way.
    #[test]
    fn a_fill_price_comes_out_of_the_swap_log() {
        // base is token1, quote is token0, both 18 decimals: paid 21 of token0
        // for 10 of token1, so the base cost 2.1 quote each.
        let p = pool(1, (18, 18));
        let unit = 1_000_000_000_000_000_000i128;
        let (paid, got) = (-21 * unit, 10 * unit);
        let px = p.fill_price(&[swap_log(&p, paid, got, None)]).unwrap();
        assert!((px - 2.1).abs() < 1e-12, "{px}");
        // The mirror trade prices the same, because only magnitudes matter.
        let px = p.fill_price(&[swap_log(&p, -paid, -got, None)]).unwrap();
        assert!((px - 2.1).abs() < 1e-12, "{px}");
    }

    /// One PoolManager emits for every pool it holds, so a swap through some
    /// other pool in the same transaction must not be read as ours.
    #[test]
    fn another_pools_swap_in_the_same_tx_is_ignored() {
        let mut p = pool(1, (18, 18));
        p.pool_id = Some(H256::from([7u8; 32]));
        let unit = 1_000_000_000_000_000_000i128;
        let mine = swap_log(&p, -21 * unit, 10 * unit, p.pool_id);
        let elsewhere = Some(H256::from([9u8; 32]));
        let theirs = swap_log(&p, -999 * unit, unit, elsewhere);
        let px = p.fill_price(&[theirs, mine]).unwrap();
        assert!((px - 2.1).abs() < 1e-12, "{px}");
    }

    /// Decimals that were never resolved would scale the price by the wrong
    /// power of ten, and a wrong price is worse than none because it is used.
    #[test]
    fn an_unscaled_pool_reports_no_fill_price() {
        let mut p = pool(1, (18, 18));
        p.decimals_known = false;
        let unit = 1_000_000_000_000_000_000i128;
        let log = swap_log(&p, -21 * unit, 10 * unit, None);
        assert!(p.fill_price(&[log]).is_none());
    }

    #[test]
    fn quote_decimals_follows_the_base_side() {
        // base_token=1 -> quote is token0
        assert_eq!(pool(1, (6, 18)).quote_decimals(), 6);
        // base_token=0 -> quote is token1
        assert_eq!(pool(0, (6, 18)).quote_decimals(), 18);
        // unresolved decimals fall back to 18 rather than scaling by 10^0
        let mut p = pool(1, (0, 0));
        p.decimals_known = false;
        assert_eq!(p.quote_decimals(), 18);
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
            base_token: None,
            threshold_pct: None,
            max_move_pct: None,
            pool_id: None,
            fee: Some(2500),
            tick_spacing: Some(60),
            hooks: None,
        };
        assert_eq!(
            derive_pool_id(&cfg, &HashMap::new()).unwrap(),
            "0x0277354251edc469597038bae48c9f6b7b80003999b511a7c5eba9a2de764f09"
                .parse::<H256>()
                .unwrap()
        );
    }
}
