//! When to let a position go. Nothing here sends anything.
//!
//! The mirror of [`crate::snipe`]: it is handed a position and the curve as it
//! stands, and it answers. Whether that answer becomes a transaction is
//! somebody else's problem, and today nobody's - there is no wallet behind
//! this yet.
//!
//! Three rules, in the order they are checked, and each of them earned its
//! place on the journals rather than being reasoned into existence. Measured
//! over the 296 launches of one night that the entry filters would have taken,
//! entering a fixed fraction of each curve and allowing ONE block between
//! seeing a price and selling into it:
//!
//! | rule                          | total over 296 launches |
//! |-------------------------------|-------------------------|
//! | trailing 5% + hard exit at 2x | +43.8 stakes            |
//! | trailing 5% + hard exit at 1.5x | +34.1                 |
//! | trailing 5% + hard exit at 3x | +33.3                   |
//! | trailing 5%                   | +31.9                   |
//! | trailing 3%                   | +26.8                   |
//! | trailing 10%                  | +25.6                   |
//!
//! **One block, not none.** Selling into the very trade that broke the stop is
//! not a fast reaction, it is an impossible one: the trade IS the price move,
//! and seeing it means the block holding it is already made. That distinction
//! is worth most of the result - the same measurement at zero blocks reports
//! +76.5 against +31.9 - and the whole of it sits in that first block. Two
//! blocks costs +27.4, three costs +27.7, five costs +24.1. So the rules here
//! are chosen against a delay that can actually be achieved, and nothing is
//! gained by pretending it could be smaller.
//!
//! **The target is why the position closes at all on the launches that run.**
//! Half of them touch 1.5x and a third touch 2x, so a hard exit at twice cost
//! is reached often enough to matter and is where the measurement peaks - 3x
//! is reached by four percent, which is too rare to pay for the launches held
//! past 2x waiting for it.
//!
//! **The ladder loses**, which was the surprise. Selling half on the way up
//! cuts the launches that would have run and does nothing for the ones that
//! fall, because a launch that dies never reaches the first rung: of these,
//! half touch 1.5x and a third touch 2x, so the first rung is missed by half
//! the group and hit by every winner. What reduces the loss is the stop, and
//! only the stop - so there is one exit here and it is the whole position.
//! (The ladders were measured before the delay above was corrected, and have
//! not been measured since; the reachability is what the rule rests on.)

use crate::curve::Curve;
use anyhow::Result;
use ethers::types::U256;

const BASIS_POINTS: u64 = 10_000;

/// A position, and the most it has been worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Held {
    pub tokens: U256,
    /// What it cost, in the pair token, fees and all. The denominator of every
    /// multiple below.
    pub cost: U256,
    /// The most it has been worth so far. Carried rather than recomputed
    /// because the curve does not remember where it has been, and a trailing
    /// stop is a statement about the path and not about the price.
    pub high: U256,
    /// The block it was opened at, for the rule that gives up on it.
    pub opened_at: u64,
}

impl Held {
    /// Note what the position is worth now. The high only ever rises.
    pub fn mark(&mut self, worth: U256) {
        if worth > self.high {
            self.high = worth;
        }
    }

    /// What it is worth as a multiple of what it cost, in hundredths.
    ///
    /// Zero cost is treated as no gain rather than as infinite: a position
    /// that cost nothing is a bug upstream, and a rule that reads it as
    /// "sell, we are up enormously" would act on that bug.
    pub fn x100(&self, worth: U256) -> u64 {
        if self.cost.is_zero() {
            return 100;
        }
        // In 512 bits: a position worth more than 2^256/100 is not a real
        // number, but reaching it by multiplying first would wrap into a
        // small one and read as a loss.
        let scaled = worth.full_mul(U256::from(100u64)) / self.cost.full_mul(U256::one());
        u64::try_from(scaled).unwrap_or(u64::MAX)
    }
}

/// What closes a position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// How much of the high it may give back before it is sold, in basis
    /// points.
    pub trail_bps: u64,
    /// A multiple of cost that closes it outright, in hundredths. 200 is 2x.
    /// Zero turns it off.
    pub take_x100: u64,
    /// Blocks after opening at which it is sold whatever it is worth. A
    /// position nobody is trading is a position that costs the round trip and
    /// then keeps costing attention.
    pub hold_blocks: u64,
    /// How far below what it is worth now we are willing to be filled.
    pub slippage_bps: u64,
}

impl Policy {
    /// Refuse a policy that cannot do what it says.
    pub fn check(&self) -> Result<()> {
        anyhow::ensure!(
            self.trail_bps > 0 && self.trail_bps < BASIS_POINTS,
            "a trailing stop of {} bps never fires or fires at once",
            self.trail_bps
        );
        anyhow::ensure!(
            self.take_x100 == 0 || self.take_x100 > 100,
            "a take profit at {}% of cost sells at a loss the moment it opens",
            self.take_x100
        );
        anyhow::ensure!(self.hold_blocks > 0, "a position that is sold on the block it opened");
        anyhow::ensure!(
            self.slippage_bps < BASIS_POINTS,
            "a slippage allowance of the whole position"
        );
        Ok(())
    }
}

/// Hold it, or let it go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Hold {
        worth: U256,
        why: &'static str,
    },
    Sell {
        /// The whole position. There is no partial exit here on purpose - see
        /// the note at the top of this file.
        tokens: U256,
        /// What we refuse to be filled below.
        min_out: U256,
        worth: U256,
        why: &'static str,
    },
}

/// What selling the whole position into this curve would return.
///
/// The number every rule below is measured against, and the only honest one:
/// a position is worth what it can be sold for, not what the last trade
/// printed. Our own size moves the price, and on these curves it moves it
/// enough to matter.
pub fn worth(c: &Curve, tokens: U256) -> U256 {
    if tokens.is_zero() {
        return U256::zero();
    }
    crate::curve::sell(c, tokens)
        .map(|p| p.quote_out)
        .unwrap_or_default()
}

/// Hold or sell, against the curve as it stands.
///
/// `block` is the chain's own, so the rule that gives up on a position is
/// measured in the chain's time rather than ours - a stalled feed or a slow
/// loop must not close a position early.
///
/// The caller marks the high before asking. Not doing so makes the trailing
/// stop compare against a stale peak, which is the difference between a stop
/// and a coin toss.
pub fn decide(h: &Held, c: &Curve, block: u64, p: &Policy) -> Exit {
    let worth = worth(c, h.tokens);
    let min_out = worth * U256::from(BASIS_POINTS - p.slippage_bps) / U256::from(BASIS_POINTS);
    let sell = |why| Exit::Sell {
        tokens: h.tokens,
        min_out,
        worth,
        why,
    };

    // Taken first, because a position that is both at its target and off its
    // high should leave at the target: that is the better of the two prices
    // and the reason the target exists.
    if p.take_x100 > 0 && h.x100(worth) >= p.take_x100 {
        return sell("at the target");
    }
    // The high is whatever the caller last marked, so a position that has
    // only ever fallen is compared against what it was worth when it opened.
    let floor = h.high * U256::from(BASIS_POINTS - p.trail_bps) / U256::from(BASIS_POINTS);
    if worth < floor {
        return sell("gave back the stop");
    }
    if block >= h.opened_at.saturating_add(p.hold_blocks) {
        return sell("held long enough");
    }
    Exit::Hold {
        worth,
        why: if worth >= h.cost { "running" } else { "under water" },
    }
}

/// One line about a position, for the console.
pub fn render(h: &Held, e: &Exit, decimals: u8, symbol: &str) -> String {
    let (worth, verb, why) = match e {
        Exit::Hold { worth, why } => (*worth, "hold", *why),
        Exit::Sell { worth, why, .. } => (*worth, "SELL", *why),
    };
    format!(
        "  {verb}  {} -> {} {}  {}.{:02}x  high {}  [{why}]",
        crate::launch::amount_of(h.cost, decimals),
        crate::launch::amount_of(worth, decimals),
        symbol,
        h.x100(worth) / 100,
        h.x100(worth) % 100,
        crate::launch::amount_of(h.high, decimals),
    )
}

/// The exit that the night's journals preferred, for a caller with nothing to
/// say about it. Narrow stop, hard exit at twice cost, and a minute.
impl Default for Policy {
    fn default() -> Self {
        Self {
            trail_bps: 500,
            take_x100: 200,
            // Roughly a minute: this chain runs 9.8 blocks to the second, and
            // a curve nobody has traded in a minute is not about to start.
            hold_blocks: 588,
            slippage_bps: 100,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(quote: u64, tokens: u64) -> Curve {
        Curve {
            quote_reserve: U256::from(quote) * U256::exp10(16),
            token_reserve: U256::from(tokens) * U256::exp10(18),
            reserved_tokens: U256::zero(),
            fee_bps: 100,
            creator_tax_bps: 0,
        }
    }

    /// A position bought at what the curve would pay for it right now, which
    /// is what one actually costs give or take the fees.
    fn held(tokens: u64, c: &Curve) -> Held {
        let tokens = U256::from(tokens) * U256::exp10(18);
        let mut h = Held {
            tokens,
            cost: worth(c, tokens),
            high: U256::zero(),
            opened_at: 1_000,
        };
        h.mark(h.cost);
        h
    }

    /// The default is what the journals chose, and a change to it is a change
    /// of strategy rather than of taste.
    #[test]
    fn the_default_policy_is_the_one_that_was_measured() {
        let p = Policy::default();
        assert_eq!(p.trail_bps, 500);
        assert_eq!(p.take_x100, 200);
        p.check().unwrap();
    }

    /// A stop wider than everything never fires; one of zero fires at once.
    #[test]
    fn a_policy_that_cannot_work_is_refused() {
        for bad in [
            Policy { trail_bps: 0, ..Default::default() },
            Policy { trail_bps: 10_000, ..Default::default() },
            Policy { take_x100: 50, ..Default::default() },
            Policy { take_x100: 100, ..Default::default() },
            Policy { hold_blocks: 0, ..Default::default() },
            Policy { slippage_bps: 10_000, ..Default::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?} should be refused");
        }
        // A take profit turned off entirely is a choice, not a mistake.
        Policy { take_x100: 0, ..Default::default() }.check().unwrap();
    }

    /// The high ratchets. A stop that follows the price back down is not a
    /// stop at all.
    #[test]
    fn the_high_only_rises() {
        let c = curve(200, 1_000_000);
        let mut h = held(1_000, &c);
        let first = h.high;
        h.mark(first * U256::from(2u64));
        assert_eq!(h.high, first * U256::from(2u64));
        h.mark(first / U256::from(2u64));
        assert_eq!(h.high, first * U256::from(2u64), "the high followed it down");
    }

    /// A position on its way up is held, however far it has come.
    #[test]
    fn a_rising_position_is_held_until_the_target() {
        let c = curve(200, 1_000_000);
        let mut h = held(1_000, &c);
        let p = Policy { take_x100: 0, ..Default::default() };
        // Every buy against the curve makes the position worth more.
        for _ in 0..5 {
            let mut richer = c;
            richer.quote_reserve *= U256::from(2u64);
            let w = worth(&richer, h.tokens);
            h.mark(w);
            assert!(
                matches!(decide(&h, &richer, 1_001, &p), Exit::Hold { .. }),
                "sold something that was still going up"
            );
        }
    }

    /// The whole point: the stop fires on the give-back from the high, not on
    /// a loss against cost.
    #[test]
    fn the_stop_fires_on_the_give_back_and_not_on_a_loss() {
        let c = curve(200, 1_000_000);
        let mut h = held(1_000, &c);
        let p = Policy { take_x100: 0, trail_bps: 500, ..Default::default() };

        // Up a long way, then back a little: still well above cost, and sold.
        let mut rich = c;
        rich.quote_reserve *= U256::from(10u64);
        h.mark(worth(&rich, h.tokens));
        let mut off = rich;
        off.quote_reserve = off.quote_reserve * U256::from(90u64) / U256::from(100u64);
        let worth_now = worth(&off, h.tokens);
        assert!(worth_now > h.cost, "the test is not testing what it says");
        match decide(&h, &off, 1_001, &p) {
            Exit::Sell { why, tokens, .. } => {
                assert_eq!(why, "gave back the stop");
                assert_eq!(tokens, h.tokens, "a partial exit, which there is not");
            }
            other => panic!("held a position that gave back the stop: {other:?}"),
        }
    }

    /// The target wins over the stop when both would fire, because it is the
    /// better price of the two.
    #[test]
    fn the_target_is_taken_before_the_stop() {
        let c = curve(200, 1_000_000);
        let mut h = held(1_000, &c);
        let mut rich = c;
        rich.quote_reserve *= U256::from(50u64);
        h.mark(worth(&rich, h.tokens) * U256::from(2u64)); // a high well above
        let p = Policy { take_x100: 200, trail_bps: 500, ..Default::default() };
        assert!(
            h.x100(worth(&rich, h.tokens)) >= 200,
            "the test is not testing what it says"
        );
        match decide(&h, &rich, 1_001, &p) {
            Exit::Sell { why, .. } => assert_eq!(why, "at the target"),
            other => panic!("{other:?}"),
        }
    }

    /// A curve nobody trades still has to be let go of.
    #[test]
    fn a_position_nobody_trades_is_sold_in_the_end() {
        let c = curve(200, 1_000_000);
        let mut h = held(1_000, &c);
        h.mark(worth(&c, h.tokens));
        let p = Policy { take_x100: 0, hold_blocks: 588, ..Default::default() };
        assert!(matches!(decide(&h, &c, 1_500, &p), Exit::Hold { .. }));
        match decide(&h, &c, 1_588, &p) {
            Exit::Sell { why, .. } => assert_eq!(why, "held long enough"),
            other => panic!("{other:?}"),
        }
    }

    /// The minimum is a floor under what we are willing to be filled at, and
    /// it is derived from what the position is worth NOW rather than from its
    /// high or its cost.
    #[test]
    fn the_minimum_allows_exactly_the_slippage_and_no_more() {
        let c = curve(200, 1_000_000);
        let mut h = held(1_000, &c);
        h.mark(worth(&c, h.tokens));
        let p = Policy { take_x100: 0, hold_blocks: 1, slippage_bps: 250, ..Default::default() };
        match decide(&h, &c, 2_000, &p) {
            Exit::Sell { min_out, worth, .. } => {
                assert_eq!(min_out, worth * U256::from(9_750u64) / U256::from(10_000u64));
                assert!(min_out < worth);
            }
            other => panic!("{other:?}"),
        }
    }

    /// A position worth nothing must not read as an enormous gain, and must
    /// not panic.
    #[test]
    fn a_worthless_position_is_not_an_enormous_gain() {
        let c = curve(200, 1_000_000);
        let h = Held {
            tokens: U256::zero(),
            cost: U256::zero(),
            high: U256::zero(),
            opened_at: 0,
        };
        assert_eq!(worth(&c, h.tokens), U256::zero());
        assert_eq!(h.x100(U256::zero()), 100);
        assert_eq!(h.x100(U256::from(1_000_000u64)), 100);
    }

    /// What the position is worth is what the curve would pay for it, our own
    /// impact included - not the price the last trade printed.
    #[test]
    fn worth_is_what_the_curve_would_pay_for_the_whole_position() {
        let c = curve(200, 1_000_000);
        let tokens = U256::from(100_000u64) * U256::exp10(18);
        let got = worth(&c, tokens);
        assert_eq!(got, crate::curve::sell(&c, tokens).unwrap().quote_out);
        // A tenth of the supply against this curve is a long way from the
        // marginal price, which is the whole reason for pricing it this way.
        let marginal = c.quote_reserve * tokens / c.token_reserve;
        assert!(got < marginal * U256::from(95u64) / U256::from(100u64));
    }
}
