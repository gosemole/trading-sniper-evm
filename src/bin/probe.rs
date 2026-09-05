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
//! Given `--ws` twice and no feed, `--watch` and `--heads` ask the same question
//! of two RPC websockets instead: which of two providers hears a swap, or a
//! block, first. Both sides are then read by the same code and stamped against
//! one `Instant` in one process - two runs against two endpoints would be
//! comparing their clocks rather than their endpoints.
//!
//! Usage:
//!   probe [--endpoint URL]... [--read URL] [--rounds 5] [--send] [--config PATH]
//!   probe --watch [--feed WSS] [--ws WSS] [--address 0x..] [--seconds 300] [--warmup 10]
//!   probe --watch --ws WSS --ws WSS [--address 0x..] [--seconds 300] [--warmup 10]
//!   probe --offset [--feed WSS] [--read URL] [--samples 10]
//!   probe --heads  [--feed WSS] [--ws WSS] [--seconds 300] [--warmup 10]
//!   probe --heads  --ws WSS --ws WSS [--seconds 300] [--warmup 10]
//!   probe --selftest --send [--ws WSS] [--endpoint URL] [--feed WSS] [--rounds 5]
//!   probe --extsload [--address 0x..] [--read URL]
//!
//! `--selftest` closes the loop the bot actually runs, end to end and in the
//! order it happens: a head arrives on the websocket, a transaction goes out
//! through the submit endpoint, the sequencer takes it, and the feed says when
//! it came back. Sending starts on the head rather than at an arbitrary moment
//! because that is when the bot sends, and a round trip begun mid-block would
//! flatter the part that waits for inclusion. It needs `--send`: every round is
//! a real signed transaction.
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
use futures_util::FutureExt;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{Eip1559TransactionRequest, Filter, H256, U256};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
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
    pools: Vec<PoolCfg>,
    #[serde(default)]
    submit_urls: Vec<String>,
    #[serde(default)]
    private_key: Option<String>,
}

/// Only what `--extsload` needs to find a slot that is not zero.
#[derive(Default, serde::Deserialize)]
struct PoolCfg {
    #[serde(default)]
    pool_id: Option<String>,
}

/// A non-empty environment variable, trimmed - `config::env_var`'s rule, so
/// the probe answers to exactly the names the bot does.
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Every value given for a repeated flag, in the order the flags appear.
///
/// A flag whose next argument is another flag contributes nothing: `--ws
/// --seconds 60` is a missing url, and taking `--seconds` for it would connect
/// to nonsense and report it as an endpoint.
fn values(args: &[String], name: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| *a == name)
        .filter_map(|(i, _)| args.get(i + 1))
        .filter(|v| !v.starts_with("--"))
        .cloned()
        .collect()
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
    /// The same distance, counted from the height the SEQUENCER had reached
    /// rather than the one the rpc admitted to. Empty unless a feed was given.
    blocks_from_feed: Vec<i64>,
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


/// Open the feed at the head rather than wherever its backlog begins.
///
/// A plain connection is handed about four and a half minutes of history and
/// streams it forward at several times real time before reaching the present.
/// Every measurement taken during that stretch is a measurement of the backlog,
/// and the first three attempts at timing this feed were ruined by it.
///
/// The broadcaster honours `Arbitrum-Requested-Sequence-Number`, and a number
/// past the end means "start at the head" rather than "wait for it" - so the
/// largest one there is asks for the present without having to know what the
/// present is. The equivalent query parameter is ignored; only the header
/// works. Tested against this chain, both facts.
async fn connect_feed(
    url: &str,
) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>
{
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().context("bad feed url")?;
    req.headers_mut().insert(
        "Arbitrum-Requested-Sequence-Number",
        u64::MAX.to_string().parse().expect("a number is a valid header value"),
    );
    let (stream, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("connecting to the feed")?;
    Ok(stream)
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
    #[serde(rename = "sequenceNumber", default)]
    sequence_number: u64,
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

/// Which two streams to compare: a feed against one websocket, or two
/// websockets against each other.
///
/// `--ws` twice is the whole switch. A feed asked for at the same time is a
/// contradiction rather than a third side and is refused, as is a third `--ws`:
/// silently keeping the first of them would measure something other than what
/// was asked for and say nothing about it.
fn sides(args: &[String], cfg: &Cfg) -> Result<(Option<String>, Vec<String>)> {
    let ws_urls = values(args, "--ws");
    anyhow::ensure!(ws_urls.len() <= 2, "at most two --ws: one per side of the comparison");
    if ws_urls.len() == 2 {
        anyhow::ensure!(
            !args.iter().any(|a| a == "--feed"),
            "pass either --feed with one --ws, or two --ws - not both"
        );
        if ws_urls[0] == ws_urls[1] {
            println!(
                "note: both --ws are the same endpoint, so this measures the spread between two \
                 connections to one host - a noise floor, not a difference between providers."
            );
        }
        return Ok((None, ws_urls));
    }
    let feed = args
        .iter()
        .position(|a| a == "--feed")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| env_var("FEED_URL"))
        .context(
            "nothing to compare against: pass --feed wss://... or set FEED_URL, or give --ws \
             twice to compare two websockets"
        )?;
    let ws = ws_urls
        .into_iter()
        .next()
        .or_else(|| env_var("WS_URL"))
        .or_else(|| cfg.ws_url.clone())
        .context("no rpc websocket: pass --ws wss://... or set WS_URL")?;
    Ok((Some(feed), vec![ws]))
}

/// A name for each side of a two-websocket run. `label` is host only, so two
/// connections to one host would otherwise print the same name twice and the
/// summary would not say which line is which.
fn two_labels(urls: &[String]) -> (String, String) {
    let (a, b) = (label(&urls[0]), label(&urls[1]));
    match a == b {
        true => (format!("{a} #1"), format!("{b} #2")),
        false => (a, b),
    }
}

/// Every transaction hash the sequencer feed announces: when it arrived, and
/// the sequence number of the message that carried it - which `--offset`
/// established is the block it belongs to, so `--selftest` can say which block
/// took a transaction without asking anyone.
///
/// Reading and decoding are separate tasks on purpose, and this is the whole
/// correctness of the measurement. The feed carries about a thousand
/// transactions a second, each needing base64, a JSON walk and a keccak; a
/// loop that decodes before returning to `next()` leaves the following frame
/// sitting in the socket until it is done, and stamps it with the time it
/// got round to it rather than the time it arrived. The backlog compounds,
/// and the feed ends up looking seconds SLOWER than a stream it is in fact
/// ahead of. So the reader does nothing but stamp and hand off.
fn feed_hashes(
    url: String,
    started: Instant,
    seen: Arc<Mutex<HashMap<H256, (Duration, u64)>>>,
) -> (tokio::task::JoinHandle<Result<()>>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Duration, String)>();
    let reader = tokio::spawn(async move {
        let stream = connect_feed(&url).await?;
        println!("feed open");
        let (_w, mut r) = stream.split();
        while let Some(msg) = r.next().await {
            let msg = msg.context("reading the feed")?;
            let at = started.elapsed();
            let text = match msg {
                tokio_tungstenite::tungstenite::Message::Text(t) => t,
                tokio_tungstenite::tungstenite::Message::Binary(b) => {
                    String::from_utf8_lossy(&b).into_owned()
                }
                _ => continue,
            };
            if tx.send((at, text)).is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });
    let decoder = tokio::spawn(async move {
        while let Some((at, text)) = rx.recv().await {
            let Ok(frame) = serde_json::from_str::<Frame>(&text) else { continue };
            let mut hashes = Vec::new();
            let mut of_message = Vec::new();
            for m in &frame.messages {
                let Some(b64) = &m.message.message.l2_msg else { continue };
                let Ok(raw) = base64_decode(b64) else { continue };
                of_message.clear();
                tx_hashes(&raw, 0, &mut of_message);
                hashes.extend(of_message.iter().map(|h| (*h, m.sequence_number)));
            }
            let mut seen = seen.lock().await;
            for (h, seq) in hashes {
                // First sighting only: the feed replays recent history when a
                // connection opens, and a transaction announced twice is not
                // news the second time.
                seen.entry(h).or_insert((at, seq));
            }
        }
    });
    (reader, decoder)
}

/// Every transaction hash one websocket's log subscription reports, stamped on
/// arrival. Both sides of a two-websocket run go through this same function, so
/// a difference between them is a difference between the endpoints rather than
/// between two ways of reading one.
fn ws_hashes(
    url: String,
    filter: Filter,
    started: Instant,
    seen: Arc<Mutex<HashMap<H256, (Duration, u64)>>>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let provider = Provider::<Ws>::connect(&url).await.context("connecting to the rpc")?;
        println!("{} open", label(&url));
        let mut logs = provider.subscribe_logs(&filter).await.context("eth_subscribe(logs)")?;
        while let Some(log) = logs.next().await {
            let at = started.elapsed();
            if let Some(h) = log.transaction_hash {
                let block = log.block_number.map(|b| b.as_u64()).unwrap_or_default();
                seen.lock().await.entry(h).or_insert((at, block));
            }
        }
        Ok(())
    })
}

/// End the run, and fail if a side ended it first.
///
/// A task that has already finished did not finish because the run is over: it
/// is a websocket that could not connect, or a stream that dropped. Its map is
/// then empty or short, and reporting that as "the other side was faster" is
/// the worst answer this tool could give - so the error is raised instead of
/// the numbers. A task still running is simply stopped.
async fn settle(tasks: Vec<tokio::task::JoinHandle<Result<()>>>) -> Result<()> {
    for t in tasks {
        if !t.is_finished() {
            t.abort();
            continue;
        }
        match t.await {
            Ok(inner) => inner?,
            Err(e) => anyhow::bail!("a stream task ended early: {e}"),
        }
    }
    Ok(())
}

/// How much earlier one stream knows about a swap than the other does: the
/// sequencer feed against an rpc's log subscription, or - with `--ws` twice -
/// one rpc against another.
async fn watch(args: &[String], cfg: &Cfg) -> Result<()> {
    let value = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let seconds: u64 = value("--seconds")
        .map(|v| v.parse())
        .transpose()
        .context("--seconds is not a number")?
        .unwrap_or(300);
    // Small now that `connect_feed` asks for the head: what is left to settle is
    // a connection warming up, not four minutes of history being replayed.
    let warmup: u64 = value("--warmup")
        .map(|v| v.parse())
        .transpose()
        .context("--warmup is not a number")?
        .unwrap_or(10);
    anyhow::ensure!(warmup < seconds, "--warmup must be shorter than --seconds");
    let (feed_url, ws_urls) = sides(args, cfg)?;
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
    let from_a: Arc<Mutex<HashMap<H256, (Duration, u64)>>> = Arc::new(Mutex::new(HashMap::new()));
    let from_b: Arc<Mutex<HashMap<H256, (Duration, u64)>>> = Arc::new(Mutex::new(HashMap::new()));

    // `a` is the side a positive number says was first, and both sides are
    // spawned before either is awaited: a connection opened after the other has
    // been streaming for a second would be behind by that second.
    let mut decoder = None;
    let mut tasks = Vec::new();
    let (first, second) = match &feed_url {
        Some(url) => {
            let (reader, decode) = feed_hashes(url.clone(), started, Arc::clone(&from_a));
            decoder = Some(decode);
            tasks.push(reader);
            tasks.push(ws_hashes(ws_urls[0].clone(), filter, started, Arc::clone(&from_b)));
            ("feed".to_string(), "rpc".to_string())
        }
        None => {
            let f = filter.clone();
            tasks.push(ws_hashes(ws_urls[0].clone(), f, started, Arc::clone(&from_a)));
            tasks.push(ws_hashes(ws_urls[1].clone(), filter, started, Arc::clone(&from_b)));
            two_labels(&ws_urls)
        }
    };

    println!(
        "watching for {seconds}s, ignoring the first {warmup}s while both connections settle: \
         {first} against {second}{}",
        address.map(|a| format!(", logs for {a}")).unwrap_or_default()
    );
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    settle(tasks).await?;
    if let Some(decoder) = decoder {
        decoder.abort();
    }

    let a = from_a.lock().await;
    let b = from_b.lock().await;
    let after = Duration::from_secs(warmup);
    // Kept in arrival order first, because a measurement that drifts is a
    // measurement that is wrong, and the only way to see drift is to look at
    // when each comparison was made rather than at the sorted middle of them.
    let mut over_time: Vec<(Duration, i64)> = b
        .iter()
        .filter(|(_, (t_b, _))| *t_b >= after)
        .filter_map(|(h, (t_b, _))| {
            a.get(h).map(|(t_a, _)| (*t_b, t_b.as_millis() as i64 - t_a.as_millis() as i64))
        })
        .collect();
    over_time.sort_by_key(|(t, _)| *t);
    let mut lead: Vec<i64> = over_time.iter().map(|(_, d)| *d).collect();
    println!(
        "\n{first} saw {} transactions, {second} saw {}, {} in both after the warmup",
        a.len(),
        b.len(),
        lead.len()
    );
    if lead.is_empty() {
        println!(
            "nothing to compare: no transaction appeared in both streams. Either the address \
             filter matches nothing, or the run was too short to catch a swap."
        );
        return Ok(());
    }
    report_lead(&mut lead, &mut over_time, "swaps", &first, &second);
    match feed_url.is_some() {
        true => println!(
            "\nthe median is the head start a feed-driven signal would have. Only the swaps both \
             streams saw are counted, so a transaction the rpc never reported cannot flatter it."
        ),
        false => println!(
            "\nthe median is how much later {second} reports a swap than {first}. Only the swaps \
             both endpoints reported are counted, so one that dropped a log cannot flatter itself."
        ),
    }
    Ok(())
}

/// The same summary for either comparison: how far behind the second side was,
/// whether the first won, and - the part that decides whether any of it may be
/// believed - whether the answer had stopped moving by the end of the run.
fn report_lead(
    lead: &mut [i64],
    over_time: &mut [(Duration, i64)],
    unit: &str,
    first: &str,
    second: &str,
) {
    lead.sort_unstable();
    let q = |p: f64| lead[((lead.len() as f64 * p) as usize).min(lead.len() - 1)];
    println!(
        "{second} behind {first}, ms:  min {}  p25 {}  median {}  p75 {}  max {}",
        lead[0],
        q(0.25),
        q(0.5),
        q(0.75),
        lead[lead.len() - 1]
    );
    let behind = lead.iter().filter(|d| **d <= 0).count();
    println!(
        "{first} was first for {} of {} {unit} ({:.0}%)",
        lead.len() - behind,
        lead.len(),
        (lead.len() - behind) as f64 * 100.0 / lead.len() as f64
    );
    over_time.sort_by_key(|(t, _)| *t);
    let median_of = |v: &[(Duration, i64)]| -> i64 {
        let mut d: Vec<i64> = v.iter().map(|(_, x)| *x).collect();
        d.sort_unstable();
        d.get(d.len() / 2).copied().unwrap_or(0)
    };
    // Five slices of the counted window rather than a single before-and-after:
    // the question is not only whether the number moved but whether it has
    // stopped moving, and a curve that is still bending says the run was too
    // short however tidy its median looks.
    let slice = over_time.len() / 5;
    if slice == 0 {
        return;
    }
    let by_fifth: Vec<i64> =
        (0..5).map(|i| median_of(&over_time[i * slice..(i + 1) * slice])).collect();
    let as_text: Vec<String> = by_fifth.iter().map(|d| format!("{d}")).collect();
    println!("median by fifth of the counted window, ms:  {}", as_text.join("  "));
    let spread = by_fifth.iter().max().unwrap() - by_fifth.iter().min().unwrap();
    if spread > 200 {
        println!(
            "  ^ these should all be the same. A spread of {spread} ms means one side is still \
             catching up and is being timed on when it was PROCESSED rather than when it \
             arrived - run longer, or raise --warmup, and do not read the numbers above as \
             latency yet."
        );
    }
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

/// What the feed's `sequenceNumber` is, in block numbers.
///
/// Nitro turns each sequencer message into one L2 block, so the two should
/// differ by a constant - but "should" is not "does", and reading a height off
/// the feed is only safe once that constant is known rather than assumed. This
/// establishes it the only way that cannot be fooled: take a transaction out of
/// a feed message, ask the RPC which block it ended up in, and subtract.
///
/// It works while the feed is still catching up, which is most of the first
/// minute after connecting: the mapping from a message to its block is a fact
/// about the chain, not about when we happened to read it.
async fn offset(args: &[String], cfg: &Cfg) -> Result<()> {
    let value = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let samples: usize = value("--samples")
        .map(|v| v.parse())
        .transpose()
        .context("--samples is not a number")?
        .unwrap_or(10);
    let feed_url = value("--feed")
        .or_else(|| env_var("FEED_URL"))
        .context("no feed to read: pass --feed wss://... or set FEED_URL")?;
    let read_url = value("--read")
        .or_else(|| env_var("HTTP_URL"))
        .or_else(|| cfg.http_url.clone())
        .context("nowhere to ask: pass --read URL or set HTTP_URL")?;
    let read = Provider::<Http>::try_from(read_url.clone()).context("bad read endpoint")?;

    let stream = connect_feed(&feed_url).await?;
    println!("feed open, reads via {}\n", label(&read_url));
    let (_w, mut r) = stream.split();

    println!("{:>14}  {:>14}  {:>10}", "sequenceNumber", "block", "difference");
    let mut offsets: Vec<i128> = Vec::new();
    while let Some(msg) = r.next().await {
        let text = match msg.context("reading the feed")? {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Binary(b) => {
                String::from_utf8_lossy(&b).into_owned()
            }
            _ => continue,
        };
        let Ok(frame) = serde_json::from_str::<Frame>(&text) else { continue };
        for m in &frame.messages {
            if offsets.len() >= samples {
                break;
            }
            let Some(b64) = &m.message.message.l2_msg else { continue };
            let Ok(raw) = base64_decode(b64) else { continue };
            let mut hashes = Vec::new();
            tx_hashes(&raw, 0, &mut hashes);
            // One transaction is enough, but not any one: a message caught
            // before its block was executed answers `null`, and that is a
            // question asked too early rather than a mismatch. Try the ones in
            // it until the chain knows about one.
            for h in hashes {
                match read.get_transaction(h).await {
                    Ok(Some(tx)) => {
                        let Some(block) = tx.block_number.map(|b| b.as_u64()) else { continue };
                        let d = m.sequence_number as i128 - block as i128;
                        println!("{:>14}  {:>14}  {:>10}", m.sequence_number, block, d);
                        offsets.push(d);
                        break;
                    }
                    _ => continue,
                }
            }
        }
        if offsets.len() >= samples {
            break;
        }
    }

    if offsets.is_empty() {
        println!("\nnothing resolved: no transaction from the feed was known to the rpc");
        return Ok(());
    }
    let first = offsets[0];
    let steady = offsets.iter().all(|d| *d == first);
    println!(
        "\n{} samples, difference {}",
        offsets.len(),
        match steady {
            true => format!("constant at {first} - a feed height is a block height minus {first}"),
            false => format!(
                "NOT constant ({} to {}) - the two do not track one another, and a height read \
                 off the feed cannot be trusted",
                offsets.iter().min().unwrap(),
                offsets.iter().max().unwrap()
            ),
        }
    );
    Ok(())
}

/// Every block height the sequencer feed announces, stamped on arrival. No
/// decoding: `--offset` established that a feed message's sequence number IS the
/// block number, so the transactions in the frame are left alone.
fn feed_heights(
    url: String,
    started: Instant,
    seen: Arc<Mutex<HashMap<u64, Duration>>>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let stream = connect_feed(&url).await?;
        println!("feed open");
        let (_w, mut r) = stream.split();
        while let Some(msg) = r.next().await {
            let at = started.elapsed();
            let text = match msg.context("reading the feed")? {
                tokio_tungstenite::tungstenite::Message::Text(t) => t,
                tokio_tungstenite::tungstenite::Message::Binary(b) => {
                    String::from_utf8_lossy(&b).into_owned()
                }
                _ => continue,
            };
            let Ok(frame) = serde_json::from_str::<Frame>(&text) else { continue };
            let mut seen = seen.lock().await;
            for m in &frame.messages {
                seen.entry(m.sequence_number).or_insert(at);
            }
        }
        Ok(())
    })
}

/// Every block height one websocket announces on `newHeads`, stamped on
/// arrival. As with `ws_hashes`, both sides of a two-websocket run share it.
fn ws_heights(
    url: String,
    started: Instant,
    seen: Arc<Mutex<HashMap<u64, Duration>>>,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        let provider = Provider::<Ws>::connect(&url).await.context("connecting to the rpc")?;
        println!("{} open", label(&url));
        let mut heads = provider.subscribe_blocks().await.context("eth_subscribe(newHeads)")?;
        while let Some(head) = heads.next().await {
            let at = started.elapsed();
            if let Some(n) = head.number {
                seen.lock().await.entry(n.as_u64()).or_insert(at);
            }
        }
        Ok(())
    })
}

/// Who says "block N exists" first: the sequencer feed or the RPC's `newHeads`
/// - or, with `--ws` given twice, which of two RPCs.
///
/// Both are pushed, so this costs no requests at all - and needs no decoding
/// either, because `--offset` established that a feed message's sequence number
/// IS the block number. Only the number is read out of each frame; the
/// transactions in it are left alone.
///
/// This is the honest version of the question the fee watcher raises implicitly:
/// a subscription is cheaper than asking, but only worth having if it is not
/// also later.
async fn heads(args: &[String], cfg: &Cfg) -> Result<()> {
    let value = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let seconds: u64 = value("--seconds")
        .map(|v| v.parse())
        .transpose()
        .context("--seconds is not a number")?
        .unwrap_or(300);
    // Small now that `connect_feed` asks for the head: what is left to settle is
    // a connection warming up, not four minutes of history being replayed.
    let warmup: u64 = value("--warmup")
        .map(|v| v.parse())
        .transpose()
        .context("--warmup is not a number")?
        .unwrap_or(10);
    anyhow::ensure!(warmup < seconds, "--warmup must be shorter than --seconds");
    let (feed_url, ws_urls) = sides(args, cfg)?;

    let started = Instant::now();
    let from_a: Arc<Mutex<HashMap<u64, Duration>>> = Arc::new(Mutex::new(HashMap::new()));
    let from_b: Arc<Mutex<HashMap<u64, Duration>>> = Arc::new(Mutex::new(HashMap::new()));

    let mut tasks = Vec::new();
    let (first, second) = match &feed_url {
        Some(url) => {
            tasks.push(feed_heights(url.clone(), started, Arc::clone(&from_a)));
            tasks.push(ws_heights(ws_urls[0].clone(), started, Arc::clone(&from_b)));
            ("feed".to_string(), "rpc".to_string())
        }
        None => {
            tasks.push(ws_heights(ws_urls[0].clone(), started, Arc::clone(&from_a)));
            tasks.push(ws_heights(ws_urls[1].clone(), started, Arc::clone(&from_b)));
            two_labels(&ws_urls)
        }
    };

    println!(
        "watching for {seconds}s, ignoring the first {warmup}s while both connections settle: \
         {first} against {second}"
    );
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    settle(tasks).await?;

    let a = from_a.lock().await;
    let b = from_b.lock().await;
    let after = Duration::from_secs(warmup);
    let mut over_time: Vec<(Duration, i64)> = b
        .iter()
        .filter(|(_, t)| **t >= after)
        .filter_map(|(n, t_b)| {
            a.get(n).map(|t_a| (*t_b, t_b.as_millis() as i64 - t_a.as_millis() as i64))
        })
        .collect();
    over_time.sort_by_key(|(t, _)| *t);
    let mut lead: Vec<i64> = over_time.iter().map(|(_, d)| *d).collect();
    println!(
        "\n{first} announced {} blocks, {second} {}, {} counted after the warmup",
        a.len(),
        b.len(),
        lead.len()
    );
    // The heights each stream ended on. A difference here is the same story the
    // milliseconds tell, in the unit that matters for trading.
    if let (Some(x), Some(y)) = (a.keys().max(), b.keys().max()) {
        println!(
            "last height: {first} {x}, {second} {y} ({first} ahead by {})",
            *x as i64 - *y as i64
        );
    }
    if lead.is_empty() {
        println!("nothing to compare: no block was announced by both within the counted window");
        return Ok(());
    }
    report_lead(&mut lead, &mut over_time, "blocks", &first, &second);
    println!(
        "\nboth sides are pushed, so this cost no requests. A positive median is how much \
         earlier {first} knows a block exists than {second} admits it."
    );
    Ok(())
}

/// Everything `--selftest` needs, gathered by `main` from the same flags,
/// config and key that `--send` already reads.
struct SelfTest {
    ws_url: String,
    feed_url: Option<String>,
    submit: Provider<Http>,
    submit_label: String,
    read: Provider<Http>,
    read_label: String,
    wallet: LocalWallet,
    chain_id: u64,
    max_fee: U256,
    tip: U256,
    nonce: U256,
    rounds: usize,
}

/// The bot's own loop, measured end to end: head -> send -> sequencer -> seen.
///
/// Three numbers per round, and they divide the budget between the parts that
/// can be fixed separately:
///
/// * `send` - from the head landing on the websocket to `eth_sendRawTransaction`
///   returning. This is the endpoint's, and it is what `--endpoint` changes.
/// * `seen` - from the head to the transaction coming back. With a feed this is
///   a pushed sighting, so it is a real latency; without one it is polled
///   `eth_getTransactionByHash` and can never be finer than a round trip to the
///   reading endpoint, which is why the feed is the honest version.
/// * `+blocks` - how many blocks passed between the head that triggered the
///   round and the block that took the transaction. The one figure no polling
///   interval can distort, and the one that says whether a dip is still there.
///
/// One transaction is in flight at a time and each round waits for its own to
/// come back, so the local nonce and the chain never disagree.
async fn selftest(t: SelfTest) -> Result<()> {
    let started = Instant::now();
    let seen: Arc<Mutex<HashMap<H256, (Duration, u64)>>> = Arc::new(Mutex::new(HashMap::new()));
    let feed_tasks =
        t.feed_url.as_ref().map(|url| feed_hashes(url.clone(), started, Arc::clone(&seen)));

    // Subscribed before the first send, so no round is triggered by a head that
    // arrived while this was still connecting.
    let provider =
        Provider::<Ws>::connect(&t.ws_url).await.context("connecting to the rpc websocket")?;
    let mut heads = provider.subscribe_blocks().await.context("eth_subscribe(newHeads)")?;
    println!(
        "heads from {}, sending through {}, sightings {}",
        label(&t.ws_url),
        t.submit_label,
        match t.feed_url.is_some() {
            true => "from the feed".to_string(),
            false => format!("polled from {} - no --feed given", t.read_label),
        }
    );

    // One uncounted call, the same warm-up the endpoint table does: the first
    // request to a cold endpoint pays a TLS handshake, and round one would
    // otherwise report it as latency. An endpoint that refuses to be read has
    // still opened the connection by the time it says so.
    let _ = t.submit.get_block_number().await;

    let mut nonce = t.nonce;
    let mut send_ms: Vec<i64> = Vec::new();
    let mut seen_ms: Vec<i64> = Vec::new();
    let mut blocks: Vec<i64> = Vec::new();
    let mut from_tip: Vec<i64> = Vec::new();
    for round in 1..=t.rounds {
        // Whatever piled up while the last round was waiting is history, and
        // dropped unread. A round triggered by a buffered head would send
        // against a block the chain has already left behind, and would be
        // stamped with the time it was READ rather than the time it arrived -
        // both of which make the loop look later the longer the run goes on.
        while heads.next().now_or_never().flatten().is_some() {}
        let Some(head) = heads.next().await else {
            anyhow::bail!("the head stream ended after {} round(s)", round - 1);
        };
        let at_head = Instant::now();
        let height = head.number.map(|n| n.as_u64()).unwrap_or_default();
        // Where the SEQUENCER was when this round began, which is not where the
        // rpc says the head is: a block has to be executed and indexed before
        // `newHeads` mentions it, and the sequencer is already past it by then.
        // Counted from the rpc head alone, a transaction that was next in line
        // still looks several blocks late.
        let tip = match t.feed_url.is_some() {
            true => seen.lock().await.values().map(|(_, seq)| *seq).max().unwrap_or_default(),
            false => 0,
        };

        let req = Eip1559TransactionRequest::new()
            .from(t.wallet.address())
            .to(t.wallet.address())
            .value(U256::zero())
            .chain_id(t.chain_id)
            .nonce(nonce)
            .gas(U256::from(SELF_SEND_GAS))
            .max_fee_per_gas(t.max_fee)
            .max_priority_fee_per_gas(t.tip);
        let typed: TypedTransaction = req.into();
        let sig = t.wallet.sign_transaction(&typed).await.context("signing")?;
        let bytes = typed.rlp_signed(&sig);
        let hash = H256::from(ethers::utils::keccak256(&bytes));

        if let Err(e) = t.submit.send_raw_transaction(bytes).await {
            // Signing is local and the nonce is fresh every round, so a refusal
            // is the endpoint's answer and worth stopping on rather than
            // averaging over.
            println!("round {round} head {height}  REFUSED by {}: {e}", t.submit_label);
            break;
        }
        let sent = at_head.elapsed();

        // Waiting on the feed costs nothing: the sighting is already in memory
        // and this only looks. The polled fallback is the one that talks.
        let mut landed = None;
        let waited = Instant::now();
        while waited.elapsed() < INCLUSION_TIMEOUT {
            match t.feed_url.is_some() {
                true => {
                    if let Some((at, seq)) = seen.lock().await.get(&hash).copied() {
                        landed = Some(((started + at).saturating_duration_since(at_head), seq));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                false => {
                    if let Ok(Some(tx)) = t.read.get_transaction(hash).await {
                        if let Some(b) = tx.block_number {
                            landed = Some((at_head.elapsed(), b.as_u64()));
                            break;
                        }
                    }
                    tokio::time::sleep(POLL_EVERY).await;
                }
            }
        }
        let Some((back, block)) = landed else {
            println!(
                "round {round} head {height}  send {:>4} ms, then NOT SEEN in {}s - stopping, \
                 the nonce is stuck",
                sent.as_millis(),
                INCLUSION_TIMEOUT.as_secs()
            );
            break;
        };

        let delta = block as i64 - height as i64;
        // A zero tip is "the feed has said nothing yet", not "block zero".
        let behind_tip = (tip != 0).then(|| block as i64 - tip as i64);
        send_ms.push(sent.as_millis() as i64);
        seen_ms.push(back.as_millis() as i64);
        blocks.push(delta);
        if let Some(d) = behind_tip {
            from_tip.push(d);
        }
        println!(
            "round {round} head {height}  send {:>4} ms   seen {:>4} ms   in block {block} \
             (+{delta} from the rpc head{})",
            sent.as_millis(),
            back.as_millis(),
            behind_tip.map(|d| format!(", +{d} from the sequencer at {tip}")).unwrap_or_default()
        );
        nonce += U256::one();
    }

    if let Some((reader, decoder)) = feed_tasks {
        reader.abort();
        decoder.abort();
    }
    if send_ms.is_empty() {
        println!("\nnothing completed a round");
        return Ok(());
    }
    let median = |v: &mut Vec<i64>| -> i64 {
        v.sort_unstable();
        v[v.len() / 2]
    };
    println!(
        "\n{} round(s): median head->send {} ms, head->seen {} ms, {} blocks late from the rpc \
         head{}",
        send_ms.len(),
        median(&mut send_ms),
        median(&mut seen_ms),
        median(&mut blocks),
        match from_tip.is_empty() {
            true => String::new(),
            false => format!(", {} from the sequencer", median(&mut from_tip)),
        }
    );
    println!(
        "head->seen is the whole loop the bot lives in. The count from the SEQUENCER is the \
         honest one: the rpc head is a block already executed and indexed, so counting from it \
         charges this loop for blocks it was never in a position to reach."
    );
    Ok(())
}

/// Does this PoolManager expose v4's batch storage read, and does it agree with
/// `eth_getStorageAt`?
///
/// It matters because every tick walk here reads storage one slot at a time -
/// up to thirty-two bitmap words plus a read per crossed tick, all sequential
/// round trips. `extsload(bytes32[])` would collapse that into two calls. A
/// generic Multicall would not: these are raw storage reads, not contract
/// calls, and only the manager itself can serve them in bulk.
///
/// The router here is a fork, so this is asked rather than assumed. Both halves
/// are checked: that the call answers at all, and that what it answers matches
/// what the node reports for the same slot - a method that exists but reads
/// something else would be worse than one that is missing.
async fn extsload(args: &[String], cfg: &Cfg) -> Result<()> {
    let value = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };
    let read_url = value("--read")
        .or_else(|| env_var("HTTP_URL"))
        .or_else(|| cfg.http_url.clone())
        .context("nowhere to ask: pass --read URL or set HTTP_URL")?;
    let manager: ethers::types::Address = value("--address")
        .or_else(|| cfg.pool_manager.clone())
        .context("no pool manager: pass --address 0x... or set pool_manager in the config")?
        .parse()
        .context("--address is not an address")?;
    let read = Provider::<Http>::try_from(read_url.clone()).context("bad read endpoint")?;
    // Every read here is pinned to one block, and this is not a detail. A
    // pool's slot0 is rewritten by every swap, and this chain produces ten
    // blocks a second: two unpinned requests land on different blocks and
    // disagree about a value that was never wrong. Two behind the head, because
    // a block the node has just announced is not always readable yet.
    let at = read
        .get_block_number()
        .await
        .context("eth_blockNumber")?
        .as_u64()
        .saturating_sub(2);
    println!("asking {manager:?} via {}, all reads at block {at}\n", label(&read_url));

    // Slots of a real pool, not of the manager's own header. A pool's `slot0`
    // and `liquidity` are non-zero on a live pool, and that is the point: two
    // zero slots would be matched by a method that answers zero to everything,
    // which is exactly the failure this check exists to catch.
    //
    // `_pools[poolId]` lives at keccak(poolId . POOLS_SLOT), with liquidity
    // three words in - the same arithmetic `depth::TickReader` uses.
    const POOLS_SLOT: u64 = 6;
    let pool_id: H256 = cfg
        .pools
        .iter()
        .find_map(|p| p.pool_id.as_ref())
        .context("no pool_id in the config to test against - add one, or test another way")?
        .parse()
        .context("pool_id is not a 32-byte hash")?;
    let mut preimage = [0u8; 64];
    preimage[..32].copy_from_slice(&pool_id.0);
    U256::from(POOLS_SLOT).to_big_endian(&mut preimage[32..]);
    let base = U256::from_big_endian(&ethers::utils::keccak256(preimage));
    let slots: [U256; 2] = [base, base + U256::from(3)];
    println!("testing against pool {pool_id:?}, slot0 and liquidity\n");
    let mut truth = Vec::new();
    for slot in slots {
        let mut b = [0u8; 32];
        slot.to_big_endian(&mut b);
        let w: H256 = read
            .request(
                "eth_getStorageAt",
                (format!("{manager:?}"), format!("0x{:064x}", slot), format!("0x{at:x}")),
            )
            .await
            .context("eth_getStorageAt")?;
        println!("eth_getStorageAt 0x{slot:064x}: {w:?}");
        anyhow::ensure!(
            !w.is_zero(),
            "that slot reads zero, so matching it would prove nothing - is this pool live?"
        );
        truth.push(w);
    }

    let sel = |sig: &str| ethers::utils::keccak256(sig.as_bytes())[..4].to_vec();
    let word = |v: U256| {
        let mut b = [0u8; 32];
        v.to_big_endian(&mut b);
        b
    };

    // extsload(bytes32) - one slot, one word back.
    let mut one = sel("extsload(bytes32)");
    one.extend_from_slice(&word(slots[0]));

    // extsload(bytes32,uint256) - a run of consecutive slots from a start.
    let mut run = sel("extsload(bytes32,uint256)");
    run.extend_from_slice(&word(slots[0]));
    run.extend_from_slice(&word(U256::from(2)));

    // extsload(bytes32[]) - an arbitrary set, which is the one the tick walk
    // needs: bitmap words and tick slots are scattered, not consecutive.
    let mut many = sel("extsload(bytes32[])");
    many.extend_from_slice(&word(U256::from(0x20)));
    many.extend_from_slice(&word(U256::from(slots.len())));
    for slot in slots {
        many.extend_from_slice(&word(slot));
    }

    println!();
    for (name, data, expect_words) in [
        ("extsload(bytes32)", one, 1usize),
        ("extsload(bytes32,uint256)", run, 2),
        ("extsload(bytes32[])", many, 2),
    ] {
        let tx = ethers::types::TransactionRequest::new()
            .to(manager)
            .data(ethers::types::Bytes::from(data));
        match read.call(&tx.into(), Some(ethers::types::BlockId::from(at))).await {
            Ok(out) => {
                // The dynamic forms return an offset and a length before the
                // words; the fixed one returns the word alone. Rather than
                // decode each shape, look for the expected words anywhere in
                // the return - if they are there, the method reads what the
                // node reads.
                let hay = out.0.as_ref();
                let found = truth
                    .iter()
                    .take(expect_words)
                    .filter(|w| hay.windows(32).any(|c| c == w.as_bytes()))
                    .count();
                println!(
                    "{name:28} OK, {} bytes back, {found} of {expect_words} slot(s) match \
                     eth_getStorageAt",
                    hay.len()
                );
            }
            Err(e) => println!("{name:28} unavailable: {e}"),
        }
    }
    println!(
        "\nthe one that matters is extsload(bytes32[]): the tick walk needs scattered slots, \
         not a consecutive run."
    );
    Ok(())
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
    if args.iter().any(|a| a == "--offset") {
        return offset(&args, &cfg).await;
    }
    if args.iter().any(|a| a == "--heads") {
        return heads(&args, &cfg).await;
    }
    if args.iter().any(|a| a == "--extsload") {
        return extsload(&args, &cfg).await;
    }
    // Asked here, before a single request goes out: a self-test that cannot
    // send is not a shorter measurement, it is no measurement, and finding that
    // out after the endpoints have been read would be finding it out late.
    anyhow::ensure!(
        !args.iter().any(|a| a == "--selftest") || send,
        "--selftest sends real transactions on every head: add --send to confirm"
    );

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
            blocks_from_feed: Vec::new(),
            refused: 0,
            ping_err: None,
        });
    }

    println!("chain {chain_id}, {} endpoint(s), {rounds} round(s)", targets.len());
    println!("reads via {}", label(&read_url));

    // The height the sequencer has actually reached, if a feed was given.
    //
    // This exists because every `blocks` figure here is counted from a head,
    // and which head decides what the number means: the rpc's head is a block
    // that has been executed and indexed, while the sequencer may already be
    // several blocks past it. Counted from the rpc, a transaction can look four
    // blocks late when it was in fact next in line. `--offset` established that
    // a feed message's sequence number IS the block number, so this is a
    // reference the sequencer itself would recognise.
    let feed_head = Arc::new(AtomicU64::new(0));
    if let Some(feed_url) = value("--feed").or_else(|| env_var("FEED_URL")) {
        let head = Arc::clone(&feed_head);
        tokio::spawn(async move {
            let stream = match connect_feed(&feed_url).await {
                Ok(s) => s,
                Err(e) => {
                    println!("feed unavailable, counting from the rpc head only: {e:#}");
                    return;
                }
            };
            let (_w, mut r) = stream.split();
            while let Some(Ok(msg)) = r.next().await {
                let text = match msg {
                    tokio_tungstenite::tungstenite::Message::Text(t) => t,
                    tokio_tungstenite::tungstenite::Message::Binary(b) => {
                        String::from_utf8_lossy(&b).into_owned()
                    }
                    _ => continue,
                };
                let Ok(frame) = serde_json::from_str::<Frame>(&text) else { continue };
                if let Some(top) = frame.messages.iter().map(|m| m.sequence_number).max() {
                    head.fetch_max(top, Ordering::Relaxed);
                }
            }
        });
    }

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

    // After the key, the fees and the nonce, because the self-test needs every
    // one of them and asking for them twice could disagree with itself.
    if args.iter().any(|a| a == "--selftest") {
        // One endpoint per run: the loop is a single transaction in flight
        // against a single nonce, and splitting it across endpoints would
        // measure the endpoints in different blocks rather than the loop.
        if targets.len() > 1 {
            println!("more than one endpoint given; sending through {} only\n", targets[0].label);
        }
        let ws_url = value("--ws")
            .or_else(|| env_var("WS_URL"))
            .or(cfg.ws_url)
            .context("no rpc websocket to take heads from: pass --ws wss://... or set WS_URL")?;
        return selftest(SelfTest {
            ws_url,
            feed_url: value("--feed").or_else(|| env_var("FEED_URL")),
            submit: targets[0].http.clone(),
            submit_label: targets[0].label.clone(),
            read,
            read_label: label(&read_url),
            wallet,
            chain_id,
            max_fee,
            tip,
            nonce,
            rounds,
        })
        .await;
    }

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
            let sequenced = feed_head.load(Ordering::Relaxed);

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
            // Only when the feed had said something before the send. A zero
            // here is "not known", not "the head was block zero".
            let from_feed = (sequenced != 0).then(|| landed as i64 - sequenced as i64);
            if let Some(d) = from_feed {
                targets[i].blocks_from_feed.push(d);
            }
            println!(
                "round {round} {:>44}  accept {accept_ms:>4} ms   landed +{delta} from the rpc \
                 head{}  ({head} -> {landed}), seen after {wait_ms:>4} ms",
                targets[i].label,
                from_feed
                    .map(|d| format!(", +{d} from the sequencer at {sequenced}"))
                    .unwrap_or_default()
            );
            nonce += U256::one();
        }
    }

    report(&targets);
    Ok(())
}

fn report(targets: &[Target]) {
    println!(
        "\n{:>44}  {:^17}  {:^17}  {:^17}  {:>6}  {:>6}  refused",
        "endpoint", "ping min/med/max", "accept min/med/max", "seen min/med/max", "vs rpc",
        "vs seq"
    );
    for t in targets {
        println!(
            "{:>44}  {}  {}  {}  {}  {}  {:>7}",
            t.label,
            stats(&t.ping_ms),
            stats(&t.accept_ms),
            stats(&t.wait_ms),
            block_stats(&t.blocks),
            block_stats(&t.blocks_from_feed),
            t.refused
        );
    }
    println!(
        "\nall times in ms. The two block columns are the same distance measured from two \
         different heads: `vs rpc` from the block the reading endpoint had executed, `vs seq` \
         from the height the sequencer had reached. The gap between them is how far behind the \
         chain the rpc's idea of \"now\" is - and `vs seq` is the one that says whether a \
         transaction was actually next in line."
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
