//! Wallet-touching operations: token approvals today, swap submission next.
//!
//! Everything here is built and printed first and only sent when the caller
//! passes `--execute`, so the exact transaction can be inspected before any
//! value moves.

use crate::config::Config;
use anyhow::{Context, Result};
use ethers::middleware::SignerMiddleware;
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, BlockNumber, Bytes, Eip1559TransactionRequest, TransactionRequest, U256};
use ethers::utils::keccak256;

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
async fn fee_params(http: &Provider<Http>) -> Result<(U256, U256)> {
    let block = http
        .get_block(BlockNumber::Latest)
        .await
        .context("fetching latest block for base fee")?
        .context("latest block missing")?;
    let base = block.base_fee_per_gas.unwrap_or_default();
    let tip: U256 = http
        .request("eth_maxPriorityFeePerGas", ())
        .await
        .unwrap_or_else(|_| U256::from(1_000_000u64));
    let tip = tip.max(U256::from(1_000_000u64));
    Ok((base * 2 + tip, tip))
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
