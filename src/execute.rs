//! Universal Router calldata for a Uniswap v4 exact-input swap, and a read of
//! the true output straight out of the router.
//!
//! # The ABI this chain actually deploys
//!
//! The router deployed here is a fork of UniversalRouter that adds per-hop
//! price limits (its ABI carries `V4TooLittleReceivedPerHopSingle`,
//! `V3TooLittleReceivedPerHop`, `V3HopPriceAndPathLengthMismatch` and friends).
//! That shows up as one extra word in `ExactInputSingleParams`, and encoding
//! without it reverts with **empty** revert data - no selector, no message:
//!
//! ```text
//!   PoolKey  poolKey     (currency0, currency1, fee, tickSpacing, hooks)
//!   bool     zeroForOne
//!   uint128  amountIn
//!   uint128  amountOutMinimum
//!   uint256  <extra>     <-- this fork only; see below
//!   bytes    hookData
//! ```
//!
//! The extra word is left at zero. Its meaning is *not* established: probing it
//! with 0, 1 and 1e30 changed nothing about the swap, so it is neither a price
//! limit the pool would reject (1 would be out of bounds) nor a minimum the
//! action enforces (1e30 would refuse). Zero is what the router treats as
//! "unset" everywhere else in this fork - on the v3 side an empty per-hop price
//! array is likewise what disables the check - so zero is the safe value, and
//! the field is deliberately not exposed until its semantics are known.
//!
//! Encoding it without that field shifts `hookData`'s offset onto the last
//! word, so the decoder reads a length from `currency0` and fails its calldata
//! bounds check, which in Solidity is a bare `revert(0, 0)`. The tell is that
//! pools with a *native* currency0 (numerically 0, a valid length) kept working
//! while every ERC-20 pool failed. `SWAP_EXACT_IN` (0x07, the multi-hop action)
//! is skewed the same way and is not used here at all: a route of any length is
//! built from one `SWAP_EXACT_IN_SINGLE` per hop instead, which needs no
//! `PathKey` encoding and is what the on-chain probing confirmed works.
//!
//! # Shape of the call
//!
//! ```text
//!   UniversalRouter.execute(commands, inputs, deadline)
//!     commands  = [V4_SWAP]
//!     inputs[0] = abi.encode(actions, params)
//!       actions   = [SWAP_EXACT_IN_SINGLE * hops, SETTLE_ALL, TAKE_ALL]
//!       params[i] = one hop; the first spends amountIn, the rest spend
//!                   OPEN_DELTA (0), meaning "whatever the previous hop left"
//!       SETTLE_ALL(currencyIn, amountIn)  - pays the debt, capped
//!       TAKE_ALL(currencyOut, minOut)     - collects the credit, floored
//! ```
//!
//! Per-hop minimums stay at 0: `TAKE_ALL` alone guards the trade, and it guards
//! the number that matters - what finally lands in the wallet. `SETTLE_ALL`
//! pulls the input through Permit2 from the caller and `TAKE_ALL` pays the
//! output to the caller, so nothing rests in the router between calls.

use crate::route::{Hop, Route, Venue};
use anyhow::{Context, Result};
use ethers::abi::{encode, ParamType, Token as AbiToken};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, Bytes, TransactionRequest, U256};
use ethers::utils::keccak256;

/// Universal Router commands.
const CMD_V3_SWAP_EXACT_IN: u8 = 0x00;
const CMD_V4_SWAP: u8 = 0x10;

/// v4-periphery `Actions` opcodes.
const ACTION_SWAP_EXACT_IN_SINGLE: u8 = 0x06;
const ACTION_SETTLE: u8 = 0x0b;
const ACTION_TAKE: u8 = 0x0e;
const ACTION_TAKE_ALL: u8 = 0x0f;

/// `ActionConstants.OPEN_DELTA` / `CONTRACT_BALANCE` on the **v4** side, both
/// zero: an amount of zero means "whatever is already owed / already held",
/// which is how one hop feeds the next without predicting the amount off chain.
const OPEN_DELTA: U256 = U256::zero();

/// `Constants.CONTRACT_BALANCE`: "spend everything this router holds of that
/// currency". It is `1 << 255` on **both** sides, and it is NOT the zero that
/// means `OPEN_DELTA`. Three amounts that all look like "use what is there"
/// have to be kept apart, and each was established by probing the deployment:
///
/// | where | zero means | whole balance |
/// |---|---|---|
/// | v4 swap `amountIn` | the full open credit | (not used) |
/// | v4 `SETTLE` amount | the full open *debt* - nothing yet, so nothing gets paid | `1 << 255` |
/// | v4 `TAKE` amount | the full open credit | (not used) |
/// | v3 leg `amountIn` | not a sentinel at all: a zero-sized swap, which the pool rejects with `"AS"` | `1 << 255` |
///
/// Passing zero to `SETTLE` after a v3 leg is the quiet one: it settles the
/// debt, there is no debt, so the next hop swaps nothing and the whole thing
/// dies on `SwapAmountCannotBeZero` with the tokens sitting right there.
fn contract_balance() -> U256 {
    U256::one() << 255
}

/// `ActionConstants.MSG_SENDER`: the router resolves address(1) to its caller.
fn msg_sender() -> Address {
    Address::from_low_u64_be(1)
}

/// `ActionConstants.ADDRESS_THIS`: address(2) is the router itself, where a leg
/// leaves its output for the next leg to pick up.
fn address_this() -> Address {
    Address::from_low_u64_be(2)
}

fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}

fn u128_arg(v: U256, what: &str) -> Result<U256> {
    anyhow::ensure!(
        v <= U256::from(u128::MAX),
        "{what} ({v}) does not fit in the uint128 the router expects"
    );
    Ok(v)
}

/// Native ETH is paid as `msg.value`; every other input is pulled via Permit2.
pub fn call_value(route: &Route) -> U256 {
    if route.input.address == Address::zero() {
        route.amount_in
    } else {
        U256::zero()
    }
}

/// Split the route into the longest possible runs of one protocol.
///
/// Each run becomes a single Universal Router command, so a route that stays on
/// one protocol costs one command however many pools it crosses, and only an
/// actual v3/v4 boundary costs a hand-off through the router's own balance.
fn legs(route: &Route) -> Vec<Vec<&Hop>> {
    let mut out: Vec<Vec<&Hop>> = Vec::new();
    for hop in &route.hops {
        match out.last_mut() {
            Some(leg) if leg[0].venue.is_v4() == hop.venue.is_v4() => leg.push(hop),
            _ => out.push(vec![hop]),
        }
    }
    out
}

/// The v3 path format: `tokenIn (fee tokenOut)+`, packed with no padding.
fn v3_path(leg: &[&Hop]) -> Result<Vec<u8>> {
    let mut path = Vec::with_capacity(20 + leg.len() * 23);
    path.extend_from_slice(leg[0].input.as_bytes());
    for hop in leg {
        anyhow::ensure!(
            hop.fee <= 0xff_ffff,
            "v3 fee {} does not fit in the uint24 the path packs it into",
            hop.fee
        );
        path.extend_from_slice(&hop.fee.to_be_bytes()[1..4]);
        path.extend_from_slice(hop.output.as_bytes());
    }
    Ok(path)
}

/// One `V3_SWAP_EXACT_IN` input.
///
/// The trailing `uint256[]` is this fork's per-hop price limit. It is left
/// empty, which switches the check off; a non-empty array must have exactly one
/// entry per pool in the path or the router rejects it with
/// `V3HopPriceAndPathLengthMismatch`. Omitting the field altogether - as
/// upstream UniversalRouter's ABI would - reverts with `SliceOutOfBounds`.
fn v3_leg(route: &Route, leg: &[&Hop], first: bool, last: bool, min_out: U256) -> Result<Vec<u8>> {
    Ok(encode(&[
        AbiToken::Address(if last { msg_sender() } else { address_this() }),
        // Not the first leg means the tokens are already sitting on the router,
        // and this sentinel tells it to spend all of them.
        AbiToken::Uint(if first { route.amount_in } else { contract_balance() }),
        AbiToken::Uint(min_out),
        AbiToken::Bytes(v3_path(leg)?),
        AbiToken::Bool(first),
        AbiToken::Array(Vec::new()),
    ]))
}

/// `abi.encode(IV4Router.ExactInputSingleParams)` for one hop.
///
/// The word after `amountOutMinimum` is this fork's addition and is left at
/// zero; see the module docs for why nothing else is safe to put there.
fn exact_in_single(hop: &Hop, amount_in: U256, min_out: U256) -> Result<Vec<u8>> {
    // Checked before it is encoded, not after: a negative spacing would be
    // widened by `as u64` into a huge positive int24 rather than its two's
    // complement, and the encoding would silently describe a different pool.
    anyhow::ensure!(
        hop.tick_spacing > 0,
        "tick spacing {} is not positive",
        hop.tick_spacing
    );
    let hooks = match &hop.venue {
        Venue::V4 { hooks, .. } => *hooks,
        Venue::V3 { pool } => anyhow::bail!("{pool:?} is a v3 pool; it has no v4 PoolKey"),
    };
    let pool_key = AbiToken::Tuple(vec![
        AbiToken::Address(hop.currency0),
        AbiToken::Address(hop.currency1),
        AbiToken::Uint(U256::from(hop.fee)),
        AbiToken::Int(U256::from(hop.tick_spacing as u64)),
        AbiToken::Address(hooks),
    ]);
    Ok(encode(&[AbiToken::Tuple(vec![
        pool_key,
        AbiToken::Bool(hop.zero_for_one()),
        AbiToken::Uint(u128_arg(amount_in, "amountIn")?),
        AbiToken::Uint(u128_arg(min_out, "amountOutMinimum")?),
        AbiToken::Uint(U256::zero()), // this fork's extra word
        AbiToken::Bytes(Vec::new()),  // hookData
    ])]))
}

/// One `V4_SWAP` input: `abi.encode(bytes actions, bytes[] params)`.
///
/// Always the same shape, whichever end of the route it sits at:
/// `SETTLE` puts the input currency onto the pool manager, every hop then
/// swaps the whole open credit, and a final take moves the output out. Only
/// where the money comes from and where it goes changes.
fn v4_leg(route: &Route, leg: &[&Hop], first: bool, last: bool, min_out: U256) -> Result<Vec<u8>> {
    let mut actions = vec![ACTION_SETTLE];
    actions.extend(std::iter::repeat_n(ACTION_SWAP_EXACT_IN_SINGLE, leg.len()));
    actions.push(if last { ACTION_TAKE_ALL } else { ACTION_TAKE });

    let mut params = Vec::with_capacity(actions.len());
    // What is settled here becomes the open credit the first hop swaps, and the
    // router casts that credit to uint128. Refusing it locally beats sending a
    // transaction that reverts on an UnsafeCast.
    let settle_amount = if first {
        u128_arg(route.amount_in, "amount_in")?
    } else {
        contract_balance()
    };
    params.push(AbiToken::Bytes(encode(&[
        AbiToken::Address(leg[0].input),
        // The first leg is paid for by the caller through Permit2; a later one
        // spends what the previous leg left on the router.
        AbiToken::Uint(settle_amount),
        AbiToken::Bool(first),
    ])));
    for hop in leg {
        params.push(AbiToken::Bytes(exact_in_single(hop, OPEN_DELTA, U256::zero())?));
    }
    let out = leg[leg.len() - 1].output;
    params.push(AbiToken::Bytes(if last {
        // TAKE_ALL(currency, minimum) - the whole slippage guard of the route.
        encode(&[AbiToken::Address(out), AbiToken::Uint(min_out)])
    } else {
        // TAKE(currency, recipient, amount): park it on the router for the
        // next leg rather than paying it out.
        encode(&[
            AbiToken::Address(out),
            AbiToken::Address(address_this()),
            AbiToken::Uint(OPEN_DELTA),
        ])
    }));

    Ok(encode(&[
        AbiToken::Bytes(actions),
        AbiToken::Array(params),
    ]))
}

/// Full calldata for `UniversalRouter.execute(bytes,bytes[],uint256)`.
pub fn execute_calldata(route: &Route, min_out: U256, deadline: U256) -> Result<Bytes> {
    anyhow::ensure!(!route.hops.is_empty(), "route has no hops");
    let legs = legs(route);
    let mut commands = Vec::with_capacity(legs.len());
    let mut inputs = Vec::with_capacity(legs.len());
    for (i, leg) in legs.iter().enumerate() {
        let first = i == 0;
        let last = i + 1 == legs.len();
        // Only the last leg carries the minimum: an intermediate one has no
        // business refusing an amount that the rest of the route still acts on.
        let leg_min = if last { min_out } else { U256::zero() };
        if leg[0].venue.is_v4() {
            commands.push(CMD_V4_SWAP);
            inputs.push(AbiToken::Bytes(v4_leg(route, leg, first, last, leg_min)?));
        } else {
            commands.push(CMD_V3_SWAP_EXACT_IN);
            inputs.push(AbiToken::Bytes(v3_leg(route, leg, first, last, leg_min)?));
        }
    }

    let mut data = selector("execute(bytes,bytes[],uint256)");
    data.extend_from_slice(&encode(&[
        AbiToken::Bytes(commands),
        AbiToken::Array(inputs),
        AbiToken::Uint(deadline),
    ]));
    Ok(Bytes::from(data))
}

/// A deadline `secs` from now, in unix seconds.
pub fn deadline_in(secs: u64) -> U256 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    U256::from(now.saturating_add(secs))
}

/// The swap as one transaction, ready to print and - only if the caller says
/// so - to send.
pub fn pending_swap(
    router: Address,
    route: &Route,
    min_out: U256,
    deadline: U256,
) -> Result<crate::swap::PendingTx> {
    Ok(crate::swap::PendingTx {
        label: format!(
            "UniversalRouter.execute  {} hop(s) in {} leg(s), amountOutMinimum {min_out}",
            route.hops.len(),
            legs(route).len()
        ),
        to: router,
        data: execute_calldata(route, min_out, deadline)?,
        value: call_value(route),
    })
}

/// Reduce a verified output by a route's slippage tolerance, in integer
/// arithmetic so nothing is lost to a float round trip.
///
/// The input is whatever the router just said this pair pays, so the tolerance
/// covers only what can move between the quote and the block the swap lands in.
pub fn apply_slippage(amount: U256, pct: f64) -> U256 {
    // Basis points, clamped so a silly config cannot produce a zero floor or a
    // floor above the quote itself.
    let bps = ((pct * 100.0).round() as i64).clamp(1, 9_900) as u64;
    amount * U256::from(10_000u64 - bps) / U256::from(10_000u64)
}

// ---------------------------------------------------------------------------
// on-chain verification
// ---------------------------------------------------------------------------

/// What the router itself says the route produces.
pub struct OnChainQuote {
    pub amount_out: U256,
    /// True when the router reported the figure itself; false when it was
    /// bracketed by bisection and is a lower bound within `PRECISION_PPM`.
    pub exact: bool,
    pub probes: u32,
}

/// Stop bisecting once the bracket is this tight, in parts per million.
const PRECISION_PPM: u64 = 100; // 0.01%
/// Hard cap on eth_calls, so a pathological pool cannot spin forever.
const MAX_PROBES: u32 = 80;

enum Probe {
    Ok,
    Reverted(String),
}

/// Distinguish "the contract rejected this" from "the request never ran".
/// Only the first is a data point; the second must not be read as a limit.
fn is_revert(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("revert") || m.contains("code: 3,") || m.contains("code: 3 ")
}

async fn probe(
    http: &Provider<Http>,
    router: Address,
    from: Address,
    route: &Route,
    min_out: U256,
    deadline: U256,
    at: Option<u64>,
) -> Result<Probe> {
    let tx = TransactionRequest::new()
        .from(from)
        .to(router)
        .value(call_value(route))
        .data(execute_calldata(route, min_out, deadline)?);
    match http.call(&tx.into(), at.map(ethers::types::BlockId::from)).await {
        Ok(_) => Ok(Probe::Ok),
        Err(e) => {
            let msg = e.to_string();
            if is_revert(&msg) {
                Ok(Probe::Reverted(msg))
            } else {
                Err(anyhow::anyhow!(msg)).context("eth_call could not be made at all")
            }
        }
    }
}

/// `V4TooLittleReceived(uint256 minRequested, uint256 amountReceived)` names the
/// amount that *did* come out, so one deliberately failing call reads the true
/// output. The second field is that amount.
fn amount_from_too_little(msg: &str) -> Option<U256> {
    let data = revert_payload(msg)?;
    if data.len() < 68 || data[0..4] != *selector("V4TooLittleReceived(uint256,uint256)") {
        return None;
    }
    Some(U256::from_big_endian(&data[36..68]))
}

/// Ask the router what the route really yields, right now.
///
/// One call with an impossible minimum is enough for a route that ends on v4:
/// `V4TooLittleReceived` carries the amount that would have been paid, so the
/// router answers its own question. A route ending on v3 refuses with
/// `V3TooLittleReceived()`, which carries nothing, so those fall back to
/// bisecting `amountOutMinimum` - ~25 calls instead of one, landing within
/// `PRECISION_PPM` from below. That matters for auto-buy latency: an armed
/// route is worth ending on a v4 pool where there is a choice.
///
/// Read-only either way: `eth_call` moves nothing.
pub async fn verify(
    http: &Provider<Http>,
    router: Address,
    from: Address,
    route: &Route,
    hint: U256,
    deadline: U256,
    at: Option<u64>,
) -> Result<OnChainQuote> {
    let mut probes = 1u32;
    // Above any real output, and still inside the uint128 the hops accept.
    let impossible = U256::from(u128::MAX);
    if let Probe::Reverted(msg) = probe(http, router, from, route, impossible, deadline, at).await? {
        if let Some(amount) = amount_from_too_little(&msg) {
            anyhow::ensure!(
                !amount.is_zero(),
                "the route executes but returns nothing: {}",
                explain_revert(&msg)
            );
            return Ok(OnChainQuote {
                amount_out: amount,
                exact: true,
                probes,
            });
        }
    }

    // A floor of 1 rather than 0: TAKE_ALL with a zero minimum also passes when
    // the swap returned nothing at all, which would look like success.
    let mut lo = U256::one();
    probes += 1;
    if let Probe::Reverted(msg) = probe(http, router, from, route, lo, deadline, at).await? {
        anyhow::bail!(
            "the swap reverts even with amountOutMinimum = 1, so nothing about it is \
             executable right now: {}",
            explain_revert(&msg)
        );
    }

    // Grow the bracket until a minimum is refused. Doubling keeps this to a
    // handful of calls even when the local estimate is far off.
    let mut hi = if hint > lo { hint } else { U256::from(2) };
    loop {
        probes += 1;
        match probe(http, router, from, route, hi, deadline, at).await? {
            Probe::Reverted(_) => break,
            Probe::Ok => {
                lo = hi;
                let next = hi.saturating_mul(U256::from(2));
                anyhow::ensure!(
                    probes < MAX_PROBES && next > hi,
                    "the swap still succeeds at amountOutMinimum = {hi}; giving up looking for \
                     an upper bound"
                );
                hi = next;
            }
        }
    }

    // Bisect. `mid == lo` means the two are adjacent, so there is nothing left
    // to split and the answer is exact.
    while probes < MAX_PROBES {
        let gap = hi - lo;
        let tolerance = (lo / U256::from(1_000_000u64)) * U256::from(PRECISION_PPM);
        if gap <= std::cmp::max(tolerance, U256::one()) {
            break;
        }
        let mid = lo + gap / 2;
        if mid == lo {
            break;
        }
        probes += 1;
        match probe(http, router, from, route, mid, deadline, at).await? {
            Probe::Ok => lo = mid,
            Probe::Reverted(_) => hi = mid,
        }
    }

    Ok(OnChainQuote {
        amount_out: lo,
        exact: false,
        probes,
    })
}

/// Run the exact transaction that would be sent, as a call. Anything that would
/// make it revert on chain - slippage, allowance, balance, deadline - fails
/// here first, for free.
pub async fn dry_run(
    http: &Provider<Http>,
    router: Address,
    from: Address,
    route: &Route,
    min_out: U256,
    deadline: U256,
) -> Result<()> {
    match probe(http, router, from, route, min_out, deadline, None).await? {
        Probe::Ok => Ok(()),
        Probe::Reverted(msg) => Err(anyhow::anyhow!(
            "the transaction as built reverts: {}",
            explain_revert(&msg)
        )),
    }
}

// ---------------------------------------------------------------------------
// revert decoding
// ---------------------------------------------------------------------------

/// Custom errors these contracts throw, so a failure reads as a name instead of
/// four hex bytes. Anything not listed still prints its selector.
const KNOWN_ERRORS: &[&str] = &[
    // The router's own, from its verified ABI. The `PerHop` family is this
    // fork's addition and does not exist in upstream UniversalRouter.
    "V4TooLittleReceived(uint256,uint256)",
    "V4TooLittleReceivedPerHop(uint256,uint256,uint256)",
    "V4TooLittleReceivedPerHopSingle(uint256,uint256)",
    "V4TooMuchRequested(uint256,uint256)",
    "V4TooMuchRequestedPerHop(uint256,uint256,uint256)",
    "V4TooMuchRequestedPerHopSingle(uint256,uint256)",
    "V3TooLittleReceived()",
    "V3TooLittleReceivedPerHop(uint256,uint256,uint256)",
    "V3TooMuchRequested()",
    "V3TooMuchRequestedPerHop(uint256,uint256,uint256)",
    "V3HopPriceAndPathLengthMismatch()",
    "V3InvalidSwap()",
    "V3InvalidAmountOut()",
    "V3InvalidCaller()",
    "V2TooLittleReceived()",
    "V2TooLittleReceivedPerHop(uint256,uint256,uint256)",
    "V2TooMuchRequested()",
    "V2InvalidPath()",
    "V2InvalidHopPriceLength()",
    "InvalidHopPriceLength()",
    "InvalidPath()",
    "InvalidCommandType(uint256)",
    "InvalidAction(bytes4)",
    "UnsupportedAction(uint256)",
    "InputLengthMismatch()",
    "LengthMismatch()",
    "SliceOutOfBounds()",
    "TransactionDeadlinePassed()",
    "ContractLocked()",
    "BalanceTooLow()",
    "InsufficientToken()",
    "InsufficientETH()",
    "InsufficientBalance()",
    "ETHNotAccepted()",
    "InvalidEthSender()",
    "FromAddressIsNotOwner()",
    "SafeERC20FailedOperation(address)",
    "UnsafeCast()",
    "DeltaNotPositive(address)",
    "DeltaNotNegative(address)",
    // Thrown underneath the router, by the PoolManager or by Permit2.
    "CurrencyNotSettled()",
    "PoolNotInitialized()",
    "SwapAmountCannotBeZero()",
    "NotEnoughLiquidity(bytes32)",
    "AllowanceExpired(uint256)",
    "InsufficientAllowance(uint256)",
    "TransferFromFailed()",
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
    use crate::route::{Token as RouteToken, Venue};
    use ethers::types::H256;

    /// Every byte set, so an address cannot be found by accident inside the
    /// zero padding of an offset or a length word.
    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    fn v4_hop(i: u8, o: u8, fee: u32) -> Hop {
        let (c0, c1) = if i < o { (addr(i), addr(o)) } else { (addr(o), addr(i)) };
        Hop {
            venue: Venue::V4 { pool_id: H256::from([i + o; 32]), hooks: addr(0xee) },
            currency0: c0,
            currency1: c1,
            fee,
            tick_spacing: 35,
            input: addr(i),
            output: addr(o),
            input_decimals: 18,
            output_decimals: 18,
        }
    }

    fn v3_hop(i: u8, o: u8, fee: u32) -> Hop {
        let (c0, c1) = if i < o { (addr(i), addr(o)) } else { (addr(o), addr(i)) };
        Hop {
            venue: Venue::V3 { pool: addr(i * 16 + o) },
            currency0: c0,
            currency1: c1,
            fee,
            tick_spacing: 60,
            input: addr(i),
            output: addr(o),
            input_decimals: 18,
            output_decimals: 18,
        }
    }

    fn route_of(hops: Vec<Hop>) -> Route {
        let last = hops[hops.len() - 1].output;
        let first = hops[0].input;
        Route {
            name: "t".into(),
            input: RouteToken { address: first, decimals: 18, symbol: "IN".into() },
            output: RouteToken { address: last, decimals: 18, symbol: "OUT".into() },
            amount_in: U256::from(1000u64),
            max_slippage_pct: 1.0,
            hops,
        }
    }

    /// The v4 struct as this fork decodes it, extra word and all.
    fn single_params_type() -> ParamType {
        ParamType::Tuple(vec![
            ParamType::Tuple(vec![
                ParamType::Address,
                ParamType::Address,
                ParamType::Uint(24),
                ParamType::Int(24),
                ParamType::Address,
            ]),
            ParamType::Bool,
            ParamType::Uint(128),
            ParamType::Uint(128),
            ParamType::Uint(256),
            ParamType::Bytes,
        ])
    }

    fn unwrap(data: &Bytes) -> (Vec<u8>, Vec<Vec<u8>>) {
        let outer = ethers::abi::decode(
            &[
                ParamType::Bytes,
                ParamType::Array(Box::new(ParamType::Bytes)),
                ParamType::Uint(256),
            ],
            &data[4..],
        )
        .unwrap();
        let commands = match &outer[0] {
            AbiToken::Bytes(c) => c.clone(),
            _ => panic!(),
        };
        let inputs = match &outer[1] {
            AbiToken::Array(i) => i
                .iter()
                .map(|t| match t {
                    AbiToken::Bytes(b) => b.clone(),
                    _ => panic!(),
                })
                .collect(),
            _ => panic!(),
        };
        (commands, inputs)
    }

    fn v4_actions(input: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        let d = ethers::abi::decode(
            &[ParamType::Bytes, ParamType::Array(Box::new(ParamType::Bytes))],
            input,
        )
        .unwrap();
        let actions = match &d[0] {
            AbiToken::Bytes(a) => a.clone(),
            _ => panic!(),
        };
        let params = match &d[1] {
            AbiToken::Array(p) => p
                .iter()
                .map(|t| match t {
                    AbiToken::Bytes(b) => b.clone(),
                    _ => panic!(),
                })
                .collect(),
            _ => panic!(),
        };
        (actions, params)
    }

    fn v3_input_type() -> Vec<ParamType> {
        vec![
            ParamType::Address,
            ParamType::Uint(256),
            ParamType::Uint(256),
            ParamType::Bytes,
            ParamType::Bool,
            ParamType::Array(Box::new(ParamType::Uint(256))),
        ]
    }

    #[test]
    fn slippage_floor_is_below_the_quote_and_never_zero() {
        let q = U256::from(1_000_000u64);
        assert_eq!(apply_slippage(q, 1.0), U256::from(990_000u64));
        assert_eq!(apply_slippage(q, 5.0), U256::from(950_000u64));
        assert!(apply_slippage(q, 0.0001) < q, "a tolerance too small to express still shaves");
        assert!(apply_slippage(q, 200.0) > U256::zero(), "and an absurd one cannot floor at zero");
    }

    #[test]
    fn execute_selector_is_the_published_one() {
        assert_eq!(hex::encode(selector("execute(bytes,bytes[],uint256)")), "3593564c");
    }

    #[test]
    fn one_protocol_is_one_command_however_many_hops() {
        let r = route_of(vec![v4_hop(1, 2, 3477), v4_hop(2, 3, 0)]);
        assert_eq!(legs(&r).len(), 1);
        let r = route_of(vec![v3_hop(1, 2, 10000), v3_hop(2, 3, 3000)]);
        assert_eq!(legs(&r).len(), 1);
    }

    #[test]
    fn only_a_protocol_boundary_starts_a_new_leg() {
        let r = route_of(vec![v3_hop(1, 2, 10000), v4_hop(2, 3, 0), v4_hop(3, 4, 500)]);
        let l = legs(&r);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].len(), 1, "the v3 run");
        assert_eq!(l[1].len(), 2, "both v4 hops share one command");

        let (commands, inputs) = unwrap(&execute_calldata(&r, U256::from(9u64), U256::zero()).unwrap());
        assert_eq!(commands, vec![CMD_V3_SWAP_EXACT_IN, CMD_V4_SWAP]);
        assert_eq!(inputs.len(), 2);
    }

    #[test]
    fn a_v4_only_route_settles_from_the_caller_and_pays_the_caller() {
        let r = route_of(vec![v4_hop(1, 2, 3477), v4_hop(2, 3, 0)]);
        let data = execute_calldata(&r, U256::from(950u64), U256::from(1234u64)).unwrap();
        assert_eq!(hex::encode(&data[..4]), "3593564c");
        let (commands, inputs) = unwrap(&data);
        assert_eq!(commands, vec![CMD_V4_SWAP]);

        let (actions, params) = v4_actions(&inputs[0]);
        assert_eq!(
            actions,
            vec![
                ACTION_SETTLE,
                ACTION_SWAP_EXACT_IN_SINGLE,
                ACTION_SWAP_EXACT_IN_SINGLE,
                ACTION_TAKE_ALL
            ]
        );
        assert_eq!(params.len(), actions.len(), "one param per action");

        let settle = ethers::abi::decode(
            &[ParamType::Address, ParamType::Uint(256), ParamType::Bool],
            &params[0],
        )
        .unwrap();
        assert_eq!(settle[0], AbiToken::Address(addr(1)), "the input currency");
        assert_eq!(settle[1], AbiToken::Uint(U256::from(1000u64)), "the whole amount in");
        assert_eq!(settle[2], AbiToken::Bool(true), "paid by the caller");

        // Every hop spends the open delta the settle created.
        for p in &params[1..3] {
            assert_eq!(U256::from_big_endian(&p[0..32]), U256::from(32u64), "leading offset");
            let f = match ethers::abi::decode(&[single_params_type()], p).unwrap().remove(0) {
                AbiToken::Tuple(f) => f,
                _ => panic!(),
            };
            assert_eq!(f[2], AbiToken::Uint(OPEN_DELTA), "amountIn");
            assert_eq!(f[3], AbiToken::Uint(U256::zero()), "per-hop minimum left to TAKE_ALL");
            assert_eq!(f[4], AbiToken::Uint(U256::zero()), "the fork's extra word stays zero");
        }

        let take =
            ethers::abi::decode(&[ParamType::Address, ParamType::Uint(256)], &params[3]).unwrap();
        assert_eq!(take[0], AbiToken::Address(addr(3)), "the route's output");
        assert_eq!(take[1], AbiToken::Uint(U256::from(950u64)), "floored at minOut");
    }

    #[test]
    fn a_v3_only_route_packs_the_whole_path_into_one_command() {
        let r = route_of(vec![v3_hop(1, 2, 10000), v3_hop(2, 3, 3000)]);
        let (commands, inputs) = unwrap(&execute_calldata(&r, U256::from(7u64), U256::zero()).unwrap());
        assert_eq!(commands, vec![CMD_V3_SWAP_EXACT_IN]);

        let d = ethers::abi::decode(&v3_input_type(), &inputs[0]).unwrap();
        assert_eq!(d[0], AbiToken::Address(msg_sender()), "paid straight to the caller");
        assert_eq!(d[1], AbiToken::Uint(U256::from(1000u64)));
        assert_eq!(d[2], AbiToken::Uint(U256::from(7u64)));
        assert_eq!(d[4], AbiToken::Bool(true), "payerIsUser");
        assert_eq!(d[5], AbiToken::Array(Vec::new()), "per-hop price check off");

        // tokenIn (fee tokenOut)+, packed, no padding.
        let path = match &d[3] {
            AbiToken::Bytes(b) => b.clone(),
            _ => panic!(),
        };
        assert_eq!(path.len(), 20 + 2 * 23);
        assert_eq!(&path[0..20], addr(1).as_bytes());
        assert_eq!(&path[20..23], &[0x00, 0x27, 0x10], "10000 as uint24");
        assert_eq!(&path[23..43], addr(2).as_bytes());
        assert_eq!(&path[43..46], &[0x00, 0x0b, 0xb8], "3000 as uint24");
        assert_eq!(&path[46..66], addr(3).as_bytes());
    }

    #[test]
    fn a_mixed_route_hands_the_middle_token_over_through_the_router() {
        // v3 -> v4: the v3 leg must leave its output on the router, and the v4
        // leg must pay itself from that balance rather than from the caller.
        let r = route_of(vec![v3_hop(1, 2, 10000), v4_hop(2, 3, 0)]);
        let (commands, inputs) = unwrap(&execute_calldata(&r, U256::from(5u64), U256::zero()).unwrap());
        assert_eq!(commands, vec![CMD_V3_SWAP_EXACT_IN, CMD_V4_SWAP]);

        let v3 = ethers::abi::decode(&v3_input_type(), &inputs[0]).unwrap();
        assert_eq!(v3[0], AbiToken::Address(address_this()), "output parked on the router");
        assert_eq!(v3[1], AbiToken::Uint(U256::from(1000u64)), "the caller funds the first leg");
        assert_eq!(v3[2], AbiToken::Uint(U256::zero()), "no minimum on an intermediate leg");
        assert_eq!(v3[4], AbiToken::Bool(true), "payerIsUser on the first leg");

        let (actions, params) = v4_actions(&inputs[1]);
        assert_eq!(
            actions,
            vec![ACTION_SETTLE, ACTION_SWAP_EXACT_IN_SINGLE, ACTION_TAKE_ALL]
        );
        let settle = ethers::abi::decode(
            &[ParamType::Address, ParamType::Uint(256), ParamType::Bool],
            &params[0],
        )
        .unwrap();
        assert_eq!(settle[0], AbiToken::Address(addr(2)), "the middle token");
        assert_eq!(settle[1], AbiToken::Uint(contract_balance()), "spend the router's whole balance");
        assert_eq!(settle[2], AbiToken::Bool(false), "not the caller - the router pays");
        let take =
            ethers::abi::decode(&[ParamType::Address, ParamType::Uint(256)], &params[2]).unwrap();
        assert_eq!(take[1], AbiToken::Uint(U256::from(5u64)), "the last leg carries the minimum");
    }

    #[test]
    fn a_v4_leg_that_is_not_last_parks_its_output_on_the_router() {
        let r = route_of(vec![v4_hop(1, 2, 3477), v3_hop(2, 3, 10000)]);
        let (commands, inputs) = unwrap(&execute_calldata(&r, U256::from(5u64), U256::zero()).unwrap());
        assert_eq!(commands, vec![CMD_V4_SWAP, CMD_V3_SWAP_EXACT_IN]);

        let (actions, params) = v4_actions(&inputs[0]);
        assert_eq!(
            actions,
            vec![ACTION_SETTLE, ACTION_SWAP_EXACT_IN_SINGLE, ACTION_TAKE],
            "TAKE, not TAKE_ALL: an intermediate leg pays the router, not the caller"
        );
        let take = ethers::abi::decode(
            &[ParamType::Address, ParamType::Address, ParamType::Uint(256)],
            &params[2],
        )
        .unwrap();
        assert_eq!(take[0], AbiToken::Address(addr(2)));
        assert_eq!(take[1], AbiToken::Address(address_this()));

        let v3 = ethers::abi::decode(&v3_input_type(), &inputs[1]).unwrap();
        assert_eq!(v3[0], AbiToken::Address(msg_sender()), "the last leg pays the caller");
        assert_eq!(v3[1], AbiToken::Uint(contract_balance()), "spend what the v4 leg left");
        assert_eq!(v3[4], AbiToken::Bool(false), "the router pays, not the caller");
        assert_eq!(v3[2], AbiToken::Uint(U256::from(5u64)), "the minimum lands here");
    }

    /// Picking up what the previous leg left is `CONTRACT_BALANCE` on both
    /// sides, while `OPEN_DELTA` means the open credit inside a v4 batch. Both
    /// are "whatever is there", they are different numbers, and swapping them
    /// fails quietly - so each one is pinned here.
    #[test]
    fn handing_over_uses_contract_balance_and_never_open_delta() {
        assert_eq!(OPEN_DELTA, U256::zero());
        assert_eq!(contract_balance(), U256::one() << 255);
        assert_ne!(OPEN_DELTA, contract_balance(), "these are not interchangeable");

        // v4 -> v3: the v3 leg spends what the v4 leg parked on the router.
        let r = route_of(vec![v4_hop(1, 2, 3477), v3_hop(2, 3, 10000)]);
        let (_, inputs) = unwrap(&execute_calldata(&r, U256::from(5u64), U256::zero()).unwrap());
        let v3 = ethers::abi::decode(&v3_input_type(), &inputs[1]).unwrap();
        assert_eq!(v3[1], AbiToken::Uint(contract_balance()), "v3 amountIn");

        // v3 -> v4: the settle picks the balance up, and the hop after it
        // swaps the credit that settle created.
        let r = route_of(vec![v3_hop(1, 2, 10000), v4_hop(2, 3, 0)]);
        let (_, inputs) = unwrap(&execute_calldata(&r, U256::from(5u64), U256::zero()).unwrap());
        let (_, params) = v4_actions(&inputs[1]);
        let settle = ethers::abi::decode(
            &[ParamType::Address, ParamType::Uint(256), ParamType::Bool],
            &params[0],
        )
        .unwrap();
        assert_eq!(
            settle[1],
            AbiToken::Uint(contract_balance()),
            "settle takes the balance, not the debt - zero here settles nothing"
        );
        let f = match ethers::abi::decode(&[single_params_type()], &params[1]).unwrap().remove(0) {
            AbiToken::Tuple(f) => f,
            _ => panic!(),
        };
        assert_eq!(f[2], AbiToken::Uint(OPEN_DELTA), "the hop swaps the open credit");
    }

    #[test]
    fn a_v3_pool_cannot_be_encoded_as_a_v4_hop() {
        let hop = v3_hop(1, 2, 10000);
        assert!(exact_in_single(&hop, U256::one(), U256::zero()).is_err());
    }

    #[test]
    fn native_input_is_paid_as_value() {
        let mut r = route_of(vec![v4_hop(1, 2, 3477)]);
        assert_eq!(call_value(&r), U256::zero());
        r.input.address = Address::zero();
        assert_eq!(call_value(&r), r.amount_in);
    }

    #[test]
    fn amounts_wider_than_uint128_are_refused() {
        let mut r = route_of(vec![v4_hop(1, 2, 3477)]);
        r.amount_in = U256::from(u128::MAX) + 1;
        assert!(execute_calldata(&r, U256::one(), U256::zero()).is_err());
    }

    #[test]
    fn the_real_output_is_read_out_of_the_refusal() {
        let mut d = selector("V4TooLittleReceived(uint256,uint256)");
        d.extend_from_slice(&encode(&[
            AbiToken::Uint(U256::from(u128::MAX)),
            AbiToken::Uint(U256::from(497_718_820_983_400_484u64)),
        ]));
        let msg = format!("(code: 3, message: execution reverted, data: Some(String(\"0x{}\")))",
            hex::encode(&d));
        assert_eq!(
            amount_from_too_little(&msg),
            Some(U256::from(497_718_820_983_400_484u64))
        );
        // A different error must not be read as an amount.
        let other = "(code: 3, message: execution reverted, data: Some(String(\"0x5212cba1\")))";
        assert_eq!(amount_from_too_little(other), None);
    }

    #[test]
    fn nested_execution_failure_is_unwrapped() {
        let inner = {
            let mut d = selector("V4TooLittleReceived(uint256,uint256)");
            d.extend_from_slice(&encode(&[
                AbiToken::Uint(U256::from(100u64)),
                AbiToken::Uint(U256::from(99u64)),
            ]));
            d
        };
        let mut outer = selector("ExecutionFailed(uint256,bytes)");
        outer.extend_from_slice(&encode(&[
            AbiToken::Uint(U256::zero()),
            AbiToken::Bytes(inner),
        ]));
        let s = decode_revert(&outer);
        assert!(s.contains("command 0 failed"), "{s}");
        assert!(s.contains("V4TooLittleReceived"), "{s}");
    }

    #[test]
    fn plain_string_reverts_are_readable() {
        let mut d = vec![0x08, 0xc3, 0x79, 0xa0];
        d.extend_from_slice(&encode(&[AbiToken::String("STF".into())]));
        assert_eq!(decode_revert(&d), "revert \"STF\"");
    }

    #[test]
    fn an_empty_revert_is_named_for_what_it_means() {
        assert!(decode_revert(&[]).contains("layout"));
    }

    #[test]
    fn payload_is_found_inside_provider_prose() {
        let msg = "(code: 3, message: execution reverted, data: Some(\"0x08c379a0\"))";
        assert!(revert_payload(msg).is_some());
        assert!(revert_payload("connection refused").is_none());
    }

    #[test]
    fn transport_errors_are_not_mistaken_for_reverts() {
        assert!(is_revert("execution reverted: STF"));
        assert!(is_revert("(code: 3, message: execution reverted)"));
        assert!(!is_revert("error sending request for url (...): connection closed"));
        assert!(!is_revert("(code: -32005, message: rate limit exceeded)"));
    }
}
