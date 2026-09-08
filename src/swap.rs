//! Wallet-touching operations: token approvals today, swap submission next.
//!
//! Everything here is built and printed first and only sent when the caller
//! passes `--execute`, so the exact transaction can be inspected before any
//! value moves.

use crate::config::Config;
use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip2718::TypedTransaction;
use ethers::types::{
    Address, BlockNumber, Bytes, Eip1559TransactionRequest, TransactionRequest, H256, U256,
};
use ethers::abi::{ParamType, Token as AbiToken};
use ethers::utils::keccak256;

/// A tip floor, for chains that report zero: a transaction still has to be
/// worth including.
const TIP_FLOOR: u64 = 1_000_000;








fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}





/// The signing key, preferring the environment over the config file.
///
/// The wallet address is logged; the key itself never is.
pub fn load_wallet(cfg: &Config, chain_id: u64) -> Result<LocalWallet> {
    // `Config::load` already merged PRIVATE_KEY over the file, so there is one
    // place the key can come from by the time we get here.
    let source = if std::env::var_os("PRIVATE_KEY").is_some() {
        "PRIVATE_KEY env"
    } else {
        "config"
    };
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


/// One transaction to build, show, and maybe send.
pub struct PendingTx {
    pub label: String,
    pub to: Address,
    pub data: Bytes,
    /// Native value to attach. Only a swap whose input currency is ETH needs
    /// one; everything else moves through Permit2 and sends zero.
    pub value: U256,
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
        crate::rpc::retrying("eth_getBlockByNumber(latest)", || async {
            Ok(http.get_block(BlockNumber::Latest).await?)
        }),
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
    #[allow(dead_code)]
    pub fn width(&self) -> usize {
        self.endpoints.len()
    }

    /// The endpoints this will submit through, for logging at startup.
    #[allow(dead_code)]
    pub fn labels(&self) -> Vec<&str> {
        self.endpoints.iter().map(|e| e.label.as_str()).collect()
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



/// The next nonce to use, counting transactions already broadcast but not yet
/// mined - `latest` would hand out one that is already spoken for.
pub async fn pending_nonce(http: &Provider<Http>, owner: Address) -> Result<u64> {
    let n = crate::rpc::retrying("eth_getTransactionCount(pending)", || async {
        http.get_transaction_count(owner, Some(BlockNumber::Pending.into()))
            .await
            .context("eth_getTransactionCount(pending)")
    })
    .await?;
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


/// How often to ask whether a transaction has landed.
///
/// The first question is asked when the answer is due rather than on a timer
/// that knows nothing about the chain, so this tracks inclusion: it was three
/// to five blocks and is now faster, and a receipt learnt late is a position
/// held open and a pool held busy for no reason.
const RECEIPT_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// How long to keep asking. In the ordinary case the first question answers it;
/// a transaction that has not landed in three seconds - thirty blocks - is not
/// merely slow, and the questions in between are the price of telling a slow
/// one from a dropped one.
const RECEIPT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// A transaction that has stopped being pending.
pub struct Landed {
    pub outcome: Outcome,
    /// Present whenever the chain had a receipt to give, whatever it said. Not
    /// read today: a buy is confirmed by its outcome and sized by what was
    /// asked for, and the wrapper reports the fill in an event nobody parses
    /// yet. Kept because that event is where a real fill would come from.
    #[allow(dead_code)]
    pub logs: Vec<ethers::types::Log>,
}

/// Why a transaction reverted, by running it again as a call.
///
/// A receipt carries no reason - the chain does not store one - but replaying
/// the same call against the block it landed in usually reproduces the revert,
/// and a call's revert comes back with its data. Two requests, only ever on a
/// revert, and in a task nothing is waiting on.
///
/// "Usually", because a call sees the state at the END of that block while the
/// transaction ran somewhere inside it. So a replay that SUCCEEDS is itself an
/// answer, and a useful one: whatever it hit was gone by the end of the block,
/// which for a swap means something else in the same block moved the pool.
async fn why_reverted(http: &Provider<Http>, hash: H256, at: Option<ethers::types::U64>) -> String {
    let tx = match http.get_transaction(hash).await {
        Ok(Some(t)) => t,
        _ => return "could not read the transaction back to replay it".to_string(),
    };
    let Some(to) = tx.to else {
        return "the transaction created a contract; there is nothing to replay".to_string();
    };
    let req = TransactionRequest::new()
        .from(tx.from)
        .to(to)
        .value(tx.value)
        .data(tx.input.clone());
    let block = at.map(|n| ethers::types::BlockId::Number(BlockNumber::Number(n)));
    match http.call(&req.into(), block).await {
        Ok(_) => "it does not revert when replayed at the end of that block, so what it \
                  depended on was changed by something else inside the block"
            .to_string(),
        // Anything this does not have a name for still prints its selector,
        // which is enough to look up.
        Err(e) => explain_revert(&e.to_string()),
    }
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
            // Asked here rather than left to the operator: the quote, the
            // minimum and the size are all in the log above, and none of them
            // says which was wrong.
            let why = why_reverted(http, hash, r.block_number).await;
            tracing::error!(
                tx = ?hash, block = ?r.block_number, label, why = %why,
                "REVERTED"
            );
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





// ---------------------------------------------------------------------------
// revert decoding
// ---------------------------------------------------------------------------

/// The custom errors these two contracts throw, so a failed trade reads as a
/// name rather than four hex bytes.
///
/// Taken from the wrapper's source and the curve's ABI rather than written
/// from memory - a signature guessed wrong hashes to a selector that matches
/// nothing, which is the same as not listing it, only misleading. Anything not
/// here still prints its selector, which is enough to look up.
const KNOWN_ERRORS: &[&str] = &[
    // PonsSniper, ours.
    "NotOwner()",
    "NotPendingOwner()",
    "Reentrancy()",
    "TooLate(uint256,uint256)",
    "SnipeTaxTooHigh(uint256,uint256)",
    "NothingReceived()",
    "TaxCeilingExceeded(uint256,uint256)",
    "NotAPonsCurve(address,address)",
    "CurveNotOpen()",
    // The one that costs 99% and is refused structurally. Seeing this means a
    // buy aimed at a step landed in the launch second itself.
    "TheLaunchSecond(uint256,uint256)",
    "NothingHeld(address)",
    "MoreThanHeld(uint256,uint256)",
    // PonsV2BondingCurve, theirs.
    "AlreadyGraduated()",
    "CurveGraduated()",
    "InsufficientInputAmount()",
    "InsufficientLiquidity()",
    "InsufficientOutputAmount()",
    "MinimumOutputRequired()",
    "NotInitialized()",
    "ReentrancyGuardReentrantCall()",
    "SafeERC20FailedOperation(address)",
    // What a buy that was too slow looks like: the price moved past the
    // minimum it was sent with.
    "SlippageExceeded(uint256,uint256)",
    // Thrown by both, with the same meaning in each.
    "NativeValueMismatch(uint256,uint256)",
    "UnexpectedNativeValue()",
    "TransferFailed()",
    "ZeroAddress()",
    "ZeroAmount()",
];

/// Pull the revert payload out of whatever prose the provider wrapped it in.
fn revert_payload(msg: &str) -> Option<Vec<u8>> {
    let start = msg.find("0x")? + 2;
    let mut hex_str: String = msg[start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if !hex_str.len().is_multiple_of(2) {
        hex_str.pop();
    }
    if hex_str.len() < 8 {
        return None;
    }
    hex::decode(hex_str).ok()
}

/// Turn revert bytes into something readable, unwrapping the Universal
/// Router's `ExecutionFailed` so the error that actually fired is the one shown.
pub fn decode_revert(data: &[u8]) -> String {
    if data.is_empty() {
        // The signature of an ABI decode that failed its bounds check - which
        // on this chain means the struct layout does not match the deployment.
        return "empty revert data (a bare revert; usually a calldata layout the \
                deployed contract does not accept)"
            .to_string();
    }
    if data.len() < 4 {
        return format!("0x{}", hex::encode(data));
    }
    let sel = &data[0..4];
    let body = &data[4..];

    if sel == [0x08, 0xc3, 0x79, 0xa0] {
        if let Ok(t) = ethers::abi::decode(&[ParamType::String], body) {
            if let Some(AbiToken::String(s)) = t.into_iter().next() {
                return format!("revert \"{s}\"");
            }
        }
    }
    if sel == selector("ExecutionFailed(uint256,bytes)").as_slice() {
        if let Ok(t) = ethers::abi::decode(&[ParamType::Uint(256), ParamType::Bytes], body) {
            if let [AbiToken::Uint(i), AbiToken::Bytes(inner)] = t.as_slice() {
                return format!("command {i} failed: {}", decode_revert(inner));
            }
        }
    }
    for sig in KNOWN_ERRORS {
        if sel == selector(sig).as_slice() {
            let name = sig.split('(').next().unwrap_or(sig);
            if body.is_empty() {
                return name.to_string();
            }
            return format!("{name} args 0x{}", hex::encode(body));
        }
    }
    format!("unrecognised error 0x{}", hex::encode(sel))
}

/// Best-effort explanation of a provider error string.
pub fn explain_revert(msg: &str) -> String {
    match revert_payload(msg) {
        Some(data) => decode_revert(&data),
        None => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;


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
}
