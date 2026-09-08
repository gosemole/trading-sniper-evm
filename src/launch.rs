//! Launchpads: hearing that a token exists at all.
//!
//! A pool feed reports what a pool did; this reports that a pool has just come
//! into being. The two are separate on purpose - a launch has no price history,
//! no ladder and no inventory behind it, and nothing the fall bot knows applies
//! to a token that is one block old.
//!
//! One launchpad so far, and it speaks twice. `PonsV2LaunchFactory` mints the
//! token and opens its bonding curve:
//!
//! ```solidity
//! event TokenLaunched(
//!     address indexed token,
//!     address indexed curve,
//!     address indexed deployer,
//!     address pairToken,
//!     uint256 launchConfigId,
//!     uint256 graduationThreshold
//! );
//! ```
//!
//! and the launcher in front of it, whose `launchAndBuy` does that and buys in
//! the same transaction, reports what its own buy paid:
//!
//! ```solidity
//! event Launched(
//!     address indexed token,
//!     address indexed curve,
//!     address indexed recipient,
//!     address launcher,
//!     uint256 quoteSpent,
//!     uint256 tokensReceived
//! );
//! ```
//!
//! Both are watched, because they answer different questions and only one of
//! them always happens. The factory's is the launch: every token comes through
//! it, and it names the `pairToken` - which is the currency a buy would have to
//! spend and is in no other log. The launcher's is an opinion about the token,
//! by the only person who has expressed one yet, and it is missing entirely
//! from a launch made straight through the factory.
//!
//! An atomic launch emits both, in one transaction, factory first. They are
//! reported as they arrive rather than folded together: which of the two showed
//! up is itself the fact worth having, and `tx` is the same on both for
//! whoever wants to join them.
//!
//! Both ABIs are checked in under `abi/`, and the signatures above are rebuilt
//! from them in a test.
//!
//! This module only listens. It decides nothing and sends nothing.

use anyhow::{Context, Result};
use ethers::abi::{ParamType, Token};
use ethers::providers::{Http, Middleware, Provider, Ws};
use ethers::types::{Address, Bytes, Filter, Log, TransactionRequest, ValueOrArray, H256, U256};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// `PonsV2LaunchFactory` - where a token and its curve actually come from.
///
/// The trust anchor: every launch passes through this address, whether or not
/// a dev buy went with it.
pub const PONS_V2_FACTORY: &str = "0x7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e";

/// The PonsV2 launcher: the contract whose `launchAndBuy` emits `Launched`.
///
/// Not the factory. This one calls `PonsV2LaunchFactory.launchTokenFor` above
/// and then buys from the curve it got back, all in one transaction.
pub const PONS_V2: &str = "0xe33E9E479dF8802cb0866d5d05258bEc4cF62948";

/// Every address these events are believed from, by default.
///
/// Filtering on them is a safety property rather than a saving: an event
/// signature is not owned by anybody, and any contract at all can emit these
/// six words with a token address of its choosing. Watching by topic alone
/// would let a stranger name the token a sniper buys. Widen it only knowing
/// that (`--launchpad any`).
pub const KNOWN_PADS: [&str; 2] = [PONS_V2_FACTORY, PONS_V2];

/// Canonical signatures (no `indexed`, no names) -> keccak for topic0.
pub const TOKEN_LAUNCHED_SIG: &str =
    "TokenLaunched(address,address,address,address,uint256,uint256)";
pub const LAUNCHED_SIG: &str = "Launched(address,address,address,address,uint256,uint256)";

/// What the factory says when its owner changes the snipe tax. Both carry one
/// unindexed `uint256` and nothing else.
pub const SNIPE_TAX_START_SIG: &str = "SnipeTaxStartBpsUpdated(uint256)";
pub const SNIPE_TAX_SECONDS_SIG: &str = "SnipeTaxSecondsUpdated(uint256)";

pub fn snipe_tax_start_topic() -> H256 {
    crate::pool::event_topic(SNIPE_TAX_START_SIG)
}

pub fn snipe_tax_seconds_topic() -> H256 {
    crate::pool::event_topic(SNIPE_TAX_SECONDS_SIG)
}

pub fn token_launched_topic() -> H256 {
    crate::pool::event_topic(TOKEN_LAUNCHED_SIG)
}

pub fn launched_topic() -> H256 {
    crate::pool::event_topic(LAUNCHED_SIG)
}

/// The snipe tax as the factory currently has it set.
///
/// Not a property of a launch: it is one pair of numbers for the whole
/// launchpad, which the owner can change at any time. A buy in the launch
/// second pays `start_bps` of itself, decaying to nothing over `seconds`.
///
/// The decay is [`snipe_tax_bps`], and it is a step per SECOND rather than a
/// curve: `block.timestamp` is whole seconds, so every block inside one second
/// pays exactly the same tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnipeTax {
    pub start_bps: u64,
    pub seconds: u64,
}

/// A launch as the sequencer took it, before any block exists.
///
/// The feed carries signed transactions, not logs, so this is everything the
/// calldata says and nothing the chain decided: there is no token address and
/// no curve address here, because neither has been created yet. What it buys is
/// the time between the sequencer accepting the transaction and the log for it
/// arriving - and the launch parameters, which are wanted before that log
/// rather than after it.
#[derive(Debug, Clone)]
pub struct Incoming {
    /// When this arrived, stamped before anything is done about it.
    ///
    /// Not taken later: the lead over the log is the whole measurement, and
    /// taking it after a lookup would measure the lookup instead - reading
    /// zero for a launch the feed genuinely saw first.
    pub seen: std::time::Instant,
    /// The feed's own sequence number for the message that carried it. On this
    /// chain that number IS the block height the transaction is heading for,
    /// which makes a replayed backlog obvious instead of a thing to wonder
    /// about.
    pub seq: u64,
    /// The second the sequencer stamped this message with, which is the
    /// `block.timestamp` its block will carry - and so, for a launch, the
    /// `launchedAt` the whole snipe tax counts from. Known here before the
    /// block exists.
    pub chain_time: u64,
    pub tx: H256,
    /// Which launchpad contract it was sent to.
    pub to: Address,
    /// Recovered from the signature, so it is who signed rather than who the
    /// transaction says. `None` when recovery failed.
    pub from: Option<Address>,
    pub call: LaunchCall,
}

/// What a buy would pay right now, in basis points of itself.
///
/// A port of the curve's own `currentSnipeTaxBps`, arithmetic and all:
///
/// ```solidity
/// if (snipeTaxExempt[recipient]) return 0;
/// if (startBps == 0) return 0;
/// uint256 elapsed = block.timestamp - launchedAt;
/// if (elapsed >= window) return 0;
/// return startBps >> ((elapsed * 14) / window);
/// ```
///
/// The shift is the whole story, and so is the type of `elapsed`.
/// `block.timestamp` is **whole seconds**, so this is not a decay curve but a
/// staircase with one step per second: every block inside the same second pays
/// exactly the same, and being fifty milliseconds earlier than somebody else
/// buys nothing at all unless it lands in an earlier second.
///
/// With the 9900 bps and 3s window this launchpad is set to, the whole
/// schedule is three numbers: 99% in the launch second, 6.18% in the next one,
/// 0.19% in the one after, then free.
pub fn snipe_tax_bps(tax: &SnipeTax, elapsed_secs: u64) -> u64 {
    if tax.start_bps == 0 || elapsed_secs >= tax.seconds {
        return 0;
    }
    // `elapsed < window` from here, so the shift is at most 13 and the divisor
    // is not zero. Both are still written so they cannot be either.
    let shift = elapsed_secs.saturating_mul(14) / tax.seconds.max(1);
    tax.start_bps.checked_shr(shift as u32).unwrap_or(0)
}

/// What a buy pays at each second after the launch, until it pays nothing.
///
/// Every distinct step, not a sample: this is the whole decision about when to
/// buy, and it is short enough to print in full.
pub fn snipe_tax_schedule(tax: &SnipeTax) -> Vec<(u64, u64)> {
    (0..tax.seconds.min(32))
        .map(|s| (s, snipe_tax_bps(tax, s)))
        .collect()
}

/// The schedule as one line, for the log.
pub fn snipe_tax_line(tax: &SnipeTax) -> String {
    let steps: Vec<String> = snipe_tax_schedule(tax)
        .into_iter()
        .map(|(s, bps)| format!("+{s}s {bps} bps"))
        .collect();
    format!("{}, +{}s free", steps.join(", "), tax.seconds)
}

/// Anything the launchpad said. Launches, and the settings a launch is judged
/// against.
/// Which leg of a trade a transaction was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leg {
    Buy,
    Sell,
}

/// A transaction of ours, and what became of it.
#[derive(Debug, Clone)]
pub struct Settled {
    pub curve: Address,
    pub leg: Leg,
    pub hash: ethers::types::H256,
    /// True only for a receipt that says the call succeeded. Anything else -
    /// reverted, dropped, or no longer knowable - is false, because acting on
    /// a trade that may not exist is worse than missing one that does.
    pub ok: bool,
    pub why: String,
    pub nonce: u64,
    /// The sender's next nonce, re-read whenever a transaction did not land,
    /// so a gap left by a dropped one does not stall everything after it.
    pub resync_nonce: Option<u64>,
    /// What the chain charged in gas, whether or not the trade happened. A
    /// reverted buy costs this and returns nothing, which is the whole reason
    /// it is carried back rather than left in a log line.
    pub cost: ethers::types::U256,
    /// The chain could not say whether this happened, and it may still. The
    /// difference between this and a plain failure is the difference between
    /// two ways of retrying: a reverted transaction is gone and can be
    /// replaced at once, and one that may yet land must not be raced.
    pub pending: bool,
}

/// One trade on one curve, and where in the chain it sat.
#[derive(Debug, Clone)]
pub struct TradeAt {
    pub curve: Address,
    pub block: u64,
    /// Where in the block this log sat. With `block` it names the trade
    /// uniquely, which is what lets the same one arrive twice - once from the
    /// subscription and once from the backfill - without being applied twice.
    pub index: u64,
    pub trade: crate::curve::Trade,
}

/// A launch on its way to the resolver, with whatever was already cached.
///
/// The caches live in the loop that decides things, so it is the loop that
/// says what is missing; the resolver only makes the requests.
#[derive(Debug, Clone)]
pub struct Wanted {
    pub launch: Launch,
    pub call: Option<LaunchCall>,
    pub lead: Option<std::time::Duration>,
    pub launched_at: Option<u64>,
    /// The pair token nothing knows about yet, if there is one.
    pub want_quote: Option<Address>,
    /// The curve to read the launch's terms off, when the calldata that would
    /// have carried them did not decode.
    pub want_terms: Option<Address>,
}

/// A launch with everything the endpoint had to be asked for already in hand.
///
/// The launch arrives as a log; the calldata behind it, the second its block
/// carries and what its pair token is are three more requests. Made in the
/// loop that decides things they cost that loop its budget - it has a hundred
/// milliseconds to aim at a step of the tax window, and one round trip is a
/// third of that. So they are made somewhere else and the launch arrives here
/// again, complete.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub launch: Launch,
    pub call: Option<LaunchCall>,
    /// How far ahead of the log the sequencer feed carried this, when it did.
    pub lead: Option<std::time::Duration>,
    pub launched_at: Option<u64>,
    /// The pair token that had to be looked up, and what it turned out to be.
    pub quote: Option<(Address, Quote, Option<PairEconomics>)>,
    /// The creator tax and curve fee, read off the curve because the calldata
    /// did not say. Present only for a launch through an entry point this does
    /// not decode.
    pub terms: Option<(u64, u64)>,
}

/// Boxed for the same reason as the rest of this enum: a trade is a couple of
/// hundred bytes and a settings change is eight, and they share a channel.
#[derive(Debug, Clone)]
pub enum Heard {
    /// From the sequencer feed: a launch that has been accepted but not yet
    /// mined, or at any rate not yet reported as a log.
    Incoming(Box<Incoming>),
    /// Something happened on a curve. Every curve on the chain reports these,
    /// and the ones we are not following are dropped where they arrive.
    Trade(Box<TradeAt>),
    /// A launch that has been through the resolver and needs nothing more.
    Ready(Box<Resolved>),
    /// How a transaction we sent ended. Reported back rather than waited on:
    /// the loop that sent it has a step of somebody else's tax window to aim
    /// at while this one is confirming.
    Landed(Box<Settled>),
    /// Boxed: a launch is two hundred bytes and a settings change is eight, and
    /// every one of these goes down a channel sized for the settings.
    Launch(Box<Launch>),
    /// The factory's owner moved the snipe tax. Rare, and worth hearing the
    /// moment it happens: it changes what every subsequent buy pays.
    SnipeTaxStartBps(u64),
    SnipeTaxSeconds(u64),
}

/// One log from a launchpad, whichever of the two it was.
///
/// The parts every launchpad log has in common sit here; what only one of them
/// knows sits in [`What`]. A second launchpad joins by adding a variant, not by
/// growing this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Which contract said so. Kept because it is what makes the rest of this
    /// trustworthy.
    pub pad: Address,
    pub token: Address,
    /// The bonding curve holding the token, and the thing a buy would call.
    pub curve: Address,
    pub block: u64,
    /// The same on both logs of an atomic launch, and the only thing that joins
    /// them.
    pub tx: H256,
    pub what: What,
}

/// What this particular log was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum What {
    /// The factory: a token exists and its curve is open. Exactly one per
    /// launch, and the only log that names the currency the curve trades in.
    Created {
        deployer: Address,
        /// What the curve is bought with. The zero address is the native
        /// currency.
        pair_token: Address,
        launch_config_id: U256,
        /// What the curve has to take in `pair_token` before it graduates.
        graduation_threshold: U256,
    },
    /// The launcher's own buy, in the same transaction as the launch. Only an
    /// atomic launch has one.
    DevBuy {
        /// Who received it - exempted from the snipe tax by the launcher.
        recipient: Address,
        /// Who paid for the launch.
        launcher: Address,
        /// In `pair_token`'s units, which this log does not name: see the
        /// factory's log in the same transaction.
        quote_spent: U256,
        tokens_received: U256,
    },
}

/// Follow every launchpad over one websocket until the connection ends.
///
/// An empty `pads` watches the signatures everywhere, which is a firehose in
/// name only - these logs are rare - but see [`KNOWN_PADS`] for why it is not
/// the default.
pub async fn watch(ws_url: &str, pads: &[Address], out: mpsc::Sender<Heard>) -> Result<()> {
    let provider = Provider::<Ws>::connect(ws_url)
        .await
        .context("connect ws")?;
    // One subscription for all four events. Two would be two sockets' worth of
    // bookkeeping for logs that arrive in the same transaction anyway - and the
    // two settings events cost nothing to carry: they are emitted only when the
    // owner changes something, which is how the snipe tax stays current without
    // a single request per launch.
    let mut filter = Filter::new().topic0(ValueOrArray::Array(vec![
        token_launched_topic(),
        launched_topic(),
        snipe_tax_start_topic(),
        snipe_tax_seconds_topic(),
    ]));
    if !pads.is_empty() {
        filter = filter.address(pads.to_vec());
    }
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe to launches")?;
    info!(
        pads = %if pads.is_empty() {
            "any".to_string()
        } else {
            pads.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>().join(",")
        },
        "watching for launches"
    );

    while let Some(log) = stream.next().await {
        // A reorged-out log did not happen. A launch that is unwound takes its
        // token with it, and there is nothing to buy.
        if log.removed.unwrap_or(false) {
            debug!(block = ?log.block_number, "skipping removed launch log");
            continue;
        }
        let heard = match decode(&log) {
            Ok(l) => l,
            Err(e) => {
                warn!(err = %format!("{e:#}"), addr = ?log.address, "failed to decode launch log");
                continue;
            }
        };
        if out.send(heard).await.is_err() {
            info!("nobody is listening any more");
            return Ok(());
        }
    }
    warn!("launch stream ended");
    Ok(())
}

fn decode(log: &Log) -> Result<Heard> {
    let topic0 = *log.topics.first().context("log has no topics")?;
    // The two settings events: one unindexed word, nothing else.
    if topic0 == snipe_tax_start_topic() || topic0 == snipe_tax_seconds_topic() {
        anyhow::ensure!(
            log.data.0.len() >= 32,
            "settings data too short: {} bytes",
            log.data.0.len()
        );
        let v = U256::from_big_endian(&log.data.0[0..32]);
        // Basis points and a window of seconds, both small by construction. A
        // value that does not fit is not a value we can act on, and saying so
        // beats keeping its low 64 bits.
        anyhow::ensure!(
            v <= U256::from(u64::MAX),
            "settings value out of range: {v}"
        );
        return Ok(if topic0 == snipe_tax_start_topic() {
            Heard::SnipeTaxStartBps(v.low_u64())
        } else {
            Heard::SnipeTaxSeconds(v.low_u64())
        });
    }

    // Both events lay out the same way - three indexed addresses, then an
    // address and two uint256 in the data - so the shape is checked once and
    // only the meaning of the words differs below.
    anyhow::ensure!(
        log.topics.len() == 4,
        "launch log has {} topics, expected 4",
        log.topics.len()
    );
    let data = &log.data.0;
    anyhow::ensure!(
        data.len() >= 96,
        "launch data too short: {} bytes",
        data.len()
    );
    let addr = Address::from_slice(&data[12..32]);
    let first = U256::from_big_endian(&data[32..64]);
    let second = U256::from_big_endian(&data[64..96]);

    let what = if topic0 == token_launched_topic() {
        What::Created {
            deployer: topic_address(&log.topics[3]),
            pair_token: addr,
            launch_config_id: first,
            graduation_threshold: second,
        }
    } else if topic0 == launched_topic() {
        What::DevBuy {
            recipient: topic_address(&log.topics[3]),
            launcher: addr,
            quote_spent: first,
            tokens_received: second,
        }
    } else {
        anyhow::bail!("not a launch event: topic0 {topic0:?}");
    };

    Ok(Heard::Launch(Box::new(Launch {
        pad: log.address,
        token: topic_address(&log.topics[1]),
        curve: topic_address(&log.topics[2]),
        block: log
            .block_number
            .context("launch log missing block number")?
            .as_u64(),
        tx: log
            .transaction_hash
            .context("launch log missing transaction hash")?,
        what,
    })))
}

/// A Nitro sequencer feed frame. Only the transactions are wanted; the rest of
/// the envelope - sequence numbers, L1 header, delayed-message counts - is
/// deliberately not modelled, because none of it decides anything here.
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
    #[serde(default)]
    header: Header,
    #[serde(rename = "l2Msg", default)]
    l2_msg: Option<String>,
}

/// Nitro's `L1IncomingMessageHeader`, of which one field matters.
///
/// `timestamp` is the second the sequencer stamped this message with, and it
/// is what the L2 block built from it carries as `block.timestamp` - which is
/// what the snipe tax counts in. So the feed hands over the launch second
/// itself, before the block exists and long before a log could be asked for
/// it.
///
/// `blockNumber` here is L1's, not this chain's: the L2 block number is the
/// message's own `sequenceNumber`.
#[derive(serde::Deserialize, Default)]
struct Header {
    #[serde(default)]
    timestamp: u64,
    #[serde(rename = "blockNumber", default)]
    l1_block: u64,
}

/// Nitro's L2 message kinds, the two that carry transactions.
const L2_BATCH: u8 = 3;
const L2_SIGNED_TX: u8 = 4;

/// Every signed transaction inside one decoded `l2Msg`.
///
/// A signed transaction is its bytes after the kind byte. A batch is a run of
/// length-prefixed sub-messages, each of which is one of these again.
///
/// `depth` exists because the format permits a batch inside a batch and this
/// reads bytes off the network: two levels is more than the sequencer produces,
/// and a cycle must not become a stack overflow.
fn raw_txs<'a>(l2: &'a [u8], depth: u8, out: &mut Vec<&'a [u8]>) {
    if depth > 2 {
        return;
    }
    match l2.first() {
        Some(&L2_SIGNED_TX) => out.push(&l2[1..]),
        Some(&L2_BATCH) => {
            let mut i = 1;
            while i + 8 <= l2.len() {
                let len = u64::from_be_bytes(l2[i..i + 8].try_into().unwrap()) as usize;
                i += 8;
                let Some(end) = i.checked_add(len).filter(|e| *e <= l2.len()) else {
                    return;
                };
                raw_txs(&l2[i..end], depth + 1, out);
                i = end;
            }
        }
        _ => {}
    }
}

/// Where the feed's launches go, counted at every step.
///
/// The feed has now failed twice in a way nothing reported: it connects, it
/// carries frames, it stamps every block - so the clock works and decisions
/// are made - and not one launch is recognised in any of it. Every stage of
/// the path from a frame to a launch ends in a `?`, so a break anywhere in it
/// looks exactly like a quiet chain.
///
/// These say which stage. `to_a_pad` above zero with `decoded` at zero means
/// the calldata changed; `txs` above zero with `to_a_pad` at zero means the
/// launches are going somewhere new; `txs` at zero means the transactions are
/// not being read out of the frames at all.
#[derive(Debug, Default, Clone, Copy)]
struct Funnel {
    frames: u64,
    messages: u64,
    l2: u64,
    txs: u64,
    undecodable: u64,
    to_a_pad: u64,
    decoded: u64,
}

/// One signed transaction, if it is a launch sent to one of `pads`.
///
/// Ordered to do the cheap work first: the feed carries every transaction on
/// the chain, and all but a handful are refused on their `to` address before
/// anything is decoded. Recovering the sender is left to the very end, for the
/// same reason - it is an elliptic-curve operation, and it is not spent on
/// somebody else's swap.
fn launch_in_tx(
    raw: &[u8],
    seq: u64,
    chain_time: u64,
    pads: &[Address],
    seen: &mut Funnel,
) -> Option<Incoming> {
    use ethers::core::utils::rlp::{self, Decodable};
    let Ok(mut tx) = ethers::types::Transaction::decode(&rlp::Rlp::new(raw)) else {
        // Typed envelopes and this chain's own transaction types both land
        // here, and most of them are nobody's business. Counted rather than
        // logged: it is the ratio that says whether something changed.
        seen.undecodable += 1;
        return None;
    };
    let to = tx.to?;
    if !pads.is_empty() && !pads.contains(&to) {
        return None;
    }
    seen.to_a_pad += 1;
    let call = match decode_call(&tx.input) {
        Ok(c) => c,
        Err(e) => {
            // A transaction to the launchpad itself whose calldata we cannot
            // read. Said out loud: this is a launch going past.
            debug!(
                tx = ?tx.hash, ?to,
                selector = %format!("0x{}", hex::encode(tx.input.get(..4).unwrap_or_default())),
                err = %format!("{e:#}"),
                "the feed carried something to a launchpad that does not decode"
            );
            return None;
        }
    };
    seen.decoded += 1;
    // The hash of exactly these bytes rather than the decoder's own. It is
    // the key the log for this launch is matched on, and these bytes are the
    // canonical encoding as the sequencer carried them - `raw_txs` strips only
    // the feed's own kind byte - so this is the hash the chain will report,
    // whatever the decoder makes of the envelope. The two agree today for
    // every type tested; this does not depend on their continuing to.
    tx.hash = ethers::types::H256::from(ethers::utils::keccak256(raw));
    Some(Incoming {
        seen: std::time::Instant::now(),
        seq,
        chain_time,
        tx: tx.hash,
        to,
        from: tx.recover_from_mut().ok(),
        call,
    })
}

/// TODO(dead weight): on this chain the feed buys nothing. Measured over an
/// hour: it finds the launches - forty a minute, matching the log rate - and
/// delivers every one of them AFTER the log for the same launch has arrived,
/// 13 of 14, then 11 of 11, then 22 of 22. It publishes one frame per block,
/// so there is no pre-block lead to have; `behind_blocks` sits at 1 to 3.
/// Meanwhile it decodes ~130 transactions a second in the same process that
/// must decide inside a hundred milliseconds. Either delete it, or keep it
/// only for a relay that is actually ahead - and measure that before trusting
/// it. FEED is unset in every config for this reason.
///
/// Follow the sequencer's own feed until the connection ends.
///
/// This hears a launch when the sequencer accepts it, which is before the block
/// exists and well before a log subscription can report it. What it cannot say
/// is what the launch became: the token and its curve are created inside the
/// transaction, so those still come from the factory's log.
///
/// The feed is a plain WebSocket carrying its own JSON, not JSON-RPC, so it is
/// read directly rather than through a provider.
pub async fn watch_feed(
    url: &str,
    pads: &[Address],
    // The chain's clock against ours: the last second the feed carried, and
    // when we saw it. A step of the tax window opens at a chain second, and a
    // transaction has to be sent before that in our own time - so one has to
    // be expressed in the other.
    anchor: std::sync::Arc<std::sync::RwLock<Option<(u64, std::time::Instant)>>>,
    // Block -> the second it carries. The feed stamps every message with the
    // timestamp its block will have, so following it fills this in for the
    // whole chain - and a trade's own second is then a lookup rather than an
    // estimate from block numbers, which is wrong by a whole step whenever a
    // second holds more or fewer than the usual ten blocks.
    seconds: std::sync::Arc<std::sync::RwLock<std::collections::BTreeMap<u64, u64>>>,
    out: mpsc::Sender<Heard>,
) -> Result<()> {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    // A requested sequence number past the end means "start at the head"
    // rather than "wait for it", so the largest one there is asks for the
    // present without having to know what the present is.
    let mut req = url.into_client_request().context("bad feed url")?;
    req.headers_mut().insert(
        "Arbitrum-Requested-Sequence-Number",
        u64::MAX
            .to_string()
            .parse()
            .expect("a number is a valid header value"),
    );
    let (mut stream, _) = tokio_tungstenite::connect_async(req)
        .await
        .context("connecting to the feed")?;
    info!(pads = pads.len(), "sequencer feed connected");
    let mut first: Option<u64> = None;
    let mut last_stamp: u64 = 0;
    // A fresh connection starts a few blocks behind the head and works
    // forward, so the first transitions it reports are catch-up rather than
    // the live boundary.
    const WARMUP: std::time::Duration = std::time::Duration::from_secs(3);
    const PHASE_SAMPLES: usize = 20;
    let connected = std::time::Instant::now();
    let mut phase: Vec<u64> = Vec::with_capacity(PHASE_SAMPLES);
    // How much this connection actually carried, so a run that reconnects
    // forever says whether it is being refused or is going quiet.
    let mut frames: u64 = 0;
    // Where the launches go, and a line about it every so often. The feed has
    // twice carried frames all night and recognised nothing in them, and
    // nothing said so.
    let mut seen = Funnel::default();
    let mut last_seq: u64 = 0;
    let mut told = std::time::Instant::now();
    const TELL_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

    // A connection that stops speaking without closing looks identical to a
    // quiet chain, and this chain is never quiet: blocks land every ~100ms, so
    // silence this long is a dead socket rather than a lull. Without it a
    // half-open connection is held forever and nothing reconnects.
    const SILENCE: std::time::Duration = std::time::Duration::from_secs(20);

    loop {
        let Ok(next) = tokio::time::timeout(SILENCE, stream.next()).await else {
            anyhow::bail!(
                "no frame for {}s (carried {frames} in {:?})",
                SILENCE.as_secs(),
                connected.elapsed()
            );
        };
        let Some(msg) = next else { break };
        frames += 1;
        seen.frames += 1;
        if told.elapsed() >= TELL_EVERY {
            told = std::time::Instant::now();
            // At info, because a feed that recognises nothing is the failure
            // that has actually happened, twice, and it is invisible otherwise.
            // Theirs, not ours: how far behind the chain's own head the relay
            // is running. The headers arrive over a different socket and name
            // the same blocks, so this is one number against another.
            let head = seconds.read().ok().and_then(|s| s.keys().next_back().copied());
            let behind = head.map(|h| h.saturating_sub(last_seq)).unwrap_or(0);
            if seen.decoded == 0 {
                warn!(
                    frames = seen.frames, messages = seen.messages, l2 = seen.l2,
                    txs = seen.txs, undecodable = seen.undecodable,
                    to_a_pad = seen.to_a_pad, behind_blocks = behind,
                    "the feed has recognised no launches at all"
                );
            } else {
                info!(
                    frames = seen.frames, txs = seen.txs, to_a_pad = seen.to_a_pad,
                    decoded = seen.decoded, behind_blocks = behind,
                    "the feed is finding launches"
                );
            }
            seen = Funnel::default();
        }
        let text = match msg.context("reading the feed")? {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
            tokio_tungstenite::tungstenite::Message::Close(_) => break,
            // Pings are answered by the library; nothing else carries messages.
            _ => continue,
        };
        let frame: Frame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                debug!(err = %e, "feed frame not understood");
                continue;
            }
        };
        for m in frame.messages {
            // Said once per connection: if it is far behind the head, this
            // relay hands out history on connect and an "incoming" launch may
            // be older than a log already printed.
            if first.is_none() {
                first = Some(m.sequence_number);
                info!(
                    sequence = m.sequence_number,
                    timestamp = m.message.message.header.timestamp,
                    l1_block = m.message.message.header.l1_block,
                    "feed starts here"
                );
            }
            last_seq = last_seq.max(m.sequence_number);
            let stamped = m.message.message.header.timestamp;
            if let Ok(mut s) = seconds.write() {
                s.insert(m.sequence_number, stamped);
                // A few minutes of blocks, which is longer than any curve is
                // followed for.
                while s.len() > 4096 {
                    let Some(&oldest) = s.keys().next() else {
                        break;
                    };
                    s.remove(&oldest);
                }
            }
            if stamped > last_stamp {
                // The chain's clock against ours, taken at the moment it
                // moves. Everything about aiming at a step of the tax window
                // is measured from this pair.
                if let Ok(mut a) = anchor.write() {
                    *a = Some((stamped, std::time::Instant::now()));
                }
                // WHERE IN OUR OWN SECOND the chain's second turns over. The
                // tax steps on that boundary, so this is the difference
                // between sending now and sending in half a second - and it
                // cannot be had from a timestamp alone, which is a whole
                // number and says nothing about when it changed.
                //
                // One sample is worthless. Blocks are ~100ms and not evenly
                // spaced, so the first block of a new second lands anywhere
                // inside a block interval of the true boundary, and the feed
                // adds its own delivery jitter on top. What is wanted is the
                // spread as well as the middle, and both need a run of them.
                if last_stamp != 0 && connected.elapsed() > WARMUP {
                    phase.push(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_millis() as u64,
                    );
                    if phase.len() >= PHASE_SAMPLES {
                        let (at, late) = middle_of(&phase);
                        // Every twenty boundaries, which on this chain is
                        // every twenty seconds. Worth a spot check and not
                        // worth a line each time.
                        debug!(
                            at_ms = at,
                            seen_up_to_ms_late = late,
                            samples = phase.len(),
                            "the chain's second turns over here in ours"
                        );
                        phase.clear();
                    }
                }
                last_stamp = stamped;
            }
            seen.messages += 1;
            let Some(b64) = m.message.message.l2_msg else {
                continue;
            };
            let Ok(raw) = base64_decode(&b64) else {
                continue;
            };
            seen.l2 += 1;
            let mut txs = Vec::new();
            raw_txs(&raw, 0, &mut txs);
            seen.txs += txs.len() as u64;
            for tx in txs {
                let Some(incoming) = launch_in_tx(tx, m.sequence_number, stamped, pads, &mut seen)
                else {
                    continue;
                };
                if out.send(Heard::Incoming(Box::new(incoming))).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
    warn!(
        frames,
        lived = ?connected.elapsed(),
        "the sequencer feed ended"
    );
    Ok(())
}

/// Where a run of millisecond-of-second readings says the boundary is, and how
/// late the readings scatter behind it.
///
/// The **earliest** reading, not the middle one. The error here is one-sided:
/// a new timestamp cannot be seen before it exists, only after - by however
/// long it takes the next block to be built and delivered. So every sample is
/// the boundary plus a non-negative lag, and the smallest of them is the
/// closest to the truth. A median would sit half a block interval late, every
/// time, in the same direction.
///
/// Circular, because ordinary sorting is wrong on a clock: 990ms and 10ms are
/// twenty milliseconds apart, not nine hundred and eighty, and a boundary near
/// the top of the second would otherwise average out to the middle of it.
fn middle_of(samples: &[u64]) -> (u64, u64) {
    let reference = samples[0] as i64;
    let mut offsets: Vec<i64> = samples
        .iter()
        .map(|s| {
            let d = *s as i64 - reference;
            if d > 500 {
                d - 1000
            } else if d < -500 {
                d + 1000
            } else {
                d
            }
        })
        .collect();
    offsets.sort_unstable();
    let earliest = offsets[0];
    let spread = (offsets[offsets.len() - 1] - earliest).unsigned_abs();
    (((reference + earliest).rem_euclid(1000)) as u64, spread)
}

/// Follow every curve on the chain, and drop what is not being followed.
///
/// One subscription for all of them rather than one per launch. A curve's
/// address does not exist until its launch, so per-curve subscriptions would
/// mean opening one inside the window a launch is decided in - and closing
/// them again as launches age. Watching the four topics everywhere costs one
/// subscription and a filter in this process, and these logs are small.
pub async fn watch_curves(
    ws_url: &str,
    followed: std::sync::Arc<std::sync::RwLock<std::collections::HashSet<Address>>>,
    out: mpsc::Sender<Heard>,
) -> Result<()> {
    use futures_util::StreamExt as _;
    let provider = Provider::<Ws>::connect(ws_url)
        .await
        .context("connect ws")?;
    let filter = Filter::new().topic0(ValueOrArray::Array(crate::curve::trade_topics()));
    let mut stream = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe to curve trades")?;
    info!("watching curve trades");
    // Trades for curves not yet spoken for. Small and short-lived: this is a
    // race of milliseconds, not a queue.
    // Every curve on the chain lands here until its launch is recognised, and
    // the launches worth catching are the ones whose whole opening bundle
    // arrives in one block. Five hundred entries were a couple of seconds of
    // chain traffic - the bundle was evicted before its own launch was read.
    const HOLD_AT_MOST: usize = 8192;
    const HOLD_FOR: std::time::Duration = std::time::Duration::from_secs(5);
    let mut held: std::collections::VecDeque<(
        std::time::Instant,
        Address,
        u64,
        u64,
        crate::curve::Trade,
    )> = std::collections::VecDeque::new();

    while let Some(log) = stream.next().await {
        if log.removed.unwrap_or(false) {
            continue;
        }
        let trade = match crate::curve::decode_trade(&log) {
            Ok(t) => t,
            Err(e) => {
                debug!(err = %format!("{e:#}"), addr = ?log.address, "curve log not understood");
                continue;
            }
        };
        let Some(block) = log.block_number.map(|b| b.as_u64()) else {
            continue;
        };
        let Some(index) = log.log_index.map(|i| i.as_u64()) else {
            continue;
        };

        // Whatever is held for a curve that has since been identified, in the
        // order it happened - checked on EVERY log rather than only when
        // another trade on that same curve turns up.
        //
        // Waiting for a second trade was the bug: a launch's whole opening
        // bundle arrives while the curve is still unknown, and the next trade
        // on it can be forty blocks later. Until then the reserves stood at
        // the opening ones, and every decision made in those seconds was made
        // against a curve that no longer existed.
        if !held.is_empty() {
            let known: Vec<usize> = match followed.read() {
                Ok(f) => held
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, c, _, _, _))| f.contains(c))
                    .map(|(i, _)| i)
                    .collect(),
                Err(_) => Vec::new(),
            };
            for i in known.into_iter().rev() {
                let Some((_, c, b, ix, t)) = held.remove(i) else {
                    continue;
                };
                if out
                    .send(Heard::Trade(Box::new(TradeAt {
                        curve: c,
                        block: b,
                        index: ix,
                        trade: t,
                    })))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
            held.retain(|(at, _, _, _, _)| at.elapsed() < HOLD_FOR);
        }

        // A curve nobody asked for is dropped here rather than in the loop
        // that decides things. Every curve on the chain reports these, and a
        // firehose in front of the launch signals does not merely waste work,
        // it delays the thing being measured.
        if !followed.read().is_ok_and(|f| f.contains(&log.address)) {
            if held.len() >= HOLD_AT_MOST {
                held.pop_front();
            }
            held.push_back((std::time::Instant::now(), log.address, block, index, trade));
            continue;
        }

        if out
            .send(Heard::Trade(Box::new(TradeAt {
                curve: log.address,
                block,
                index,
                trade,
            })))
            .await
            .is_err()
        {
            return Ok(());
        }
    }
    warn!("curve stream ended");
    Ok(())
}


/// The chain's clock, from its own block headers.
///
/// The sequencer feed carries this too and carries it earlier, which is why it
/// used to be the only source. But it was the ONLY source, and the tick that
/// decides things gives up on the first line without it - so every run where
/// the feed faltered decided nothing at all, silently, while the launches went
/// past. One overnight run made not one decision for that reason.
///
/// Headers arrive after their block is built rather than before, so this is a
/// later reading of the same boundary than the feed's. That was worth caring
/// about when the aim was to land inside a particular second of the snipe tax
/// window and pay 19 basis points instead of 618. It is not worth caring about
/// now: the entry is the free step, where the tax is zero and stays zero, and
/// there is no boundary left to hit - only a moment to be after.
///
/// So it defers to the feed when the feed is alive, and takes over when it is
/// not.
pub async fn watch_heads(
    ws_url: &str,
    anchor: std::sync::Arc<std::sync::RwLock<Option<(u64, std::time::Instant)>>>,
    seconds: std::sync::Arc<std::sync::RwLock<std::collections::BTreeMap<u64, u64>>>,
    // The base fee of the newest block, for whoever is paying one. It arrives
    // in the header we are already reading, so it costs nothing and is never
    // more than one block old - which on a chain running ten blocks a second
    // is the difference between a fee that can be included and one that
    // cannot.
    base_fee: std::sync::Arc<std::sync::RwLock<ethers::types::U256>>,
) -> Result<()> {
    use ethers::providers::{Middleware, Provider, Ws};
    use futures_util::StreamExt;

    let provider = Provider::<Ws>::connect(ws_url)
        .await
        .context("connecting for block headers")?;
    let mut stream = provider
        .subscribe_blocks()
        .await
        .context("subscribing to block headers")?;
    info!("watching block headers for the chain's clock");

    // Blocks land every ~100ms here, so this much silence is a dead socket.
    const SILENCE: std::time::Duration = std::time::Duration::from_secs(20);
    // How stale the anchor has to be before this replaces it. The feed sets it
    // on every turnover, so anything older than a couple of seconds means the
    // feed is not doing so any more.
    const DEFER_FOR: std::time::Duration = std::time::Duration::from_secs(2);

    let mut heads: u64 = 0;
    loop {
        let Ok(next) = tokio::time::timeout(SILENCE, stream.next()).await else {
            anyhow::bail!("no block header for {}s", SILENCE.as_secs());
        };
        let Some(head) = next else { break };
        heads += 1;
        let (Some(number), stamped) = (head.number, head.timestamp.as_u64()) else {
            continue;
        };
        let number = number.as_u64();
        if let Some(fee) = head.base_fee_per_gas {
            if let Ok(mut b) = base_fee.write() {
                *b = fee;
            }
        }
        if let Ok(mut s) = seconds.write() {
            s.insert(number, stamped);
            while s.len() > 4096 {
                let Some(&oldest) = s.keys().next() else { break };
                s.remove(&oldest);
            }
        }
        if let Ok(mut a) = anchor.write() {
            let stale = a.is_none_or(|(second, at)| {
                stamped > second && at.elapsed() > DEFER_FOR
            });
            if stale {
                *a = Some((stamped, std::time::Instant::now()));
            }
        }
    }
    warn!(heads, "the block header stream ended");
    Ok(())
}

/// The trades a curve made before anything was listening to it.
///
/// A curve's address does not exist until its launch, so it cannot be in a log
/// filter until the launch log arrives - and by then the launch block, and
/// often the one after it, have already been dispatched to whoever was
/// subscribed at the time. Nothing can deliver them late. On an ordinary
/// launch that costs nothing, because the first trade after it is seconds
/// away. On a bundled one it costs everything: the bundle buys in the launch
/// transaction and the block after, and 61 of 3540 journals from one night
/// show the chain pricing a fill off a reserve between 8% and 200% above the
/// one the bot was carrying - every one of them a bundle, and bundles are the
/// only group that has ever measured profitable.
///
/// So they are asked for once, by number. Sent down the same channel as the
/// live ones and told apart by `(block, index)`, because the subscription may
/// well have caught some of them and applying a trade twice is worse than
/// missing it.
///
/// Runs in its own task: this is an RPC round trip, and the loop it reports to
/// is deciding things on a hundred-millisecond budget.
pub async fn backfill(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    curve: Address,
    from: u64,
    out: mpsc::Sender<Heard>,
) {
    // In two passes, because the launch block IS the head. Asking for blocks
    // that do not exist yet is not an empty answer, it is
    // `invalid block range params` and the whole request is lost - including
    // the launch block, which is the one that matters and the one we already
    // know exists, having just read a log out of it.
    //
    // So: that block on its own, at once. Then the few after it, once enough
    // time has passed for them to be there. The bundle's own buys are split
    // across exactly these two.
    let mut sent = ask(http, curve, from, from, &out).await;
    // Six blocks of this chain, and well inside the second before the first
    // step of the tax window opens.
    tokio::time::sleep(std::time::Duration::from_millis(620)).await;
    sent += ask(http, curve, from + 1, from + 4, &out).await;
    debug!(curve = ?curve, from, sent, "backfilled a curve's opening trades");
}

/// One range, or nothing and a reason.
async fn ask(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    curve: Address,
    from: u64,
    to: u64,
    out: &mpsc::Sender<Heard>,
) -> usize {
    use ethers::providers::Middleware;
    let filter = ethers::types::Filter::new()
        .address(curve)
        .topic0(crate::curve::trade_topics())
        .from_block(from)
        .to_block(to);
    let logs = match crate::rpc::retrying("eth_getLogs", || async {
        http.get_logs(&filter).await.map_err(anyhow::Error::from)
    })
    .await
    {
        Ok(l) => l,
        Err(e) => {
            // Not fatal, and not silent: what is lost is the opening state of
            // exactly the launches worth having.
            warn!(
                curve = ?curve, from, to, err = %format!("{e:#}"),
                "cannot read what this curve did before we were listening"
            );
            return 0;
        }
    };
    let mut sent = 0usize;
    for log in logs {
        if log.removed.unwrap_or(false) {
            continue;
        }
        let (Some(block), Some(index)) = (
            log.block_number.map(|b| b.as_u64()),
            log.log_index.map(|i| i.as_u64()),
        ) else {
            continue;
        };
        let Ok(trade) = crate::curve::decode_trade(&log) else {
            continue;
        };
        if out
            .send(Heard::Trade(Box::new(TradeAt {
                curve,
                block,
                index,
                trade,
            })))
            .await
            .is_err()
        {
            return sent;
        }
        sent += 1;
    }
    sent
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("l2Msg is not base64")
}

/// Read the snipe tax off the factory, once.
///
/// The only two requests this whole watcher makes on its own behalf. After
/// them the numbers are kept current from `SnipeTaxStartBpsUpdated` and
/// `SnipeTaxSecondsUpdated` on the same subscription, so a launch never waits
/// on a call to know what it is being taxed - and a change made by the owner
/// while this is running arrives as a log rather than as a stale number nobody
/// re-read.
pub async fn read_snipe_tax(http: &Provider<Http>, factory: Address) -> Result<SnipeTax> {
    let start_bps = call_u64(http, factory, "snipeTaxStartBps()").await?;
    let seconds = call_u64(http, factory, "snipeTaxSeconds()").await?;
    Ok(SnipeTax { start_bps, seconds })
}

/// The two terms a launch's calldata would have said, read off the curve.
///
/// A launch through an entry point this does not decode has no calldata worth
/// anything: no name, no exemption list, and - the one that matters - no
/// creator tax. Without that the opening curve cannot be built, so those
/// launches were not merely undecided about, they were not followed at all,
/// and nothing about them reached the journals.
///
/// The curve itself knows. One call, off the loop that decides, and a launch
/// through a wrapper is followed like any other - marked as one, because what
/// is still missing about it is real: who was exempted from the snipe tax, and
/// whether the maker bought their own launch.
pub async fn read_curve_terms(http: &Provider<Http>, curve: Address) -> Option<(u64, u64)> {
    let creator_tax_bps = call_u64(http, curve, "creatorTaxBps()").await.ok()?;
    let fee_bps = call_u64(http, curve, "feeBps()").await.ok()?;
    Some((creator_tax_bps, fee_bps))
}

/// One entry of the factory's launch configuration table.
///
/// A launch names its own by id, and the id is in its log. Everything a curve
/// opens with is here: how many tokens are minted, what the curve charges, and
/// the two numbers that decide where it starts and where it graduates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchConfig {
    pub supply: U256,
    pub curve_fee_bps: u64,
    pub phantom_quote: U256,
    pub graduation_threshold: U256,
    pub pool_fee: u32,
    pub tick_spacing: i32,
    pub enabled: bool,
}

/// What a pair token is worth a launch being quoted in, as the factory has it.
///
/// The launch config carries one phantom reserve and one threshold, but a pair
/// token can override both - which is why a launch quoted in USDG graduates at
/// 8090 and one in ETH at 4.2. These are the numbers a curve actually opens
/// with, and `decimals` comes back with them, so this replaces asking the
/// token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairEconomics {
    pub phantom_quote: U256,
    pub graduation_threshold: U256,
    pub decimals: u8,
}

/// `pairTokenEconomics(pairToken)`.
pub async fn read_pair_economics(
    http: &Provider<Http>,
    factory: Address,
    pair_token: Address,
) -> Result<PairEconomics> {
    let mut data = crate::pool::selector("pairTokenEconomics(address)").to_vec();
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(pair_token.as_bytes());
    data.extend_from_slice(&word);
    let data = Bytes::from(data);

    let res: Bytes = crate::rpc::retrying("eth_call pairTokenEconomics", || {
        let tx = TransactionRequest::new().to(factory).data(data.clone());
        async move {
            http.call(&tx.into(), None)
                .await
                .context("eth_call pairTokenEconomics")
        }
    })
    .await?;
    anyhow::ensure!(res.len() >= 96, "short return for pairTokenEconomics");
    let decimals = U256::from_big_endian(&res[64..96]);
    anyhow::ensure!(
        decimals <= U256::from(36u64),
        "decimals out of range: {decimals}"
    );
    Ok(PairEconomics {
        phantom_quote: U256::from_big_endian(&res[0..32]),
        graduation_threshold: U256::from_big_endian(&res[32..64]),
        decimals: decimals.low_u32() as u8,
    })
}

/// How many configurations the factory holds.
pub async fn read_launch_config_count(http: &Provider<Http>, factory: Address) -> Result<u64> {
    call_u64(http, factory, "launchConfigCount()").await
}

/// `getLaunchConfig(id)`, decoded.
///
/// Read rather than assumed. Everything about the opening curve was recovered
/// from watching launches price themselves - and seven of them agreeing is
/// evidence, not a value anybody wrote down. This is the value somebody wrote
/// down.
pub async fn read_launch_config(
    http: &Provider<Http>,
    factory: Address,
    id: u64,
) -> Result<LaunchConfig> {
    let mut data = crate::pool::selector("getLaunchConfig(uint256)").to_vec();
    let mut word = [0u8; 32];
    U256::from(id).to_big_endian(&mut word);
    data.extend_from_slice(&word);
    let data = Bytes::from(data);

    let res: Bytes = crate::rpc::retrying("eth_call getLaunchConfig", || {
        let tx = TransactionRequest::new().to(factory).data(data.clone());
        async move {
            http.call(&tx.into(), None)
                .await
                .context("eth_call getLaunchConfig")
        }
    })
    .await?;

    // A struct return of fixed-size fields is encoded inline, so this is seven
    // words and no offset - but it is decoded against the types rather than
    // sliced by hand, because `tickSpacing` is signed and reading it as
    // unsigned would turn a negative spacing into an enormous positive one.
    let tokens = ethers::abi::decode(
        &[ParamType::Tuple(vec![
            ParamType::Uint(256), // supply
            ParamType::Uint(256), // curveFeeBps
            ParamType::Uint(256), // phantomQuote
            ParamType::Uint(256), // graduationThreshold
            ParamType::Uint(24),  // poolFee
            ParamType::Int(24),   // tickSpacing
            ParamType::Bool,      // enabled
        ])],
        &res,
    )
    .context("decoding getLaunchConfig")?;
    let f = match tokens.first() {
        Some(Token::Tuple(t)) if t.len() == 7 => t.clone(),
        _ => anyhow::bail!("getLaunchConfig did not return a LaunchConfig"),
    };
    let uint = |i: usize| -> Result<U256> {
        f[i].clone()
            .into_uint()
            .with_context(|| format!("LaunchConfig[{i}] is not a uint"))
    };
    let fee = uint(1)?;
    anyhow::ensure!(
        fee <= U256::from(u64::MAX),
        "curveFeeBps out of range: {fee}"
    );
    let pool_fee = uint(4)?;
    anyhow::ensure!(
        pool_fee <= U256::from(u32::MAX),
        "poolFee out of range: {pool_fee}"
    );
    Ok(LaunchConfig {
        supply: uint(0)?,
        curve_fee_bps: fee.low_u64(),
        phantom_quote: uint(2)?,
        graduation_threshold: uint(3)?,
        pool_fee: pool_fee.low_u32(),
        tick_spacing: as_i24(
            f[5].clone()
                .into_int()
                .context("tickSpacing is not an int")?,
        ),
        enabled: matches!(f[6], Token::Bool(true)),
    })
}

/// A signed 24-bit integer, as ABI two's complement in a 256-bit word.
fn as_i24(v: U256) -> i32 {
    // The sign bit of the ABI word, not of the 24-bit field: ethabi widens an
    // int24 to a full word, so a negative one arrives with every high bit set.
    if v.bit(255) {
        // Two's complement of a 256-bit value: -(2^256 - v), taken through the
        // low 32 bits, which is all an int24 can occupy.
        -(((!v) + U256::one()).low_u32() as i64) as i32
    } else {
        v.low_u32() as i32
    }
}

async fn call_u64(http: &Provider<Http>, to: Address, sig: &str) -> Result<u64> {
    let data = crate::pool::selector(sig);
    let res: Bytes = crate::rpc::retrying(sig, || {
        let tx = TransactionRequest::new().to(to).data(data.clone());
        async move {
            http.call(&tx.into(), None)
                .await
                .with_context(|| format!("eth_call {sig}"))
        }
    })
    .await?;
    anyhow::ensure!(res.len() >= 32, "short return for {sig}");
    let v = U256::from_big_endian(&res[0..32]);
    anyhow::ensure!(v <= U256::from(u64::MAX), "{sig} out of range: {v}");
    Ok(v.low_u64())
}

/// What the pair token of a launch turned out to be.
///
/// Kept per curve, because only the factory's log names the pair token and the
/// launcher's log in the same transaction has to be printed in its units. The
/// factory's arrives first, so by the time a dev buy needs this it is here.
#[derive(Debug, Clone)]
pub struct Quote {
    pub decimals: u8,
    pub symbol: String,
}

/// Everything known about one launch, from however many places it came.
///
/// An atomic launch emits two logs, in one transaction: the factory's, which is
/// the launch, and the launcher's, which is its own dev buy. They are rendered
/// as one entry - two would read as two launches - and either may be missing: a
/// token minted straight through the factory has no dev buy, and a dev buy
/// whose launch arrived before this process did has nothing in front of it.
#[derive(Default)]
pub struct Report<'a> {
    pub created: Option<&'a Launch>,
    pub dev: Option<&'a Launch>,
    /// What the pair token is. Without it an amount in that token is printed
    /// raw rather than guessed at, because a guess of 18 on a six-decimal token
    /// is wrong by a factor of a million and still looks like a number.
    pub quote: Option<&'a Quote>,
    /// What the transaction asked for, which no log says.
    pub call: Option<&'a LaunchCall>,
    /// The launchpad's setting at this moment, not this launch's own.
    pub tax: Option<SnipeTax>,
    /// The launch second, as the sequencer stamped it. Only the feed knows
    /// this - a log says which block, and a block's timestamp is another
    /// request - and it is what turns the tax schedule from "seconds after
    /// something" into wall-clock seconds to aim at.
    pub launched_at: Option<u64>,
    /// How long before this log the sequencer's feed carried the same
    /// transaction. The whole of what a feed-driven signal would buy, measured
    /// rather than assumed.
    pub lead: Option<std::time::Duration>,
    /// Why this launch is not being followed, when it is not.
    pub refused: Option<&'a str>,
    /// How it was made, when the calldata did not say and the curve was asked
    /// instead. Marks the entry so a launch through a wrapper is not read as
    /// an ordinary one with fields missing.
    pub via: Option<&'a str>,
}

/// What a launch is quoted in, from whichever of its two accounts knows.
///
/// The factory's log names the pair token; so does the calldata, which arrives
/// first when there is a feed. A dev buy log names neither.
pub fn pair_of(l: &Launch, call: Option<&LaunchCall>) -> Option<Address> {
    match l.what {
        What::Created { pair_token, .. } => Some(pair_token),
        _ => call.map(|c| c.pair_token),
    }
}

/// UTC, to the millisecond, the way the log lines around these are stamped.
///
/// Every printed entry carries one because the whole point of two sources is
/// which arrived first, and without a stamp the order in a terminal is a guess.
fn stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let s = now.as_secs() % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s / 3600,
        (s % 3600) / 60,
        s % 60,
        now.subsec_millis()
    )
}

/// `0x6abee1…956d`. Enough to recognise, tell apart and paste into a search;
/// the whole thing is in the transaction for anyone who needs to act on it.
fn short(a: &Address) -> String {
    let h = format!("{a:?}");
    format!("{}…{}", &h[..8], &h[h.len() - 4..])
}

fn short_tx(h: &H256) -> String {
    let s = format!("{h:?}");
    format!("{}…{}", &s[..10], &s[s.len() - 6..])
}

/// An amount in its own units, cut to six decimals.
///
/// A launch quotes eighteen of them and the last twelve are never read: what
/// matters at a glance is 0.086 rather than 0.086000000000000001.
fn amount(v: U256, decimals: u8) -> String {
    let full = crate::units::format_units(v, decimals);
    match full.split_once('.') {
        Some((whole, frac)) => {
            let frac = frac.get(..6).unwrap_or(frac).trim_end_matches('0');
            if frac.is_empty() {
                whole.to_string()
            } else {
                format!("{whole}.{frac}")
            }
        }
        None => full,
    }
}

/// A supply-sized number, short. These run to eight figures and eighteen
/// decimals, and no decision is made on the tail of one.
fn tokens(v: U256) -> String {
    let x = crate::units::format_units(v, 18)
        .parse::<f64>()
        .unwrap_or(f64::NAN);
    match x.abs() {
        n if n >= 1e9 => format!("{:.2}B", x / 1e9),
        n if n >= 1e6 => format!("{:.2}M", x / 1e6),
        n if n >= 1e3 => format!("{:.1}K", x / 1e3),
        _ => amount(v, 18),
    }
}

/// An amount in its own units, cut to six decimals. Public so the loop that
/// decides things can print one without eighteen digits of tail.
pub fn amount_of(v: U256, decimals: u8) -> String {
    amount(v, decimals)
}

/// A supply-sized number, short. Public so the loop that decides things can
/// print one without a formatter of its own.
pub fn tokens_of(v: U256) -> String {
    tokens(v)
}

fn is_known(a: &Address) -> bool {
    KNOWN_PADS
        .iter()
        .any(|k| k.parse::<Address>().is_ok_and(|p| p == *a))
}

/// Could this launch still have a dev buy coming in the same transaction?
///
/// Only the launcher's `launchAndBuy` emits `Launched`, and a transaction that
/// went straight to the factory's `launchToken` is a different shape
/// altogether - there is no second log to wait for, so waiting for one is
/// 250ms of nothing. With an unknown entry point the answer is yes: not
/// knowing is not the same as knowing there is nothing.
pub fn may_carry_dev_buy(call: Option<&LaunchCall>) -> bool {
    call.is_none_or(|c| c.via != "launchToken")
}

/// What the launch itself said it graduates at, from the factory's own log.
pub fn threshold_of(l: &Launch) -> Option<U256> {
    match l.what {
        What::Created {
            graduation_threshold,
            ..
        } => Some(graduation_threshold),
        _ => None,
    }
}

/// A launch the sequencer has taken, in one line, before its block exists.
///
/// One line and not an entry, because half of what an entry says does not exist
/// yet: the token and the curve are created inside this transaction. What is
/// here is what was asked for.
pub fn render_incoming(i: &Incoming, quote: Option<&Quote>) -> String {
    let pair = match quote {
        Some(q) => q.symbol.clone(),
        None => short(&i.call.pair_token),
    };
    let mut out = format!(
        "{} feed    {} ({})  seq {}  launchedAt {}  pair {pair}",
        stamp(),
        i.call.name,
        i.call.symbol,
        i.seq,
        i.chain_time,
    );
    if let Some(v) = i.call.quote_in {
        out += &format!(
            "  dev buy {}",
            match quote {
                Some(q) => format!("{} {}", amount(v, q.decimals), q.symbol),
                None => format!("{v} raw"),
            }
        );
    }
    if let Some(from) = i.from {
        out += &format!("  from {}", short(&from));
    }
    out += &format!("  {}", i.call.via);
    // Only when it went somewhere unexpected. In the ordinary case `via`
    // already says which of the two contracts took it.
    if !is_known(&i.to) {
        out += &format!(" at {}", short(&i.to));
    }
    out += &format!("  tx {}", short_tx(&i.tx));
    out
}

/// One launch, as a block of lines.
///
/// Ordered by what a person reads first: what launched, then in what currency
/// and how much of it the launcher put in - the only opinion anybody has
/// expressed about the thing yet - then who else is exempt from the tax we
/// would pay, and last the addresses to act on.
///
/// The snipe tax rides on the title line rather than taking one of its own: it
/// is one setting for the whole launchpad and the same on every entry, but it
/// is also the number that decides whether buying at all is worth it.
pub fn render(r: &Report) -> String {
    let Some(any) = r.created.or(r.dev) else {
        return String::new();
    };
    let in_quote = |v: U256| match r.quote {
        Some(q) => format!("{} {}", amount(v, q.decimals), q.symbol),
        None => format!("{v} raw"),
    };
    let symbol = r.quote.map(|q| q.symbol.as_str());

    // The title: what it is called, if the calldata said; the token address if
    // nobody has.
    let mut out = format!(
        "{} {}  {}  block {}",
        stamp(),
        if r.created.is_some() {
            "launch "
        } else {
            "dev buy"
        },
        match r.call {
            Some(c) => format!("{} ({})", c.name, c.symbol),
            None => short(&any.token),
        },
        any.block,
    );
    if let Some(lead) = r.lead {
        // Measured between two sources in one process against one clock, so it
        // is a difference between endpoints rather than between machines.
        out += &format!("  feed +{}ms", lead.as_millis());
    }
    if let Some(t) = r.tax {
        // On the title line rather than a line of its own: it is one setting
        // for the whole launchpad and identical on every entry, but it is also
        // the number that decides whether a buy is worth making at all.
        out += &format!("  tax {}bps/{}s", t.start_bps, t.seconds);
    }
    // The token address only when the title did not already carry it, which is
    // whenever the calldata gave this thing a name.
    out += &match r.call {
        Some(_) => format!(
            "\n  token      {}   curve {}",
            short(&any.token),
            short(&any.curve)
        ),
        None => format!("\n  curve      {}", short(&any.curve)),
    };

    let mut deployer = None;
    if let Some(What::Created {
        deployer: d,
        pair_token,
        launch_config_id,
        graduation_threshold,
    }) = r.created.map(|l| &l.what)
    {
        deployer = Some(*d);
        out += &format!(
            "\n  pair       {}   config #{launch_config_id}, graduates at {}",
            symbol
                .map(str::to_string)
                .unwrap_or_else(|| short(pair_token)),
            in_quote(*graduation_threshold),
        );
    }
    if let Some(c) = r.call {
        // The creator's cut is paid on the way out as well as in, so it is part
        // of what a round trip costs before the pool has moved at all.
        out += &format!(
            "\n  deployer   {}   creator tax {} bps{}",
            match deployer {
                Some(d) => short(&d),
                None => short(&c.creator_fee_recipient),
            },
            c.creator_tax_bps,
            if c.buyback_enabled {
                ", buyback on"
            } else {
                ""
            },
        );
    } else if let Some(d) = deployer {
        out += &format!("\n  deployer   {}", short(&d));
    }

    if let Some(What::DevBuy {
        recipient,
        launcher,
        quote_spent,
        tokens_received,
    }) = r.dev.map(|l| &l.what)
    {
        out += &format!(
            "\n  dev buy    {} -> {} -> {}",
            in_quote(*quote_spent),
            tokens(*tokens_received),
            short(recipient),
        );
        // What it asked for, next to what it got: a fill clamped by the curve
        // shows up here and nowhere else.
        // Only when it says something. A minimum of a few wei renders as "0"
        // through a formatter built for supply-sized numbers, and "(min 0)"
        // reads as "no minimum" when it means the opposite.
        if let Some(min) = r.call.and_then(|c| c.min_tokens_out) {
            let shown = tokens(min);
            if !min.is_zero() && shown != "0" {
                out += &format!(" (min {shown})");
            }
        }
        // Who paid is worth naming only when it is not the deployer, which on
        // these launches it usually is.
        if Some(*launcher) != deployer {
            out += &format!("\n  launcher   {}", short(launcher));
        }
    }
    // The tax schedule in absolute seconds: what to aim at, rather than an
    // offset from a moment nobody wrote down.
    if let (Some(t), Some(at), None) = (r.tax, r.launched_at, r.refused) {
        let steps: Vec<String> = snipe_tax_schedule(&t)
            .into_iter()
            .skip(1)
            .map(|(s, bps)| format!("{bps} bps at {}", at + s))
            .chain(std::iter::once(format!("free at {}", at + t.seconds)))
            .collect();
        out += &format!("\n  window     {}", steps.join(", "));
    }
    if let Some(c) = r.call {
        if !c.exemptions.is_empty() {
            // Who is NOT paying the snipe tax. We would not be on this list.
            let who: Vec<String> = c.exemptions.iter().map(short).collect();
            out += &format!("\n  exempt     {}", who.join(", "));
        }
    }

    out += &format!("\n  tx         {}", short_tx(&any.tx));
    match (r.call, r.via) {
        (Some(c), _) => out += &format!("   via {}", c.via),
        // Said out loud rather than left blank: this launch is followed and
        // priced like any other, but nobody could read who was exempted from
        // the snipe tax or whether the maker bought their own launch.
        (None, Some(v)) => out += &format!("   via {v} (terms read off the curve)"),
        (None, None) => {}
    }
    // Only when something unexpected said it. In the ordinary case these are
    // the two known contracts on every single entry, which is a line nobody
    // reads and everybody scrolls past.
    let pads: Vec<&Address> = [r.created.map(|l| &l.pad), r.dev.map(|l| &l.pad)]
        .into_iter()
        .flatten()
        .filter(|p| !is_known(p))
        .collect();
    if !pads.is_empty() {
        let who: Vec<String> = pads.into_iter().map(short).collect();
        out += &format!("\n  pads       {}", who.join(", "));
    }
    out
}

/// The launch as its own transaction asked for it.
///
/// Everything here is in the calldata and in **no log**: the token's name, what
/// the creator taxes every trade, and who the launcher exempted from the snipe
/// tax. A sniper is not on that exemption list, so reading it is reading who
/// else is not paying what we would pay.
///
/// The four entry points differ only after the token parameters, so all of them
/// decode here: `launchToken` with and without exemptions, `launchTokenFor`,
/// and the launcher's `launchAndBuy`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchCall {
    /// Which entry point it came in through.
    pub via: &'static str,
    pub name: String,
    pub symbol: String,
    /// What the curve will trade in. Second and third argument of every entry
    /// point, so both are had the same way whichever one was called - and from
    /// the feed this is the only place they exist, the factory's log not having
    /// happened yet.
    pub pair_token: Address,
    pub launch_config_id: U256,
    pub creator_fee_recipient: Address,
    /// What the creator takes off every trade, in basis points. Paid on the way
    /// out as well as in.
    pub creator_tax_bps: u16,
    pub buyback_enabled: bool,
    /// `launchAndBuy` only: the dev buy as it was asked for, before the chain
    /// answered. `min_tokens_out` is the launcher's own slippage opinion.
    pub quote_in: Option<U256>,
    pub min_tokens_out: Option<U256>,
    pub recipient: Option<Address>,
    /// Addresses the launcher declared exempt from the snipe tax. `launchAndBuy`
    /// adds its own recipient to this on chain, so the list as sent can be one
    /// short of the list that ends up in force.
    pub exemptions: Vec<Address>,
}

/// `PonsV2LaunchFactory.TokenParams`, as a type to decode against.
fn token_params_type() -> ParamType {
    ParamType::Tuple(vec![
        ParamType::String,                            // name
        ParamType::String,                            // symbol
        ParamType::String,                            // logo
        ParamType::String,                            // description
        ParamType::Tuple(vec![ParamType::String; 5]), // socials
        ParamType::Address,                           // creatorFeeRecipient
        ParamType::Uint(16),                          // creatorTaxBps
        ParamType::Bool,                              // buybackEnabled
        ParamType::FixedBytes(32),                    // expectedEconomics
        ParamType::FixedBytes(32),                    // salt
    ])
}

/// The tuple above, spelled the way a selector is hashed from.
const TP: &str = "(string,string,string,string,(string,string,string,string,string),address,\
                  uint16,bool,bytes32,bytes32)";

fn four(sig: &str) -> [u8; 4] {
    let s = crate::pool::selector(sig);
    [s[0], s[1], s[2], s[3]]
}

/// Every way a launch is asked for, and the arguments each carries.
///
/// One list, so that what is decoded and what is recognised cannot drift apart
/// - and so a test can hold these signatures against the ABIs in `abi/`.
fn entry_points() -> Vec<(&'static str, String, Vec<ParamType>)> {
    let tp = token_params_type();
    let u = ParamType::Uint(256);
    let a = ParamType::Address;
    let addrs = ParamType::Array(Box::new(ParamType::Address));
    vec![
        (
            "launchAndBuy",
            format!("launchAndBuy({TP},uint256,address,uint256,uint256,address,address[])"),
            vec![
                tp.clone(),
                u.clone(),
                a.clone(),
                u.clone(),
                u.clone(),
                a.clone(),
                addrs.clone(),
            ],
        ),
        (
            "launchTokenFor",
            format!("launchTokenFor({TP},uint256,address,address,address[])"),
            vec![tp.clone(), u.clone(), a.clone(), a.clone(), addrs.clone()],
        ),
        (
            "launchToken",
            format!("launchToken({TP},uint256,address,address[])"),
            vec![tp.clone(), u.clone(), a.clone(), addrs],
        ),
        (
            "launchToken",
            format!("launchToken({TP},uint256,address)"),
            vec![tp, u, a],
        ),
    ]
}

/// Decode a launch transaction's calldata.
///
/// The whole argument list is decoded, not just the part that is wanted: a
/// transaction whose tail does not fit the signature it claims is not a launch
/// this understands, and reading the head of it anyway is how a lie about a
/// token gets believed.
pub fn decode_call(input: &[u8]) -> Result<LaunchCall> {
    anyhow::ensure!(
        input.len() >= 4,
        "calldata too short: {} bytes",
        input.len()
    );
    let sel = &input[..4];
    let (via, _, types) = entry_points()
        .into_iter()
        .find(|(_, sig, _)| sel == four(sig))
        .with_context(|| format!("not a launch call: selector 0x{}", hex::encode(sel)))?;

    let args = ethers::abi::decode(&types, &input[4..]).context("decoding launch calldata")?;
    let params = match args.first() {
        Some(Token::Tuple(t)) if t.len() == 10 => t.clone(),
        _ => anyhow::bail!("launch calldata did not carry TokenParams"),
    };
    let string_at = |i: usize| match &params[i] {
        Token::String(s) => Ok(s.clone()),
        t => Err(anyhow::anyhow!(
            "TokenParams[{i}] is {t:?}, expected a string"
        )),
    };
    let tax = match &params[6] {
        Token::Uint(v) if *v <= U256::from(u16::MAX) => v.low_u32() as u16,
        t => anyhow::bail!("creatorTaxBps is {t:?}"),
    };
    let list_at = |i: usize| -> Vec<Address> {
        match args.get(i) {
            Some(Token::Array(v)) => v.iter().filter_map(|t| t.clone().into_address()).collect(),
            _ => Vec::new(),
        }
    };
    let uint_at = |i: usize| args.get(i).and_then(|t| t.clone().into_uint());
    let atomic = via == "launchAndBuy";

    Ok(LaunchCall {
        via,
        name: string_at(0)?,
        symbol: string_at(1)?,
        launch_config_id: uint_at(1).context("launchConfigId missing")?,
        pair_token: args
            .get(2)
            .and_then(|t| t.clone().into_address())
            .context("pairToken is not an address")?,
        creator_fee_recipient: params[5]
            .clone()
            .into_address()
            .context("creatorFeeRecipient is not an address")?,
        creator_tax_bps: tax,
        buyback_enabled: matches!(params[7], Token::Bool(true)),
        quote_in: atomic.then(|| uint_at(3)).flatten(),
        min_tokens_out: atomic.then(|| uint_at(4)).flatten(),
        recipient: atomic
            .then(|| args.get(5).and_then(|t| t.clone().into_address()))
            .flatten(),
        exemptions: match via {
            "launchAndBuy" => list_at(6),
            "launchTokenFor" => list_at(4),
            _ => list_at(3),
        },
    })
}

/// An `address` topic is the address in the low 20 bytes of the word.
fn topic_address(t: &H256) -> Address {
    Address::from_slice(&t.as_bytes()[12..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known answers. A topic is what the subscription asks for, so getting one
    /// wrong is a bot that hears nothing and says nothing about it - the one
    /// failure that looks exactly like a quiet chain.
    #[test]
    fn the_topics_are_the_signature_hashes() {
        assert_eq!(
            format!("{:?}", token_launched_topic()),
            "0x8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607"
        );
        assert_eq!(
            format!("{:?}", launched_topic()),
            "0xdcacba5e347ae7abd91cb519eb877af8fa7774e347b85dd3ddcd24a2ba8cdf37"
        );
        // Same six types in the same order, different names: only the hash
        // tells them apart, and decoding reads the words differently.
        assert_ne!(token_launched_topic(), launched_topic());
    }

    /// The event signature, rebuilt from the ABI checked in beside this file.
    fn from_abi(json: &str, event: &str) -> (String, Vec<bool>) {
        let abi: serde_json::Value = serde_json::from_str(json).unwrap();
        let e = abi
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "event" && e["name"] == event)
            .unwrap_or_else(|| panic!("{event} is not in this abi"));
        let inputs = e["inputs"].as_array().unwrap();
        let types: Vec<&str> = inputs.iter().map(|i| i["type"].as_str().unwrap()).collect();
        let indexed: Vec<bool> = inputs
            .iter()
            .map(|i| i["indexed"].as_bool().unwrap())
            .collect();
        (format!("{event}({})", types.join(",")), indexed)
    }

    /// The topics this subscribes with are hand-written from two ABIs, and a
    /// hand-written signature is exactly the kind of thing that is right until
    /// somebody redeploys. The ABIs live in `abi/` so that this can check them:
    /// a renamed argument type, an argument added, and this fails here rather
    /// than by hearing nothing on a chain that is busy.
    #[test]
    fn the_signatures_match_the_saved_abis() {
        let (sig, indexed) = from_abi(
            include_str!("../abi/PonsV2LaunchFactory.json"),
            "TokenLaunched",
        );
        assert_eq!(sig, TOKEN_LAUNCHED_SIG);
        // `decode` reads three addresses out of the topics and three words out
        // of the data, for both events. Which arguments are indexed decides
        // that, and it is not part of the signature or of the topic hash - so
        // it could change under us without a single topic changing.
        assert_eq!(indexed, vec![true, true, true, false, false, false]);

        let (sig, indexed) = from_abi(include_str!("../abi/PonsV2LaunchAndBuy.json"), "Launched");
        assert_eq!(sig, LAUNCHED_SIG);
        assert_eq!(indexed, vec![true, true, true, false, false, false]);
    }

    #[test]
    fn the_known_addresses_parse() {
        for pad in KNOWN_PADS {
            assert!(pad.parse::<Address>().is_ok(), "{pad}");
        }
        // The launcher is a caller of the factory, never the factory itself;
        // watching only the wrong one of the two is a feed that stays silent.
        assert_ne!(
            PONS_V2.parse::<Address>().unwrap(),
            PONS_V2_FACTORY.parse::<Address>().unwrap()
        );
    }

    fn word(tail: &str) -> H256 {
        let mut w = [0u8; 32];
        let b = hex::decode(tail).unwrap();
        w[32 - b.len()..].copy_from_slice(&b);
        H256::from(w)
    }

    /// The launch a log turned into, or a panic - every log these tests build
    /// is a launch.
    fn launch_of(log: &Log) -> Launch {
        match decode(log).unwrap() {
            Heard::Launch(l) => *l,
            other => panic!("{other:?}"),
        }
    }

    fn log(pad: &str, topic0: H256, addr: &str, first: u64, second: u64) -> Log {
        let mut data = Vec::new();
        data.extend_from_slice(word(addr).as_bytes());
        let mut w = [0u8; 32];
        U256::from(first).to_big_endian(&mut w);
        data.extend_from_slice(&w);
        U256::from(second).to_big_endian(&mut w);
        data.extend_from_slice(&w);
        Log {
            address: pad.parse().unwrap(),
            topics: vec![
                topic0,
                word("1111111111111111111111111111111111111111"),
                word("2222222222222222222222222222222222222222"),
                word("3333333333333333333333333333333333333333"),
            ],
            data: data.into(),
            block_number: Some(7u64.into()),
            transaction_hash: Some(H256::repeat_byte(9)),
            ..Default::default()
        }
    }

    #[test]
    fn the_factory_log_decodes_to_its_parts() {
        let l = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "4444444444444444444444444444444444444444",
            3,
            1_000_000_000_000_000_000,
        ));
        assert_eq!(l.pad, PONS_V2_FACTORY.parse::<Address>().unwrap());
        assert_eq!(
            format!("{:?}", l.token),
            "0x1111111111111111111111111111111111111111"
        );
        assert_eq!(
            format!("{:?}", l.curve),
            "0x2222222222222222222222222222222222222222"
        );
        assert_eq!(l.block, 7);
        match l.what {
            What::Created {
                deployer,
                pair_token,
                launch_config_id,
                graduation_threshold,
            } => {
                assert_eq!(
                    format!("{deployer:?}"),
                    "0x3333333333333333333333333333333333333333"
                );
                assert_eq!(
                    format!("{pair_token:?}"),
                    "0x4444444444444444444444444444444444444444"
                );
                assert_eq!(launch_config_id, U256::from(3));
                assert_eq!(graduation_threshold, U256::exp10(18));
            }
            other => panic!("{other:?}"),
        }
    }

    /// The two events share a layout, so reading one as the other is silent:
    /// a `launchConfigId` would be spent as an amount and a `pairToken` read as
    /// the launcher. Only topic0 separates them.
    #[test]
    fn the_launcher_log_decodes_as_a_dev_buy() {
        let l = launch_of(&log(
            PONS_V2,
            launched_topic(),
            "5555555555555555555555555555555555555555",
            1_500_000_000_000_000_000,
            42_000,
        ));
        match l.what {
            What::DevBuy {
                recipient,
                launcher,
                quote_spent,
                tokens_received,
            } => {
                assert_eq!(
                    format!("{recipient:?}"),
                    "0x3333333333333333333333333333333333333333"
                );
                assert_eq!(
                    format!("{launcher:?}"),
                    "0x5555555555555555555555555555555555555555"
                );
                assert_eq!(quote_spent, U256::from(1_500_000_000_000_000_000u64));
                assert_eq!(tokens_received, U256::from(42_000));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A log that does not fit is not this event, whatever its topic0 says.
    /// Reading one anyway is how a sniper is handed an address of somebody
    /// else's choosing.
    #[test]
    fn a_log_that_does_not_fit_is_refused() {
        let mut short = log(PONS_V2_FACTORY, token_launched_topic(), "11", 1, 2);
        short.data = vec![0u8; 64].into();
        assert!(decode(&short).is_err());

        let mut untopiced = log(PONS_V2_FACTORY, token_launched_topic(), "11", 1, 2);
        untopiced.topics.truncate(2);
        assert!(decode(&untopiced).is_err());

        let mut stranger = log(PONS_V2_FACTORY, token_launched_topic(), "11", 1, 2);
        stranger.topics[0] = H256::repeat_byte(1);
        assert!(decode(&stranger).is_err());
    }

    /// The feed hands over bytes, not JSON-RPC: a batch of length-prefixed
    /// messages, each of which may be another batch. Walking it wrong loses
    /// transactions silently, which on this path means missing a launch.
    #[test]
    fn a_batch_yields_every_transaction_in_it() {
        let one = [L2_SIGNED_TX, 0xaa, 0xbb];
        let two = [L2_SIGNED_TX, 0xcc];
        let mut batch = vec![L2_BATCH];
        for m in [&one[..], &two[..]] {
            batch.extend_from_slice(&(m.len() as u64).to_be_bytes());
            batch.extend_from_slice(m);
        }
        let mut out = Vec::new();
        raw_txs(&batch, 0, &mut out);
        assert_eq!(out, vec![&[0xaa, 0xbb][..], &[0xcc][..]]);

        // A length that runs past the end is a truncated frame, not a reason to
        // read somebody else's memory or to panic.
        let mut bad = vec![L2_BATCH];
        bad.extend_from_slice(&u64::MAX.to_be_bytes());
        bad.push(L2_SIGNED_TX);
        let mut out = Vec::new();
        raw_txs(&bad, 0, &mut out);
        assert!(out.is_empty());

        // Nothing at all, and a kind that carries no transaction.
        let mut out = Vec::new();
        raw_txs(&[], 0, &mut out);
        raw_txs(&[9, 9, 9], 0, &mut out);
        assert!(out.is_empty());
    }

    /// One signed transaction, straight off the wire: decoded, recognised as a
    /// launch by where it is going and what it carries, and its sender
    /// recovered. Everything the feed path does before it says a word.
    #[test]
    fn a_signed_launch_transaction_is_found_in_the_feed() {
        use ethers::signers::{LocalWallet, Signer};
        use ethers::types::transaction::eip2718::TypedTransaction;
        use ethers::types::TransactionRequest;

        let wallet: LocalWallet =
            "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318"
                .parse()
                .unwrap();
        let pad: Address = PONS_V2.parse().unwrap();
        let (_, sig, _) = entry_points().into_iter().next().unwrap();
        let data = calldata(
            &sig,
            &[
                params_token("Foo Coin", "FOO", 250, false),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                Token::Uint(U256::exp10(17)),
                Token::Uint(U256::zero()),
                Token::Address(wallet.address()),
                Token::Array(vec![]),
            ],
        );
        let req: TypedTransaction = TransactionRequest::new()
            .to(pad)
            .data(data)
            .nonce(0)
            .gas(3_000_000)
            .gas_price(1)
            .chain_id(4663u64)
            .into();
        let signature = wallet.sign_transaction_sync(&req).unwrap();
        let raw = req.rlp_signed(&signature);

        let found = launch_in_tx(&raw, 57_136_763, 1_788_814_627, &[pad], &mut Funnel::default())
            .expect("a launch");
        assert_eq!(found.seq, 57_136_763);
        assert_eq!(found.chain_time, 1_788_814_627);
        assert_eq!(found.to, pad);
        assert_eq!(found.from, Some(wallet.address()));
        assert_eq!(found.call.name, "Foo Coin");
        assert_eq!(found.call.quote_in, Some(U256::exp10(17)));
        // The hash the RPC will report for it, which is what the log for this
        // launch will be matched on. Getting it wrong costs the lead time and
        // one wasted request, silently.
        assert_eq!(found.tx, req.hash(&signature));

        // A launch sent somewhere we do not trust is not our launch.
        assert!(launch_in_tx(
            &raw,
            1,
            0,
            &[PONS_V2_FACTORY.parse().unwrap()],
            &mut Funnel::default()
        )
        .is_none());
        // And somebody else's transaction to the same contract is not a launch.
        let other: TypedTransaction = TransactionRequest::new()
            .to(pad)
            .data(vec![0xde, 0xad, 0xbe, 0xef])
            .nonce(0)
            .gas(21_000)
            .gas_price(1)
            .chain_id(4663u64)
            .into();
        let s2 = wallet.sign_transaction_sync(&other).unwrap();
        assert!(
            launch_in_tx(&other.rlp_signed(&s2), 1, 0, &[pad], &mut Funnel::default()).is_none()
        );
    }

    /// The launchpad's own numbers, against the curve's own arithmetic.
    ///
    /// `elapsed` is `block.timestamp - launchedAt` and `block.timestamp` is
    /// whole seconds, so this is a staircase and not a curve. Reading it as a
    /// curve - interpolating between the steps - would say a buy 200ms after
    /// the launch pays about 92%, when what it really pays is 99%: the entire
    /// first second is one step.
    #[test]
    fn the_snipe_tax_is_a_step_per_second() {
        let tax = SnipeTax {
            start_bps: 9900,
            seconds: 3,
        };
        assert_eq!(snipe_tax_bps(&tax, 0), 9900); // 99%
        assert_eq!(snipe_tax_bps(&tax, 1), 618); // 6.18%
        assert_eq!(snipe_tax_bps(&tax, 2), 19); // 0.19%
        assert_eq!(snipe_tax_bps(&tax, 3), 0);
        assert_eq!(snipe_tax_bps(&tax, 4), 0);

        // Each step is one shift, and the shift is integer division: 9900 >> 4
        // is 618 rather than 618.75, which is the contract's answer and so is
        // ours.
        assert_eq!(9900u64 >> 4, 618);
        assert_eq!(9900u64 >> 9, 19);
    }

    /// A window long enough for the shift to run out, and the degenerate
    /// settings that must not divide by zero or shift past the width.
    #[test]
    fn the_tax_never_divides_by_zero_or_shifts_off_the_end() {
        let long = SnipeTax {
            start_bps: 9900,
            seconds: 60,
        };
        assert_eq!(snipe_tax_bps(&long, 0), 9900);
        // 13 is the largest shift the formula can produce: elapsed is at most
        // window - 1, so (elapsed * 14) / window is at most 13.
        assert_eq!(snipe_tax_bps(&long, 59), 9900 >> 13);
        for s in 0..60 {
            assert!(snipe_tax_bps(&long, s) <= 9900);
        }

        assert_eq!(
            snipe_tax_bps(
                &SnipeTax {
                    start_bps: 0,
                    seconds: 3
                },
                0
            ),
            0
        );
        assert_eq!(
            snipe_tax_bps(
                &SnipeTax {
                    start_bps: 9900,
                    seconds: 0
                },
                0
            ),
            0
        );
        assert_eq!(
            snipe_tax_bps(
                &SnipeTax {
                    start_bps: 9900,
                    seconds: 1
                },
                0
            ),
            9900
        );
        assert_eq!(
            snipe_tax_bps(
                &SnipeTax {
                    start_bps: u64::MAX,
                    seconds: 2
                },
                1
            ),
            u64::MAX >> 7
        );
    }

    #[test]
    fn the_schedule_is_every_step_and_then_free() {
        let line = snipe_tax_line(&SnipeTax {
            start_bps: 9900,
            seconds: 3,
        });
        assert_eq!(line, "+0s 9900 bps, +1s 618 bps, +2s 19 bps, +3s free");
    }

    /// The tax port reads the curve's state, so the curve's ABI is where its
    /// assumptions are checked: that these are the names it keeps them under,
    /// that the tax is asked per recipient, and that a buy reports the tax it
    /// was actually charged - which is the only thing that can ever prove the
    /// port right.
    #[test]
    fn the_curve_still_says_what_the_tax_port_assumes() {
        let abi: serde_json::Value =
            serde_json::from_str(include_str!("../abi/PonsV2BondingCurve.json")).unwrap();
        let items = abi.as_array().unwrap();
        let has =
            |kind: &str, name: &str| items.iter().any(|i| i["type"] == kind && i["name"] == name);
        for f in [
            "currentSnipeTaxBps",
            "snipeTaxStartBps",
            "snipeTaxSeconds",
            "launchedAt",
            "snipeTaxExempt",
        ] {
            assert!(has("function", f), "the curve no longer has {f}");
        }

        // `buy` is what a snipe would call, and its shape is the one thing here
        // that a wrong assumption would burn money on.
        let buy = items
            .iter()
            .find(|i| i["type"] == "function" && i["name"] == "buy")
            .expect("buy");
        let args: Vec<&str> = buy["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["type"].as_str().unwrap())
            .collect();
        assert_eq!(args, ["uint256", "uint256", "address"]);
        assert_eq!(buy["stateMutability"], "payable");

        // The tax actually charged, in the log of every buy.
        let curve_buy = items
            .iter()
            .find(|i| i["type"] == "event" && i["name"] == "CurveBuy")
            .expect("CurveBuy");
        let fields: Vec<&str> = curve_buy["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            fields,
            ["buyer", "recipient", "quoteIn", "tokensOut", "fee", "tax"]
        );
    }

    /// A settings change is one word and no topics but its own, and it must not
    /// be read through the launch decoder that sits next to it.
    #[test]
    fn a_settings_log_decodes_to_the_setting() {
        let mut w = [0u8; 32];
        U256::from(1500u64).to_big_endian(&mut w);
        let mut l = Log {
            address: PONS_V2_FACTORY.parse().unwrap(),
            topics: vec![snipe_tax_start_topic()],
            data: w.to_vec().into(),
            block_number: Some(7u64.into()),
            transaction_hash: Some(H256::zero()),
            ..Default::default()
        };
        assert!(matches!(decode(&l).unwrap(), Heard::SnipeTaxStartBps(1500)));

        l.topics = vec![snipe_tax_seconds_topic()];
        assert!(matches!(decode(&l).unwrap(), Heard::SnipeTaxSeconds(1500)));

        // A value too big to hold is not one to act on. Keeping its low 64 bits
        // would make a tax of 2^64+1 bps look like a tax of one.
        l.data = vec![0xffu8; 32].into();
        assert!(decode(&l).is_err());
    }

    fn params_token(name: &str, symbol: &str, tax: u16, buyback: bool) -> Token {
        Token::Tuple(vec![
            Token::String(name.to_string()),
            Token::String(symbol.to_string()),
            Token::String("ipfs://logo".to_string()),
            Token::String("a description".to_string()),
            Token::Tuple(vec![Token::String(String::new()); 5]),
            Token::Address("6666666666666666666666666666666666666666".parse().unwrap()),
            Token::Uint(tax.into()),
            Token::Bool(buyback),
            Token::FixedBytes(vec![0u8; 32]),
            Token::FixedBytes(vec![1u8; 32]),
        ])
    }

    fn calldata(sig: &str, args: &[Token]) -> Vec<u8> {
        let mut out = four(sig).to_vec();
        out.extend(ethers::abi::encode(args));
        out
    }

    /// `creatorTaxBps` and the exemption list are in the calldata and in no
    /// log at all - and the exemption list is who does NOT pay the snipe tax
    /// that we would.
    #[test]
    fn a_launch_and_buy_calldata_decodes() {
        let exempt: Address = "7777777777777777777777777777777777777777".parse().unwrap();
        let recipient: Address = "8888888888888888888888888888888888888888".parse().unwrap();
        let (_, sig, _) = entry_points().into_iter().next().unwrap();
        let data = calldata(
            &sig,
            &[
                params_token("Foo Coin", "FOO", 250, true),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                Token::Uint(U256::exp10(17)),
                Token::Uint(U256::from(1234u64)),
                Token::Address(recipient),
                Token::Array(vec![Token::Address(exempt)]),
            ],
        );

        let c = decode_call(&data).unwrap();
        assert_eq!(c.via, "launchAndBuy");
        assert_eq!(c.name, "Foo Coin");
        assert_eq!(c.symbol, "FOO");
        assert_eq!(c.creator_tax_bps, 250);
        assert!(c.buyback_enabled);
        assert_eq!(c.quote_in, Some(U256::exp10(17)));
        assert_eq!(c.min_tokens_out, Some(U256::from(1234u64)));
        assert_eq!(c.recipient, Some(recipient));
        assert_eq!(c.exemptions, vec![exempt]);
    }

    /// The three factory entry points carry their arguments in different
    /// places, and the exemption list is at a different index in each.
    #[test]
    fn every_entry_point_finds_its_exemptions() {
        let exempt: Address = "7777777777777777777777777777777777777777".parse().unwrap();
        let list = Token::Array(vec![Token::Address(exempt)]);
        let eps = entry_points();

        let (_, sig, _) = &eps[1];
        let data = calldata(
            sig,
            &[
                params_token("A", "A", 0, false),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                Token::Address(exempt),
                list.clone(),
            ],
        );
        let c = decode_call(&data).unwrap();
        assert_eq!(c.via, "launchTokenFor");
        assert_eq!(c.exemptions, vec![exempt]);
        // Only the atomic path buys, so only it has an amount to report.
        assert_eq!(c.quote_in, None);

        let (_, sig, _) = &eps[2];
        let data = calldata(
            sig,
            &[
                params_token("A", "A", 0, false),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                list,
            ],
        );
        assert_eq!(decode_call(&data).unwrap().exemptions, vec![exempt]);

        let (_, sig, _) = &eps[3];
        let data = calldata(
            sig,
            &[
                params_token("A", "A", 0, false),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
            ],
        );
        let c = decode_call(&data).unwrap();
        assert!(c.exemptions.is_empty());
    }

    /// Somebody else's transaction, and a truncated one. Neither is a launch,
    /// and neither may half-decode into one.
    #[test]
    fn foreign_calldata_is_refused() {
        assert!(decode_call(&[]).is_err());
        assert!(decode_call(&[0xde, 0xad, 0xbe, 0xef]).is_err());
        let (_, sig, _) = entry_points().into_iter().next().unwrap();
        let mut data = calldata(
            &sig,
            &[
                params_token("A", "A", 0, false),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                Token::Uint(0.into()),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                Token::Array(vec![]),
            ],
        );
        data.truncate(data.len() - 32);
        assert!(decode_call(&data).is_err());
    }

    /// The signatures the selectors are hashed from, against the ABIs. A
    /// renamed or reordered argument moves the selector, and a launch would
    /// then simply never decode - silently, because a report has no idea what
    /// it did not print.
    #[test]
    fn the_call_signatures_match_the_saved_abis() {
        fn sigs(json: &str, name: &str) -> Vec<String> {
            fn canon(t: &serde_json::Value) -> String {
                match t["type"].as_str().unwrap() {
                    ty if ty.starts_with("tuple") => {
                        let inner: Vec<String> = t["components"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(canon)
                            .collect();
                        format!("({}){}", inner.join(","), ty.trim_start_matches("tuple"))
                    }
                    ty => ty.to_string(),
                }
            }
            let abi: serde_json::Value = serde_json::from_str(json).unwrap();
            abi.as_array()
                .unwrap()
                .iter()
                .filter(|f| f["type"] == "function" && f["name"] == name)
                .map(|f| {
                    let args: Vec<String> =
                        f["inputs"].as_array().unwrap().iter().map(canon).collect();
                    format!("{name}({})", args.join(","))
                })
                .collect()
        }

        let factory = include_str!("../abi/PonsV2LaunchFactory.json");
        let launcher = include_str!("../abi/PonsV2LaunchAndBuy.json");
        let mut from_abi = sigs(factory, "launchToken");
        from_abi.extend(sigs(factory, "launchTokenFor"));
        from_abi.extend(sigs(launcher, "launchAndBuy"));
        from_abi.sort();

        let mut ours: Vec<String> = entry_points().into_iter().map(|(_, sig, _)| sig).collect();
        ours.sort();
        assert_eq!(ours, from_abi);
    }

    fn usdg() -> Quote {
        Quote {
            decimals: 6,
            symbol: "USDG".to_string(),
        }
    }

    /// The one that bit: USDG has six decimals, and a graduation of 8090 USDG
    /// printed at eighteen reads 0.00000000809 - a number, wrong by a million
    /// million, that nothing about the line says to distrust.
    #[test]
    fn amounts_are_in_the_pair_token_own_units() {
        let created = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "5fc5360d0400a0fd4f2af552add042d716f1d168",
            0,
            8_090_000_000,
        ));
        let out = render(&Report {
            created: Some(&created),
            quote: Some(&usdg()),
            ..Default::default()
        });
        assert!(out.contains("graduates at 8090 USDG"), "{out}");
        assert!(out.contains("pair       USDG"), "{out}");
    }

    /// Without the pair token there is no honest way to scale an amount, and
    /// eighteen is a guess that looks like an answer.
    #[test]
    fn an_unknown_pair_token_prints_raw() {
        let created = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "11",
            0,
            8_090_000_000,
        ));
        let out = render(&Report {
            created: Some(&created),
            ..Default::default()
        });
        assert!(out.contains("graduates at 8090000000 raw"), "{out}");
    }

    /// One transaction, one entry. Two would read as two launches.
    #[test]
    fn both_logs_of_one_transaction_render_as_one_launch() {
        let created = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "5fc5360d0400a0fd4f2af552add042d716f1d168",
            0,
            8_090_000_000,
        ));
        let dev = launch_of(&log(
            PONS_V2,
            launched_topic(),
            "3333333333333333333333333333333333333333",
            248_710_096,
            1_000_000_000_000_000_000,
        ));
        assert_eq!(created.tx, dev.tx);

        let out = render(&Report {
            created: Some(&created),
            dev: Some(&dev),
            quote: Some(&usdg()),
            ..Default::default()
        });
        println!("\n{out}\n");
        assert_eq!(out.matches("block 7").count(), 1, "{out}");
        assert!(out.contains("dev buy    248.710096 USDG -> 1 ->"), "{out}");
        // Both logs came from contracts this knows, so nothing about where
        // they came from is worth a line.
        assert!(!out.contains("pads"), "{out}");
        // The launcher here IS the deployer, and a line repeating it is noise.
        assert!(!out.contains("launcher"), "{out}");
    }

    /// What the calldata and the settings add to an entry, neither of which is
    /// in any log.
    #[test]
    fn the_entry_carries_the_tax_it_would_pay() {
        let created = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "5fc5360d0400a0fd4f2af552add042d716f1d168",
            0,
            8_090_000_000,
        ));
        let exempt: Address = "7777777777777777777777777777777777777777".parse().unwrap();
        let call = LaunchCall {
            via: "launchAndBuy",
            name: "Foo Coin".to_string(),
            symbol: "FOO".to_string(),
            creator_fee_recipient: exempt,
            creator_tax_bps: 250,
            buyback_enabled: true,
            pair_token: "5fc5360d0400a0fd4f2af552add042d716f1d168".parse().unwrap(),
            launch_config_id: U256::zero(),
            quote_in: None,
            min_tokens_out: None,
            recipient: None,
            exemptions: vec![exempt],
        };
        let out = render(&Report {
            created: Some(&created),
            quote: Some(&usdg()),
            call: Some(&call),
            tax: Some(SnipeTax {
                start_bps: 1500,
                seconds: 60,
            }),
            ..Default::default()
        });
        assert!(out.contains("Foo Coin (FOO)"), "{out}");
        assert!(out.contains("creator tax 250 bps, buyback on"), "{out}");
        assert!(out.contains("exempt     0x777777"), "{out}");
        assert!(out.contains("via launchAndBuy"), "{out}");
        // On the title line, where it costs no line of its own.
        assert!(
            out.lines().next().unwrap().contains("tax 1500bps/60s"),
            "{out}"
        );
    }

    /// A boundary sitting near the top of a second is the case that breaks a
    /// naive average: 990 and 10 are twenty milliseconds apart, and averaging
    /// them lands at 500 - the exact opposite side of the second.
    #[test]
    fn the_boundary_is_measured_around_the_clock() {
        let (at, late) = middle_of(&[990, 995, 5, 10, 0]);
        assert_eq!(
            at, 990,
            "the earliest reading is the closest to the boundary"
        );
        assert_eq!(late, 20);

        // The boundary is where the FIRST of them saw it, not the middle: the
        // rest are late by however long the next block took.
        let (at, late) = middle_of(&[400, 410, 420, 430, 440]);
        assert_eq!(at, 400);
        assert_eq!(late, 40);

        // One sample says where it was, and that it scattered nowhere.
        assert_eq!(middle_of(&[713]), (713, 0));
    }

    /// A struct of fixed-size fields comes back inline, and one of them is
    /// signed. Reading `tickSpacing` as unsigned would turn -60 into eight
    /// million, and a pool key built from it would be a different pool.
    #[test]
    fn a_launch_config_decodes_including_its_signed_field() {
        let encoded = ethers::abi::encode(&[Token::Tuple(vec![
            Token::Uint(U256::exp10(27)),
            Token::Uint(U256::from(100u64)),
            Token::Uint(U256::from(168u64) * U256::exp10(16)),
            Token::Uint(U256::from(42u64) * U256::exp10(17)),
            Token::Uint(U256::from(10_000u64)),
            Token::Int(U256::MAX - U256::from(59u64)), // -60 in two's complement
            Token::Bool(true),
        ])]);
        let tokens = ethers::abi::decode(
            &[ParamType::Tuple(vec![
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(24),
                ParamType::Int(24),
                ParamType::Bool,
            ])],
            &encoded,
        )
        .unwrap();
        let Some(Token::Tuple(f)) = tokens.first() else {
            panic!("not a tuple")
        };
        assert_eq!(f[0].clone().into_uint().unwrap(), U256::exp10(27));
        assert_eq!(as_i24(f[5].clone().into_int().unwrap()), -60);

        // And the ordinary positive case, which is what these launches use.
        assert_eq!(as_i24(U256::from(200u64)), 200);
        assert_eq!(as_i24(U256::zero()), 0);
        assert_eq!(as_i24(U256::MAX), -1);
    }

    /// Waiting for a log that cannot come is 250ms spent on nothing - and on a
    /// three second tax window, 250ms is not nothing.
    #[test]
    fn only_the_atomic_path_is_waited_on() {
        let mut call = LaunchCall {
            via: "launchToken",
            name: "A".to_string(),
            symbol: "A".to_string(),
            pair_token: Address::zero(),
            launch_config_id: U256::zero(),
            creator_fee_recipient: Address::zero(),
            creator_tax_bps: 0,
            buyback_enabled: false,
            quote_in: None,
            min_tokens_out: None,
            recipient: None,
            exemptions: vec![],
        };
        assert!(!may_carry_dev_buy(Some(&call)));

        call.via = "launchAndBuy";
        assert!(may_carry_dev_buy(Some(&call)));
        // launchTokenFor is what the launcher calls, so a transaction sent
        // straight to it came from something we have not identified - and an
        // unidentified caller may well buy too.
        call.via = "launchTokenFor";
        assert!(may_carry_dev_buy(Some(&call)));
        // No calldata at all: not knowing is not knowing there is nothing.
        assert!(may_carry_dev_buy(None));
    }

    /// The tax schedule in seconds anybody can look at a clock and compare
    /// against - which is the only form of it a decision can be made on.
    #[test]
    fn the_window_is_printed_in_absolute_seconds() {
        let created = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "5fc5360d0400a0fd4f2af552add042d716f1d168",
            0,
            8_090_000_000,
        ));
        let out = render(&Report {
            created: Some(&created),
            quote: Some(&usdg()),
            tax: Some(SnipeTax {
                start_bps: 9900,
                seconds: 3,
            }),
            launched_at: Some(1_788_814_627),
            ..Default::default()
        });
        assert!(
            out.contains(
                "window     618 bps at 1788814628, 19 bps at 1788814629, free at 1788814630"
            ),
            "{out}"
        );

        // Without the launch second there is nothing to aim at, and a schedule
        // relative to an unknown moment is worse than none.
        let out = render(&Report {
            created: Some(&created),
            tax: Some(SnipeTax {
                start_bps: 9900,
                seconds: 3,
            }),
            ..Default::default()
        });
        assert!(!out.contains("window"), "{out}");
    }

    /// A dev buy whose launch happened before this process was listening still
    /// has to print, and must not claim to be a launch.
    #[test]
    fn a_dev_buy_on_its_own_is_not_called_a_launch() {
        let dev = launch_of(&log(
            PONS_V2,
            launched_topic(),
            "5555555555555555555555555555555555555555",
            1,
            2,
        ));
        let out = render(&Report {
            dev: Some(&dev),
            ..Default::default()
        });
        assert!(out.contains("dev buy  0x1111"), "{out}");
        assert!(out.contains("block 7"), "{out}");
        assert!(!out.contains("graduates"), "{out}");
        // Nobody said who deployed it, so the payer is worth naming.
        assert!(out.contains("launcher   0x5555"), "{out}");
    }

    /// A launch with no block number cannot be reported as having happened at
    /// one; a pending log is not a launch yet.
    #[test]
    fn a_pending_log_is_not_a_launch() {
        let mut pending = log(PONS_V2_FACTORY, token_launched_topic(), "11", 1, 2);
        pending.block_number = None;
        assert!(decode(&pending).is_err());
    }
    /// A launch through an entry point this does not decode is followed like
    /// any other now - its terms come off the curve rather than out of the
    /// calldata - and the entry has to say so. Read as an ordinary launch with
    /// blank fields it would look like one whose maker put in nothing and
    /// exempted nobody, which is the opposite of not knowing.
    #[test]
    fn a_launch_through_an_unknown_entry_point_says_so() {
        let created = launch_of(&log(
            PONS_V2_FACTORY,
            token_launched_topic(),
            "0000000000000000000000000000000000000000",
            0,
            4_200_000_000_000_000_000,
        ));
        let out = render(&Report {
            created: Some(&created),
            via: Some("unknown entry point"),
            ..Default::default()
        });
        assert!(out.contains("via unknown entry point"), "{out}");
        assert!(out.contains("terms read off the curve"), "{out}");

        // And an ordinary one still says how it was made, with no such note.
        let plain = render(&Report {
            created: Some(&created),
            ..Default::default()
        });
        assert!(!plain.contains("unknown entry point"), "{plain}");
        assert!(!plain.contains("terms read off the curve"), "{plain}");
    }

    /// The same, for a typed transaction.
    ///
    /// The test above builds a legacy one, and on those the decoder's own
    /// `hash` is right - which is why the feed matched about one launch in
    /// thirty and nothing said so. A typed transaction hashes over its type
    /// byte and its body together; the decoder fills `hash` in from the body
    /// alone, so a sighting was filed under a key no log would ever carry.
    #[test]
    fn a_typed_launch_is_filed_under_the_hash_the_chain_will_report() {
        use ethers::signers::{LocalWallet, Signer};
        use ethers::types::transaction::eip1559::Eip1559TransactionRequest;
        use ethers::types::transaction::eip2718::TypedTransaction;

        let wallet: LocalWallet =
            "4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318"
                .parse()
                .unwrap();
        let pad: Address = PONS_V2.parse().unwrap();
        let (_, sig, _) = entry_points().into_iter().next().unwrap();
        let data = calldata(
            &sig,
            &[
                params_token("Foo Coin", "FOO", 250, false),
                Token::Uint(0.into()),
                Token::Address(Address::zero()),
                Token::Uint(U256::exp10(17)),
                Token::Uint(U256::zero()),
                Token::Address(wallet.address()),
                Token::Array(vec![]),
            ],
        );

        let req: TypedTransaction = Eip1559TransactionRequest::new()
            .to(pad)
            .data(data)
            .nonce(0)
            .gas(3_000_000)
            .max_fee_per_gas(1)
            .max_priority_fee_per_gas(1)
            .chain_id(4663u64)
            .into();
        let signature = wallet.sign_transaction_sync(&req).unwrap();
        let raw = req.rlp_signed(&signature);

        let found = launch_in_tx(&raw, 1, 1, &[pad], &mut Funnel::default())
            .expect("a typed launch is still a launch");
        assert_eq!(found.call.name, "Foo Coin");
        assert_eq!(found.tx, req.hash(&signature), "filed under the wrong hash");
    }

}
