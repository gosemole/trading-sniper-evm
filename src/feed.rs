//! Watching pools and saying what they did. Nothing here decides anything.
//!
//! One task per pool subscribes to its swaps and turns each log into a `Tick`.
//! A tick is a statement of fact - this pool, this block, this price - and it
//! carries everything the log carried, because the log is the freshest and
//! cheapest source of pool state there is: price, in-range liquidity, and on v4
//! the fee the swap was actually charged, hook override included.
//!
//! Every swap produces a tick, not just the interesting ones. What counts as
//! interesting is a question about strategy, and strategy is somewhere else:
//! buying wants a fall, selling wants a rise, and a feed that only reported
//! falls could serve one and not the other.

use crate::pool::{v3_swap_topic, v4_swap_topic, Pool};
use crate::price;
use crate::route::PoolRef;
use anyhow::{Context, Result};
use ethers::providers::{Middleware, Provider, Ws};
use ethers::types::{Filter, Log, U256};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// One swap, as the pool reported it.
#[derive(Debug, Clone)]
pub struct Tick {
    pub pool: PoolRef,
    pub block: u64,
    pub sqrt: U256,
    /// In-range liquidity after the swap.
    pub liquidity: u128,
    /// Fee actually charged, when the log says. v3 does not report it and does
    /// not need to: its fee is fixed at creation.
    pub lp_fee: Option<u32>,
    /// Price of the pool's base token, in units of the other one.
    pub price: f64,
}

/// Follow every pool over ONE websocket until the connection ends.
///
/// One socket, not one per pool. Each pool still gets its own subscription and
/// its own task - the decoding and the backpressure are unchanged - but they
/// share the connection underneath, because `Provider<Ws>` is a handle to it
/// and cloning hands out another handle rather than dialling again.
///
/// Endpoints commonly meter connections separately from requests, and the old
/// shape spent one per pool plus a fourth on the gas-price stream. Worse, a
/// dead endpoint produced a reconnect storm multiplied by the number of pools,
/// each with its own backoff, all hammering the thing that was already
/// refusing. Now the connection is retried once for all of them.
///
/// The cost is that they now fall together: whichever pool goes quiet first
/// takes the connection down and all of them come back together. That is close
/// to what happened anyway - sockets fail because the endpoint or the network
/// did, not one pool at a time - and the alternative was worse: letting the
/// others run on meant a refused subscription had no way back, because a
/// reconnect needed every pool to stop first.
pub async fn run_all(pools: Vec<Pool>, ws_url: String, out: mpsc::Sender<Tick>) -> Result<()> {
    let provider = Provider::<Ws>::connect(&ws_url).await.context("connect ws")?;
    info!(pools = pools.len(), "websocket connected");

    let mut names = Vec::with_capacity(pools.len());
    let mut following = Vec::with_capacity(pools.len());
    for pool in pools {
        names.push(pool.name.clone());
        // A clone shares the socket. Each task owns one so it can hold the
        // borrow its own subscription needs.
        let provider = provider.clone();
        let out = out.clone();
        following.push(tokio::spawn(async move { follow(&provider, pool, out).await }));
    }
    anyhow::ensure!(!following.is_empty(), "no pools to follow");

    // The FIRST to end, not the last. Waiting for all of them meant a pool
    // whose subscription was refused sat dead while the others ran happily on
    // - no reconnect, because a reconnect needed everyone to stop, and nothing
    // said the pool had gone quiet. One going dark now takes the connection
    // down and the caller brings all of them back together.
    let (ended, which, rest) = futures_util::future::select_all(following).await;
    let name = names.get(which).map(String::as_str).unwrap_or("?");
    for task in rest {
        task.abort();
    }
    drop(provider);

    match ended {
        Ok(Ok(())) => {
            warn!(pool = %name, "this pool's stream ended; reconnecting all of them");
            Ok(())
        }
        Ok(Err(e)) => Err(e).with_context(|| format!("following {name}")),
        Err(e) => Err(anyhow::anyhow!("{e}")).with_context(|| format!("following {name}")),
    }
}

/// One pool's subscription, over a connection somebody else owns.
async fn follow(provider: &Provider<Ws>, pool: Pool, out: mpsc::Sender<Tick>) -> Result<()> {
    let topic = match pool.version.as_str() {
        "v4" => v4_swap_topic(),
        _ => v3_swap_topic(),
    };
    let mut filter = Filter::new().address(pool.filter_addresses()).topic0(topic);
    // v4: the PoolManager emits swaps for every pool it holds, so the pool id
    // has to be part of the subscription or we would be sent the whole chain.
    if let Some(pid) = pool.pool_id {
        filter = filter.topic1(ethers::types::ValueOrArray::Value(pid));
    }
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .with_context(|| format!("subscribe logs for {}", pool.name))?;

    info!(
        pool = %pool.name, version = %pool.version, addr = ?pool.address,
        "subscribed"
    );

    while let Some(log) = stream.next().await {
        // A reorged-out log did not happen and must not be reported as if it did.
        if log.removed.unwrap_or(false) {
            debug!(pool = %pool.name, block = ?log.block_number, "skipping removed log");
            continue;
        }
        // Defensive: the subscription already filters on topic1 for v4.
        if let Some(pid) = pool.pool_id {
            if log.topics.get(1) != Some(&pid) {
                debug!(pool = %pool.name, "skipping log for another poolId");
                continue;
            }
        }
        let tick = match decode(&pool, &log) {
            Ok(t) => t,
            Err(e) => {
                warn!(pool = %pool.name, err = %e, "failed to decode swap log");
                continue;
            }
        };
        // Waits when the channel is full, which is backpressure rather than a
        // stall: the strategy hands its slow work to other tasks, so a full
        // channel means something is badly wrong rather than merely busy.
        if out.send(tick).await.is_err() {
            info!(pool = %pool.name, "nobody is listening any more");
            return Ok(());
        }
    }
    warn!(pool = %pool.name, "stream ended");
    Ok(())
}

fn decode(pool: &Pool, log: &Log) -> Result<Tick> {
    let block = log.block_number.context("log missing block number")?.as_u64();
    let data = &log.data.0;
    // Both v3 and v4 Swap lay out the non-indexed args as
    // [amount0][amount1][sqrtPriceX96][liquidity][tick]... so the offsets below
    // hold for either version. v4 adds one more word after the tick, and that
    // last word is the whole reason this log is worth more than a price: it is
    // the fee the swap was charged, hook override and all.
    anyhow::ensure!(data.len() >= 128, "log data too short: {} bytes", data.len());
    let sqrt = U256::from_big_endian(&data[64..96]);
    // liquidity is uint128: low 16 bytes of its 32-byte word.
    let liquidity = u128::from_be_bytes(data[112..128].try_into().unwrap());
    // fee is uint24: low 3 bytes of the sixth word.
    let lp_fee = (data.len() >= 192)
        .then(|| u32::from_be_bytes([0, data[189], data[190], data[191]]));
    let price = price::display_price(sqrt, pool.decimals0, pool.decimals1, pool.base_token);
    anyhow::ensure!(price.is_finite() && price > 0.0, "non-finite price from sqrt");
    Ok(Tick {
        pool: pool.pool_ref(),
        block,
        sqrt,
        liquidity,
        lp_fee,
        price,
    })
}
