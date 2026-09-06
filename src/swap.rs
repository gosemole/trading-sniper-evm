//! Wallet-touching operations: token approvals today, swap submission next.
//!
//! Everything here is built and printed first and only sent when the caller
//! passes `--execute`, so the exact transaction can be inspected before any
//! value moves.

use crate::config::Config;
use anyhow::{Context, Result};
use ethers::middleware::SignerMiddleware;
use ethers::providers::{Http, Middleware, Provider, Ws};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{
    Address, BlockNumber, Bytes, Eip1559TransactionRequest, TransactionRequest, H256, U256,
};
use ethers::utils::keccak256;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A tip floor, for chains that report zero: a transaction still has to be
/// worth including.
const TIP_FLOOR: u64 = 1_000_000;

/// How long a block header stays usable as a gas price before falling back to
/// asking. Long enough to ride out a dropped subscription being re-established,
/// short enough that a dead watcher cannot quietly price transactions off a
/// stale number.
const FEES_STALE_AFTER_SECS: u64 = 10;

/// The gas price, kept current from the chain's own block stream rather than
/// asked for at the moment it is needed.
///
/// `base_fee_per_gas` is in every block header, so a `newHeads` subscription
/// already carries it: asking per transaction spends two round trips on a
/// number that arrived anyway, and adds a way for a buy to fail that has
/// nothing to do with the buy. The tip is not in the header and moves slowly,
/// so it is refreshed on a timer instead of per block.
pub struct FeeWatch {
    base_fee: AtomicU64,
    tip: AtomicU64,
    /// Unix seconds of the last header seen; zero means none yet.
    seen_at: AtomicU64,
}

impl Default for FeeWatch {
    fn default() -> Self {
        Self {
            base_fee: AtomicU64::new(0),
            tip: AtomicU64::new(TIP_FLOOR),
            seen_at: AtomicU64::new(0),
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl FeeWatch {
    /// Follow `newHeads` until the process ends, reconnecting on its own.
    pub fn watch(self: &Arc<Self>, ws_url: String, http: Provider<Http>) {
        let me = Arc::clone(self);
        tokio::spawn(async move {
            let mut backoff = std::time::Duration::from_secs(3);
            loop {
                match me.follow(&ws_url, &http).await {
                    Ok(()) => tracing::warn!("gas price stream closed, reconnecting"),
                    Err(e) => tracing::warn!(err = %e, "gas price stream failed, reconnecting"),
                }
                // A stale price is never used: `params` falls back to asking.
                me.seen_at.store(0, Ordering::Relaxed);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
            }
        });
    }

    async fn follow(&self, ws_url: &str, http: &Provider<Http>) -> Result<()> {
        use futures_util::StreamExt;
        let provider = Provider::<Ws>::connect(ws_url)
            .await
            .context("connect ws for gas price")?;
        let mut heads = provider.subscribe_blocks().await.context("subscribe newHeads")?;
        let mut tip_checked = 0u64;
        while let Some(block) = heads.next().await {
            if let Some(base) = block.base_fee_per_gas {
                self.base_fee.store(base.min(U256::from(u64::MAX)).as_u64(), Ordering::Relaxed);
                self.seen_at.store(now_secs(), Ordering::Relaxed);
            }
            // The tip is a separate call, and blocks here are a tenth of a
            // second apart, so it is worth far less often than every one - and
            // far less often than every five seconds, which is twelve requests
            // a minute spent on a number that moves slowly and is floored
            // anyway.
            let now = now_secs();
            if now.saturating_sub(tip_checked) >= 20 {
                tip_checked = now;
                if let Ok(tip) = http.request::<_, U256>("eth_maxPriorityFeePerGas", ()).await {
                    self.tip.store(tip.min(U256::from(u64::MAX)).as_u64(), Ordering::Relaxed);
                }
            }
        }
        Ok(())
    }

    /// Fees to sign with. Returns instantly from the last header, and only asks
    /// the node when no header is recent enough to trust.
    pub async fn params(&self, http: &Provider<Http>) -> Result<(U256, U256)> {
        let seen = self.seen_at.load(Ordering::Relaxed);
        if seen != 0 && now_secs().saturating_sub(seen) <= FEES_STALE_AFTER_SECS {
            let base = U256::from(self.base_fee.load(Ordering::Relaxed));
            let tip = U256::from(self.tip.load(Ordering::Relaxed).max(TIP_FLOOR));
            return Ok((base * 2 + tip, tip));
        }
        fee_params(http).await
    }
}

/// Canonical Permit2, deployed at the same address on every chain via the
/// deterministic deployer. Overridable in config in case this chain differs.
pub const PERMIT2_DEFAULT: &str = "0x000000000022D473030F116dDEE9F6B43aC78BA3";

fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}

fn word(v: U256) -> [u8; 32] {
    let mut b = [0u8; 32];
    v.to_big_endian(&mut b);
    b
}

fn addr_word(a: Address) -> [u8; 32] {
    let mut b = [0u8; 32];
    b[12..].copy_from_slice(a.as_bytes());
    b
}

/// `type(uint160).max` - Permit2 stores allowances as uint160.
fn max_uint160() -> U256 {
    (U256::one() << 160) - 1
}

/// `type(uint48).max` - Permit2 treats this expiration as "never expires".
fn max_uint48() -> U256 {
    (U256::one() << 48) - 1
}

/// The signing key, preferring the environment over the config file.
///
/// The wallet address is logged; the key itself never is.
pub fn load_wallet(cfg: &Config, chain_id: u64) -> Result<LocalWallet> {
    // `Config::load` already merged PRIVATE_KEY over the file, so there is one
    // place the key can come from by the time we get here.
    let source = if std::env::var_os("PRIVATE_KEY").is_some() { "PRIVATE_KEY env" } else { "config" };
    let key = cfg.private_key.expose().trim().trim_start_matches("0x");
    anyhow::ensure!(
        !key.is_empty(),
        "no signing key: set the PRIVATE_KEY environment variable, or private_key in config"
    );
    let wallet: LocalWallet = key
        .parse()
        .context("signing key is not a valid 32-byte hex private key")?;
    let wallet = wallet.with_chain_id(chain_id);
    tracing::info!(address = ?wallet.address(), source, "wallet loaded");
    Ok(wallet)
}

pub fn resolve_addr(opt: &Option<String>, default: Option<&str>, what: &str) -> Result<Address> {
    let raw = opt
        .as_deref()
        .or(default)
        .with_context(|| format!("{what} is not configured"))?;
    raw.parse()
        .with_context(|| format!("{what}: '{raw}' is not an address"))
}

/// One transaction to build, show, and maybe send.
pub struct PendingTx {
    pub label: String,
    pub to: Address,
    pub data: Bytes,
    /// Native value to attach. Only a swap whose input currency is ETH needs
    /// one; everything else moves through Permit2 and sends zero.
    pub value: U256,
}

impl PendingTx {
    pub fn print(&self, index: usize) {
        println!("  [{index}] {}", self.label);
        println!("      to    {:?}", self.to);
        if !self.value.is_zero() {
            println!("      value {} wei", self.value);
        }
        println!("      data  0x{}", hex::encode(&self.data));
    }
}

/// Unlimited approval for spending `token` through the Universal Router.
///
/// Two transactions are required, and both are one-time:
///
/// 1. the ERC-20 approves Permit2 for `type(uint256).max`;
/// 2. Permit2 approves the router for `type(uint160).max` with an expiration
///    of `type(uint48).max`.
///
/// Native ETH needs neither - it is passed as msg.value.
pub fn build_unlimited_approval(
    token: Address,
    permit2: Address,
    router: Address,
) -> Result<Vec<PendingTx>> {
    anyhow::ensure!(
        token != Address::zero(),
        "native ETH needs no approval; it is sent as msg.value"
    );

    let mut erc20 = selector("approve(address,uint256)");
    erc20.extend_from_slice(&addr_word(permit2));
    erc20.extend_from_slice(&word(U256::MAX));

    let mut p2 = selector("approve(address,address,uint160,uint48)");
    p2.extend_from_slice(&addr_word(token));
    p2.extend_from_slice(&addr_word(router));
    p2.extend_from_slice(&word(max_uint160()));
    p2.extend_from_slice(&word(max_uint48()));

    Ok(vec![
        PendingTx {
            label: format!("ERC20.approve(Permit2, MAX)  token {token:?}"),
            to: token,
            data: Bytes::from(erc20),
            value: U256::zero(),
        },
        PendingTx {
            label: "Permit2.approve(token, router, uint160 MAX, never expires)".to_string(),
            to: permit2,
            data: Bytes::from(p2),
            value: U256::zero(),
        },
    ])
}

/// EIP-1559 fees with enough headroom that a rising base fee does not strand
/// the transaction.
///
/// The base fee can climb 12.5% per block, so paying twice the current one
/// covers roughly six blocks of continuous growth. The tip is whatever the node
/// suggests, with a small floor for chains that report zero.
pub async fn fee_params(http: &Provider<Http>) -> Result<(U256, U256)> {
    // Two independent questions, so one round trip rather than two.
    let (block, tip) = tokio::join!(
        http.get_block(BlockNumber::Latest),
        http.request::<_, U256>("eth_maxPriorityFeePerGas", ()),
    );
    let base = block
        .context("fetching latest block for base fee")?
        .context("latest block missing")?
        .base_fee_per_gas
        .unwrap_or_default();
    // A chain that reports no tip still needs one to be included.
    let tip = tip
        .unwrap_or_else(|_| U256::from(TIP_FLOOR))
        .max(U256::from(TIP_FLOOR));
    Ok((base * 2 + tip, tip))
}

/// Broadcast one transaction and return the moment the node has it.
///
/// Nonce, fees and gas limit are all supplied rather than looked up, because
/// each of those lookups is a round trip and none of them has to happen here:
/// the nonce is counted locally (asking the node returns the same number twice
/// while the first transaction is unmined), the fees can be fetched alongside
/// whatever else the caller is waiting for, and the gas limit belongs to the
/// shape of the transaction rather than to this moment. Nothing waits for a
/// receipt either - the transaction is on its way regardless, and waiting is
/// the part that would hold everything else up.
pub async fn send_nowait(
    to: &Broadcaster,
    wallet: &LocalWallet,
    tx: &PendingTx,
    nonce: U256,
    fees: (U256, U256),
    gas_limit: U256,
) -> Result<H256> {
    let (max_fee, tip) = fees;
    let req = Eip1559TransactionRequest::new()
        .from(wallet.address())
        .to(tx.to)
        .data(tx.data.clone())
        .value(tx.value)
        .chain_id(wallet.chain_id())
        .nonce(nonce)
        .gas(gas_limit)
        .max_fee_per_gas(max_fee)
        .max_priority_fee_per_gas(tip);
    let typed: TypedTransaction = req.into();
    let sig = wallet
        .sign_transaction(&typed)
        .await
        .context("signing transaction")?;
    // The hash is a property of the signed bytes, not of whoever accepted
    // them: every endpoint is handed the identical transaction and would
    // report the identical hash. Computing it here means the hash is known
    // even when the endpoint that accepted it answers slowly, or answers
    // oddly, and it is the same hash the receipt will be found under.
    let raw = typed.rlp_signed(&sig);
    let hash = H256::from(ethers::utils::keccak256(&raw));
    to.send(raw, hash).await
}

/// Somewhere to submit a signed transaction - one endpoint or several.
///
/// Submission is the only round trip a buy actually waits on, and it is the one
/// that decides whether the trade exists at all. A single endpoint makes that a
/// single point of failure and a single queue to sit in; several, written to at
/// once, mean the transaction is in the fastest mempool that answered rather
/// than in whichever one happened to be configured.
///
/// Everything else - calls, gas, receipts - still goes to the one HTTP endpoint
/// this is built alongside. Only the broadcast fans out, because only the
/// broadcast benefits: a read answered twice is the same answer.
pub struct Broadcaster {
    endpoints: Vec<Endpoint>,
}

struct Endpoint {
    http: Provider<Http>,
    /// Scheme and host only. Endpoint URLs carry API keys in their path or
    /// query, and a log line is exactly the place one should not appear.
    label: String,
}

/// Scheme and host of a URL, with any credential-bearing path, query or
/// userinfo dropped. Falls back to a fixed placeholder rather than to the URL
/// itself, so a URL this cannot parse still cannot leak a key into a log.
fn endpoint_label(url: &str) -> String {
    let rest = url.split("://").nth(1).unwrap_or(url);
    let scheme = url.split("://").next().unwrap_or("");
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Anything before an `@` is userinfo, which is a credential.
    let host = authority.rsplit('@').next().unwrap_or("");
    match (scheme.is_empty() || scheme == url, host.is_empty()) {
        (_, true) => "endpoint".to_string(),
        (true, false) => host.to_string(),
        (false, false) => format!("{scheme}://{host}"),
    }
}

impl Broadcaster {
    /// Build from a list of endpoint URLs, in the order they are preferred for
    /// reporting. An empty list is a configuration error rather than a silent
    /// no-op: nothing could ever be sent.
    pub fn new(urls: &[String]) -> Result<Self> {
        anyhow::ensure!(!urls.is_empty(), "no endpoint to broadcast through");
        let mut endpoints = Vec::with_capacity(urls.len());
        for url in urls {
            let http = Provider::<Http>::try_from(url.clone())
                .with_context(|| format!("bad submit endpoint '{}'", endpoint_label(url)))?;
            let label = endpoint_label(url);
            endpoints.push(Endpoint { http, label });
        }
        Ok(Self { endpoints })
    }

    /// How many endpoints a broadcast reaches.
    pub fn width(&self) -> usize {
        self.endpoints.len()
    }

    /// The endpoints this will submit through, for logging at startup.
    pub fn labels(&self) -> Vec<&str> {
        self.endpoints.iter().map(|e| e.label.as_str()).collect()
    }

    /// Keep every submission connection warm, for the reason `keep_warm`
    /// explains: a cold connection pays a TLS handshake on exactly the request
    /// a buy is waiting for, and these are the requests a buy waits for.
    pub fn keep_warm(self: &Arc<Self>) {
        for (i, _) in self.endpoints.iter().enumerate() {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                    let _ = me.endpoints[i].http.get_chainid().await;
                }
            });
        }
    }

    /// Submit the same signed transaction to every endpoint at once, and return
    /// as soon as one of them has taken it.
    ///
    /// Each send runs in a task of its own rather than as a future this selects
    /// over, because returning early from a select DROPS the futures it did not
    /// pick - which would cancel the very requests this exists to make. Here
    /// the slower endpoints finish on their own; having the transaction in more
    /// than one mempool is the point, not a side effect to be tidied away.
    ///
    /// An endpoint answering "already known" is counted as success: it means
    /// the transaction is in that mempool, which is all this was asking for.
    /// The whole call fails only when no endpoint took it, and then the caller
    /// treats the nonce as unspent - which is why a false success here would be
    /// far worse than a false failure.
    async fn send(&self, raw: ethers::types::Bytes, hash: H256) -> Result<H256> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(self.endpoints.len().max(1));
        for endpoint in &self.endpoints {
            // Cloning the provider clones a handle to the same connection
            // pool, so the warm connection is the one that gets used.
            let http = endpoint.http.clone();
            let label = endpoint.label.clone();
            let raw = raw.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let outcome = http.send_raw_transaction(raw).await;
                let took = started.elapsed().as_millis();
                let ok = match &outcome {
                    Ok(_) => true,
                    Err(e) => already_in_a_mempool(&e.to_string()),
                };
                match ok {
                    true => tracing::debug!(endpoint = %label, took_ms = took, "submitted"),
                    false => tracing::warn!(
                        endpoint = %label, took_ms = took,
                        err = %outcome.as_ref().err().map(|e| e.to_string()).unwrap_or_default(),
                        "submit endpoint refused the transaction"
                    ),
                }
                let _ = tx.send((label, ok, took)).await;
            });
        }
        drop(tx);

        let mut refused = 0usize;
        while let Some((label, ok, took)) = rx.recv().await {
            if ok {
                tracing::info!(endpoint = %label, took_ms = took, ?hash, "broadcast");
                return Ok(hash);
            }
            refused += 1;
        }
        anyhow::bail!("every submit endpoint refused the transaction ({refused} tried)")
    }
}

/// Whether an error from `eth_sendRawTransaction` means the node already holds
/// this transaction, which is the outcome asked for rather than a failure.
///
/// Deliberately narrow. "nonce too low" is NOT here: it can equally mean the
/// nonce is genuinely spent, and reading that as success would leave a buy
/// believing in a transaction that will never exist. Being wrong in that
/// direction costs a position; being wrong the other way costs one retry.
fn already_in_a_mempool(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("already known")
        || e.contains("known transaction")
        || e.contains("alreadyknown")
        || e.contains("transaction already exists")
}

/// What this transaction would cost to run right now, with headroom.
///
/// Worth measuring once per route rather than once per send: the shape of the
/// call decides it, and the only thing that moves it between sends is how many
/// ticks a swap crosses. Unused gas is refunded, so erring high costs nothing
/// but the balance that has to be there to cover it.
pub async fn measure_gas(http: &Provider<Http>, from: Address, tx: &PendingTx) -> Result<U256> {
    let req = TransactionRequest::new()
        .from(from)
        .to(tx.to)
        .value(tx.value)
        .data(tx.data.clone());
    let gas = http
        .estimate_gas(&req.into(), None)
        .await
        .with_context(|| format!("estimating gas for {}", tx.label))?;
    Ok(gas * 2)
}

/// Keep the HTTP connection pool warm.
///
/// A reused connection answers in about 50ms; one that has to be established
/// first pays a TLS handshake and takes 350-400ms - seven times worse, and it
/// lands on exactly the request a buy is waiting for. The pool drops idle
/// connections after about ninety seconds, and buys are minutes apart, so
/// something has to touch it in between. This is that something: it costs one
/// trivial request a minute and cannot be skipped by anything else failing.
pub fn keep_warm(http: Provider<Http>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(20)).await;
            let _ = http.get_chainid().await;
        }
    });
}

/// The next nonce to use, counting transactions already broadcast but not yet
/// mined - `latest` would hand out one that is already spoken for.
pub async fn pending_nonce(http: &Provider<Http>, owner: Address) -> Result<u64> {
    let n = http
        .get_transaction_count(owner, Some(BlockNumber::Pending.into()))
        .await
        .context("eth_getTransactionCount(pending)")?;
    Ok(n.as_u64())
}

/// How a broadcast transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Confirmed,
    Reverted,
    Dropped,
    /// We stopped being able to tell. Treated as "did not happen" everywhere it
    /// matters, because acting on a trade that may not exist is worse than
    /// missing one that does.
    Unknown,
}

impl Outcome {
    pub fn happened(self) -> bool {
        self == Outcome::Confirmed
    }
}

/// `Transfer(address,address,uint256)`, the only place a receipt says how much
/// actually moved.
const TRANSFER_TOPIC: [u8; 32] = [
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
];

/// How much of `token` a transaction actually delivered to `to`.
///
/// The quote is what the swap was expected to pay; this is what it paid. They
/// differ by however much the pool moved between the two, which is exactly the
/// error that would otherwise accumulate in an average entry price - and, worse,
/// the error that would make a later sale ask for more than is there.
///
/// Every matching transfer is summed rather than the first taken: a route may
/// pay out in more than one, and a token may charge a fee by sending less.
pub fn received(logs: &[ethers::types::Log], token: Address, to: Address) -> Option<U256> {
    let mut total = U256::zero();
    let mut seen = false;
    for log in logs {
        if log.address != token || log.topics.len() < 3 || log.topics[0].0 != TRANSFER_TOPIC {
            continue;
        }
        // `to` is the second indexed argument, right-aligned in its topic.
        if Address::from_slice(&log.topics[2].as_bytes()[12..]) != to {
            continue;
        }
        if log.data.0.len() < 32 {
            continue;
        }
        total += U256::from_big_endian(&log.data.0[..32]);
        seen = true;
    }
    seen.then_some(total)
}

/// How often to ask whether a transaction has landed. Inclusion on this chain
/// was measured at three to five blocks - four hundred milliseconds or so - so
/// the first question is asked when the answer is due rather than on a timer
/// that knows nothing about the chain.
const RECEIPT_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// How long to keep asking. Six questions at most, and in the ordinary case
/// one: a transaction that has not landed in three seconds - thirty blocks -
/// is not merely slow.
const RECEIPT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// A transaction that has stopped being pending.
pub struct Landed {
    pub outcome: Outcome,
    /// Present whenever the chain had a receipt to give, whatever it said.
    pub logs: Vec<ethers::types::Log>,
}

/// Follow a broadcast transaction to its receipt, saying how it ended and
/// carrying the logs so a caller can read what actually moved.
pub async fn await_receipt(http: &Provider<Http>, hash: H256, label: &str) -> Landed {
    // Asked directly rather than through `PendingTransaction`, which polls at
    // the provider's interval whatever the transport - a websocket provider
    // polls identically - and whose default of seven seconds meant a revert was
    // learnt about seventy blocks after it happened, with the position reserved
    // and the pool held busy throughout.
    let mut waited = std::time::Duration::ZERO;
    let mut receipt = None;
    while waited < RECEIPT_GRACE {
        tokio::time::sleep(RECEIPT_POLL).await;
        waited += RECEIPT_POLL;
        match http.get_transaction_receipt(hash).await {
            Ok(Some(r)) => {
                receipt = Some(r);
                break;
            }
            // No receipt yet, or the node could not say. Neither is an answer,
            // and both wait: an endpoint that errors once is not evidence that
            // a transaction failed.
            Ok(None) => {}
            Err(e) => tracing::debug!(tx = ?hash, err = %e, label, "no answer yet"),
        }
    }

    // Nothing landed inside the grace. Whether that is a transaction the chain
    // never had or one still waiting its turn decides whether the caller may
    // roll its position back, and the two must not be reported as one: rolling
    // back a transaction that is still going to land is how a position comes to
    // exist on chain and not on the books. So it is worth one more question.
    let outcome = match receipt {
        Some(r) => Ok(Some(r)),
        None => match http.get_transaction(hash).await {
            Ok(None) => Ok(None),
            Ok(Some(_)) => Err(anyhow::anyhow!(
                "still pending after {}s and may yet land - NOT known to have failed",
                RECEIPT_GRACE.as_secs()
            )),
            Err(e) => Err(anyhow::anyhow!("{e}")),
        },
    };

    match outcome {
        Ok(Some(r)) if r.status == Some(1u64.into()) => {
            tracing::info!(
                tx = ?hash, block = ?r.block_number, gas_used = ?r.gas_used, label,
                "confirmed"
            );
            Landed {
                outcome: Outcome::Confirmed,
                logs: r.logs,
            }
        }
        Ok(Some(r)) => {
            tracing::error!(tx = ?hash, block = ?r.block_number, label, "REVERTED");
            Landed {
                outcome: Outcome::Reverted,
                logs: r.logs,
            }
        }
        Ok(None) => {
            tracing::warn!(tx = ?hash, label, "dropped from the mempool");
            Landed {
                outcome: Outcome::Dropped,
                logs: Vec::new(),
            }
        }
        Err(e) => {
            tracing::warn!(tx = ?hash, err = %e, label, "lost track of the transaction");
            Landed {
                outcome: Outcome::Unknown,
                logs: Vec::new(),
            }
        }
    }
}

/// Send a prepared batch in order, waiting for each receipt.
pub async fn send_all(
    http: &Provider<Http>,
    wallet: LocalWallet,
    txs: &[PendingTx],
) -> Result<()> {
    let from = wallet.address();
    let chain_id = wallet.chain_id();
    let client = SignerMiddleware::new(http.clone(), wallet);
    let (max_fee, tip) = fee_params(http).await?;
    println!("  gas: maxFeePerGas={max_fee} maxPriorityFeePerGas={tip}");

    for (i, tx) in txs.iter().enumerate() {
        let mut req = Eip1559TransactionRequest::new()
            .from(from)
            .to(tx.to)
            .data(tx.data.clone())
            .value(tx.value)
            .chain_id(chain_id)
            .max_fee_per_gas(max_fee)
            .max_priority_fee_per_gas(tip);
        // Estimate with headroom: the approval target may do more work than a
        // bare transfer, and a too-tight limit reverts on chain, not locally.
        let gas = client
            .estimate_gas(&req.clone().into(), None)
            .await
            .with_context(|| format!("estimating gas for tx {i} ({})", tx.label))?;
        req = req.gas(gas * 5 / 4);
        println!("  [{i}] sending {} (gas limit {})...", tx.label, gas * 5 / 4);
        let pending = client
            .send_transaction(req, None)
            .await
            .with_context(|| format!("sending tx {i}"))?;
        let hash = pending.tx_hash();
        println!("      tx {hash:?}, waiting for receipt");
        let receipt = pending
            .await
            .with_context(|| format!("waiting for tx {i}"))?
            .with_context(|| format!("tx {i} ({hash:?}) was dropped"))?;
        anyhow::ensure!(
            receipt.status == Some(1u64.into()),
            "tx {i} ({hash:?}) reverted"
        );
        println!("      confirmed in block {:?}", receipt.block_number);
    }
    Ok(())
}

/// Does this token already have both approvals in place?
pub async fn check_approvals(
    http: &Provider<Http>,
    token: Address,
    owner: Address,
    permit2: Address,
    router: Address,
) -> Result<(U256, U256)> {
    let mut a = selector("allowance(address,address)");
    a.extend_from_slice(&addr_word(owner));
    a.extend_from_slice(&addr_word(permit2));
    let res = http
        .call(&TransactionRequest::new().to(token).data(Bytes::from(a)).into(), None)
        .await
        .context("erc20 allowance()")?;
    let erc20_allowance = if res.len() >= 32 {
        U256::from_big_endian(&res[0..32])
    } else {
        U256::zero()
    };

    let mut p = selector("allowance(address,address,address)");
    p.extend_from_slice(&addr_word(owner));
    p.extend_from_slice(&addr_word(token));
    p.extend_from_slice(&addr_word(router));
    let res = http
        .call(&TransactionRequest::new().to(permit2).data(Bytes::from(p)).into(), None)
        .await
        .context("permit2 allowance()")?;
    // returns (uint160 amount, uint48 expiration, uint48 nonce)
    let permit2_allowance = if res.len() >= 32 {
        U256::from_big_endian(&res[0..32])
    } else {
        U256::zero()
    };
    Ok((erc20_allowance, permit2_allowance))
}

/// Spendable balance of `token` for `owner`; native ETH reads the account
/// balance instead of an ERC-20.
pub async fn balance_of(http: &Provider<Http>, token: Address, owner: Address) -> Result<U256> {
    if token == Address::zero() {
        return http.get_balance(owner, None).await.context("eth_getBalance");
    }
    let mut data = selector("balanceOf(address)");
    data.extend_from_slice(&addr_word(owner));
    let res = http
        .call(&TransactionRequest::new().to(token).data(Bytes::from(data)).into(), None)
        .await
        .context("erc20 balanceOf()")?;
    anyhow::ensure!(res.len() >= 32, "short balanceOf() return");
    Ok(U256::from_big_endian(&res[0..32]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Bytes, Log, H256};

    fn transfer_log(token: Address, to: Address, amount: u64) -> Log {
        let mut to_topic = [0u8; 32];
        to_topic[12..].copy_from_slice(to.as_bytes());
        let mut data = [0u8; 32];
        U256::from(amount).to_big_endian(&mut data);
        Log {
            address: token,
            topics: vec![
                H256::from(TRANSFER_TOPIC),
                H256::zero(),
                H256::from(to_topic),
            ],
            data: Bytes::from(data.to_vec()),
            ..Default::default()
        }
    }

    fn a(b: u8) -> Address {
        Address::from([b; 20])
    }

    #[test]
    fn the_transfer_topic_is_the_published_one() {
        assert_eq!(
            hex::encode(TRANSFER_TOPIC),
            "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
        );
    }

    #[test]
    fn only_transfers_of_that_token_to_that_address_are_counted() {
        let (token, other, me, someone) = (a(1), a(2), a(9), a(8));
        let logs = vec![
            transfer_log(token, me, 100),
            transfer_log(token, someone, 500),  // not ours
            transfer_log(other, me, 700),       // not that token
            transfer_log(token, me, 23),        // routes may pay in parts
        ];
        assert_eq!(received(&logs, token, me), Some(U256::from(123u64)));
    }

    #[test]
    fn a_receipt_with_nothing_for_us_says_so() {
        let (token, me) = (a(1), a(9));
        assert_eq!(received(&[], token, me), None);
        assert_eq!(received(&[transfer_log(token, a(8), 5)], token, me), None);
        // Zero is a real answer and not the same as no answer: a swap that
        // delivered nothing must not be read as "could not tell".
        assert_eq!(received(&[transfer_log(token, me, 0)], token, me), Some(U256::zero()));
    }

    #[test]
    fn a_malformed_log_is_skipped_rather_than_trusted() {
        let (token, me) = (a(1), a(9));
        let mut short = transfer_log(token, me, 10);
        short.data = Bytes::from(vec![0u8; 8]);
        let mut untopiced = transfer_log(token, me, 10);
        untopiced.topics.truncate(2);
        assert_eq!(received(&[short, untopiced], token, me), None);
    }

    #[test]
    fn approval_calldata_is_well_formed() {
        let token: Address = "0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168".parse().unwrap();
        let permit2: Address = PERMIT2_DEFAULT.parse().unwrap();
        let router: Address = "0x06AfBA43Fd06227fA663b0DAecF536f6EaA6bf99".parse().unwrap();
        let txs = build_unlimited_approval(token, permit2, router).unwrap();
        assert_eq!(txs.len(), 2);

        // ERC20.approve(spender, amount): 4 + 32 + 32
        assert_eq!(txs[0].to, token);
        assert_eq!(txs[0].data.len(), 68);
        assert_eq!(hex::encode(&txs[0].data[..4]), "095ea7b3");
        assert_eq!(Address::from_slice(&txs[0].data[16..36]), permit2);
        assert_eq!(U256::from_big_endian(&txs[0].data[36..68]), U256::MAX);

        // Permit2.approve(token, spender, uint160, uint48): 4 + 4*32
        assert_eq!(txs[1].to, permit2);
        assert_eq!(txs[1].data.len(), 132);
        assert_eq!(Address::from_slice(&txs[1].data[16..36]), token);
        assert_eq!(Address::from_slice(&txs[1].data[48..68]), router);
        assert_eq!(U256::from_big_endian(&txs[1].data[68..100]), max_uint160());
        assert_eq!(U256::from_big_endian(&txs[1].data[100..132]), max_uint48());
    }

    #[test]
    fn native_eth_is_rejected() {
        let e = build_unlimited_approval(
            Address::zero(),
            PERMIT2_DEFAULT.parse().unwrap(),
            Address::zero(),
        );
        assert!(e.is_err());
    }

    /// Endpoint URLs carry API keys in their path or query, and these labels
    /// go straight into log lines. Nothing after the host may survive.
    #[test]
    fn an_endpoint_label_cannot_carry_a_key() {
        let host = "https://rpc.example.com";
        let cases = [
            ("https://rpc.example.com/v2/SECRETKEY", host),
            ("https://rpc.example.com/?apikey=SECRET", host),
            ("https://user:PASSWORD@rpc.example.com/x", host),
            ("http://127.0.0.1:8545", "http://127.0.0.1:8545"),
            ("rpc.example.com/SECRET", "rpc.example.com"),
        ];
        for (url, want) in cases {
            let got = endpoint_label(url);
            assert_eq!(got, want, "from {url}");
            assert!(!got.contains("SECRET"), "{got} still carries the key");
            assert!(!got.contains("PASSWORD"), "{got} carries the password");
        }
        // Anything unparseable becomes a placeholder rather than the URL, so
        // the fallback cannot leak either.
        assert_eq!(endpoint_label(""), "endpoint");
    }

    /// A node saying it already holds the transaction has done what was asked.
    /// Anything ambiguous must not be read that way: treating a real refusal as
    /// a send leaves a buy waiting on a receipt that will never come, and the
    /// nonce counter believing a number was spent.
    #[test]
    fn only_an_unambiguous_duplicate_counts_as_sent() {
        assert!(already_in_a_mempool("already known"));
        assert!(already_in_a_mempool("known transaction: 0xabc"));
        assert!(already_in_a_mempool("ALREADY KNOWN"));
        assert!(already_in_a_mempool("transaction already exists"));

        // "nonce too low" can equally mean genuinely spent, so it stays out.
        assert!(!already_in_a_mempool("nonce too low"));
        assert!(!already_in_a_mempool("replacement transaction underpriced"));
        assert!(!already_in_a_mempool("insufficient funds for gas"));
        assert!(!already_in_a_mempool("intrinsic gas too low"));
        assert!(!already_in_a_mempool("connection reset by peer"));
        assert!(!already_in_a_mempool(""));
    }

    /// An empty endpoint list is a configuration error, not a broadcaster that
    /// silently sends nothing.
    #[test]
    fn a_broadcaster_needs_somewhere_to_send() {
        assert!(Broadcaster::new(&[]).is_err());
        let one = Broadcaster::new(&["http://127.0.0.1:8545".to_string()]).unwrap();
        assert_eq!(one.width(), 1);
        assert_eq!(one.labels(), vec!["http://127.0.0.1:8545"]);
    }

    #[test]
    fn max_constants_are_right() {
        assert_eq!(max_uint160(), U256::from_dec_str(
            "1461501637330902918203684832716283019655932542975").unwrap());
        assert_eq!(max_uint48(), U256::from(281_474_976_710_655u64));
    }
}
