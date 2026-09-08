//! Deciding what to do about a launch, one step of its tax window at a time.
//!
//! No plan is made in advance. A plan priced at launch is a plan for a curve
//! nobody can still buy: by the time a step opens, the wallets exempted from
//! the snipe tax have traded, and on a bundled launch they have moved the
//! price several times over. So nothing is decided until a step is about to
//! open, and then it is decided against the reserves the curve actually has.
//!
//! One signal per step - 618 bps, then 19, then free - and one decision each.
//! The decision is a pure function of what is known at that moment, which is
//! what makes it testable: the same signal always produces the same answer,
//! and the answers are what a launch is judged by afterwards.
//!
//! **Buying twice is the failure this exists to prevent.** A step opens every
//! second and a transaction takes longer than that to settle, so the same
//! launch is asked about again while an answer is still in the air. The
//! position below is the memory that stops it: a curve with something in
//! flight is not bought again, a curve already bought is not bought again, and
//! a buy that reverted may be retried at a later step but never at the same
//! one.

use ethers::types::{Address, U256};

/// Where we stand with one launch.
///
/// Three of these are not reached yet: nothing is sent, so nothing is in
/// flight, held or refused by the chain. They are here rather than added later
/// because they are what the decision above reads, and the rules that depend
/// on them - never buy twice, never repeat a failed step - are the ones worth
/// having right before a wallet is attached rather than after.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Position {
    /// Nothing decided yet.
    Watching,
    /// Ruled out for good. A launch does not become buyable again because a
    /// second passed.
    Skipped { why: String },
    /// A buy is in the air for this step. Nothing else is sent until it
    /// settles, whatever the next step says.
    InFlight { step: u64 },
    /// Bought. One position per launch; the steps after it are not entries.
    Bought {
        step: u64,
        spend: U256,
        tokens: U256,
    },
    /// The chain refused the buy. A later step may be tried - the price moved
    /// or the tax fell - but never this one again.
    Failed { step: u64, why: String },
}

/// What is known about a launch that does not change.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Facts {
    /// Both are for the journal and for reading a decision back later, not for
    /// making one: a launch is judged on its terms and its price, never on its
    /// name.
    pub curve: Address,
    pub name: String,
    pub symbol: String,
    pub quote_symbol: String,
    pub quote_decimals: u8,
    pub creator_tax_bps: u64,
    /// How the launch was made. `launchAndBuy` means the launcher bought their
    /// own token in the same transaction; `launchToken` means they did not put
    /// in a single wei. Empty when the calldata was never decoded.
    pub via: &'static str,
    /// What the launcher spent on their own launch, against the curve's
    /// phantom reserve, in hundredths of a percent. Their own money is the
    /// only statement of intent available before anyone else has traded.
    pub dev_buy_x100: u64,
    /// What is known about whoever is behind this, from their previous
    /// launches. Absent for a first sighting, which most launches are: on one
    /// night 2797 of 3473 came from an address that never launched again.
    /// Present far more often than the deployer address alone would suggest,
    /// because operators are recognised by the wallets around them rather than
    /// by the address that signed - see [`crate::operators`].
    pub operator: Option<crate::operators::Verdict>,
    /// How many wallets the launch declared free of the snipe tax. They buy
    /// before anyone else can and sell into whoever comes next, so this is the
    /// single most predictive field there is.
    pub exempt: usize,
    /// The curve as it opened, for measuring how far it has already run.
    pub opening: crate::curve::Curve,
}

/// A step of the tax window, about to open.
pub struct Signal<'a> {
    pub step: u64,
    pub tax_bps: u64,
    /// The chain second this step opens at.
    pub opens_at: u64,
    /// How long until then, by our own clock. Negative means it has opened.
    pub in_ms: i64,
    /// The curve as it stands, from the cache the trade logs keep current.
    pub now: &'a crate::curve::Curve,
    pub facts: &'a Facts,
    pub position: &'a Position,
}

/// What this is willing to do.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// How many of an operator's positions must have closed before their
    /// record is allowed to refuse a launch. Zero never refuses on it.
    pub operator_needs: usize,
    /// What to spend, in the pair token's own units.
    pub spend: U256,
    /// How much of the price to give away, in basis points. Must stay below
    /// the gap between two tax steps for the minimum to also refuse a fill at
    /// the dearer one.
    pub slippage_bps: u64,
    /// The most snipe tax worth paying. The launch second is not reachable
    /// through this at any value - the wrapper refuses it outright.
    pub max_tax_bps: u64,
    /// A creator tax above this makes the round trip cost more than any edge:
    /// it is charged on the way in AND on the way out.
    pub max_creator_tax_bps: u64,
    /// How far above the opening price this will still buy, in hundredths. A
    /// bundled launch opens its free window at four or five times the price
    /// the curve started at, and buying there is buying the bundle's exit.
    pub max_run_x100: u64,
    /// A launch with more declared exemptions than this is somebody's
    /// arrangement rather than a market.
    pub max_exempt: usize,
    /// Refuse a launch whose maker did not buy their own token in the same
    /// transaction - `launchToken` rather than `launchAndBuy`.
    pub require_dev_buy: bool,
    /// The smallest dev buy worth following, against the phantom reserve, in
    /// hundredths of a percent.
    pub min_dev_buy_x100: u64,
}

/// What to do about this step.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    Buy {
        spend: U256,
        min_tokens_out: U256,
        why: String,
    },
    /// Not this step, but ask again at the next one.
    Wait { why: String },
    /// Not this launch, ever.
    Skip { why: String },
}

/// How far the curve has run from where it opened, in hundredths.
///
/// Measured on price rather than on either reserve alone: a launch is bought
/// into by some and sold out of by others, and only the ratio says what a
/// buyer now pays against what the first buyer paid.
pub fn run_x100(facts: &Facts, now: &crate::curve::Curve) -> u64 {
    let opened = price_x1e18(&facts.opening);
    let current = price_x1e18(now);
    if opened.is_zero() {
        return 0;
    }
    (current * U256::from(100u64) / opened)
        .min(U256::from(u64::MAX))
        .low_u64()
}

/// Quote per token, scaled, for comparing two states of the same curve.
fn price_x1e18(c: &crate::curve::Curve) -> U256 {
    if c.token_reserve.is_zero() {
        return U256::zero();
    }
    c.quote_reserve * U256::exp10(18) / c.token_reserve
}

/// Why this launch is not worth following at all, if it is not.
///
/// The terms a launch is refused on are known before it has traded: what the
/// creator takes off both legs, and how many wallets were let in free. Neither
/// changes, so a launch failing here is not watched, not journalled and not
/// asked about again - and the same function decides that and the `Skip` in
/// `decide`, so the two cannot drift apart.
pub fn refuse_outright(facts: &Facts, p: &Policy) -> Option<String> {
    if facts.creator_tax_bps > p.max_creator_tax_bps {
        return Some(format!(
            "creator tax {} bps, charged on the way in and again on the way out",
            facts.creator_tax_bps
        ));
    }
    if facts.exempt > p.max_exempt {
        return Some(format!(
            "{} wallets exempt from the snipe tax; they buy before anyone else can",
            facts.exempt
        ));
    }
    // Whether the maker put in their own money, and how much.
    //
    // The sharpest thing in the journals, and available from the feed before
    // the block exists: of the launches made through `launchToken` - where the
    // maker bought nothing - four in five saw no trade at all in their first
    // minute. Through `launchAndBuy`, none were dead. A launch nobody is
    // willing to buy at the price they set it at is a launch with no second
    // buyer either.
    if p.require_dev_buy && facts.via != "launchAndBuy" {
        return Some(if facts.via.is_empty() {
            "no calldata, so no telling whether the maker bought their own launch".to_string()
        } else {
            format!("made through {}: the maker bought none of it", facts.via)
        });
    }
    if facts.dev_buy_x100 < p.min_dev_buy_x100 {
        return Some(format!(
            "dev buy {}.{:02}% of the curve, under the {}.{:02}% worth following",
            facts.dev_buy_x100 / 100,
            facts.dev_buy_x100 % 100,
            p.min_dev_buy_x100 / 100,
            p.min_dev_buy_x100 % 100
        ));
    }
    // Last, because it is the only one of these that is about the people
    // rather than the terms - and the only one that needs a history to have
    // been kept. On one night the same operator ran 35 launches without a
    // single one worth holding, behind 35 different deployer addresses.
    if p.operator_needs > 0 {
        if let Some(v) = facts.operator.filter(|v| v.is_poor(p.operator_needs)) {
            return Some(format!(
                "this operator's last {} positions came back at {}.{:02}x of cost, over {} launches",
                v.closed,
                v.median_x100.unwrap_or(0) / 100,
                v.median_x100.unwrap_or(0) % 100,
                v.launches,
            ));
        }
    }
    None
}

/// What the maker spent on their own launch, against the phantom reserve.
///
/// Measured against the curve rather than in absolute terms so it means the
/// same thing on every pair: 0.1 ETH into a 1.68 ETH curve and 1 NVDA into a
/// 16.64 NVDA one are the same statement.
pub fn dev_buy_x100(spend: U256, opening: &crate::curve::Curve) -> u64 {
    if opening.quote_reserve.is_zero() {
        return 0;
    }
    (spend * U256::from(10_000u64) / opening.quote_reserve)
        .min(U256::from(u64::MAX))
        .low_u64()
}

/// The whole decision, as a function of what is known.
///
/// Ordered so the cheapest and most final answers come first: a launch ruled
/// out is not re-examined, a position already taken is not added to, and only
/// then is the price of this particular step looked at.
pub fn decide(s: &Signal, p: &Policy) -> Decision {
    // Already settled, one way or another.
    match s.position {
        Position::Skipped { why } => return Decision::Skip { why: why.clone() },
        Position::Bought { step, .. } => {
            return Decision::Wait {
                why: format!("already bought at +{step}s"),
            }
        }
        Position::InFlight { step } => {
            return Decision::Wait {
                why: format!("a buy from +{step}s is still in flight"),
            }
        }
        Position::Failed { step, .. } if *step == s.step => {
            return Decision::Wait {
                why: "this step already failed".to_string(),
            }
        }
        _ => {}
    }

    // Permanent refusals: nothing about a later step makes these better. The
    // same ones that stop a launch being followed in the first place.
    if let Some(why) = refuse_outright(s.facts, p) {
        return Decision::Skip { why };
    }

    // This step's own price.
    if s.tax_bps > p.max_tax_bps {
        return Decision::Wait {
            why: format!("{} bps is more than this pays", s.tax_bps),
        };
    }
    let run = run_x100(s.facts, s.now);
    if run > p.max_run_x100 {
        return Decision::Skip {
            why: format!("already {}.{:02}x the opening price", run / 100, run % 100),
        };
    }

    // Priced against the curve as it stands, never against how it opened.
    let fill = match crate::curve::buy(s.now, p.spend, s.tax_bps) {
        Ok(f) => f,
        Err(e) => {
            return Decision::Wait {
                why: format!("cannot price it: {e}"),
            }
        }
    };
    if fill.tokens_out.is_zero() {
        return Decision::Wait {
            why: "nothing to buy".to_string(),
        };
    }
    let min_tokens_out =
        fill.tokens_out * U256::from(10_000 - p.slippage_bps) / U256::from(10_000u64);
    Decision::Buy {
        spend: fill.spent,
        min_tokens_out,
        why: format!(
            "{} bps, {}.{:02}x the opening price",
            s.tax_bps,
            run / 100,
            run % 100
        ),
    }
}

/// One decision, as a line.
pub fn render(s: &Signal, d: &Decision, curve: Address, _took: std::time::Duration) -> String {
    let head = format!(
        "  step +{}s {:>4} bps  in {:>5}ms  {} ({})  {}.{:02}x",
        s.step,
        s.tax_bps,
        s.in_ms,
        if s.facts.symbol.is_empty() {
            short(&curve)
        } else {
            s.facts.symbol.clone()
        },
        s.facts.quote_symbol,
        run_x100(s.facts, s.now) / 100,
        run_x100(s.facts, s.now) % 100,
    );
    match d {
        Decision::Buy {
            spend,
            min_tokens_out,
            why,
        } => format!(
            "{head}  BUY {} {} min {}  [{why}]",
            crate::launch::amount_of(*spend, s.facts.quote_decimals),
            s.facts.quote_symbol,
            crate::launch::tokens_of(*min_tokens_out),
        ),
        Decision::Wait { why } => format!("{head}  wait  [{why}]"),
        Decision::Skip { why } => format!("{head}  SKIP  [{why}]"),
    }
}

fn short(a: &Address) -> String {
    let h = format!("{a:?}");
    format!("{}…{}", &h[..8], &h[h.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(quote: u64, tokens: u64) -> crate::curve::Curve {
        crate::curve::Curve {
            quote_reserve: U256::from(quote) * U256::exp10(16),
            token_reserve: U256::from(tokens) * U256::exp10(18),
            reserved_tokens: U256::from(285_714_285u64) * U256::exp10(18),
            fee_bps: 100,
            creator_tax_bps: 0,
        }
    }

    fn facts(exempt: usize, creator_tax_bps: u64) -> Facts {
        Facts {
            operator: None,
            via: "launchAndBuy",
            dev_buy_x100: 500,
            curve: Address::zero(),
            name: "T".into(),
            symbol: "T".into(),
            quote_symbol: "ETH".into(),
            quote_decimals: 18,
            creator_tax_bps,
            exempt,
            // 1.68 ETH against a billion tokens.
            opening: curve(168, 1_000_000_000),
        }
    }

    fn policy() -> Policy {
        Policy {
            operator_needs: 0,
            spend: U256::exp10(17), // 0.1
            slippage_bps: 100,
            max_tax_bps: 19,
            max_creator_tax_bps: 300,
            max_run_x100: 200,
            max_exempt: 4,
            require_dev_buy: true,
            min_dev_buy_x100: 500,
        }
    }

    fn signal<'a>(
        step: u64,
        tax_bps: u64,
        now: &'a crate::curve::Curve,
        facts: &'a Facts,
        position: &'a Position,
    ) -> Signal<'a> {
        Signal {
            step,
            tax_bps,
            opens_at: 1_788_825_495,
            in_ms: 300,
            now,
            facts,
            position,
        }
    }

    /// The ordinary case: a step opens, the curve has not run away, buy.
    #[test]
    fn a_cheap_step_on_an_untouched_curve_is_bought() {
        let f = facts(0, 0);
        let now = curve(168, 1_000_000_000);
        let d = decide(&signal(2, 19, &now, &f, &Position::Watching), &policy());
        match d {
            Decision::Buy {
                spend,
                min_tokens_out,
                ..
            } => {
                assert_eq!(spend, U256::exp10(17));
                assert!(min_tokens_out > U256::zero());
            }
            other => panic!("{other:?}"),
        }
    }

    /// Buying twice is the failure this whole module exists to prevent. A step
    /// opens every second; a transaction takes longer than that to settle.
    #[test]
    fn a_launch_is_never_bought_twice() {
        let f = facts(0, 0);
        let now = curve(168, 1_000_000_000);
        let p = policy();

        let flying = Position::InFlight { step: 1 };
        assert!(matches!(
            decide(&signal(2, 19, &now, &f, &flying), &p),
            Decision::Wait { .. }
        ));

        let held = Position::Bought {
            step: 1,
            spend: U256::exp10(17),
            tokens: U256::exp10(24),
        };
        assert!(matches!(
            decide(&signal(2, 19, &now, &f, &held), &p),
            Decision::Wait { .. }
        ));
        // And still not at the free step, which is the one most likely to be
        // asked last.
        assert!(matches!(
            decide(&signal(3, 0, &now, &f, &held), &p),
            Decision::Wait { .. }
        ));
    }

    /// A refusal from the chain is not a reason to stop, but the step that
    /// produced it is not tried again either.
    #[test]
    fn a_failed_step_is_not_repeated_but_a_later_one_is_allowed() {
        let f = facts(0, 0);
        let now = curve(168, 1_000_000_000);
        let p = policy();
        let failed = Position::Failed {
            step: 2,
            why: "SlippageExceeded".into(),
        };

        assert!(matches!(
            decide(&signal(2, 19, &now, &f, &failed), &p),
            Decision::Wait { .. }
        ));
        assert!(matches!(
            decide(&signal(3, 0, &now, &f, &failed), &p),
            Decision::Buy { .. }
        ));
    }

    /// What the journals showed: a bundled launch opens its free window at
    /// four or five times the price it started at. Buying there is buying the
    /// bundle's exit.
    #[test]
    fn a_curve_that_has_already_run_is_refused() {
        let f = facts(0, 0);
        // The state a bundle leaves behind: 3.66 against 459M, which is 4.7x.
        let bundled = curve(366, 459_016_393);
        assert_eq!(run_x100(&f, &bundled), 474);
        match decide(&signal(3, 0, &bundled, &f, &Position::Watching), &policy()) {
            Decision::Skip { why } => assert!(why.contains("4.74x"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    /// Both permanent refusals, and both from fields known before a single
    /// trade has happened.
    #[test]
    fn a_launch_can_be_ruled_out_before_it_trades() {
        let now = curve(168, 1_000_000_000);
        let p = policy();
        // 700 bps was seen live: charged on the way in and again on the way
        // out, so a round trip starts 16% down.
        let greedy = facts(0, 700);
        assert!(matches!(
            decide(&signal(2, 19, &now, &greedy, &Position::Watching), &p),
            Decision::Skip { .. }
        ));
        // And the same answer before a single trade, which is what stops the
        // launch being followed at all.
        assert!(refuse_outright(&greedy, &p).is_some());
        assert!(refuse_outright(&facts(0, 0), &p).is_none());
        // Nineteen exempt wallets were seen live too, and they took half the
        // curve before anyone else could bid.
        let arranged = facts(19, 0);
        assert!(matches!(
            decide(&signal(2, 19, &now, &arranged, &Position::Watching), &p),
            Decision::Skip { .. }
        ));
        assert!(refuse_outright(&arranged, &p).is_some());

        // The sharpest of them, and the only one known before the block: a
        // maker who bought none of their own launch. Four in five of these saw
        // no trade at all in their first minute.
        let unbought = Facts { via: "launchToken", dev_buy_x100: 0, ..facts(0, 0) };
        assert!(refuse_outright(&unbought, &p).is_some());
        assert!(matches!(
            decide(&signal(2, 19, &now, &unbought, &Position::Watching), &p),
            Decision::Skip { .. }
        ));
        // Bought, but barely: a tenth of a percent of the curve is not a
        // statement of intent.
        let token = Facts { dev_buy_x100: 10, ..facts(0, 0) };
        assert!(refuse_outright(&token, &p).is_some());
        // And a launch the maker actually put money into passes.
        assert!(refuse_outright(&facts(0, 0), &p).is_none());

        // Turned off, an unbought launch is followed like any other.
        let lax = Policy { require_dev_buy: false, min_dev_buy_x100: 0, ..policy() };
        assert!(refuse_outright(&unbought, &lax).is_none());
    }

    /// The size is measured against the curve, so it says the same thing on a
    /// 1.68 ETH launch and a 16.64 NVDA one.
    #[test]
    fn a_dev_buy_is_measured_against_the_curve() {
        let eth = curve(168, 1_000_000_000);
        let nvda = curve(1664, 1_000_000_000);
        assert_eq!(dev_buy_x100(U256::exp10(17), &eth), 595); // 0.1 of 1.68
        assert_eq!(dev_buy_x100(U256::exp10(18), &nvda), 600); // 1.0 of 16.64
        assert_eq!(dev_buy_x100(U256::zero(), &eth), 0);
    }

    /// A step dearer than this pays is waited out rather than refused: the
    /// next one is cheaper by construction.
    #[test]
    fn an_expensive_step_is_waited_out_and_the_next_one_is_taken() {
        let f = facts(0, 0);
        let now = curve(168, 1_000_000_000);
        let p = policy();
        assert!(matches!(
            decide(&signal(1, 618, &now, &f, &Position::Watching), &p),
            Decision::Wait { .. }
        ));
        assert!(matches!(
            decide(&signal(2, 19, &now, &f, &Position::Watching), &p),
            Decision::Buy { .. }
        ));

        // And a policy that is willing to pay for the first step takes it.
        let eager = Policy {
            max_tax_bps: 618,
            ..p
        };
        assert!(matches!(
            decide(&signal(1, 618, &now, &f, &Position::Watching), &eager),
            Decision::Buy { .. }
        ));
    }

    /// The minimum is what makes a step distinguishable from the one before
    /// it, so the allowance has to stay narrower than the gap between them.
    #[test]
    fn the_minimum_refuses_a_fill_at_the_dearer_step() {
        let f = facts(0, 0);
        let now = curve(168, 1_000_000_000);
        let p = policy();
        let Decision::Buy { min_tokens_out, .. } =
            decide(&signal(2, 19, &now, &f, &Position::Watching), &p)
        else {
            panic!("expected a buy")
        };
        // What the same spend would get one second early, at 618 bps.
        let dearer = crate::curve::buy(&now, p.spend, 618).unwrap();
        assert!(
            min_tokens_out > dearer.tokens_out,
            "a fill at 618 bps would satisfy this minimum"
        );
    }
    /// A record is only allowed to refuse once there is enough of it, and only
    /// when it is actually bad. On one night the same operator ran 35 launches
    /// with nothing worth holding on any of them, behind 35 different deployer
    /// addresses - this is the only rule here that could have seen that.
    #[test]
    fn an_operator_with_a_bad_record_is_passed_over() {
        let poor = crate::operators::Verdict {
            launches: 35,
            dead: 12,
            closed: 9,
            median_x100: Option::Some(84),
        };
        let good = crate::operators::Verdict {
            median_x100: Option::Some(140),
            ..poor
        };
        let thin = crate::operators::Verdict { closed: 2, ..poor };

        let p = Policy { operator_needs: 5, ..policy() };
        let with = |v| Facts { operator: v, ..facts(1, 0) };

        let why = refuse_outright(&with(Option::Some(poor)), &p).expect("should refuse");
        assert!(why.contains("0.84x"), "{why}");
        assert_eq!(refuse_outright(&with(Option::Some(good)), &p), None);
        assert_eq!(
            refuse_outright(&with(Option::Some(thin)), &p),
            None,
            "two closed positions are not a record"
        );
        assert_eq!(refuse_outright(&with(None), &p), None, "a stranger is not refused");
        // Turned off entirely, the record cannot refuse anything.
        let off = Policy { operator_needs: 0, ..p };
        assert_eq!(refuse_outright(&with(Option::Some(poor)), &off), None);
    }

}
