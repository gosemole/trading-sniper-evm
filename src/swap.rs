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
            // second apart, so it is worth far less often than every one.
            let now = now_secs();
            if now.saturating_sub(tip_checked) >= 5 {
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
    http: &Provider<Http>,
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
    let pending = http
        .send_raw_transaction(typed.rlp_signed(&sig))
        .await
        .context("broadcasting transaction")?;
    Ok(pending.tx_hash())
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

/// The next nonce to use, counting transactions already broadcast but not yet
/// mined - `latest` would hand out one that is already spoken for.
pub async fn pending_nonce(http: &Provider<Http>, owner: Address) -> Result<u64> {
    let n = http
        .get_transaction_count(owner, Some(BlockNumber::Pending.into()))
        .await
        .context("eth_getTransactionCount(pending)")?;
    Ok(n.as_u64())
}

/// Follow a broadcast transaction to its receipt and say how it ended. Meant to
/// be spawned: nothing waits on it.
pub async fn report_receipt(http: Provider<Http>, hash: H256, label: String) {
    match ethers::providers::PendingTransaction::new(hash, &http).await {
        Ok(Some(r)) if r.status == Some(1u64.into()) => tracing::info!(
            tx = ?hash, block = ?r.block_number, gas_used = ?r.gas_used, label,
            "confirmed"
        ),
        Ok(Some(r)) => tracing::error!(tx = ?hash, block = ?r.block_number, label, "REVERTED"),
        Ok(None) => tracing::warn!(tx = ?hash, label, "dropped from the mempool"),
        Err(e) => tracing::warn!(tx = ?hash, err = %e, label, "lost track of the transaction"),
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

    #[test]
    fn max_constants_are_right() {
        assert_eq!(max_uint160(), U256::from_dec_str(
            "1461501637330902918203684832716283019655932542975").unwrap());
        assert_eq!(max_uint48(), U256::from(281_474_976_710_655u64));
    }
}
