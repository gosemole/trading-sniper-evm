//! Reading a token, and naming a thing on chain.
//!
//! What is left of a much larger module: the sniper needs a selector, an event
//! topic, and the two facts about a pair token that decide how every amount in
//! a journal is printed - its decimals and its symbol. Both are fixed at
//! deployment, which is what makes caching them honest rather than convenient.

use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, Bytes, TransactionRequest, H256, U256};
use ethers::utils::keccak256;

/// topic0 of the given event signature.
pub fn event_topic(sig: &str) -> H256 {
    H256::from_slice(&keccak256(sig.as_bytes()))
}

pub fn selector(sig: &str) -> Bytes {
    let h = keccak256(sig.as_bytes());
    Bytes::from(h[..4].to_vec())
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
    let res: Bytes = crate::rpc::retrying("eth_call symbol()", || {
        let tx = tx.clone();
        async move {
            http.call(&tx.into(), None)
                .await
                .context("eth_call symbol()")
        }
    })
    .await?;
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

pub async fn call_address(provider: &Provider<Http>, to: Address, data: &Bytes) -> Result<Address> {
    let res: Bytes = crate::rpc::retrying("eth_call address", || {
        let tx = TransactionRequest::new().to(to).data(data.clone());
        async move {
            provider
                .call(&tx.into(), None)
                .await
                .context("eth_call address")
        }
    })
    .await?;
    anyhow::ensure!(res.len() >= 32, "short return for address call");
    Ok(Address::from_slice(&res[12..32]))
}

/// One `uint256` off a contract, for the calls that answer with a quantity.
pub async fn call_u256(
    provider: &Provider<Http>,
    to: Address,
    data: &Bytes,
) -> Result<ethers::types::U256> {
    let res: Bytes = crate::rpc::retrying("eth_call u256", || {
        let tx = TransactionRequest::new().to(to).data(data.clone());
        async move { provider.call(&tx.into(), None).await.context("eth_call u256") }
    })
    .await?;
    anyhow::ensure!(res.len() >= 32, "short return for u256 call");
    Ok(ethers::types::U256::from_big_endian(&res[..32]))
}

async fn call_u8(provider: &Provider<Http>, to: Address, data: &Bytes) -> Result<u8> {
    let res: Bytes = crate::rpc::retrying("eth_call u8", || {
        let tx = TransactionRequest::new().to(to).data(data.clone());
        async move { provider.call(&tx.into(), None).await.context("eth_call u8") }
    })
    .await?;
    anyhow::ensure!(!res.is_empty(), "empty return for u8 call");
    Ok(res[res.len() - 1])
}
