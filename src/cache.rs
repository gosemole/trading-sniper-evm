//! What the chain has already told us and cannot un-tell.
//!
//! Recovering one v4 PoolKey costs about two dozen archive reads: the block it
//! was created in is found by bisection, and only then can its `Initialize` log
//! be fetched. Doing that for every pool and every hop on every start is most
//! of the time between launching the bot and it watching anything.
//!
//! Everything kept here is immutable by construction, which is what makes
//! caching it honest rather than merely convenient:
//!
//! - a v4 PoolKey **is** the preimage of the pool id, so it cannot change
//!   without becoming a different pool - and a cached one is re-hashed and
//!   checked against its id before it is used, so a corrupted or hand-edited
//!   file is caught rather than believed;
//! - a v3 pool's currencies, fee and tick spacing are set at creation and have
//!   no setter;
//! - an ERC-20's decimals and symbol are fixed in every token these routes
//!   touch.
//!
//! Nothing that moves - prices, liquidity, balances, allowances - is ever kept
//! here. Delete the file and the only cost is a slow start.

use anyhow::{Context, Result};
use ethers::types::{Address, H256};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A pool's identity, as recovered once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolKey {
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    /// v4 only; a v3 pool has no hooks.
    #[serde(default)]
    pub hooks: Option<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenInfo {
    pub decimals: u8,
    pub symbol: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    pools: HashMap<String, PoolKey>,
    #[serde(default)]
    tokens: HashMap<String, TokenInfo>,
}

struct Cache {
    path: PathBuf,
    store: Store,
    dirty: bool,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Start using a file. Called once, before anything resolves a pool; without it
/// every lookup simply misses and the bot behaves as it always did.
pub fn open(path: &Path) -> Result<usize> {
    let store: Store = match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let known = store.pools.len();
    let _ = CACHE.set(Mutex::new(Cache {
        path: path.to_path_buf(),
        store,
        dirty: false,
    }));
    Ok(known)
}

fn with<T>(f: impl FnOnce(&mut Cache) -> T) -> Option<T> {
    let c = CACHE.get()?;
    // A poisoned lock would mean a panic while holding it. Nothing here can
    // panic, and losing the cache is not worth propagating a failure for.
    c.lock().ok().map(|mut g| f(&mut g))
}

fn v4_key(pool_id: H256) -> String {
    format!("v4:{pool_id:?}").to_lowercase()
}

fn v3_key(pool: Address) -> String {
    format!("v3:{pool:?}").to_lowercase()
}

fn token_key(token: Address) -> String {
    format!("{token:?}").to_lowercase()
}

/// A v4 PoolKey, but only if it still hashes to the id it is filed under.
///
/// `derive` is the same hash the rest of the code checks against, passed in so
/// this module does not have to know how a pool id is built.
pub fn v4_pool(pool_id: H256, derive: impl Fn(&PoolKey) -> H256) -> Option<PoolKey> {
    let key = with(|c| c.store.pools.get(&v4_key(pool_id)).cloned())??;
    if derive(&key) == pool_id {
        return Some(key);
    }
    tracing::warn!(
        ?pool_id,
        "cached PoolKey does not hash to its own id; ignoring it and reading the chain"
    );
    with(|c| {
        c.store.pools.remove(&v4_key(pool_id));
        c.dirty = true;
    });
    None
}

pub fn put_v4_pool(pool_id: H256, key: PoolKey) {
    with(|c| {
        if c.store.pools.insert(v4_key(pool_id), key).is_none() {
            c.dirty = true;
        }
    });
}

pub fn v3_pool(pool: Address) -> Option<PoolKey> {
    with(|c| c.store.pools.get(&v3_key(pool)).cloned())?
}

pub fn put_v3_pool(pool: Address, key: PoolKey) {
    with(|c| {
        if c.store.pools.insert(v3_key(pool), key).is_none() {
            c.dirty = true;
        }
    });
}

pub fn token(token: Address) -> Option<TokenInfo> {
    with(|c| c.store.tokens.get(&token_key(token)).cloned())?
}

pub fn put_token(addr: Address, info: TokenInfo) {
    with(|c| {
        let e = c.store.tokens.entry(token_key(addr)).or_insert_with(|| info.clone());
        // A later lookup may know the symbol where an earlier one did not.
        if e.symbol.is_none() && info.symbol.is_some() {
            e.symbol = info.symbol.clone();
            c.dirty = true;
        } else if *e != info && e.symbol.is_some() {
            // Leave what is there; decimals and symbols do not change, so a
            // disagreement is a reason to trust neither silently.
            tracing::debug!(?addr, "cached token info differs from what was just read");
        }
        if !c.store.tokens.contains_key(&token_key(addr)) {
            c.dirty = true;
        }
    });
}

/// Write what has been learned, if anything has. Cheap to call when nothing
/// changed.
pub fn flush() {
    let Some(res) = with(|c| {
        if !c.dirty {
            return Ok(false);
        }
        let tmp = c.path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(&c.store).context("serialising pool cache")?;
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &c.path)
            .with_context(|| format!("replacing {}", c.path.display()))?;
        c.dirty = false;
        Ok::<bool, anyhow::Error>(true)
    }) else {
        return;
    };
    match res {
        Ok(true) => tracing::info!("pool cache written"),
        Ok(false) => {}
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "could not write the pool cache"),
    }
}
