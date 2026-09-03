//! `probe` - how fast an endpoint takes a transaction, and how fast the chain
//! then includes it.
//!
//! Written because the bot's own logs put the whole detect-to-send budget in
//! one place: `quote_ms=0`, `took_ms=0`, `send_ms=350`. On a chain with ~100 ms
//! blocks that single round trip is three and a half blocks, which is the
//! difference between buying a dip and buying its recovery. This measures the
//! same round trip against any other endpoint - a sequencer, a public RPC -
//! without touching the bot or risking a swap.
//!
//! Three numbers per endpoint, because they answer different questions:
//!
//! * `ping` - a plain `eth_blockNumber`. Network distance, nothing else.
//! * `accept` - `eth_sendRawTransaction` returning. Distance PLUS whatever the
//!   endpoint does before admitting a transaction, which is the part a proxy in
//!   front of a node adds. This is what `send_ms` in the bot's log is made of.
//! * `blocks` - how many blocks passed between the head at send and the block
//!   the transaction landed in. The only figure here that does not depend on
//!   how fast a receipt can be polled for, and the one that decides whether a
//!   dip is still there.
//!
//! Standalone on purpose: it shares no code with the bot, so measuring cannot
//! change what trades.
//!
//! `--watch` is a different question with the same purpose: not how fast an
//! endpoint takes a transaction, but how late we hear about someone else's.
//! It listens to the Nitro sequencer feed and to the RPC's log subscription at
//! once and matches them by TRANSACTION HASH - the one key both streams agree
//! on, needing no assumption about how either numbers its messages. The
//! difference is how much earlier the feed knows about a swap, which is the
//! whole of what a faster signal would buy.
//!
//! Usage:
//!   probe [--endpoint URL]... [--read URL] [--rounds 5] [--send] [--config PATH]
//!   probe --watch [--feed WSS] [--ws WSS] [--address 0x..] [--seconds 60]
//!
//! Takes what it needs from the environment, the same names the bot itself
//! reads over its config file: `SUBMIT_URLS` (comma separated) for what to
//! measure, `HTTP_URL` for where to read the head block and the receipts, and
//! `PRIVATE_KEY` to sign with. A config file is consulted only for whatever
//! the environment and the flags left unanswered, and not having one is fine:
//! with `--endpoint` given, nothing else is needed to measure `ping`.
//!
//! Without `--send` nothing is signed and only `ping` is measured. With it,
//! each round sends one real transaction per endpoint, in turn, waiting for
//! each to land before the next - so no two ever hold the same nonce.
//!
//! DO NOT run with `--send` while the bot is running. Both sign with the same
//! key, and a nonce claimed twice costs the bot a buy.

use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider, StreamExt, Ws};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Eip1559TransactionRequest, Filter, H256, U256};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Only the fields this needs, all optional: the file is a last resort behind
/// the flags and the environment, and every field it might answer can equally
/// come from either. Everything else in it is ignored, so the probe does not
/// have to be kept in step with the bot's own config struct.
#[derive(Default, serde::Deserialize)]
struct Cfg {
    #[serde(default)]
    http_url: Option<String>,
    #[serde(default)]
    ws_url: Option<String>,
    #[serde(default)]
    pool_manager: Option<String>,
    #[serde(default)]
    submit_urls: Vec<String>,
    #[serde(default)]
    private_key: Option<String>,
}

/// A non-empty environment variable, trimmed - `config::env_var`'s rule, so
/// the probe answers to exactly the names the bot does.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// A self-send costs exactly this and cannot cost more: no call, no storage.
const SELF_SEND_GAS: u64 = 21_000;

/// How long to wait for a receipt before calling the round lost. Generous
/// against a ~100 ms block; anything near it is a stuck transaction rather than
/// a slow one, and the nonce would then block every round after it.
const INCLUSION_TIMEOUT: Duration = Duration::from_secs(30);

/// How often to ask for the receipt. The wait is reported for context only - it
/// can never be finer than one round trip to the reading endpoint, which is
/// exactly the quantity under measurement. `blocks` is the honest figure.
const POLL_EVERY: Duration = Duration::from_millis(25);

/// Scheme and host only. Endpoint URLs carry API keys in their path or query,
/// and a printed table is exactly where one must not appear.
fn label(url: &str) -> String {
    // Long enough for a hostname, short enough that the table keeps its
    // columns: a label that overruns its field pushes every number right, and
    // the header then stops describing what is under it.
    const WIDTH: usize = 44;
    let rest = url.split("://").nth(1).unwrap_or(url);
    let scheme = url.split("://").next().unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    let full = match (scheme.is_empty() || scheme == url, host.is_empty()) {
        (_, true) => "endpoint".to_string(),
        (true, false) => host.to_string(),
        (false, false) => format!("{scheme}://{host}"),
    };
    match full.len() > WIDTH {
        // Trimmed from the left: a host's own name is at the END of it, and
        // that is the part that tells two endpoints apart.
        true => format!("...{}", &full[full.len() - WIDTH + 3..]),
        false => full,
    }
}

struct Target {
    label: String,
    http: Provider<Http>,
    ping_ms: Vec<u128>,
    accept_ms: Vec<u128>,
    wait_ms: Vec<u128>,
    blocks: Vec<i64>,
    refused: usize,
    /// Why `ping` is empty, when it is. Kept rather than printed on the spot so
    /// one unreadable endpoint does not repeat itself once per round.
    ping_err: Option<String>,
}

fn stats(v: &[u128]) -> String {
    if v.is_empty() {
        return format!("{:^17}", "-");
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    format!("{:>5} {:>5} {:>5}", s[0], s[s.len() / 2], s[s.len() - 1])
}

fn block_stats(v: &[i64]) -> String {
    if v.is_empty() {
        return format!("{:>6}", "-");
    }
    format!("{:>6.1}", v.iter().sum::<i64>() as f64 / v.len() as f64)
}


/// A Nitro sequencer feed frame. Only the transactions are wanted; the rest of
/// the envelope - sequence numbers, L1 header, delayed-message counts - is
/// deliberately not modelled, because none of it is a key this can match on.
#[derive(serde::Deserialize)]
struct Frame {
    #[serde(default)]
    messages: Vec<Sequenced>,
}

#[derive(serde::Deserialize)]
struct Sequenced {
    message: Envelope,
}

#[derive(serde::Deserialize)]
struct Envelope {
    message: L2,
}

#[derive(serde::Deserialize)]
struct L2 {
    #[serde(rename = "l2Msg", default)]
    l2_msg: Option<String>,
}

/// Nitro's L2 message kinds, the two that carry transactions.
const L2_BATCH: u8 = 3;
const L2_SIGNED_TX: u8 = 4;

/// Every transaction hash inside one decoded `l2Msg`.
///
/// A signed transaction is its bytes after the kind byte, and its hash is the
/// keccak of exactly those bytes - the same hash the RPC will report for it,
/// which is what makes this comparable at all. A batch is a run of
/// length-prefixed sub-messages, each of which is one of these again.
///
/// `depth` exists because the format permits a batch inside a batch and this
/// reads bytes off the network: two levels is more than the sequencer produces,
/// and a cycle must not become a stack overflow.
fn tx_hashes(l2: &[u8], depth: u8, out: &mut Vec<H256>) {
    if depth > 2 {
        return;
    }
    match l2.first() {
        Some(&L2_SIGNED_TX) => out.push(H256::from(ethers::utils::keccak256(&l2[1..]))),
        Some(&L2_BATCH) => {
            let mut i = 1;
            while i + 8 <= l2.len() {
                let len = u64::from_be_bytes(l2[i..i + 8].try_into().unwrap()) as usize;
                i += 8;
                let Some(end) = i.checked_add(len).filter(|e| *e <= l2.len()) else {
                    return;
                };
                tx_hashes(&l2[i..end], depth + 1, out);
                i = end;
            }
        }
        _ => {}
    }
}

/// How much earlier the sequencer feed knows about a swap than the RPC does.
async fn watch(args: &[String], cfg: &Cfg) -> Result<()> {
    let value = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let seconds: u64 = value("--seconds")
        .map(|v| v.parse())
        .transpose()
        .context("--seconds is not a number")?
        .unwrap_or(60);
    let feed_url = value("--feed")
        .or_else(|| env_var("FEED_URL"))
        .context("no feed to listen to: pass --feed wss://... or set FEED_URL")?;
    let ws_url = value("--ws")
        .or_else(|| env_var("WS_URL"))
        .or_else(|| cfg.ws_url.clone())
        .context("no rpc websocket: pass --ws wss://... or set WS_URL")?;
    // Filtered to one contract on purpose. Unfiltered, the RPC would report
    // every log on the chain and the two streams would be compared on traffic
    // this bot never looks at; filtered, the matched set IS the set of swaps it
    // trades on, which is the population the answer is about.
    let address = value("--address").or_else(|| cfg.pool_manager.clone());
    let filter = match &address {
        Some(a) => Filter::new().address(a.parse::<ethers::types::Address>().context("--address")?),
        None => Filter::new(),
    };

    let started = Instant::now();
    let from_feed: Arc<Mutex<HashMap<H256, Duration>>> = Arc::new(Mutex::new(HashMap::new()));
    let from_rpc: Arc<Mutex<HashMap<H256, Duration>>> = Arc::new(Mutex::new(HashMap::new()));

    let feed_seen = Arc::clone(&from_feed);
    let feed_task = tokio::spawn(async move {
        let (stream, _) = tokio_tungstenite::connect_async(&feed_url)
            .await
            .context("connecting to the feed")?;
        println!("feed open");
        let (_w, mut r) = stream.split();
        let mut frames = 0u64;
        while let Some(msg) = r.next().await {
            let msg = msg.context("reading the feed")?;
            let text = match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => t,
                tokio_tungstenite::tungstenite::Message::Binary(b) => {
                    String::from_utf8_lossy(&b).into_owned()
                }
                _ => continue,
            };
            let at = started.elapsed();
            frames += 1;
            let Ok(frame) = serde_json::from_str::<Frame>(&text) else { continue };
            let mut hashes = Vec::new();
            for m in &frame.messages {
                let Some(b64) = &m.message.message.l2_msg else { continue };
                let Ok(raw) = base64_decode(b64) else { continue };
                tx_hashes(&raw, 0, &mut hashes);
            }
            let mut seen = feed_seen.lock().await;
            for h in hashes {
                // First sighting only: a transaction reannounced later is not
                // news, and letting it overwrite would make the feed look slower
                // than it is.
                seen.entry(h).or_insert(at);
            }
        }
        Ok::<u64, anyhow::Error>(frames)
    });

    let rpc_seen = Arc::clone(&from_rpc);
    let rpc_task = tokio::spawn(async move {
        let provider = Provider::<Ws>::connect(&ws_url).await.context("connecting to the rpc")?;
        println!("rpc open");
        let mut logs = provider.subscribe_logs(&filter).await.context("eth_subscribe(logs)")?;
        while let Some(log) = logs.next().await {
            let at = started.elapsed();
            if let Some(h) = log.transaction_hash {
                rpc_seen.lock().await.entry(h).or_insert(at);
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    println!(
        "watching for {seconds}s: feed against the rpc log stream{}",
        address.map(|a| format!(" for {a}")).unwrap_or_default()
    );
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    feed_task.abort();
    rpc_task.abort();

    let feed = from_feed.lock().await;
    let rpc = from_rpc.lock().await;
    let mut lead: Vec<i64> = rpc
        .iter()
        .filter_map(|(h, t_rpc)| {
            feed.get(h).map(|t_feed| t_rpc.as_millis() as i64 - t_feed.as_millis() as i64)
        })
        .collect();
    println!(
        "\nfeed saw {} transactions, the rpc reported {} matching logs, {} in both",
        feed.len(),
        rpc.len(),
        lead.len()
    );
    if lead.is_empty() {
        println!(
            "nothing to compare: no transaction appeared in both streams. Either the address \
             filter matches nothing, or the run was too short to catch a swap."
        );
        return Ok(());
    }
    lead.sort_unstable();
    let q = |p: f64| lead[((lead.len() as f64 * p) as usize).min(lead.len() - 1)];
    println!(
        "rpc behind the feed, ms:  min {}  p25 {}  median {}  p75 {}  max {}",
        lead[0],
        q(0.25),
        q(0.5),
        q(0.75),
        lead[lead.len() - 1]
    );
    let first = lead.iter().filter(|d| **d <= 0).count();
    println!(
        "the feed was first for {} of {} swaps ({:.0}%)",
        lead.len() - first,
        lead.len(),
        (lead.len() - first) as f64 * 100.0 / lead.len() as f64
    );
    println!(
        "\nthe median is the head start a feed-driven signal would have. Only the swaps both \
         streams saw are counted, so a transaction the rpc never reported cannot flatter it."
    );
    Ok(())
}

/// Standard base64, no padding assumptions beyond the usual. Written out rather
/// than pulled in because it is fifteen lines and the alternative is a
/// dependency in the tree for one field of one message.
fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("l2Msg is not base64")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let send = args.iter().any(|a| a == "--send");
    let rounds: usize = value("--rounds")
        .map(|v| v.parse())
        .transpose()
        .context("--rounds is not a number")?
        .unwrap_or(5);
    // Explicitly asked for, else the usual name, else nothing - a config file
    // is optional and a missing one is not an error. Only a file that was named
    // or that exists and cannot be read is.
    let named = value("--config");
    let cfg_path = named.clone().unwrap_or_else(|| "config.toml".to_string());
    let cfg: Cfg = match std::fs::read_to_string(&cfg_path) {
        Ok(raw) => toml::from_str(&raw).with_context(|| format!("parsing {cfg_path}"))?,
        Err(e) if named.is_none() && e.kind() == std::io::ErrorKind::NotFound => Cfg::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {cfg_path}")),
    };

    if args.iter().any(|a| a == "--watch") {
        return watch(&args, &cfg).await;
    }

    // Every `--endpoint` given, else what the bot itself broadcasts through -
    // so a bare run measures the status quo rather than nothing.
    let given: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--endpoint")
        .filter_map(|(i, _)| args.get(i + 1).cloned())
        .collect();
    let urls = match given.is_empty() {
        false => given,
        true => env_var("SUBMIT_URLS")
            .map(|v| v.split(',').map(|u| u.trim().to_string()).filter(|u| !u.is_empty()).collect())
            .unwrap_or(cfg.submit_urls),
    };
    anyhow::ensure!(
        !urls.is_empty(),
        "nothing to measure: pass --endpoint URL, or set SUBMIT_URLS"
    );

    // Reads - the head block, the receipts - all go to one endpoint whichever
    // endpoint is under test, so the comparison stays fair and only the
    // submission changes. The first endpoint measured is a fine reader when
    // nothing else says otherwise, which is what lets a bare `--endpoint` run
    // need no configuration at all.
    let read_url = value("--read")
        .or_else(|| env_var("HTTP_URL"))
        .or(cfg.http_url)
        .unwrap_or_else(|| urls[0].clone());
    let read = Provider::<Http>::try_from(read_url.clone()).context("bad read endpoint")?;
    let chain_id = read.get_chainid().await.context("eth_chainId")?.as_u64();

    let mut targets: Vec<Target> = Vec::with_capacity(urls.len());
    for url in &urls {
        targets.push(Target {
            label: label(url),
            http: Provider::<Http>::try_from(url.clone())
                .with_context(|| format!("bad endpoint '{}'", label(url)))?,
            ping_ms: Vec::new(),
            accept_ms: Vec::new(),
            wait_ms: Vec::new(),
            blocks: Vec::new(),
            refused: 0,
            ping_err: None,
        });
    }

    println!("chain {chain_id}, {} endpoint(s), {rounds} round(s)", targets.len());
    println!("reads via {}", label(&read_url));

    // One uncounted call each: the first request to a cold endpoint pays a TLS
    // handshake, and measuring that would say more about this process's age
    // than about the endpoint.
    for t in &targets {
        let _ = t.http.get_block_number().await;
    }

    for _ in 0..rounds {
        for t in targets.iter_mut() {
            let started = Instant::now();
            match t.http.get_block_number().await {
                Ok(_) => t.ping_ms.push(started.elapsed().as_millis()),
                // Said once rather than once a round. An endpoint may take
                // transactions and still refuse to be read - a sequencer often
                // serves little but `eth_sendRawTransaction` - and a bare dash
                // in the table would leave that looking like a bug in here.
                Err(e) => {
                    t.ping_err.get_or_insert(e.to_string());
                }
            }
        }
    }
    for t in &targets {
        if let Some(e) = &t.ping_err {
            println!("{} does not answer eth_blockNumber: {e}", t.label);
        }
    }

    if !send {
        report(&targets);
        println!(
            "\nonly `ping` was measured. Add --send to sign and submit real transactions \
             (0 value to self, {SELF_SEND_GAS} gas each) and get `accept` and `blocks` too."
        );
        return Ok(());
    }

    // The key, preferring the environment over the file, the same way the bot
    // does. Never printed - only the address it derives.
    let key = env_var("PRIVATE_KEY")
        .or(cfg.private_key)
        .context("no signing key: set PRIVATE_KEY")?;
    let wallet = LocalWallet::from_str(key.trim().trim_start_matches("0x"))
        .context("the signing key is not a valid secp256k1 key")?
        .with_chain_id(chain_id);
    let me = wallet.address();

    let (max_fee, tip) = read
        .estimate_eip1559_fees(None)
        .await
        .context("estimating the gas price")?;
    // One nonce read here and counted up locally. Every send below waits for
    // its own receipt first, so the chain and this counter never disagree.
    let mut nonce = read
        .get_transaction_count(me, Some(ethers::types::BlockNumber::Pending.into()))
        .await
        .context("eth_getTransactionCount")?;

    println!(
        "sending as {me:?}, from nonce {nonce}, {rounds} rounds x {} endpoints = {} transactions",
        targets.len(),
        rounds * targets.len()
    );
    println!("do not run this while the bot is running: one key, one nonce\n");

    for round in 1..=rounds {
        for i in 0..targets.len() {
            let req = Eip1559TransactionRequest::new()
                .from(me)
                .to(me)
                .value(U256::zero())
                .chain_id(chain_id)
                .nonce(nonce)
                .gas(U256::from(SELF_SEND_GAS))
                .max_fee_per_gas(max_fee)
                .max_priority_fee_per_gas(tip);
            let typed: TypedTransaction = req.into();
            let sig = wallet.sign_transaction(&typed).await.context("signing")?;
            let bytes = typed.rlp_signed(&sig);
            let hash = H256::from(ethers::utils::keccak256(&bytes));

            // Read the head BEFORE the clock starts, so neither this call nor
            // its round trip is charged to the endpoint under test.
            let head = read.get_block_number().await.context("eth_blockNumber")?.as_u64();

            let started = Instant::now();
            // Reduced to a plain outcome at once: the `PendingTransaction` this
            // returns borrows the provider, and holding it would keep every
            // target borrowed for the rest of the round. Nothing here wants it
            // - the receipt is polled for by hash, on the reading endpoint.
            let accepted = targets[i]
                .http
                .send_raw_transaction(bytes)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string());
            let accept_ms = started.elapsed().as_millis();

            if let Err(e) = &accepted {
                // "Already known" cannot happen here - every round signs a new
                // nonce - so any error is a refusal worth seeing in full.
                println!("round {round} {:>44}  REFUSED: {e}", targets[i].label);
                targets[i].refused += 1;
                continue;
            }
            targets[i].accept_ms.push(accept_ms);

            let waited = Instant::now();
            let mut receipt = None;
            while waited.elapsed() < INCLUSION_TIMEOUT {
                match read.get_transaction_receipt(hash).await {
                    Ok(Some(r)) => {
                        receipt = Some(r);
                        break;
                    }
                    _ => tokio::time::sleep(POLL_EVERY).await,
                }
            }
            let Some(r) = receipt else {
                println!(
                    "round {round} {:>44}  accept {accept_ms:>4} ms, then NO RECEIPT in {}s - \
                     stopping, the nonce is stuck",
                    targets[i].label,
                    INCLUSION_TIMEOUT.as_secs()
                );
                report(&targets);
                return Ok(());
            };

            let landed = r.block_number.map(|b| b.as_u64()).unwrap_or(head);
            let delta = landed as i64 - head as i64;
            let wait_ms = waited.elapsed().as_millis();
            targets[i].wait_ms.push(wait_ms);
            targets[i].blocks.push(delta);
            println!(
                "round {round} {:>44}  accept {accept_ms:>4} ms   landed +{delta} block(s) \
                 (head {head} -> {landed}), seen after {wait_ms:>4} ms",
                targets[i].label
            );
            nonce += U256::one();
        }
    }

    report(&targets);
    Ok(())
}

fn report(targets: &[Target]) {
    println!(
        "\n{:>44}  {:^17}  {:^17}  {:^17}  {:>6}  refused",
        "endpoint", "ping min/med/max", "accept min/med/max", "seen min/med/max", "blocks"
    );
    for t in targets {
        println!(
            "{:>44}  {}  {}  {}  {}  {:>7}",
            t.label,
            stats(&t.ping_ms),
            stats(&t.accept_ms),
            stats(&t.wait_ms),
            block_stats(&t.blocks),
            t.refused
        );
    }
    println!(
        "\nall times in ms. `blocks` is the mean distance from the head at send to the block it \
         landed in - the figure that decides whether a dip is still there, and the only one not \
         limited by how fast a receipt can be polled for. It is counted from the READING \
         endpoint's head, so a reader that lags the chain is inside it: point --read at the \
         endpoint under test to take that out."
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(subs: &[&[u8]]) -> Vec<u8> {
        let mut v = vec![L2_BATCH];
        for sub in subs {
            v.extend_from_slice(&(sub.len() as u64).to_be_bytes());
            v.extend_from_slice(sub);
        }
        v
    }

    fn signed(tx: &[u8]) -> Vec<u8> {
        let mut v = vec![L2_SIGNED_TX];
        v.extend_from_slice(tx);
        v
    }

    #[test]
    fn a_batch_yields_the_hash_of_every_transaction_in_it() {
        // Both shapes the feed actually carries: a typed 1559 transaction and a
        // legacy RLP one. The hash is the keccak of the transaction bytes with
        // the kind byte stripped, which is what the RPC will report it under.
        let typed: Vec<u8> = vec![0x02, 0xf8, 0x03, 0xaa, 0xbb];
        let legacy: Vec<u8> = vec![0xf9, 0x01, 0x02, 0x03];
        let mut out = Vec::new();
        tx_hashes(&batch(&[&signed(&typed), &signed(&legacy)]), 0, &mut out);
        assert_eq!(
            out,
            vec![
                H256::from(ethers::utils::keccak256(&typed)),
                H256::from(ethers::utils::keccak256(&legacy)),
            ]
        );
    }

    #[test]
    fn a_length_that_overruns_the_buffer_stops_rather_than_panicking() {
        let mut truncated = batch(&[&signed(&[0x02, 0xff])]);
        truncated.pop();
        let mut out = Vec::new();
        tx_hashes(&truncated, 0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn nesting_cannot_run_away() {
        // A batch whose only member is itself, which the format permits to
        // describe and this must not follow forever.
        let mut deep = signed(&[0x02, 0x01]);
        for _ in 0..8 {
            deep = batch(&[&deep]);
        }
        let mut out = Vec::new();
        tx_hashes(&deep, 0, &mut out);
        assert!(out.is_empty(), "a batch nested past the limit yields nothing");
    }
}
