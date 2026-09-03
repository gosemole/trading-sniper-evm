//! Resolving a swap route from nothing but a list of v4 pool ids.
//!
//! A pool id is `keccak256(abi.encode(PoolKey))`, so it cannot be reversed —
//! but the PoolKey is published once, in the pool's `Initialize` log, and
//! `pool::v4_pool_key` recovers it. Every recovered key is then re-hashed and
//! checked against the id it came from: if that matches, all five fields are
//! certainly correct and no field was guessed.

use crate::config::RouteConfig;
use crate::pool::{
    decimals_of, pool_id_from_key, resolve_token, symbol_of, v3_pool_key, v4_pool_key,
};
use anyhow::{Context, Result};
use ethers::providers::{Http, Provider};
use ethers::types::{Address, H256, U256};
use std::collections::HashMap;

/// Which protocol a hop trades on. They are named differently on purpose: a v4
/// pool has no address of its own and is identified by the hash of its key,
/// while a v3 pool *is* a contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Venue {
    V4 { pool_id: H256, hooks: Address },
    V3 { pool: Address },
}

impl Venue {
    pub fn is_v4(&self) -> bool {
        matches!(self, Venue::V4 { .. })
    }
}

/// How a pool is addressed when a signal or a config line names it. v4 pools
/// are named by id, v3 pools by address, and the two can never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PoolRef {
    V4(H256),
    V3(Address),
}

impl std::fmt::Display for PoolRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PoolRef::V4(id) => write!(f, "v4 {id:?}"),
            PoolRef::V3(a) => write!(f, "v3 {a:?}"),
        }
    }
}

/// Parse a pool as written in config. The two forms cannot be confused: a v4
/// pool id is 32 bytes and a v3 pool address is 20.
pub fn parse_pool_ref(raw: &str) -> Result<PoolRef> {
    let t = raw.trim();
    match t.trim_start_matches("0x").len() {
        64 => Ok(PoolRef::V4(t.parse().context("not a 32-byte pool id")?)),
        40 => Ok(PoolRef::V3(t.parse().context("not a 20-byte address")?)),
        n => anyhow::bail!(
            "'{t}' is {n} hex digits; expected 64 for a v4 pool id or 40 for a v3 pool address"
        ),
    }
}

/// One pool in the chain, with the direction the swap takes through it.
#[derive(Debug, Clone)]
pub struct Hop {
    pub venue: Venue,
    pub currency0: Address,
    pub currency1: Address,
    /// Pool fee in hundredths of a bip, as both protocols count it.
    pub fee: u32,
    pub tick_spacing: i32,
    /// Currency spent in this hop.
    pub input: Address,
    /// Currency received; the next hop's input.
    pub output: Address,
    pub input_decimals: u8,
    pub output_decimals: u8,
}

impl Hop {
    /// True when the swap direction is currency0 -> currency1.
    pub fn zero_for_one(&self) -> bool {
        self.input == self.currency0
    }

    pub fn pool_ref(&self) -> PoolRef {
        match &self.venue {
            Venue::V4 { pool_id, .. } => PoolRef::V4(*pool_id),
            Venue::V3 { pool } => PoolRef::V3(*pool),
        }
    }

    /// Where the tick walk reads this pool's state from.
    pub fn tick_source(&self, manager: Address) -> crate::depth::Source {
        match &self.venue {
            Venue::V4 { pool_id, .. } => crate::depth::Source::V4 {
                manager,
                pool_id: *pool_id,
            },
            Venue::V3 { pool } => crate::depth::Source::V3 { pool: *pool },
        }
    }

    /// One line naming the pool and everything that identifies it.
    pub fn describe(&self) -> String {
        match &self.venue {
            Venue::V4 { pool_id, hooks } => format!(
                "v4 {pool_id:?}\n         fee={} tickSpacing={} hooks={hooks:?}",
                self.fee, self.tick_spacing
            ),
            Venue::V3 { pool } => format!(
                "v3 {pool:?}\n         fee={} tickSpacing={}",
                self.fee, self.tick_spacing
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Token {
    pub address: Address,
    pub decimals: u8,
    pub symbol: String,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub name: String,
    pub input: Token,
    pub output: Token,
    /// Amount to spend, in raw units of the input token.
    pub amount_in: U256,
    pub max_slippage_pct: f64,
    pub hops: Vec<Hop>,
}

/// Parse a decimal string into raw token units. Rejects a fraction longer than
/// the token supports rather than silently truncating value away.
pub fn parse_units(s: &str, decimals: u8) -> Result<U256> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty amount");
    let (int_part, frac_part) = match s.split_once('.') {
        Some((i, f)) => (i, f),
        None => (s, ""),
    };
    anyhow::ensure!(
        int_part.chars().all(|c| c.is_ascii_digit())
            && frac_part.chars().all(|c| c.is_ascii_digit()),
        "amount '{s}' is not a decimal number"
    );
    anyhow::ensure!(
        frac_part.len() <= decimals as usize,
        "amount '{s}' has {} decimals but the token only has {decimals}",
        frac_part.len()
    );
    let mut digits = String::from(if int_part.is_empty() { "0" } else { int_part });
    digits.push_str(frac_part);
    for _ in frac_part.len()..decimals as usize {
        digits.push('0');
    }
    U256::from_dec_str(&digits).with_context(|| format!("amount '{s}' does not fit in u256"))
}

/// Format raw units back into a human decimal string, for display only.
pub fn format_units(v: U256, decimals: u8) -> String {
    let s = v.to_string();
    let d = decimals as usize;
    if d == 0 {
        return s;
    }
    let s = format!("{:0>width$}", s, width = d + 1);
    let (i, f) = s.split_at(s.len() - d);
    let f = f.trim_end_matches('0');
    if f.is_empty() {
        i.to_string()
    } else {
        format!("{i}.{f}")
    }
}

impl Route {
    /// Recover every PoolKey, verify it against its id, and thread the input
    /// token through the chain to work out each hop's direction.
    pub async fn resolve(
        http: &Provider<Http>,
        manager: Address,
        cfg: &RouteConfig,
        tokens: &HashMap<String, String>,
    ) -> Result<Self> {
        let input_addr = resolve_token(tokens, &cfg.input)
            .with_context(|| format!("route '{}': bad input token", cfg.name))?;

        let mut hops = Vec::with_capacity(cfg.pools.len());
        let mut cursor = input_addr;
        for (i, raw) in cfg.pools.iter().enumerate() {
            let (venue, c0, c1, fee, tick_spacing) =
                match parse_pool_ref(raw)
                    .with_context(|| format!("route '{}': hop {i}", cfg.name))?
                {
                    PoolRef::V4(pool_id) => {
                        let (c0, c1, fee, ts, hooks) = v4_pool_key(http, manager, pool_id)
                            .await
                            .with_context(|| {
                                format!("route '{}': hop {i}: could not recover PoolKey", cfg.name)
                            })?;
                        // The whole v4 design rests on this check: a matching
                        // hash proves all five fields, so nothing is a guess.
                        let rederived = pool_id_from_key(c0, c1, fee, ts, hooks);
                        anyhow::ensure!(
                            rederived == pool_id,
                            "route '{}': hop {i}: recovered PoolKey hashes to {rederived:?}, \
                             not {pool_id:?}",
                            cfg.name
                        );
                        (Venue::V4 { pool_id, hooks }, c0, c1, fee, ts)
                    }
                    // v3 needs no such proof: the pool contract answers for
                    // itself, and the address in config is what we call.
                    PoolRef::V3(pool) => {
                        let (c0, c1, fee, ts) = v3_pool_key(http, pool).await.with_context(|| {
                            format!("route '{}': hop {i}: {pool:?} is not a v3 pool", cfg.name)
                        })?;
                        (Venue::V3 { pool }, c0, c1, fee, ts)
                    }
                };

            let output = if cursor == c0 {
                c1
            } else if cursor == c1 {
                c0
            } else {
                anyhow::bail!(
                    "route '{}': hop {i} ({raw}) holds {c0:?}/{c1:?}, neither of which is \
                     the incoming token {cursor:?} - the chain is not connected",
                    cfg.name
                );
            };
            hops.push(Hop {
                venue,
                currency0: c0,
                currency1: c1,
                fee,
                tick_spacing,
                input: cursor,
                output,
                input_decimals: decimals_of(http, cursor).await?,
                output_decimals: decimals_of(http, output).await?,
            });
            cursor = output;
        }

        anyhow::ensure!(
            cursor != input_addr,
            "route '{}': ends on the token it started with",
            cfg.name
        );

        let input = describe(http, input_addr).await?;
        let output = describe(http, cursor).await?;
        let amount_in = parse_units(&cfg.amount_in, input.decimals)
            .with_context(|| format!("route '{}': bad amount_in", cfg.name))?;
        anyhow::ensure!(!amount_in.is_zero(), "route '{}': amount_in is zero", cfg.name);

        Ok(Route {
            name: cfg.name.clone(),
            input,
            output,
            amount_in,
            max_slippage_pct: cfg.max_slippage_pct,
            hops,
        })
    }

}

impl Route {
    /// The same pools walked the other way, to sell what this route buys.
    ///
    /// Only the direction changes: the pool ids, fees, tick spacings and hooks
    /// are the ones already recovered and verified against their ids, so a
    /// reversed route needs no further on-chain resolution.
    pub fn reversed(&self, amount_in: U256) -> Route {
        let hops = self
            .hops
            .iter()
            .rev()
            .map(|h| Hop {
                input: h.output,
                output: h.input,
                input_decimals: h.output_decimals,
                output_decimals: h.input_decimals,
                ..h.clone()
            })
            .collect();
        Route {
            name: format!("{} (reversed)", self.name),
            input: self.output.clone(),
            output: self.input.clone(),
            amount_in,
            max_slippage_pct: self.max_slippage_pct,
            hops,
        }
    }
}

/// What one hop does to the amount flowing through it.
#[derive(Debug, Clone)]
pub struct HopQuote {
    pub pool: PoolRef,
    /// The pool's state this hop was priced from, kept so a caller can price
    /// the same hop again later without reading it back.
    pub sqrt_p: f64,
    pub liquidity: u128,
    pub amount_in: f64,
    pub amount_out: f64,
    pub input_decimals: u8,
    pub output_decimals: u8,
    pub ticks_crossed: u32,
    pub lp_fee: u32,
    /// How far this hop moves the pool's own price, in percent.
    pub price_impact_pct: f64,
}

#[derive(Debug, Clone)]
pub struct Quote {
    pub hops: Vec<HopQuote>,
    pub amount_out: U256,
    /// `amount_out` reduced by the route's slippage tolerance. This is the
    /// number that goes on chain as amountOutMinimum.
    pub min_out: U256,
}

pub fn u256_to_f64(v: U256) -> f64 {
    v.to_string().parse().unwrap_or(f64::MAX)
}

/// Same conversion, exposed for display code.
pub fn f64_to_u256_pub(v: f64) -> U256 {
    f64_to_u256(v)
}

fn f64_to_u256(v: f64) -> U256 {
    if v <= 0.0 || !v.is_finite() {
        return U256::zero();
    }
    U256::from_dec_str(&format!("{:.0}", v.floor())).unwrap_or_else(|_| U256::zero())
}

impl Route {
    /// Simulate the whole route, hop by hop, walking every tick each swap
    /// crosses. Read-only: this touches no wallet and sends no transaction.
    pub async fn quote(
        &self,
        http: &Provider<Http>,
        manager: Address,
        at: Option<u64>,
    ) -> Result<Quote> {
        let mut amount = u256_to_f64(self.amount_in);
        let mut out_hops = Vec::with_capacity(self.hops.len());

        for (i, hop) in self.hops.iter().enumerate() {
            let reader = crate::depth::TickReader::new(http, hop.tick_source(manager), hop.tick_spacing)
                .map(|r| r.at_block(at))
                .with_context(|| format!("hop {i}: bad tick spacing"))?;
            let state = crate::depth::read_state(&reader)
                .await
                .with_context(|| format!("hop {i}: could not read pool state"))?;
            let res = crate::depth::swap_exact_in(&reader, state, hop.zero_for_one(), amount)
                .await
                .with_context(|| format!("hop {i}: simulation failed"))?;

            anyhow::ensure!(
                res.amount_out > 0.0,
                "hop {i} ({}) returns nothing for {} in - the pool has no liquidity in that \
                 direction",
                hop.pool_ref(),
                amount
            );
            // The walk stops when it runs out of initialized ticks to cross, so
            // a thin pool can leave part of the input unspent. Reporting the
            // output of a smaller trade as if it were the answer would flatter
            // the quote exactly where the pool is worst.
            anyhow::ensure!(
                res.amount_in_used >= amount * 0.999,
                "hop {i} ({}) could only absorb {} of the {} handed to it, so this quote \
                 describes a smaller trade than the one asked for",
                hop.pool_ref(),
                res.amount_in_used,
                amount
            );
            let impact = (res.sqrt_p_after / state.sqrt_p).powi(2) - 1.0;
            out_hops.push(HopQuote {
                pool: hop.pool_ref(),
                sqrt_p: state.sqrt_p,
                liquidity: state.liquidity,
                amount_in: amount,
                amount_out: res.amount_out,
                input_decimals: hop.input_decimals,
                output_decimals: hop.output_decimals,
                ticks_crossed: res.ticks_crossed,
                lp_fee: state.lp_fee,
                price_impact_pct: impact * 100.0,
            });
            amount = res.amount_out;
        }

        let amount_out = f64_to_u256(amount);
        let min_out = f64_to_u256(amount * (1.0 - self.max_slippage_pct / 100.0));
        anyhow::ensure!(!min_out.is_zero(), "quoted output rounds to zero");
        Ok(Quote {
            hops: out_hops,
            amount_out,
            min_out,
        })
    }
}

async fn describe(http: &Provider<Http>, address: Address) -> Result<Token> {
    Ok(Token {
        address,
        decimals: decimals_of(http, address).await?,
        symbol: symbol_of(http, address)
            .await
            .unwrap_or_else(|_| format!("{address:?}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    fn two_hop() -> Route {
        let tok = |b, d, s: &str| Token { address: addr(b), decimals: d, symbol: s.into() };
        let hop = |c0: u8, c1: u8, i: u8, o: u8, di, dobs| Hop {
            venue: Venue::V4 { pool_id: H256::from([c0 + c1; 32]), hooks: addr(0xee) },
            currency0: addr(c0),
            currency1: addr(c1),
            fee: 3477,
            tick_spacing: 35,
            input: addr(i),
            output: addr(o),
            input_decimals: di,
            output_decimals: dobs,
        };
        Route {
            name: "buy C".into(),
            input: tok(1, 6, "A"),
            output: tok(3, 18, "C"),
            amount_in: U256::from(100u64),
            max_slippage_pct: 1.0,
            hops: vec![hop(1, 2, 1, 2, 6, 8), hop(2, 3, 2, 3, 8, 18)],
        }
    }

    #[test]
    fn reversing_walks_the_same_pools_the_other_way() {
        let r = two_hop();
        let back = r.reversed(U256::from(7u64));

        assert_eq!(back.input.symbol, "C", "sells what the route bought");
        assert_eq!(back.output.symbol, "A");
        assert_eq!(back.amount_in, U256::from(7u64));

        // Same pools, opposite order, and every hop flipped end to end.
        let ids: Vec<_> = back.hops.iter().map(|h| h.pool_ref()).collect();
        let mut want: Vec<_> = r.hops.iter().map(|h| h.pool_ref()).collect();
        want.reverse();
        assert_eq!(ids, want);
        assert_eq!(back.hops[0].input, r.hops[1].output);
        assert_eq!(back.hops[0].output, r.hops[1].input);
        assert_eq!(back.hops[0].input_decimals, 18);
        assert_eq!(back.hops[0].output_decimals, 8);
        assert_eq!(back.hops[1].output, r.input.address, "ends where it started");

        // The PoolKey itself is untouched, so the ids still describe it.
        for (a, b) in back.hops.iter().zip(r.hops.iter().rev()) {
            assert_eq!((a.currency0, a.currency1, a.fee, a.tick_spacing, &a.venue),
                       (b.currency0, b.currency1, b.fee, b.tick_spacing, &b.venue));
        }
        // ...and the direction flag follows from input == currency0.
        assert_eq!(back.hops[0].zero_for_one(), !r.hops[1].zero_for_one());
    }

    #[test]
    fn reversing_twice_is_the_original() {
        let r = two_hop();
        let there_and_back = r.reversed(U256::one()).reversed(r.amount_in);
        assert_eq!(there_and_back.input.address, r.input.address);
        assert_eq!(there_and_back.output.address, r.output.address);
        assert_eq!(
            there_and_back.hops.iter().map(|h| (h.pool_ref(), h.input, h.output)).collect::<Vec<_>>(),
            r.hops.iter().map(|h| (h.pool_ref(), h.input, h.output)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parses_decimal_amounts() {
        assert_eq!(parse_units("1", 6).unwrap(), U256::from(1_000_000u64));
        assert_eq!(parse_units("1.0", 6).unwrap(), U256::from(1_000_000u64));
        assert_eq!(parse_units("0.5", 6).unwrap(), U256::from(500_000u64));
        assert_eq!(parse_units("0.000001", 6).unwrap(), U256::one());
        assert_eq!(parse_units(" 12.34 ", 2).unwrap(), U256::from(1234u64));
        assert_eq!(parse_units("7", 0).unwrap(), U256::from(7u64));
        assert_eq!(
            parse_units("1", 18).unwrap(),
            U256::from_dec_str("1000000000000000000").unwrap()
        );
    }

    #[test]
    fn rejects_amounts_that_would_lose_value() {
        // more precision than the token has would silently truncate
        assert!(parse_units("0.0000001", 6).is_err());
        assert!(parse_units("1.5", 0).is_err());
        assert!(parse_units("", 18).is_err());
        assert!(parse_units("abc", 18).is_err());
        assert!(parse_units("-1", 18).is_err());
        assert!(parse_units("1e18", 18).is_err());
    }

    #[test]
    fn formats_back_to_human_units() {
        assert_eq!(format_units(U256::from(1_000_000u64), 6), "1");
        assert_eq!(format_units(U256::from(1_500_000u64), 6), "1.5");
        assert_eq!(format_units(U256::from(1u64), 6), "0.000001");
        assert_eq!(format_units(U256::zero(), 18), "0");
        assert_eq!(format_units(U256::from(42u64), 0), "42");
    }

    #[test]
    fn round_trips_through_raw_units() {
        for (s, d) in [("1.5", 18u8), ("0.000001", 6), ("123.456", 8), ("2", 18)] {
            let raw = parse_units(s, d).unwrap();
            assert_eq!(format_units(raw, d), s);
        }
    }
}
