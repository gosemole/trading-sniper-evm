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
//! Usage:
//!   probe [--endpoint URL]... [--read URL] [--rounds 5] [--send] [--config PATH]
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
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Eip1559TransactionRequest, H256, U256};
use std::str::FromStr;
use std::time::{Duration, Instant};

/// Only the fields this needs, all optional: the file is a last resort behind
/// the flags and the environment, and every field it might answer can equally
/// come from either. Everything else in it is ignored, so the probe does not
/// have to be kept in step with the bot's own config struct.
#[derive(Default, serde::Deserialize)]
struct Cfg {
    #[serde(default)]
    http_url: Option<String>,
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
    let rest = url.split("://").nth(1).unwrap_or(url);
    let scheme = url.split("://").next().unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    match (scheme.is_empty() || scheme == url, host.is_empty()) {
        (_, true) => "endpoint".to_string(),
        (true, false) => host.to_string(),
        (false, false) => format!("{scheme}://{host}"),
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
            if t.http.get_block_number().await.is_ok() {
                t.ping_ms.push(started.elapsed().as_millis());
            }
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
                println!("round {round} {:>28}  REFUSED: {e}", targets[i].label);
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
                    "round {round} {:>28}  accept {accept_ms:>4} ms, then NO RECEIPT in {}s - \
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
                "round {round} {:>28}  accept {accept_ms:>4} ms   landed +{delta} block(s) \
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
        "\n{:>28}  {:^17}  {:^17}  {:^17}  {:>6}  refused",
        "endpoint", "ping min/med/max", "accept min/med/max", "seen min/med/max", "blocks"
    );
    for t in targets {
        println!(
            "{:>28}  {}  {}  {}  {}  {:>7}",
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
         limited by how fast a receipt can be polled for."
    );
}
