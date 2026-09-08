//! One file per launch, one line per thing that happened to it.
//!
//! Written for reading later rather than for reading now. A launch is decided
//! in three seconds and understood in an afternoon: who bought, in which
//! second, at what tax, and what it cost them - and none of that can be
//! recovered afterwards from the chain without asking for every log again.
//!
//! JSON lines, appended: a header for the launch and one line per trade. The
//! format is chosen so a crash mid-write loses one line rather than a file,
//! and so `grep`, `jq` and anything else can read it without this program.
//!
//! Every amount is written twice: once in the token's own units and once in
//! the integer the chain actually moved. Both are **strings**, never JSON
//! numbers - a token amount runs to twenty-seven digits and a JSON number is a
//! double, so writing either as a number would silently round the very
//! quantities the file exists to record. The readable form is exact too: it is
//! the same integer with a decimal point put in, not a rounding of it.

use anyhow::{Context, Result};
use ethers::types::{Address, U256};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where a launch's file lives: block first, so a directory listing is in the
/// order the launches happened.
pub fn path_for(dir: &Path, block: u64, curve: Address) -> PathBuf {
    dir.join(format!("{block}-{curve:?}.jsonl"))
}

/// The launch itself, as everything known about it at the moment it opens.
#[allow(clippy::too_many_arguments)]
pub fn launch_line(
    l: &crate::launch::Launch,
    call: Option<&crate::launch::LaunchCall>,
    quote: Option<&crate::launch::Quote>,
    opening: Option<&crate::curve::Curve>,
    tax: Option<crate::launch::SnipeTax>,
    launched_at: Option<u64>,
    // Everyone the launch declared exempt, as the decisions count them: the
    // calldata list PLUS the deployer and the creator fee recipient, whom the
    // factory exempts whether or not they were named. Recording only the
    // calldata list made the file disagree with the decisions written beside
    // it, which is worse than either number alone.
    exempt: &std::collections::HashSet<Address>,
) -> Value {
    let mut v = json!({
        "kind": "launch",
        "block": l.block,
        "tx": format!("{:?}", l.tx),
        "token": format!("{:?}", l.token),
        "curve": format!("{:?}", l.curve),
    });
    if let crate::launch::What::Created {
        deployer,
        pair_token,
        launch_config_id,
        graduation_threshold,
    } = &l.what
    {
        v["deployer"] = json!(format!("{deployer:?}"));
        v["pair_token"] = json!(format!("{pair_token:?}"));
        v["launch_config_id"] = json!(launch_config_id.to_string());
        v["graduation_threshold"] = json!(graduation_threshold.to_string());
    }
    // A launch through an entry point this does not decode, followed on terms
    // read off the curve. Marked, because what is missing about it - the
    // exemption list and the maker's own buy - is exactly what the analysis
    // leans on hardest.
    if call.is_none() {
        v["via"] = json!("unknown entry point");
        v["decoded"] = json!(false);
    }
    if let Some(at) = launched_at {
        v["launched_at"] = json!(at);
    }
    if let Some(q) = quote {
        v["pair_symbol"] = json!(q.symbol);
        v["pair_decimals"] = json!(q.decimals);
    }
    if let Some(t) = tax {
        v["snipe_tax_start_bps"] = json!(t.start_bps);
        v["snipe_tax_seconds"] = json!(t.seconds);
    }
    if let Some(c) = opening {
        v["opening_quote_reserve"] = json!(c.quote_reserve.to_string());
        v["opening_token_reserve"] = json!(c.token_reserve.to_string());
        v["reserved_tokens"] = json!(c.reserved_tokens.to_string());
        v["curve_fee_bps"] = json!(c.fee_bps);
        v["creator_tax_bps"] = json!(c.creator_tax_bps);
    }
    if let Some(c) = call {
        v["name"] = json!(c.name);
        v["symbol"] = json!(c.symbol);
        v["via"] = json!(c.via);
        v["decoded"] = json!(true);
        v["creator_fee_recipient"] = json!(format!("{:?}", c.creator_fee_recipient));
        v["buyback_enabled"] = json!(c.buyback_enabled);
        // The wallets that pay no snipe tax at all. On these launches this is
        // the single most telling field in the file.
        v["declared_exempt"] = json!(c
            .exemptions
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>());
    }
    let mut all: Vec<String> = exempt.iter().map(|a| format!("{a:?}")).collect();
    all.sort();
    v["exempt"] = json!(all);
    v
}

/// One trade, and the reserves it left behind.
///
/// `elapsed` is seconds into the tax window, and is absent rather than guessed
/// when the chain's own clock for that block is not known.
pub fn trade_line(
    trade: &crate::curve::Trade,
    block: u64,
    elapsed: Option<i64>,
    fee_bps: u64,
    // The pair token's decimals, for the readable half of every quote amount.
    quote_decimals: u8,
    // Whether whoever did this was declared exempt from the snipe tax at
    // launch. Absent when the launch's calldata was never decoded and the list
    // is therefore not known - which is not the same as knowing they were not.
    exempt: Option<bool>,
    after: &crate::curve::Curve,
) -> Value {
    // The launched token is minted at eighteen decimals on this launchpad.
    let q = |v: &U256| crate::route::format_units(*v, quote_decimals);
    let t = |v: &U256| crate::route::format_units(*v, 18);
    let mut v = match trade {
        crate::curve::Trade::Buy {
            recipient,
            quote_in,
            tokens_out,
            fee,
            creator_tax,
        } => {
            // What this buy was really charged as a snipe tax, backed out of
            // the log: the curve reports the base fee and the snipe tax in one
            // field, and the base fee is a known rate on the spend.
            let base = *quote_in * U256::from(fee_bps) / U256::from(10_000u64);
            let snipe = fee.saturating_sub(base);
            let bps = if quote_in.is_zero() {
                U256::zero()
            } else {
                (snipe * U256::from(10_000u64) + *quote_in / 2) / *quote_in
            };
            json!({
                "kind": "buy",
                "who": format!("{recipient:?}"),
                "quote_in": q(quote_in),
                "tokens_out": t(tokens_out),
                "fee": q(fee),
                "creator_tax": q(creator_tax),
                "snipe_tax": q(&snipe),
                "snipe_tax_bps": bps.to_string(),
            })
        }
        crate::curve::Trade::Sell {
            seller,
            recipient,
            tokens_in,
            quote_out,
            fee,
            creator_tax,
        } => {
            json!({
                "kind": "sell",
                "who": format!("{seller:?}"),
                "to": format!("{recipient:?}"),
                "tokens_in": t(tokens_in),
                "quote_out": q(quote_out),
                "fee": q(fee),
                "creator_tax": q(creator_tax),
            })
        }
        crate::curve::Trade::Buyback {
            quote_spent,
            tokens_locked,
        } => json!({
            "kind": "buyback",
            "quote_spent": q(quote_spent),
            "tokens_locked": t(tokens_locked),
        }),
        crate::curve::Trade::Completed => json!({ "kind": "graduated" }),
    };
    v["block"] = json!(block);
    if let Some(e) = elapsed {
        v["elapsed"] = json!(e);
    }
    // Declared exempt, which is not quite the same as having paid nothing: the
    // tax is zero once the window passes too. `snipe_tax` on the same line
    // says what was actually paid, and the two together separate a wallet that
    // was let in free from one that simply arrived late.
    if let Some(e) = exempt {
        v["exempt"] = json!(e);
    }
    v["quote_reserve"] = json!(q(&after.quote_reserve));
    v["token_reserve"] = json!(t(&after.token_reserve));
    v
}

/// The block a step of the tax window opens at.
///
/// The number a buy is actually aimed at. A second is not a target - a block
/// is - and the two are not interchangeable: the step changes between two
/// consecutive blocks, and landing on the wrong side of that pair is the
/// difference between paying 618 bps and paying 9800. Without this line a file
/// says which second a trade fell in and leaves the margin it had unknowable.
pub fn window_line(step: u64, tax_bps: u64, second: u64, from_block: u64) -> Value {
    json!({
        "kind": "window",
        "step": step,
        "tax_bps": tax_bps,
        "second": second,
        "from_block": from_block,
    })
}

/// The decision taken about one step, and what it was taken on.
///
/// Written whether or not anything was sent: a launch passed over is as much
/// of a result as one bought, and the reason is what makes a run of these
/// readable afterwards.
pub fn decision_line(s: &crate::snipe::Signal, d: &crate::snipe::Decision) -> Value {
    let mut v = json!({
        "kind": "decision",
        "step": s.step,
        "tax_bps": s.tax_bps,
        "opens_at": s.opens_at,
        "lead_ms": s.in_ms,
        "run_x100": crate::snipe::run_x100(s.facts, s.now),
        "quote_reserve": crate::route::format_units(
            s.now.quote_reserve, s.facts.quote_decimals
        ),
        "token_reserve": crate::route::format_units(s.now.token_reserve, 18),
        "exempt": s.facts.exempt,
        "creator_tax_bps": s.facts.creator_tax_bps,
    });
    match d {
        crate::snipe::Decision::Buy {
            spend,
            min_tokens_out,
            why,
        } => {
            v["decision"] = json!("buy");
            v["spend"] = json!(crate::route::format_units(*spend, s.facts.quote_decimals));
            v["min_tokens_out"] = json!(crate::route::format_units(*min_tokens_out, 18));
            v["why"] = json!(why);
        }
        crate::snipe::Decision::Wait { why } => {
            v["decision"] = json!("wait");
            v["why"] = json!(why);
        }
        crate::snipe::Decision::Skip { why } => {
            v["decision"] = json!("skip");
            v["why"] = json!(why);
        }
    }
    v
}

/// How a position ended, and what it was worth when it did.
///
/// Written whether or not anything was sent. Until there is a wallet behind
/// this the whole record of an exit rule is these lines, and an operator's
/// history is built out of them rather than out of the curve's own peak -
/// which is a different number and a worse one.
pub fn exit_line(
    h: &crate::exit::Held,
    worth: U256,
    why: &str,
    block: u64,
) -> Value {
    json!({
        "kind": "exit",
        "block": block,
        "why": why,
        "held_blocks": block.saturating_sub(h.opened_at),
        "cost": crate::route::format_units(h.cost, 18),
        "worth": crate::route::format_units(worth, 18),
        "high": crate::route::format_units(h.high, 18),
        "x100": h.x100(worth),
    })
}

/// How a followed launch ended: what happened on it while we watched.
///
/// The only thing the console said that no record kept. The trades are all
/// here individually, but a minute of them is not a thing anyone reads back -
/// this is the shape of that minute in one line.
#[allow(clippy::too_many_arguments)]
pub fn done_line(
    buys: u32,
    sells: u32,
    outsiders: usize,
    peak_quote: U256,
    last_quote: U256,
    opening_quote: U256,
    quote_decimals: u8,
    position: &str,
) -> Value {
    let run = |v: U256| {
        if opening_quote.is_zero() {
            return String::new();
        }
        format!(
            "{}",
            crate::route::u256_to_f64(v) / crate::route::u256_to_f64(opening_quote)
        )
    };
    json!({
        "kind": "done",
        "buys": buys,
        "sells": sells,
        // Distinct wallets that bought and were not exempt. None of them means
        // nobody outside the bundle ever wanted it, and 15% of launches end
        // that way.
        "outsiders": outsiders,
        "peak_run": run(peak_quote),
        "last_run": run(last_quote),
        "quote_reserve": crate::route::format_units(last_quote, quote_decimals),
        "position": position,
    })
}

/// Append one line. Opened and closed per line on purpose: a launch writes a
/// few dozen lines over a minute, and a handle held open across that is a
/// handle that loses them if the process ends badly.
pub fn append(path: &Path, line: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(f, "{line}").with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve() -> crate::curve::Curve {
        crate::curve::Curve {
            quote_reserve: U256::from(1_680_000_000_000_000_000u64),
            token_reserve: U256::exp10(27),
            reserved_tokens: U256::exp10(26),
            fee_bps: 100,
            creator_tax_bps: 0,
        }
    }

    /// Amounts are strings. A token amount runs to twenty-seven digits, and a
    /// JSON number is a double - writing these as numbers would round away the
    /// last nine of them, silently, in the file that exists to record them.
    #[test]
    fn amounts_are_written_exactly() {
        let l = trade_line(
            &crate::curve::Trade::Buy {
                recipient: Address::repeat_byte(0xbb),
                quote_in: U256::from(54_965_456_296_796_696u64),
                tokens_out: U256::from_dec_str("30760000000000000000000000").unwrap(),
                fee: U256::from(549_654u64),
                creator_tax: U256::zero(),
            },
            57_223_137,
            Some(1),
            100,
            18,
            Some(false),
            &curve(),
        );
        // Both halves, and both exact: the readable one is the same integer
        // with a point put in, not a rounding of it.
        // Written once, in the token's own units, and to the last digit.
        // Twice - a decimal string beside its wei - was two numbers that
        // could disagree, and the decimal one already loses nothing.
        assert_eq!(l["tokens_out"], "30760000");
        assert_eq!(l["quote_in"], "0.054965456296796696");
        assert!(l.get("quote_in_wei").is_none());
        assert_eq!(l["elapsed"], 1);
        assert_eq!(l["exempt"], false);
        assert_eq!(l["kind"], "buy");
        assert!(l["quote_in"].is_string(), "a number here loses digits");
    }

    /// A second nobody knows is left out, not written as a zero that would
    /// read as the launch second.
    #[test]
    fn an_unknown_second_is_absent() {
        let l = trade_line(
            &crate::curve::Trade::Completed,
            1,
            None,
            100,
            18,
            None,
            &curve(),
        );
        assert!(l.get("elapsed").is_none());
        // And an exemption nobody could determine is absent rather than false:
        // not knowing is not the same as knowing they were not.
        assert!(l.get("exempt").is_none());
        assert_eq!(l["kind"], "graduated");
    }

    /// The snipe tax each buyer really paid, which is the whole reason to keep
    /// these files: it separates the wallets that were let in free from the
    /// ones that paid to be early.
    #[test]
    fn the_tax_a_buyer_paid_is_recorded() {
        let free = trade_line(
            &crate::curve::Trade::Buy {
                recipient: Address::zero(),
                quote_in: U256::from(1_000_000u64),
                tokens_out: U256::from(1u64),
                fee: U256::from(10_000u64), // the base fee alone
                creator_tax: U256::zero(),
            },
            1,
            Some(0),
            100,
            6,
            Some(true),
            &curve(),
        );
        assert_eq!(free["snipe_tax"], "0");
        assert_eq!(free["exempt"], true);
        // Six-decimal pair token, read in its own units.
        assert_eq!(free["quote_in"], "1");
        assert_eq!(free["snipe_tax_bps"], "0");

        let taxed = trade_line(
            &crate::curve::Trade::Buy {
                recipient: Address::zero(),
                quote_in: U256::from(1_000_000u64),
                tokens_out: U256::from(1u64),
                fee: U256::from(10_000u64 + 61_800), // base plus 618 bps
                creator_tax: U256::zero(),
            },
            1,
            Some(1),
            100,
            6,
            Some(false),
            &curve(),
        );
        assert_eq!(taxed["snipe_tax"], "0.0618");
        assert_eq!(taxed["snipe_tax_bps"], "618");
        assert_eq!(taxed["exempt"], false);
    }

    /// The block a step opens at is the number a buy is aimed at. A second is
    /// not: the step changes between two consecutive blocks, and landing on
    /// the wrong side of that pair is 618 bps against 9800.
    #[test]
    fn the_window_line_names_a_block_and_not_only_a_second() {
        let l = window_line(1, 618, 1_788_824_596, 57_235_397);
        assert_eq!(l["kind"], "window");
        assert_eq!(l["step"], 1);
        assert_eq!(l["tax_bps"], 618);
        assert_eq!(l["from_block"], 57_235_397u64);
        assert_eq!(l["second"], 1_788_824_596u64);
    }

    #[test]
    fn a_file_is_named_for_when_and_what() {
        let p = path_for(
            Path::new("launches"),
            57_223_137,
            Address::repeat_byte(0x11),
        );
        assert_eq!(
            p.file_name().unwrap().to_str().unwrap(),
            "57223137-0x1111111111111111111111111111111111111111.jsonl"
        );
    }

    /// Lines accumulate; a second launch does not overwrite the first.
    #[test]
    fn lines_are_appended() {
        let dir = std::env::temp_dir().join(format!("journal-test-{}", std::process::id()));
        let path = path_for(&dir, 1, Address::zero());
        let _ = std::fs::remove_file(&path);
        append(&path, &json!({"kind": "launch"})).unwrap();
        append(&path, &json!({"kind": "buy"})).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
        assert!(body
            .lines()
            .all(|l| serde_json::from_str::<Value>(l).is_ok()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
