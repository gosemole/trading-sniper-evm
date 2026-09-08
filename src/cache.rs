//! What the chain has already told us and cannot un-tell.
//!
//! One thing now: what a pair token is. Its decimals and symbol are fixed at
//! deployment and have no setter, which is what makes keeping them honest
//! rather than merely convenient - and without them every amount in a journal
//! is unreadable, because a six-decimal token printed at eighteen says
//! 0.00000000809.
//!
//! Two calls per token ever seen, and none at all after that. Nothing that
//! moves - prices, reserves, balances - is ever kept here. Delete the file and
//! the only cost is a slow start.

use anyhow::{Context, Result};
use ethers::types::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenInfo {
    pub decimals: u8,
    pub symbol: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    tokens: HashMap<String, TokenInfo>,
}

struct Cache {
    path: PathBuf,
    store: Store,
    dirty: bool,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

/// Start using a file. Called once, before anything looks a token up; without
/// it every lookup simply misses and the bot behaves as it always did.
///
/// Returns how many tokens it holds. It used to return how many POOLS - a map
/// the fall bot filled and this one never has - so the startup line reported
/// an empty cache on every run whatever the file contained, which is the kind
/// of small lie that makes the rest of a log hard to trust.
pub fn open(path: &Path) -> Result<usize> {
    let store: Store = match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Store::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let known = store.tokens.len();
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



fn token_key(token: Address) -> String {
    format!("{token:?}").to_lowercase()
}





pub fn token(token: Address) -> Option<TokenInfo> {
    with(|c| c.store.tokens.get(&token_key(token)).cloned())?
}

pub fn put_token(addr: Address, info: TokenInfo) {
    with(|c| {
        let k = token_key(addr);
        match c.store.tokens.get_mut(&k) {
            // A later lookup may know the symbol where an earlier one did not.
            Some(e) if e.symbol.is_none() && info.symbol.is_some() => {
                e.symbol = info.symbol;
                c.dirty = true;
            }
            Some(_) => {}
            None => {
                c.store.tokens.insert(k, info);
                c.dirty = true;
            }
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
        let body = serde_json::to_string_pretty(&c.store).context("serialising the token cache")?;
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &c.path)
            .with_context(|| format!("replacing {}", c.path.display()))?;
        c.dirty = false;
        Ok::<bool, anyhow::Error>(true)
    }) else {
        return;
    };
    match res {
        Ok(true) => tracing::info!("token cache written"),
        Ok(false) => {}
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "could not write the token cache"),
    }
}
