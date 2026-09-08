//! The bonding curve's arithmetic, done here instead of asked for.
//!
//! A port of `PonsV2BondingCurveMath`, which is the constant-product formula
//! with the fee taken off the input - the same shape as Uniswap v2, and the
//! same integer arithmetic. Reserves and fee are passed in rather than read,
//! because one formula prices a buy, a sell and the curve's own internal
//! buyback swap, and none of those wants a different answer.
//!
//! Ported rather than called for one reason: a quote from the chain costs a
//! round trip, and the whole of a launch happens inside three seconds. This is
//! the same arithmetic the curve will do, so a size decided here is the size
//! the curve agrees to - as long as the reserves it was given are current.
//!
//! **Exactness is the point.** Integer division truncates, `getAmountIn` adds
//! one to round in the curve's favour, and Solidity 0.8 reverts where these
//! return an error. A port that is off by one is a transaction that reverts on
//! `SlippageExceeded` for no reason anybody can see afterwards.
//!
//! [`buy`] is the curve's own `buy()` on top of that: the three fee legs come
//! off the input first and the swap itself is then priced at **zero fee**,
//! which is the one thing about it that cannot be guessed from the ABI.

// The port lands before the caller that will use it. Nothing here is dead in
// the sense that matters - every function is held to the contract by the tests
// below - but nothing buys anything yet either, and a `buy()` that needs this
// is the next thing rather than a thing that exists.
#![allow(dead_code)]

use anyhow::{Context, Result};
use ethers::types::{U256, U512};

pub const BASIS_POINTS: u64 = 10_000;

/// What an exact input buys, net of the fee charged on it.
///
/// The reverts are the contract's own, kept as errors under their Solidity
/// names: a caller that would revert on chain has to find out here rather than
/// by paying for the attempt.
pub fn amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u64,
) -> Result<U256> {
    anyhow::ensure!(!amount_in.is_zero(), "InsufficientInputAmount");
    anyhow::ensure!(
        !reserve_in.is_zero() && !reserve_out.is_zero(),
        "InsufficientLiquidity: a reserve is zero"
    );
    let out = raw_amount_out(amount_in, reserve_in, reserve_out, fee_bps)?;
    anyhow::ensure!(!out.is_zero(), "InsufficientOutputAmount");
    Ok(out)
}

/// The same quote, zero where [`amount_out`] would refuse.
///
/// For a caller that treats an unpriceable trade as a case to handle rather
/// than an error - the curve uses it for its own buyback, which folds back into
/// the creator's payout when the curve is too thin to swap against.
pub fn quote_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u64,
) -> U256 {
    if amount_in.is_zero()
        || reserve_in.is_zero()
        || reserve_out.is_zero()
        || fee_bps >= BASIS_POINTS
    {
        return U256::zero();
    }
    raw_amount_out(amount_in, reserve_in, reserve_out, fee_bps).unwrap_or_default()
}

/// What an exact output costs, net of the fee charged on the input.
///
/// Rounds **up**, by the contract's own `+ 1`: the curve must never be short a
/// wei because a division truncated in the buyer's favour.
pub fn amount_in(
    amount_out: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u64,
) -> Result<U256> {
    anyhow::ensure!(!amount_out.is_zero(), "InsufficientOutputAmount");
    anyhow::ensure!(
        !reserve_in.is_zero() && reserve_out > amount_out,
        "InsufficientLiquidity: the curve does not hold that much"
    );
    // A full-fee trade has no input that produces output, and the denominator
    // below would divide by zero rather than say so.
    anyhow::ensure!(
        fee_bps < BASIS_POINTS,
        "InsufficientLiquidity: fee is the whole trade"
    );

    let bp = U256::from(BASIS_POINTS);
    let numerator = amount_out
        .checked_mul(reserve_in)
        .and_then(|v| v.checked_mul(bp))
        .context("overflow: this trade reverts on chain")?;
    let denominator = (reserve_out - amount_out)
        .checked_mul(U256::from(BASIS_POINTS - fee_bps))
        .context("overflow: this trade reverts on chain")?;
    // `reserve_out > amount_out` and `fee_bps < BASIS_POINTS`, so this is not
    // zero; written as a check anyway, because a panic here would be a division
    // by zero inside a trading loop.
    anyhow::ensure!(!denominator.is_zero(), "InsufficientLiquidity");
    Ok(numerator / denominator + 1)
}

/// The formula itself, without the guards its callers apply.
fn raw_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u64,
) -> Result<U256> {
    // Solidity computes `BASIS_POINTS - feeBps` in checked arithmetic, so a fee
    // over 100% is a revert there and must not be a wrapped number here.
    anyhow::ensure!(
        fee_bps <= BASIS_POINTS,
        "fee of {fee_bps} bps is more than the whole trade"
    );
    let bp = U256::from(BASIS_POINTS);
    let with_fee = amount_in
        .checked_mul(U256::from(BASIS_POINTS - fee_bps))
        .context("overflow: this trade reverts on chain")?;
    let numerator = with_fee
        .checked_mul(reserve_out)
        .context("overflow: this trade reverts on chain")?;
    let denominator = reserve_in
        .checked_mul(bp)
        .and_then(|v| v.checked_add(with_fee))
        .context("overflow: this trade reverts on chain")?;
    anyhow::ensure!(!denominator.is_zero(), "InsufficientLiquidity");
    Ok(numerator / denominator)
}

/// A curve, as much of it as pricing a trade needs.
///
/// The reserves are `getReserves()`: the quote side is `phantomQuote +
/// trackedQuote - quoteFeeBalance - creatorTaxBalance`, so it is partly
/// virtual and never the contract's balance. Reading a balance instead is
/// exactly what the contract refuses to do, because anyone can send it money.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Curve {
    pub quote_reserve: U256,
    pub token_reserve: U256,
    /// The floor the curve will not sell through - the graduated pool's
    /// allocation. A buy is clamped to what is above it.
    pub reserved_tokens: U256,
    /// The trade fee, always charged on the quote leg.
    pub fee_bps: u64,
    /// The creator's own cut, layered on top of the fee and paid to them
    /// whole. Chosen at launch and in the calldata, not in any log.
    pub creator_tax_bps: u64,
}

/// What a buy would actually do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub tokens_out: U256,
    /// What is really paid. Less than what was offered when the fill was
    /// clamped, and the difference comes back.
    pub spent: U256,
    pub fee: U256,
    pub creator_tax: U256,
    pub snipe_tax: U256,
    pub refund: U256,
    /// The curve ran out of sellable allocation and filled what it could.
    pub clamped: bool,
}

impl Fill {
    /// Whether this fill clears `min_tokens_out`, by the contract's own test.
    ///
    /// Not `tokens_out >= min_tokens_out`: a clamped fill spends less than was
    /// offered, so the contract compares PRICES - `spent * min > received *
    /// out` reverts. The two are identical whenever nothing was clamped.
    ///
    /// Compared in 512 bits where the contract would revert on an overflowing
    /// product. Reaching that needs a spend times a minimum above 2^256, which
    /// no real trade is anywhere near.
    pub fn honours(&self, min_tokens_out: U256, received: U256) -> bool {
        self.spent.full_mul(min_tokens_out) <= received.full_mul(self.tokens_out)
    }
}

/// A curve the moment it opens, from a launch's own configuration.
///
/// Nothing has traded, so the quote side is only the phantom reserve and the
/// token side is the whole minted supply. `reserved_tokens` is the same
/// `Math.mulDiv` the contract does at `initialize`, and the same refusal when
/// a configuration rounds the allocation away.
pub fn at_launch(
    supply: U256,
    phantom_quote: U256,
    graduation_threshold: U256,
    fee_bps: u64,
    creator_tax_bps: u64,
) -> Result<Curve> {
    let denominator = phantom_quote
        .checked_add(graduation_threshold)
        .context("InvalidLaunchEconomics: the thresholds overflow")?;
    anyhow::ensure!(!denominator.is_zero(), "InvalidLaunchEconomics");
    let reserved = mul_div(supply, phantom_quote, denominator, false)?;
    anyhow::ensure!(
        !reserved.is_zero() && reserved < supply,
        "InvalidLaunchEconomics: the pool allocation rounds away"
    );
    Ok(Curve {
        quote_reserve: phantom_quote,
        token_reserve: supply,
        reserved_tokens: reserved,
        fee_bps,
        creator_tax_bps,
    })
}

/// What `buy(received, _, recipient)` would do, without asking anyone.
///
/// A port of `PonsV2BondingCurve.buy`, in its order:
///
/// 1. the snipe tax is capped so the three legs always leave the buyer at
///    least 1% of the spend - and only then, because an untaxed buy skips it;
/// 2. fee, creator tax and snipe tax come off the input, each floored;
/// 3. what is left is swapped at **zero fee** - the fee was already taken, and
///    passing it to the formula again would charge it twice;
/// 4. a fill past the sellable allocation is clamped to it, re-priced from the
///    token side, and grossed back up so the legs still come out of the input.
///    The rest is refunded rather than the trade being refused.
///
/// `snipe_tax_bps` is what the curve would charge THIS recipient at the
/// second the buy lands - see `launch::snipe_tax_bps`, and zero for an exempt
/// wallet.
pub fn buy(c: &Curve, received: U256, snipe_tax_bps: u64) -> Result<Fill> {
    anyhow::ensure!(!received.is_zero(), "ZeroAmount");
    let bp = U256::from(BASIS_POINTS);

    // The cap ignores the 20% ceiling on fee plus creator tax on purpose: a
    // 99% take in the launch second is the whole point of the mechanism.
    let snipe_bps = if snipe_tax_bps == 0 {
        0
    } else {
        let head_room = BASIS_POINTS
            .checked_sub(c.fee_bps)
            .and_then(|v| v.checked_sub(c.creator_tax_bps))
            .and_then(|v| v.checked_sub(100))
            .context("the fee and creator tax leave nothing to tax")?;
        snipe_tax_bps.min(head_room)
    };

    let legs = |spent: U256| -> (U256, U256, U256) {
        (
            spent * U256::from(c.fee_bps) / bp,
            spent * U256::from(c.creator_tax_bps) / bp,
            spent * U256::from(snipe_bps) / bp,
        )
    };

    let mut spent = received;
    let (mut fee, mut creator_tax, mut snipe_tax) = legs(spent);
    let net = spent
        .checked_sub(fee + creator_tax + snipe_tax)
        .context("the fees are more than the trade")?;
    let mut tokens_out = amount_out(net, c.quote_reserve, c.token_reserve, 0)?;

    let sellable = c.token_reserve.saturating_sub(c.reserved_tokens);
    anyhow::ensure!(!sellable.is_zero(), "CurveGraduated");

    let clamped = tokens_out > sellable;
    if clamped {
        tokens_out = sellable;
        let needed = amount_in(sellable, c.quote_reserve, c.token_reserve, 0)?;
        // Rounded UP, the contract's own `Math.Rounding.Ceil`: the grossed-up
        // spend must cover the net after flooring each leg, and a wei short
        // here is a fill the curve prices differently than the caller does.
        let gross = mul_div(
            needed,
            bp,
            U256::from(BASIS_POINTS - c.fee_bps - c.creator_tax_bps - snipe_bps),
            true,
        )?;
        spent = gross.min(received);
        let split = legs(spent);
        fee = split.0;
        creator_tax = split.1;
        snipe_tax = split.2;
    }

    Ok(Fill {
        tokens_out,
        spent,
        fee,
        creator_tax,
        snipe_tax,
        refund: received - spent,
        clamped,
    })
}

/// `Math.mulDiv`, in 512 bits like the contract's, rounding up when asked.
///
/// Full width because the contract has it: `a * b` overflowing 256 bits is not
/// a revert there, and a port that multiplied in 256 would refuse trades the
/// chain accepts.
fn mul_div(a: U256, b: U256, denominator: U256, ceil: bool) -> Result<U256> {
    anyhow::ensure!(!denominator.is_zero(), "division by zero");
    let d = U512::from(denominator);
    let mut n = a.full_mul(b);
    if ceil {
        n = n + d - U512::one();
    }
    let q = n / d;
    let mut bytes = [0u8; 64];
    q.to_big_endian(&mut bytes);
    anyhow::ensure!(
        bytes[..32].iter().all(|b| *b == 0),
        "overflow: this reverts on chain"
    );
    Ok(U256::from_big_endian(&bytes[32..]))
}

/// One step of the snipe tax, priced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// Seconds after the launch second. Zero is the launch second itself.
    pub elapsed: u64,
    /// The chain second this step begins at, and the one it ends before.
    pub from: u64,
    pub until: u64,
    pub tax_bps: u64,
    /// What the spend buys at this step, on the curve as it opens.
    pub tokens_out: U256,
    /// What to hand `buy` - `tokens_out` less the slippage allowance.
    pub min_tokens_out: U256,
    /// Whether `min_tokens_out` is still above what the step BEFORE this one
    /// would pay out. When it is, the minimum doubles as protection against
    /// landing a second early: the cheaper step cannot satisfy it, so the
    /// transaction reverts instead of buying at the higher tax.
    pub guards_the_step: bool,
}

/// A spend, and what it buys at every step of one launch's tax window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub spend: U256,
    pub slippage_bps: u64,
    pub steps: Vec<Step>,
}

/// The whole tax staircase for one launch, priced for a given spend.
///
/// The tax is a step per whole second, so a launch is not one decision but a
/// short list of them, each with a second it is available in and a price. This
/// is that list - what `N` buys at each step, when the step opens, and what
/// minimum to send with it.
///
/// The launch second is included and priced like the rest, because knowing
/// what it costs is the point of not buying in it.
///
/// **The slippage allowance is what makes the minimum useful for more than
/// slippage.** Set it below the gap between two steps and the minimum cannot
/// be met by the more expensive one, so a buy that lands a second early
/// reverts rather than paying the higher tax. Set it above that gap and the
/// minimum stops separating them - it still bounds the price, but it no longer
/// says which second we were in. `guards_the_step` says which of the two a
/// given allowance bought.
pub fn plan(
    c: &Curve,
    spend: U256,
    tax: &crate::launch::SnipeTax,
    launched_at: u64,
    slippage_bps: u64,
) -> Result<Plan> {
    anyhow::ensure!(
        slippage_bps < BASIS_POINTS,
        "a slippage allowance of the whole trade"
    );
    let mut steps = Vec::new();
    let mut dearer: Option<U256> = None;
    for elapsed in 0..=tax.seconds {
        let tax_bps = crate::launch::snipe_tax_bps(tax, elapsed);
        // A step that prices no fill at all is not a step to plan for.
        let Ok(fill) = buy(c, spend, tax_bps) else {
            continue;
        };
        let min_tokens_out =
            fill.tokens_out * U256::from(BASIS_POINTS - slippage_bps) / U256::from(BASIS_POINTS);
        steps.push(Step {
            elapsed,
            from: launched_at + elapsed,
            // The last step runs until the tax is gone and then forever, which
            // is the same second the window ends on.
            until: launched_at + elapsed + 1,
            tax_bps,
            tokens_out: fill.tokens_out,
            min_tokens_out,
            // Strictly above, not at: equal would be met by the dearer step
            // too, and the point is that it cannot be.
            guards_the_step: dearer.is_none_or(|d| min_tokens_out > d),
        });
        dearer = Some(fill.tokens_out);
    }
    anyhow::ensure!(!steps.is_empty(), "no step of this launch can be priced");
    Ok(Plan {
        spend,
        slippage_bps,
        steps,
    })
}

/// One of a curve's own logs, and what it did to the reserves.
///
/// The deltas are not modelled - they are read out of the event. Every field
/// needed is in it, which is what makes following a curve from its logs exact
/// rather than approximate:
///
/// * a buy adds what it spent less both fee legs, and takes out the tokens it
///   got: `quoteIn - fee - tax`, `-tokensOut`;
/// * a sell takes out what it paid the seller PLUS the fees it charged on the
///   way, because both come off the same leg: `-(quoteOut + fee + tax)`,
///   `+tokensIn`;
/// * a fee sweep moves nothing, except where it buys back: the fees were never
///   part of the reserve, but the buyback slice stays in as tradeable quote
///   and the tokens it locks leave. `+quoteSpent`, `-tokensLocked`.
///
/// A sweep with no buyback and a fee rescue both leave the reserves alone, so
/// neither needs to be heard at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trade {
    Buy {
        recipient: ethers::types::Address,
        quote_in: U256,
        tokens_out: U256,
        /// The base fee AND the snipe tax, which the curve reports together
        /// because they are paid into the same bucket.
        fee: U256,
        creator_tax: U256,
    },
    Sell {
        /// Who sold, and who took the proceeds. Kept because a journal of a
        /// launch is only worth writing if it says who did what.
        seller: ethers::types::Address,
        recipient: ethers::types::Address,
        tokens_in: U256,
        quote_out: U256,
        fee: U256,
        creator_tax: U256,
    },
    Buyback {
        quote_spent: U256,
        tokens_locked: U256,
    },
    /// The curve graduated. Nothing trades on it again.
    Completed,
}

pub const CURVE_BUY_SIG: &str = "CurveBuy(address,address,uint256,uint256,uint256,uint256)";
pub const CURVE_SELL_SIG: &str = "CurveSell(address,address,uint256,uint256,uint256,uint256)";
pub const BUYBACK_LOCKED_SIG: &str = "BuybackLocked(uint256,uint256)";
pub const CURVE_COMPLETED_SIG: &str = "CurveCompleted(address,uint256,uint256)";

/// Every topic a curve's reserves can move under.
pub fn trade_topics() -> Vec<ethers::types::H256> {
    [
        CURVE_BUY_SIG,
        CURVE_SELL_SIG,
        BUYBACK_LOCKED_SIG,
        CURVE_COMPLETED_SIG,
    ]
    .iter()
    .map(|s| crate::pool::event_topic(s))
    .collect()
}

pub fn decode_trade(log: &ethers::types::Log) -> Result<Trade> {
    let topic0 = *log.topics.first().context("log has no topics")?;
    let data = &log.data.0;
    let word = |i: usize| -> Result<U256> {
        let at = i * 32;
        anyhow::ensure!(
            data.len() >= at + 32,
            "trade data too short: {} bytes",
            data.len()
        );
        Ok(U256::from_big_endian(&data[at..at + 32]))
    };

    if topic0 == crate::pool::event_topic(CURVE_BUY_SIG) {
        anyhow::ensure!(
            log.topics.len() == 3,
            "CurveBuy has {} topics",
            log.topics.len()
        );
        Ok(Trade::Buy {
            recipient: ethers::types::Address::from_slice(&log.topics[2].as_bytes()[12..]),
            quote_in: word(0)?,
            tokens_out: word(1)?,
            fee: word(2)?,
            creator_tax: word(3)?,
        })
    } else if topic0 == crate::pool::event_topic(CURVE_SELL_SIG) {
        anyhow::ensure!(
            log.topics.len() == 3,
            "CurveSell has {} topics",
            log.topics.len()
        );
        Ok(Trade::Sell {
            seller: ethers::types::Address::from_slice(&log.topics[1].as_bytes()[12..]),
            recipient: ethers::types::Address::from_slice(&log.topics[2].as_bytes()[12..]),
            tokens_in: word(0)?,
            quote_out: word(1)?,
            fee: word(2)?,
            creator_tax: word(3)?,
        })
    } else if topic0 == crate::pool::event_topic(BUYBACK_LOCKED_SIG) {
        Ok(Trade::Buyback {
            quote_spent: word(0)?,
            tokens_locked: word(1)?,
        })
    } else if topic0 == crate::pool::event_topic(CURVE_COMPLETED_SIG) {
        Ok(Trade::Completed)
    } else {
        anyhow::bail!("not a curve trade: topic0 {topic0:?}")
    }
}

impl Curve {
    /// Move the reserves the way this trade moved the curve's own.
    ///
    /// Checked throughout. An underflow here does not mean a strange trade -
    /// it means the reserves being tracked are not the curve's any more, and
    /// carrying on from a number that is already wrong is how a local quote
    /// becomes a reverted transaction.
    pub fn apply(&mut self, trade: &Trade) -> Result<()> {
        match trade {
            Trade::Buy {
                quote_in,
                tokens_out,
                fee,
                creator_tax,
                ..
            } => {
                let net = quote_in
                    .checked_sub(*fee)
                    .and_then(|v| v.checked_sub(*creator_tax))
                    .context("a buy whose fees are more than it spent")?;
                self.quote_reserve = self
                    .quote_reserve
                    .checked_add(net)
                    .context("quote reserve overflow")?;
                self.token_reserve = self
                    .token_reserve
                    .checked_sub(*tokens_out)
                    .context("a buy took more tokens than the curve held")?;
            }
            Trade::Sell {
                tokens_in,
                quote_out,
                fee,
                creator_tax,
                ..
            } => {
                let gross = quote_out
                    .checked_add(*fee)
                    .and_then(|v| v.checked_add(*creator_tax))
                    .context("sell proceeds overflow")?;
                self.quote_reserve = self
                    .quote_reserve
                    .checked_sub(gross)
                    .context("a sell took more quote than the curve held")?;
                self.token_reserve = self
                    .token_reserve
                    .checked_add(*tokens_in)
                    .context("token reserve overflow")?;
            }
            Trade::Buyback {
                quote_spent,
                tokens_locked,
            } => {
                self.quote_reserve = self
                    .quote_reserve
                    .checked_add(*quote_spent)
                    .context("quote reserve overflow")?;
                self.token_reserve = self
                    .token_reserve
                    .checked_sub(*tokens_locked)
                    .context("a buyback locked more tokens than the curve held")?;
            }
            Trade::Completed => {}
        }
        Ok(())
    }
}

/// What this buy SHOULD have received, on the reserves as they stood.
///
/// The check that cannot be argued with: every field is in the log, including
/// what the buy was really charged, so there is nothing to assume and nothing
/// to calibrate. If this disagrees with `tokens_out`, either the reserves
/// being tracked have drifted from the curve's or the local pricing is wrong -
/// and both are worth hearing about before a trade is sized on either.
pub fn predicted_tokens_out(c: &Curve, trade: &Trade) -> Option<U256> {
    let Trade::Buy {
        quote_in,
        fee,
        creator_tax,
        ..
    } = trade
    else {
        return None;
    };
    let net = quote_in.checked_sub(*fee)?.checked_sub(*creator_tax)?;
    amount_out(net, c.quote_reserve, c.token_reserve, 0).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: u64) -> U256 {
        U256::from(v)
    }

    fn opening() -> Curve {
        let threshold = eth("4.2");
        at_launch(
            U256::exp10(9) * U256::exp10(18),
            threshold * U256::from(2u64) / U256::from(5u64),
            threshold,
            100,
            0,
        )
        .unwrap()
    }

    /// A buy read back off the curve\'s own log lands the reserves exactly
    /// where making that buy locally would have.
    ///
    /// This is the whole reason a curve can be followed without asking it
    /// anything: the log carries the spend, both fee legs and the tokens, so
    /// the delta is read rather than modelled.
    #[test]
    fn a_buy_moves_the_reserves_the_way_making_it_would_have() {
        let mut c = opening();
        let spend = eth("0.1");
        let fill = buy(&c, spend, 19).unwrap();

        // What the curve would log for that fill: fee and snipe tax together,
        // because the curve reports them in one field.
        let trade = Trade::Buy {
            recipient: ethers::types::Address::zero(),
            quote_in: fill.spent,
            tokens_out: fill.tokens_out,
            fee: fill.fee + fill.snipe_tax,
            creator_tax: fill.creator_tax,
        };
        // Predicted before applying, which is the order a live check runs in.
        assert_eq!(predicted_tokens_out(&c, &trade), Some(fill.tokens_out));

        let before = c;
        c.apply(&trade).unwrap();
        assert_eq!(c.token_reserve, before.token_reserve - fill.tokens_out);
        assert_eq!(
            c.quote_reserve,
            before.quote_reserve + fill.spent - fill.fee - fill.snipe_tax - fill.creator_tax
        );

        // And the next buy is priced against the moved curve: more spent, less
        // received, because the first one moved it.
        let second = buy(&c, spend, 19).unwrap();
        assert!(
            second.tokens_out < fill.tokens_out,
            "the curve did not move"
        );
    }

    /// A sell takes the fees off the same leg it pays out on, so the reserve
    /// loses the gross rather than the net. Reading it as the net would leave
    /// the tracked quote above the curve\'s, and every quote after it too high.
    #[test]
    fn a_sell_takes_the_gross_off_the_quote_side() {
        let mut c = opening();
        c.apply(&Trade::Buy {
            recipient: ethers::types::Address::zero(),
            quote_in: eth("1"),
            tokens_out: U256::exp10(24),
            fee: eth("0.01"),
            creator_tax: U256::zero(),
        })
        .unwrap();
        let after_buy = c;

        c.apply(&Trade::Sell {
            seller: ethers::types::Address::zero(),
            recipient: ethers::types::Address::zero(),
            tokens_in: U256::exp10(24),
            quote_out: eth("0.9"),
            fee: eth("0.01"),
            creator_tax: eth("0.005"),
        })
        .unwrap();
        assert_eq!(c.quote_reserve, after_buy.quote_reserve - eth("0.915"));
        assert_eq!(c.token_reserve, after_buy.token_reserve + U256::exp10(24));
    }

    /// The buyback slice stays in as tradeable quote and the tokens it locks
    /// leave, so a sweep that buys back moves the price and a sweep that does
    /// not moves nothing.
    #[test]
    fn a_buyback_is_the_only_sweep_that_moves_anything() {
        let mut c = opening();
        let before = c;
        c.apply(&Trade::Buyback {
            quote_spent: eth("0.01"),
            tokens_locked: U256::exp10(21),
        })
        .unwrap();
        assert_eq!(c.quote_reserve, before.quote_reserve + eth("0.01"));
        assert_eq!(c.token_reserve, before.token_reserve - U256::exp10(21));

        let mut d = opening();
        let unchanged = d;
        d.apply(&Trade::Completed).unwrap();
        assert_eq!(d, unchanged);
    }

    /// Reserves that cannot absorb a trade are reserves that are no longer the
    /// curve\'s, and saying so beats quoting from a number already wrong.
    #[test]
    fn a_trade_that_does_not_fit_is_an_error_and_not_a_wrap() {
        let mut c = opening();
        assert!(c
            .apply(&Trade::Sell {
                seller: ethers::types::Address::zero(),
                recipient: ethers::types::Address::zero(),
                tokens_in: U256::zero(),
                quote_out: eth("1000"),
                fee: U256::zero(),
                creator_tax: U256::zero(),
            })
            .is_err());
        assert!(c
            .apply(&Trade::Buy {
                recipient: ethers::types::Address::zero(),
                quote_in: eth("1"),
                tokens_out: U256::exp10(30),
                fee: U256::zero(),
                creator_tax: U256::zero(),
            })
            .is_err());
        // A buy whose fees exceed the spend is not a buy this understands.
        assert!(c
            .apply(&Trade::Buy {
                recipient: ethers::types::Address::zero(),
                quote_in: eth("1"),
                tokens_out: U256::zero(),
                fee: eth("2"),
                creator_tax: U256::zero(),
            })
            .is_err());
    }

    /// Known answers, and the topics the subscription asks for.
    #[test]
    fn the_trade_topics_are_the_signature_hashes() {
        let t = trade_topics();
        assert_eq!(
            format!("{:?}", t[0]),
            "0xec36bf571f136799e8dc0b0b8bea4b04d8bd3d43de838aab0d5fc21d4cbfc455"
        );
        assert_eq!(
            format!("{:?}", t[1]),
            "0x8113d738abdcb6b38357e9d53a54a7157861a09031b453651f0fe7fe151f59df"
        );
        assert_eq!(
            format!("{:?}", t[2]),
            "0x5feba9b0d52c92ada4b9c571c2bee52390c54f2947208ab250221e6ee32f12ff"
        );
        assert_eq!(
            format!("{:?}", t[3]),
            "0xf8d37a90738ae063b8b8058b66f5880cf3cf7ab0c5d4fa78219696591dfbfb67"
        );
    }

    /// The events, as they arrive.
    #[test]
    fn a_curve_log_decodes_to_its_trade() {
        use ethers::types::{Address, Log, H256};
        let mut data = Vec::new();
        for v in [1u64, 2, 3, 4] {
            let mut w = [0u8; 32];
            U256::from(v).to_big_endian(&mut w);
            data.extend_from_slice(&w);
        }
        let recipient = H256::from_low_u64_be(0x0b0b);
        let log = Log {
            topics: vec![trade_topics()[0], H256::zero(), recipient],
            data: data.clone().into(),
            ..Default::default()
        };
        assert_eq!(
            decode_trade(&log).unwrap(),
            Trade::Buy {
                recipient: Address::from_low_u64_be(0x0b0b),
                quote_in: U256::from(1u64),
                tokens_out: U256::from(2u64),
                fee: U256::from(3u64),
                creator_tax: U256::from(4u64),
            }
        );

        let sell = Log {
            topics: vec![trade_topics()[1], H256::zero(), recipient],
            data: data.into(),
            ..Default::default()
        };
        assert!(matches!(decode_trade(&sell).unwrap(), Trade::Sell { .. }));

        // A buy with the wrong number of topics is not this event, whatever
        // its topic0 says.
        let mangled = Log {
            topics: vec![trade_topics()[0]],
            data: vec![0u8; 128].into(),
            ..Default::default()
        };
        assert!(decode_trade(&mangled).is_err());
        // And a short one cannot be read past its end.
        let short = Log {
            topics: vec![trade_topics()[0], H256::zero(), recipient],
            data: vec![0u8; 64].into(),
            ..Default::default()
        };
        assert!(decode_trade(&short).is_err());
    }

    /// The staircase, priced, on a real opening curve.
    #[test]
    fn the_plan_prices_every_second_of_the_window() {
        let threshold = eth("4.2");
        let c = at_launch(
            U256::exp10(9) * U256::exp10(18),
            threshold * U256::from(2u64) / U256::from(5u64),
            threshold,
            100,
            0,
        )
        .unwrap();
        let tax = crate::launch::SnipeTax {
            start_bps: 9900,
            seconds: 3,
        };
        let steps = plan(&c, eth("0.1"), &tax, 1_788_817_246, 100)
            .unwrap()
            .steps;

        assert_eq!(steps.len(), 4);
        assert_eq!(
            steps.iter().map(|s| s.tax_bps).collect::<Vec<_>>(),
            vec![9900, 618, 19, 0]
        );
        // Absolute seconds, which is what a decision is made against.
        assert_eq!(steps[1].from, 1_788_817_247);
        assert_eq!(steps[1].until, 1_788_817_248);
        // Later is always more, because the tax only ever falls.
        for pair in steps.windows(2) {
            assert!(
                pair[1].tokens_out > pair[0].tokens_out,
                "the staircase does not rise"
            );
        }
    }

    /// The property that makes the minimum worth more than slippage: below the
    /// gap between two steps it refuses the dearer one, above it it does not.
    ///
    /// The gap between 618 bps and 19 bps is about six percent, so a one
    /// percent allowance separates them and a ten percent allowance does not -
    /// and a buy sent with the loose minimum would quietly pay 618 bps if it
    /// landed a second early.
    #[test]
    fn a_minimum_stops_separating_the_steps_once_it_is_wider_than_they_are() {
        let threshold = eth("4.2");
        let c = at_launch(
            U256::exp10(9) * U256::exp10(18),
            threshold * U256::from(2u64) / U256::from(5u64),
            threshold,
            100,
            0,
        )
        .unwrap();
        let tax = crate::launch::SnipeTax {
            start_bps: 9900,
            seconds: 3,
        };

        let tight = plan(&c, eth("0.1"), &tax, 0, 100).unwrap().steps;
        // The pair that matters: at 19 bps the minimum is above anything the
        // 618 step could pay, so landing a second early reverts.
        let at_19 = tight.iter().find(|s| s.tax_bps == 19).unwrap();
        let at_618 = tight.iter().find(|s| s.tax_bps == 618).unwrap();
        assert!(at_19.guards_the_step);
        assert!(at_19.min_tokens_out > at_618.tokens_out);

        // The free step does NOT separate from 19 bps at this allowance, and
        // says so: those two are 0.19% apart, which is narrower than any
        // slippage worth allowing. Aiming at free rather than at 19 is
        // therefore a preference, not something a minimum can enforce - and
        // the difference it protects is not worth enforcing anyway.
        let free = tight.iter().find(|s| s.tax_bps == 0).unwrap();
        assert!(!free.guards_the_step);

        let loose = plan(&c, eth("0.1"), &tax, 0, 1000).unwrap().steps;
        let at_19 = loose.iter().find(|s| s.tax_bps == 19).unwrap();
        let at_618 = loose.iter().find(|s| s.tax_bps == 618).unwrap();
        assert!(!at_19.guards_the_step, "10% cannot separate steps 6% apart");
        assert!(at_19.min_tokens_out < at_618.tokens_out);
    }

    /// Seven real launches off this chain, priced from their own logs alone.
    ///
    /// This is the whole model held against reality: the constant-product
    /// formula, the fee taken off the input, the creator tax on top of it, and
    /// the zero-fee swap after both. Every one of these reproduces the tokens
    /// the dev buy actually received, to the wei, across four different quote
    /// assets - including USDG, which has six decimals rather than eighteen.
    ///
    /// The configuration behind them was recovered from these numbers rather
    /// than read from the chain: a supply of 1e9, a 100 bps curve fee, and a
    /// phantom quote reserve of exactly two fifths of the graduation
    /// threshold - and the threshold is in the launch log. So the opening
    /// curve of a launch is knowable the moment it is announced, with no call
    /// to anybody. It should still be confirmed against `getLaunchConfig(0)`
    /// before a trade is sized on it, and nothing in the code above assumes
    /// it: this test is the record of the finding, not a shortcut in the path.
    #[test]
    fn every_launch_we_watched_prices_exactly() {
        const SUPPLY_TOKENS: u64 = 1_000_000_000;
        const FEE_BPS: u64 = 100;

        // name, graduation threshold, dev spend, creator tax bps, tokens out.
        // Thresholds and spends are in the quote token\'s own units.
        let launches: [(&str, U256, U256, u64, &str); 7] = [
            (
                "SwipeRWA (ETH)",
                eth("4.2"),
                eth("0.008"),
                200,
                "4597810115182253400957482",
            ),
            (
                "Zcash Mascot (ETH)",
                eth("4.2"),
                eth("0.086"),
                0,
                "48234134402936877528128080",
            ),
            (
                "Brainlet (ETH)",
                eth("4.2"),
                eth("0.026"),
                0,
                "15090224770480847022406697",
            ),
            (
                "Ponshub (ETH)",
                eth("4.2"),
                eth("0.0168"),
                200,
                "9606813905120332772110527",
            ),
            (
                "ad astra (SPCX)",
                eth("72.2"),
                U256::from(82_830_468_746_788_271u64),
                100,
                "2802851147056824129754936",
            ),
            (
                "TrainJohnson (NVDA)",
                eth("41.6"),
                U256::from(1_071_620_000_000_000u64),
                200,
                "62464331136674477006613",
            ),
            // USDG is a six-decimal token, so both its threshold and its spend
            // are six-decimal too: 8090 USDG and 374.055057 USDG.
            (
                "USDG launch",
                U256::from(8_090_000_000u64),
                U256::from(374_055_057u64),
                0,
                "102685028241770040385443287",
            ),
        ];

        for (name, threshold, spend, creator_tax_bps, want) in launches {
            // Two fifths of the graduation threshold, exactly.
            let phantom = threshold * U256::from(2u64) / U256::from(5u64);
            let supply = U256::from(SUPPLY_TOKENS) * U256::exp10(18);
            let c = at_launch(supply, phantom, threshold, FEE_BPS, creator_tax_bps).unwrap();
            // The dev buy is exempt: the factory exempts the creator and the
            // recipient inside the launch transaction itself.
            let fill = buy(&c, spend, 0).unwrap();
            assert_eq!(
                fill.tokens_out,
                U256::from_dec_str(want).unwrap(),
                "{name} priced wrong"
            );
            assert!(!fill.clamped, "{name} should not clamp on its first buy");
            assert_eq!(fill.spent, spend);
            assert!(fill.refund.is_zero());
            assert_eq!(
                fill.fee,
                spend * U256::from(FEE_BPS) / U256::from(BASIS_POINTS)
            );
            assert_eq!(
                fill.creator_tax,
                spend * U256::from(creator_tax_bps) / U256::from(BASIS_POINTS)
            );
            assert!(fill.snipe_tax.is_zero());
        }
    }

    /// What the launch second actually costs, on a real curve.
    #[test]
    fn a_sniped_buy_pays_the_tax_and_gets_the_tokens_that_are_left() {
        let threshold = eth("4.2");
        let c = at_launch(
            U256::exp10(9) * U256::exp10(18),
            threshold * U256::from(2u64) / U256::from(5u64),
            threshold,
            100,
            200,
        )
        .unwrap();
        let spend = eth("0.086");

        let free = buy(&c, spend, 0).unwrap();
        // 9900 bps is capped to 10000 - 100 - 200 - 100 = 9600, so a buy in
        // the launch second keeps 1% of its spend rather than nothing.
        let sniped = buy(&c, spend, 9900).unwrap();
        assert_eq!(
            sniped.snipe_tax,
            spend * U256::from(9600u64) / U256::from(BASIS_POINTS)
        );
        assert!(
            sniped.tokens_out * U256::from(20u64) < free.tokens_out,
            "the tax did not bite"
        );

        // One second later the tax is 618 bps, and the buy keeps most of it.
        let second = buy(&c, spend, 618).unwrap();
        assert!(second.tokens_out * U256::from(100u64) > free.tokens_out * U256::from(93u64));
        // Two seconds later it is 19 bps, which is noise next to the 1% fee.
        let third = buy(&c, spend, 19).unwrap();
        assert!(third.tokens_out * U256::from(1000u64) > free.tokens_out * U256::from(997u64));
    }

    /// The last buy of a launch: filled to the allocation, charged for what it
    /// got, and refunded the rest. The contract fills rather than reverts on
    /// purpose - reverting would let anyone grief the closing buy by slipping
    /// a small one in ahead of it.
    #[test]
    fn a_buy_past_the_allocation_is_filled_and_refunded() {
        let threshold = eth("4.2");
        let c = at_launch(
            U256::exp10(9) * U256::exp10(18),
            threshold * U256::from(2u64) / U256::from(5u64),
            threshold,
            100,
            0,
        )
        .unwrap();
        // Far more than the curve can ever sell.
        let fill = buy(&c, eth("1000"), 0).unwrap();
        assert!(fill.clamped);
        assert_eq!(fill.tokens_out, c.token_reserve - c.reserved_tokens);
        assert!(fill.spent < eth("1000"));
        assert_eq!(fill.refund, eth("1000") - fill.spent);
        // Grossed up so the legs still come out of what was spent.
        assert_eq!(
            fill.fee,
            fill.spent * U256::from(100u64) / U256::from(BASIS_POINTS)
        );

        // A clamped fill is judged on price, not quantity: asking for the
        // whole quantity at the whole offer still passes, because the price
        // paid is the price the caller\'s own arguments implied.
        assert!(fill.honours(fill.tokens_out, fill.spent));
        // And a price better than the curve can give is refused.
        assert!(!fill.honours(fill.tokens_out * U256::from(2u64), fill.spent));
    }

    /// The allocation the curve will not sell through, which is where the
    /// launch graduates.
    #[test]
    fn the_reserved_allocation_is_the_graduation_point() {
        let threshold = eth("4.2");
        let supply = U256::exp10(9) * U256::exp10(18);
        let c = at_launch(
            supply,
            threshold * U256::from(2u64) / U256::from(5u64),
            threshold,
            100,
            0,
        )
        .unwrap();
        // phantom / (phantom + threshold) = 0.4 / 1.4 = two sevenths.
        assert_eq!(
            c.reserved_tokens,
            supply * U256::from(2u64) / U256::from(7u64)
        );

        // A configuration whose allocation rounds away is refused at launch,
        // not at graduation.
        assert!(at_launch(U256::from(1u64), U256::one(), U256::exp10(30), 100, 0).is_err());
        assert!(at_launch(U256::exp10(27), U256::zero(), threshold, 100, 0).is_err());
    }

    /// Whole units of an eighteen-decimal quote, for readability above.
    fn eth(v: &str) -> U256 {
        crate::route::parse_units(v, 18).unwrap()
    }

    /// Worked by hand against the Solidity, digit for digit. 1000 in, 1% fee,
    /// a million each side:
    ///
    /// ```text
    /// withFee     = 1000 * 9900            =         9_900_000
    /// numerator   = 9_900_000 * 1_000_000  = 9_900_000_000_000
    /// denominator = 1_000_000 * 10_000 + 9_900_000 = 10_009_900_000
    /// out         = 989.02...              ->               989
    /// ```
    #[test]
    fn a_quote_truncates_where_the_contract_truncates() {
        assert_eq!(
            amount_out(u(1000), u(1_000_000), u(1_000_000), 100).unwrap(),
            u(989)
        );
    }

    /// The round trip closes: asking what 989 costs gives back the 1000 that
    /// bought it, because `getAmountIn` rounds up by one where the division
    /// would have left the curve short.
    #[test]
    fn the_round_trip_rounds_the_curves_way() {
        let out = amount_out(u(1000), u(1_000_000), u(1_000_000), 100).unwrap();
        let back = amount_in(out, u(1_000_000), u(1_000_000), 100).unwrap();
        assert_eq!(back, u(1000));
        // Never less than what was paid: the +1 is what keeps a quote from
        // being a wei short and reverting on SlippageExceeded.
        assert!(back >= u(1000));
    }

    /// With no fee it is x*y=k and nothing else, which is the one case that can
    /// be checked against arithmetic rather than against the source.
    #[test]
    fn a_free_trade_is_the_constant_product() {
        let (x, y) = (u(1_000_000), u(1_000_000));
        let dx = u(1000);
        let out = amount_out(dx, x, y, 0).unwrap();
        // k after >= k before: the curve never loses value to rounding.
        let k_before = x * y;
        let k_after = (x + dx) * (y - out);
        assert!(k_after >= k_before, "{k_after} < {k_before}");
        // With no fee the whole formula collapses to this, truncated - and
        // the truncation is a wei the curve keeps, not one the buyer gets.
        assert_eq!(out, dx * y / (x + dx));
        assert_eq!(out, u(999));
    }

    /// Every refusal the contract has, under the name it has it under. Each one
    /// is a transaction that would revert, and finding out here costs nothing.
    #[test]
    fn what_the_curve_refuses_is_refused_here() {
        let m = u(1_000_000);
        assert!(amount_out(U256::zero(), m, m, 100).is_err());
        assert!(amount_out(u(1), U256::zero(), m, 100).is_err());
        assert!(amount_out(u(1), m, U256::zero(), 100).is_err());
        // A fee of the whole trade leaves nothing to swap, so the output is
        // zero and the contract calls that InsufficientOutputAmount.
        assert!(amount_out(u(1000), m, m, BASIS_POINTS).is_err());
        // And a fee over 100% underflows in Solidity rather than wrapping.
        assert!(amount_out(u(1000), m, m, BASIS_POINTS + 1).is_err());
        // Too small to move a big curve: truncation makes this zero, and zero
        // out is a revert rather than a free trade.
        assert!(amount_out(u(1), U256::exp10(30), u(1), 100).is_err());

        assert!(amount_in(U256::zero(), m, m, 100).is_err());
        assert!(amount_in(u(1), U256::zero(), m, 100).is_err());
        // Asking for the whole reserve, or more than it holds.
        assert!(amount_in(m, m, m, 100).is_err());
        assert!(amount_in(m + 1, m, m, 100).is_err());
        assert!(amount_in(u(1), m, m, BASIS_POINTS).is_err());
    }

    /// The forgiving variant answers zero exactly where the other refuses, and
    /// the same number everywhere else.
    #[test]
    fn the_quiet_quote_is_zero_and_not_a_guess() {
        let m = u(1_000_000);
        assert_eq!(quote_amount_out(U256::zero(), m, m, 100), U256::zero());
        assert_eq!(quote_amount_out(u(1), U256::zero(), m, 100), U256::zero());
        assert_eq!(quote_amount_out(u(1), m, U256::zero(), 100), U256::zero());
        assert_eq!(quote_amount_out(u(1000), m, m, BASIS_POINTS), U256::zero());
        assert_eq!(
            quote_amount_out(u(1000), m, m, BASIS_POINTS + 1),
            U256::zero()
        );
        assert_eq!(
            quote_amount_out(u(1000), m, m, 100),
            amount_out(u(1000), m, m, 100).unwrap()
        );
    }

    /// Solidity 0.8 reverts on overflow. Wrapping instead would hand a caller a
    /// number where the chain would have refused - the one failure that costs
    /// money rather than a request.
    #[test]
    fn an_overflow_is_a_refusal_and_never_a_wrap() {
        let big = U256::MAX;
        assert!(amount_out(big, big, big, 0).is_err());
        assert!(amount_in(big / 2, big, big, 0).is_err());
        assert_eq!(quote_amount_out(big, big, big, 0), U256::zero());
    }

    /// A launch-sized case, in the units these actually trade in: a curve with
    /// 4.2 ETH of phantom quote against a billion tokens, bought with 0.086 ETH
    /// at the 100 bps the configs use.
    #[test]
    fn a_launch_sized_buy_is_priced_without_asking_anyone() {
        let quote_reserve = U256::from(42u64) * U256::exp10(17); // 4.2e18
        let token_reserve = U256::exp10(9) * U256::exp10(18); // 1e9 tokens
        let spend = U256::from(86u64) * U256::exp10(15); // 0.086e18
        let out = amount_out(spend, quote_reserve, token_reserve, 100).unwrap();
        // Roughly 2% of the curve, so roughly 2% of the supply, and strictly
        // less than the proportional share because the curve prices the move.
        assert!(
            out < token_reserve * spend / quote_reserve,
            "no impact was charged"
        );
        assert!(
            out > token_reserve / U256::from(100u64),
            "impact is implausibly large"
        );
        // And it costs what it costs: back through the other side.
        let cost = amount_in(out, quote_reserve, token_reserve, 100).unwrap();
        assert!(cost <= spend, "the round trip asks for more than was paid");
    }
}
