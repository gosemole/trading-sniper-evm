//! What we hold and what it cost, kept across restarts.
//!
//! A take-profit rule is only as good as its memory: a bot that forgets its
//! entry price after a restart either sells at a loss or never sells at all.
//! So this is written to disk after every change, and read back at startup.
//!
//! Quantities and prices are `f64` on purpose. They are not money moving - the
//! amount actually sold is always read from the chain at the time - they are
//! the weighted average that decides *when* to sell, and a threshold does not
//! need more than fifteen digits.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One token we are long, and the average it was bought at.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Position {
    /// For logs; the map key is the address.
    pub symbol: String,
    pub decimals: u8,
    /// Exactly what is held, in raw units, as a decimal string - JSON has no
    /// 256-bit integer and this number has to survive a restart intact. It is
    /// the sum of what the receipts said actually arrived, so a sale can ask
    /// for precisely it.
    pub raw: String,
    /// The same in human units, for the average and for logs. Approximate by
    /// nature; nothing is ever sold on the strength of it.
    pub qty: f64,
    /// Pool price of this token, weighted by quantity across every buy.
    pub avg_price: f64,
    pub buys: u32,
    /// Unix seconds of the last change.
    pub updated: u64,
    /// True when this was taken from the wallet at startup rather than bought.
    /// Its entry price is the price at that moment, which is an assumption, not
    /// a record - the real one is unknowable. Kept visible so a target derived
    /// from it is never mistaken for one derived from a fill.
    #[serde(default)]
    pub seeded: bool,
}

impl Position {
    /// Exactly what is held, for a sale to ask for.
    pub fn held(&self) -> ethers::types::U256 {
        ethers::types::U256::from_dec_str(&self.raw).unwrap_or_default()
    }

    /// How far the price has to rise from here to hit a `pct` take-profit.
    pub fn target(&self, pct: f64) -> f64 {
        self.avg_price * (1.0 + pct / 100.0)
    }

    /// How long this has been held, counted from the most recent buy - so
    /// averaging further into a dip restarts the clock.
    pub fn held_for(&self, now: u64) -> u64 {
        now.saturating_sub(self.updated)
    }

    /// Gain against the average entry, in percent.
    pub fn gain_pct(&self, price: f64) -> f64 {
        (price / self.avg_price - 1.0) * 100.0
    }
}

/// Which way a reservation goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

/// A trade to set aside, described in one place rather than as a row of
/// arguments that are easy to hand over in the wrong order.
pub struct Trade {
    pub side: Side,
    pub token: ethers::types::Address,
    pub symbol: String,
    pub decimals: u8,
    /// Raw units the trade was quoted at, or asked to sell.
    pub raw: ethers::types::U256,
    /// Pool price at the moment of the trade; unused for a sale.
    pub price: f64,
    /// A sale's proceeds: which token comes back, and how much. Credited to
    /// the tracked cash balance the moment the sale is reserved, and reversed
    /// automatically if it turns out never to have happened - see `rollback`.
    /// `None` for a buy, which has nothing of the kind to credit.
    pub credit: Option<(ethers::types::Address, ethers::types::U256)>,
}

/// A trade that has been broadcast but not yet confirmed.
///
/// Nothing enters the average on the strength of a transaction being *sent*: a
/// transaction can revert, be dropped, or lose a reorg, and an average built
/// from trades that never happened is worse than no average at all. So a fill
/// is held here until the chain agrees it happened.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Pending {
    pub side: Side,
    /// Lowercased address of the token the trade is about.
    pub token: String,
    pub symbol: String,
    pub decimals: u8,
    /// The size this was quoted at, in raw units. Used only when the receipt
    /// cannot be read; the receipt is the truth when there is one.
    pub raw: String,
    /// Pool price at the moment of the trade.
    pub price: f64,
    pub at: u64,
    /// See `Trade::credit`. Kept here, not just applied and forgotten, so a
    /// rollback - even one recovered from disk after a restart - knows exactly
    /// what to undo, and so `settle` can trade the optimistic figure for the
    /// receipt's real one once there is a receipt to read.
    #[serde(default)]
    credit: Option<(String, String)>,
}

impl Pending {
    /// The token this trade credits, if it credits one - so a caller can go
    /// read the real receipt for it before calling `settle`. `Inventory`
    /// applies the credit; reading the chain for it is the caller's job, the
    /// same division as `moved` already uses for the position side.
    pub fn credit_token(&self) -> Option<ethers::types::Address> {
        self.credit.as_ref().and_then(|(t, _)| t.parse().ok())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    positions: HashMap<String, Position>,
    /// Reservations, keyed by transaction hash.
    #[serde(default)]
    pending: HashMap<String, Pending>,
    /// Spendable balance of a token this bot pays with - lowercased address to
    /// raw units, the same string encoding `Position::raw` uses and for the
    /// same reason. Tracked locally instead of read from the chain on every
    /// buy: nothing else touches this wallet while the bot runs, so a chain
    /// read the hot path can skip is a chain read it never has to make. Seeded
    /// once from the real balance at startup (`set_cash`) and kept in step by
    /// every trade this bot itself sends - see `debit_cash`, `credit_cash`,
    /// and `Trade::credit` for how a sale's proceeds flow back in.
    #[serde(default)]
    cash: HashMap<String, String>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

/// Raw units to human units, for display and for the weighted average.
fn raw_to_f64(raw: ethers::types::U256, decimals: u8) -> f64 {
    raw.to_string().parse::<f64>().unwrap_or(0.0) / 10f64.powi(decimals as i32)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Addresses are the key, lowercased so the same token written two ways is one
/// position rather than two.
fn key(token: ethers::types::Address) -> String {
    format!("{token:?}").to_lowercase()
}

impl Inventory {
    /// Read what is on disk, or start empty. A missing file is not an error -
    /// it is the first run.
    pub fn load(path: &Path) -> Result<Self> {
        let mut inv = match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str::<Inventory>(&raw)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Inventory::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        inv.path = Some(path.to_path_buf());
        Ok(inv)
    }

    pub fn get(&self, token: ethers::types::Address) -> Option<&Position> {
        self.positions.get(&key(token))
    }

    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    /// The tracked balance of `token` - zero if nothing has ever been seeded
    /// or credited for it, which is the right answer for a token this bot has
    /// never touched.
    pub fn cash(&self, token: ethers::types::Address) -> ethers::types::U256 {
        self.cash_at(&key(token))
    }

    /// The same by map key, for the paths that only have the stored string.
    fn cash_at(&self, k: &str) -> ethers::types::U256 {
        self.cash
            .get(k)
            .and_then(|s| ethers::types::U256::from_dec_str(s).ok())
            .unwrap_or_default()
    }

    /// Set the tracked balance to what the chain reports right now. Unlike a
    /// position's entry price, this number is always knowable, so - unlike
    /// `seed` - this always overwrites: the chain is the only source of truth
    /// for it, and a stale local guess is never worth preferring.
    pub fn set_cash(&mut self, token: ethers::types::Address, amount: ethers::types::U256) {
        self.cash.insert(key(token), amount.to_string());
    }

    /// Commit a spend before it is sent, so a decision made a moment later -
    /// even for a different route that happens to spend the same token - sees
    /// the wallet as already spoken for rather than still full. Saturates at
    /// zero rather than go negative: the caller checks `cash()` first, and a
    /// negative balance would be a worse answer than a floored one.
    pub fn debit_cash(&mut self, token: ethers::types::Address, amount: ethers::types::U256) {
        let left = self.cash(token).saturating_sub(amount);
        self.cash.insert(key(token), left.to_string());
    }

    /// Undo a debit that was never spent, or add proceeds a sale returned.
    /// Saturating like its counterpart: `U256`'s plain `+` panics on overflow
    /// in every build, and a corrupt stored figure must not take the whole
    /// process down with it.
    pub fn credit_cash(&mut self, token: ethers::types::Address, amount: ethers::types::U256) {
        let have = self.cash(token).saturating_add(amount);
        self.cash.insert(key(token), have.to_string());
    }

    /// Set a trade aside until the chain confirms it.
    ///
    /// Returns false for a size or price that cannot mean anything, so a
    /// caller can tell "reserved" from "ignored". A sale's proceeds
    /// (`Trade::credit`) are applied to cash immediately, on the optimistic
    /// assumption that a broadcast trade lands - `rollback` reverses it if
    /// that assumption turns out wrong.
    pub fn reserve(&mut self, hash: ethers::types::H256, t: Trade) -> bool {
        if t.side == Side::Buy && (!t.price.is_finite() || t.price <= 0.0 || t.raw.is_zero()) {
            return false;
        }
        if let Some((ctoken, camount)) = t.credit {
            self.credit_cash(ctoken, camount);
        }
        self.pending.insert(
            format!("{hash:?}").to_lowercase(),
            Pending {
                side: t.side,
                token: key(t.token),
                symbol: t.symbol,
                decimals: t.decimals,
                raw: t.raw.to_string(),
                price: t.price,
                at: now_secs(),
                credit: t.credit.map(|(tok, amt)| (key(tok), amt.to_string())),
            },
        );
        true
    }

    /// The chain confirmed it.
    ///
    /// `moved` is what the receipt says actually changed hands, of the token
    /// the trade is *about* - the amount bought, or the amount sold. It is
    /// preferred over the quote in every case, because the quote is what was
    /// expected and this is what happened. Without it the reservation's own
    /// figure is used, which is the best that can be done and is why it is
    /// kept.
    ///
    /// `credit_moved` is the same idea for a sale's proceeds - what the
    /// receipt says came back, in the token `reserve` credited on the
    /// optimistic assumption the sale would land. Reconciled here rather than
    /// left as the quote: the two can differ by however much the price moved
    /// between the quote and confirmation, in either direction. Ignored for a
    /// buy, which has no credit to reconcile.
    pub fn settle(
        &mut self,
        hash: ethers::types::H256,
        moved: Option<ethers::types::U256>,
        credit_moved: Option<ethers::types::U256>,
    ) -> Option<Side> {
        let p = self.pending.remove(&format!("{hash:?}").to_lowercase())?;
        if let (Some((ctoken, quoted_str)), Some(actual)) = (&p.credit, credit_moved) {
            if let Ok(quoted) = ethers::types::U256::from_dec_str(quoted_str) {
                // Applied as one net difference, not as "take the quote back,
                // then add the real figure": this balance is shared with every
                // buy of the same token, and one may have spent it down in the
                // meantime. Subtracting the whole quote first would floor at
                // zero and then add the real amount on top of nothing - the
                // shortfall silently forgiven, the balance overstated for good.
                let have = self.cash_at(ctoken);
                let corrected = if actual >= quoted {
                    have.saturating_add(actual - quoted)
                } else {
                    have.saturating_sub(quoted - actual)
                };
                self.cash.insert(ctoken.clone(), corrected.to_string());
            }
        }
        let amount = moved.unwrap_or_else(|| {
            ethers::types::U256::from_dec_str(&p.raw).unwrap_or_default()
        });
        let human = raw_to_f64(amount, p.decimals);
        match p.side {
            Side::Buy => {
                if amount.is_zero() {
                    return Some(p.side);
                }
                let e = self
                    .positions
                    .entry(p.token.clone())
                    .or_insert_with(|| Position {
                        symbol: p.symbol.clone(),
                        decimals: p.decimals,
                        raw: "0".to_string(),
                        qty: 0.0,
                        avg_price: p.price,
                        buys: 0,
                        updated: 0,
                        seeded: false,
                    });
                let total = e.qty + human;
                e.avg_price = (e.avg_price * e.qty + p.price * human) / total;
                e.qty = total;
                e.raw = (e.held() + amount).to_string();
                e.buys += 1;
                e.updated = now_secs();
            }
            Side::Sell => {
                // Subtract rather than forget: a sale capped by the wallet
                // balance can leave part of the position behind, and that part
                // is still ours and still has an entry price.
                if let Some(e) = self.positions.get_mut(&p.token) {
                    let left = e.held().saturating_sub(amount);
                    if left.is_zero() {
                        self.positions.remove(&p.token);
                    } else {
                        e.raw = left.to_string();
                        e.qty = raw_to_f64(left, e.decimals);
                        e.updated = now_secs();
                    }
                }
            }
        }
        Some(p.side)
    }

    /// It did not happen: forget the reservation, leave the position exactly
    /// as it was, and undo any credit `reserve` applied on the strength of it.
    pub fn rollback(&mut self, hash: ethers::types::H256) -> Option<Side> {
        let p = self.pending.remove(&format!("{hash:?}").to_lowercase())?;
        if let Some((ctoken, camount)) = &p.credit {
            if let Ok(amt) = ethers::types::U256::from_dec_str(camount) {
                let left = self.cash_at(ctoken).saturating_sub(amt);
                self.cash.insert(ctoken.clone(), left.to_string());
            }
        }
        Some(p.side)
    }

    /// Adopt what the wallet already holds, at the price it is worth now.
    ///
    /// Refuses to touch a token there is already a position in: a real average
    /// built from fills is worth more than a guess, however recent.
    pub fn seed(
        &mut self,
        token: ethers::types::Address,
        symbol: &str,
        decimals: u8,
        raw: ethers::types::U256,
        price: f64,
    ) -> bool {
        if raw.is_zero() || !price.is_finite() || price <= 0.0 {
            return false;
        }
        if self.positions.contains_key(&key(token)) {
            return false;
        }
        self.positions.insert(
            key(token),
            Position {
                symbol: symbol.to_string(),
                decimals,
                raw: raw.to_string(),
                qty: raw_to_f64(raw, decimals),
                avg_price: price,
                buys: 0,
                updated: now_secs(),
                seeded: true,
            },
        );
        true
    }

    /// Everything still waiting on the chain, for a caller that has just
    /// started and needs to find out how those trades ended.
    pub fn unsettled(&self) -> Vec<(ethers::types::H256, Pending)> {
        self.pending
            .iter()
            .filter_map(|(h, p)| h.parse().ok().map(|h| (h, p.clone())))
            .collect()
    }

    /// Write to disk. Through a temporary file, so a crash mid-write leaves the
    /// previous state rather than half of this one.
    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self).context("serialising inventory")?;
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::{Address, H256, U256};

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
    }

    fn hash(b: u8) -> H256 {
        H256::from([b; 32])
    }

    /// Raw units of an 18-decimal token.
    fn raw(units: f64) -> U256 {
        U256::from_dec_str(&format!("{:.0}", units * 1e18)).unwrap()
    }

    /// A buy that reached the chain and delivered exactly what it quoted.
    fn buy(token: Address, units: f64, price: f64) -> Trade {
        Trade {
            side: Side::Buy,
            token,
            symbol: "TKN".into(),
            decimals: 18,
            raw: raw(units),
            price,
            credit: None,
        }
    }

    fn sale(token: Address, units: f64) -> Trade {
        Trade {
            side: Side::Sell,
            price: 0.0,
            ..buy(token, units, 1.0)
        }
    }

    fn filled(inv: &mut Inventory, h: u8, token: Address, units: f64, price: f64) {
        inv.reserve(hash(h), buy(token, units, price));
        inv.settle(hash(h), None, None);
    }

    #[test]
    fn the_average_is_weighted_by_size() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        filled(&mut inv, 2, addr(1), 300.0, 6.0);
        let p = inv.get(addr(1)).unwrap();
        // 100 at 10 and 300 at 6 average to 7, not to 8.
        assert_eq!(p.qty, 400.0);
        assert!((p.avg_price - 7.0).abs() < 1e-12, "{}", p.avg_price);
        assert_eq!(p.buys, 2);
        assert_eq!(p.held(), raw(400.0), "and the raw total is exact");
    }

    #[test]
    fn the_receipt_wins_over_the_quote() {
        let mut inv = Inventory::default();
        // Quoted 100, but 97 actually arrived. Both the average and the amount
        // a later sale can ask for have to follow the second number: an average
        // built on the first would drift, and a sale sized on it would ask for
        // three tokens that are not there.
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        inv.settle(hash(1), Some(raw(97.0)), None);
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(97.0));
        assert_eq!(p.qty, 97.0);
        assert_eq!(p.avg_price, 10.0);
    }

    #[test]
    fn an_unreadable_receipt_falls_back_to_the_quote() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        inv.settle(hash(1), None, None);
        assert_eq!(inv.get(addr(1)).unwrap().held(), raw(100.0));
    }

    #[test]
    fn a_buy_that_delivered_nothing_is_not_a_position() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        assert_eq!(inv.settle(hash(1), Some(U256::zero()), None), Some(Side::Buy));
        assert!(inv.get(addr(1)).is_none(), "nothing arrived, so nothing is held");
    }

    #[test]
    fn buying_a_further_dip_lowers_the_target() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        let before = inv.get(addr(1)).unwrap().target(5.0);
        filled(&mut inv, 2, addr(1), 100.0, 8.0);
        let after = inv.get(addr(1)).unwrap().target(5.0);
        assert!(after < before, "{after} should be under {before}");
        // 9.0 average, +5% -> 9.45
        assert!((after - 9.45).abs() < 1e-12, "{after}");
    }

    #[test]
    fn the_hold_clock_runs_from_the_last_buy() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 10.0, 100.0);
        let p = inv.get(addr(1)).unwrap();
        let bought_at = p.updated;
        assert_eq!(p.held_for(bought_at + 90), 90);
        // A clock that has not reached the buy yet must not read as a long
        // hold, which is what a plain subtraction would do.
        assert_eq!(p.held_for(bought_at.saturating_sub(10)), 0);
    }

    #[test]
    fn gain_is_measured_against_the_average() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 10.0, 100.0);
        let p = inv.get(addr(1)).unwrap();
        assert!((p.gain_pct(110.0) - 10.0).abs() < 1e-9);
        assert!((p.gain_pct(90.0) + 10.0).abs() < 1e-9);
    }

    #[test]
    fn nonsense_fills_are_refused_rather_than_poisoning_the_average() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        for (n, size, price) in [
            (2u8, U256::zero(), 5.0),
            (3, raw(10.0), 0.0),
            (4, raw(10.0), f64::NAN),
            (5, raw(10.0), f64::INFINITY),
        ] {
            assert!(
                !inv.reserve(hash(n), Trade { raw: size, price, ..buy(addr(1), 1.0, 1.0) }),
                "{size} at {price} should be refused"
            );
            assert!(inv.settle(hash(n), None, None).is_none(), "and nothing to settle");
        }
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(100.0));
        assert_eq!(p.avg_price, 10.0);
        assert_eq!(p.buys, 1);
    }

    #[test]
    fn seeding_adopts_the_wallet_but_never_overwrites_a_real_average() {
        let mut inv = Inventory::default();
        assert!(inv.seed(addr(1), "TKN", 18, raw(50.0), 2.0));
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(50.0));
        assert_eq!(p.avg_price, 2.0);
        assert_eq!(p.buys, 0, "nothing was bought");
        assert!(p.seeded, "and it says so");

        // A guessed price must not replace one that came from fills.
        assert!(!inv.seed(addr(1), "TKN", 18, raw(999.0), 9.0));
        assert_eq!(inv.get(addr(1)).unwrap().held(), raw(50.0));

        // Nothing to adopt, or no price to adopt it at.
        assert!(!inv.seed(addr(2), "TKN", 18, U256::zero(), 2.0));
        assert!(!inv.seed(addr(3), "TKN", 18, raw(1.0), 0.0));
        assert!(!inv.seed(addr(4), "TKN", 18, raw(1.0), f64::NAN));
    }

    #[test]
    fn a_buy_on_top_of_a_seeded_position_folds_into_its_average() {
        let mut inv = Inventory::default();
        inv.seed(addr(1), "TKN", 18, raw(100.0), 10.0);
        filled(&mut inv, 1, addr(1), 100.0, 8.0);
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(200.0));
        assert!((p.avg_price - 9.0).abs() < 1e-12, "{}", p.avg_price);
        assert_eq!(p.buys, 1);
    }

    #[test]
    fn a_reservation_changes_nothing_until_it_settles() {
        let mut inv = Inventory::default();
        assert!(inv.reserve(hash(1), buy(addr(1), 100.0, 10.0)));
        assert!(inv.get(addr(1)).is_none(), "a sent transaction is not a fill");
        assert_eq!(inv.settle(hash(1), None, None), Some(Side::Buy));
        assert_eq!(inv.get(addr(1)).unwrap().held(), raw(100.0));
    }

    #[test]
    fn a_rolled_back_buy_leaves_the_average_untouched() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        // A second buy that never lands must not drag the average down with it.
        inv.reserve(hash(2), buy(addr(1), 900.0, 1.0));
        assert_eq!(inv.rollback(hash(2)), Some(Side::Buy));
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(100.0));
        assert_eq!(p.avg_price, 10.0);
        assert_eq!(p.buys, 1);
    }

    #[test]
    fn a_sale_ends_the_position_only_once_it_confirms() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        inv.reserve(hash(3), sale(addr(1), 100.0));
        assert!(inv.get(addr(1)).is_some(), "still held while the sale is in flight");
        // A sale that fails leaves the position exactly as it was, so the next
        // attempt still knows what it is selling and at what average.
        assert_eq!(inv.rollback(hash(3)), Some(Side::Sell));
        assert_eq!(inv.get(addr(1)).unwrap().avg_price, 10.0);

        inv.reserve(hash(4), sale(addr(1), 100.0));
        assert_eq!(inv.settle(hash(4), None, None), Some(Side::Sell));
        assert!(inv.get(addr(1)).is_none());
    }

    #[test]
    fn a_partial_sale_leaves_the_rest_of_the_position() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        // Capped by the wallet balance, only 40 went. The remaining 60 are
        // still ours and still carry the price they were bought at.
        inv.reserve(hash(2), sale(addr(1), 40.0));
        inv.settle(hash(2), None, None);
        let p = inv.get(addr(1)).expect("the rest is still held");
        assert_eq!(p.held(), raw(60.0));
        assert_eq!(p.qty, 60.0);
        assert_eq!(p.avg_price, 10.0, "selling does not change what it cost");
    }

    #[test]
    fn settling_the_same_transaction_twice_does_nothing() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        assert_eq!(inv.settle(hash(1), None, None), Some(Side::Buy));
        assert_eq!(inv.settle(hash(1), None, None), None, "the reservation is gone");
        assert_eq!(inv.get(addr(1)).unwrap().held(), raw(100.0), "and was counted once");
    }

    #[test]
    fn exact_amounts_survive_a_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("mmfall-inv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inventory.json");
        let _ = std::fs::remove_file(&path);

        let mut inv = Inventory::load(&path).unwrap();
        assert!(inv.is_empty(), "a missing file is a first run, not a failure");
        // A number f64 cannot hold, which is the whole reason it is stored as
        // a string: a sale asks for exactly this.
        let odd = U256::from_dec_str("123456789012345678901").unwrap();
        inv.reserve(
            hash(7),
            Trade { symbol: "CAMELTOE".into(), raw: odd, ..buy(addr(7), 1.0, 0.0000123) },
        );
        inv.settle(hash(7), None, None);
        inv.reserve(hash(8), buy(addr(8), 1.0, 5.0));
        inv.save().unwrap();

        let back = Inventory::load(&path).unwrap();
        assert_eq!(back.get(addr(7)).unwrap().held(), odd);
        assert_eq!(back.get(addr(7)).unwrap().symbol, "CAMELTOE");
        assert_eq!(back.unsettled().len(), 1, "a trade in flight is not lost either");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_position_is_only_forgotten_by_a_sale_that_landed() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        inv.reserve(hash(2), sale(addr(1), 100.0));
        inv.settle(hash(2), None, None);
        assert!(inv.get(addr(1)).is_none());
        assert!(inv.settle(hash(2), None, None).is_none(), "and stays forgotten");
    }

    #[test]
    fn cash_starts_at_zero_and_is_set_by_seeding() {
        let mut inv = Inventory::default();
        assert_eq!(inv.cash(addr(9)), U256::zero(), "untouched means zero, not unknown");
        inv.set_cash(addr(9), raw(50.0));
        assert_eq!(inv.cash(addr(9)), raw(50.0));
        // The chain is always the source of truth for this number, so a
        // re-seed overwrites - unlike a position's entry price, there is
        // nothing here worth protecting a stale guess from.
        inv.set_cash(addr(9), raw(12.0));
        assert_eq!(inv.cash(addr(9)), raw(12.0));
    }

    #[test]
    fn a_debit_is_committed_before_the_trade_even_sends() {
        let mut inv = Inventory::default();
        inv.set_cash(addr(9), raw(100.0));
        inv.debit_cash(addr(9), raw(30.0));
        assert_eq!(inv.cash(addr(9)), raw(70.0), "spoken for immediately");
        inv.debit_cash(addr(9), raw(1000.0));
        assert_eq!(inv.cash(addr(9)), U256::zero(), "floors rather than wraps negative");
    }

    #[test]
    fn a_buy_that_never_sent_gives_its_debit_back() {
        // This mirrors what on_tick/on_report do: debit up front, then credit
        // back through the ordinary API when the buy never materialises -
        // there is no dedicated "abort" method because there is nothing this
        // could do that reserve/rollback don't already do more precisely.
        let mut inv = Inventory::default();
        inv.set_cash(addr(9), raw(100.0));
        inv.debit_cash(addr(9), raw(30.0));
        inv.credit_cash(addr(9), raw(30.0));
        assert_eq!(inv.cash(addr(9)), raw(100.0), "as if it never happened");
    }

    #[test]
    fn a_sales_proceeds_are_credited_the_moment_it_reserves() {
        let mut inv = Inventory::default();
        let proceeds = addr(2);
        inv.set_cash(proceeds, raw(10.0));
        let trade = Trade { credit: Some((proceeds, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        assert_eq!(
            inv.cash(proceeds),
            raw(50.0),
            "credited on the optimistic assumption the sale lands, before any receipt"
        );
    }

    #[test]
    fn a_sale_that_never_landed_gives_its_credit_back() {
        let mut inv = Inventory::default();
        let proceeds = addr(2);
        inv.set_cash(proceeds, raw(10.0));
        let trade = Trade { credit: Some((proceeds, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        assert_eq!(inv.rollback(hash(1)), Some(Side::Sell));
        assert_eq!(inv.cash(proceeds), raw(10.0), "the credit never really happened either");
    }

    #[test]
    fn without_a_receipt_the_quoted_credit_stands() {
        // settle() is called with no `credit_moved` when the receipt could not
        // be read - the best available answer is the one already applied.
        let mut inv = Inventory::default();
        let proceeds = addr(2);
        inv.set_cash(proceeds, raw(10.0));
        filled(&mut inv, 9, addr(1), 5.0, 1.0);
        let trade = Trade { credit: Some((proceeds, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        assert_eq!(inv.settle(hash(1), None, None), Some(Side::Sell));
        assert_eq!(inv.cash(proceeds), raw(50.0), "the quoted 40 is still all there is to go on");
    }

    #[test]
    fn settling_corrects_the_credit_to_what_the_receipt_says() {
        // The whole point of crediting at reserve time is speed, not accuracy -
        // the quote and the real proceeds can differ either way depending on
        // how the price moved before confirmation, and settle() is where that
        // gets fixed. Quoted 40, but only 33 actually arrived.
        let mut inv = Inventory::default();
        let proceeds = addr(2);
        inv.set_cash(proceeds, raw(10.0));
        filled(&mut inv, 9, addr(1), 5.0, 1.0);
        let trade = Trade { credit: Some((proceeds, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        assert_eq!(inv.cash(proceeds), raw(50.0), "optimistic, before settling");
        assert_eq!(inv.settle(hash(1), None, Some(raw(33.0))), Some(Side::Sell));
        assert_eq!(inv.cash(proceeds), raw(43.0), "10 starting + 33 real, not +40 quoted");
    }

    #[test]
    fn settling_can_correct_the_credit_upward_too() {
        // Price can move in the trade's favour just as easily as against it -
        // the receipt is authoritative in either direction, not just downward.
        let mut inv = Inventory::default();
        let proceeds = addr(2);
        inv.set_cash(proceeds, raw(10.0));
        filled(&mut inv, 9, addr(1), 5.0, 1.0);
        let trade = Trade { credit: Some((proceeds, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        inv.settle(hash(1), None, Some(raw(45.0)));
        assert_eq!(inv.cash(proceeds), raw(55.0), "10 starting + 45 real");
    }

    #[test]
    fn a_debit_landing_between_reserve_and_settle_is_not_forgiven() {
        // The proceeds token is also what other routes spend. Quoted 40, then
        // a buy took 45 out of the shared balance, then only 33 really came
        // back. Taking the whole quote off first would floor at zero and add
        // 33 on top of nothing - the 45 that left would be counted as if it
        // had not. The net difference is what has to move.
        let mut inv = Inventory::default();
        let shared = addr(2);
        inv.set_cash(shared, raw(10.0));
        filled(&mut inv, 9, addr(1), 5.0, 1.0);
        let trade = Trade { credit: Some((shared, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        inv.debit_cash(shared, raw(45.0));
        assert_eq!(inv.cash(shared), raw(5.0));
        inv.settle(hash(1), None, Some(raw(33.0)));
        // 10 - 45 + 33 = -2 -> floored, and never 33.
        assert_eq!(inv.cash(shared), U256::zero());

        // And when the real figure beats the quote, the difference is added -
        // to whatever is there now, not to a stale reading.
        let mut inv = Inventory::default();
        inv.set_cash(shared, raw(10.0));
        filled(&mut inv, 9, addr(1), 5.0, 1.0);
        let trade = Trade { credit: Some((shared, raw(40.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        inv.debit_cash(shared, raw(45.0));
        inv.settle(hash(1), None, Some(raw(47.0)));
        assert_eq!(inv.cash(shared), raw(12.0), "5 + (47 - 40)");
    }

    #[test]
    fn a_credit_saturates_rather_than_panicking() {
        // U256's `+` panics on overflow in every build profile. A corrupt
        // stored balance must not be able to take the whole process down.
        let mut inv = Inventory::default();
        inv.set_cash(addr(9), U256::MAX);
        inv.credit_cash(addr(9), raw(1.0));
        assert_eq!(inv.cash(addr(9)), U256::MAX);
        filled(&mut inv, 1, addr(1), 5.0, 1.0);
        let trade = Trade { credit: Some((addr(9), raw(1.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(2), trade);
        inv.settle(hash(2), None, Some(raw(3.0)));
        assert_eq!(inv.cash(addr(9)), U256::MAX, "and the reconciliation too");
    }

    #[test]
    fn a_buy_carries_no_credit_to_reverse() {
        // A buy's Trade has credit: None (see `buy()`); rolling it back must
        // not touch cash at all - there was nothing optimistic to undo, the
        // debit for a buy is committed by the caller, not by `reserve`.
        let mut inv = Inventory::default();
        inv.set_cash(addr(9), raw(100.0));
        inv.reserve(hash(1), buy(addr(1), 5.0, 1.0));
        assert_eq!(inv.cash(addr(9)), raw(100.0));
        inv.rollback(hash(1));
        assert_eq!(inv.cash(addr(9)), raw(100.0));
    }

    #[test]
    fn cash_survives_a_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("mmfall-cash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inventory.json");
        let _ = std::fs::remove_file(&path);

        let mut inv = Inventory::load(&path).unwrap();
        inv.set_cash(addr(9), raw(123.0));
        let trade = Trade { credit: Some((addr(9), raw(7.0))), ..sale(addr(1), 5.0) };
        inv.reserve(hash(1), trade);
        inv.save().unwrap();

        let mut back = Inventory::load(&path).unwrap();
        assert_eq!(back.cash(addr(9)), raw(130.0));
        // And the pending credit itself round-trips too, so a rollback after a
        // restart still knows what to undo.
        assert_eq!(back.rollback(hash(1)), Some(Side::Sell));
        assert_eq!(back.cash(addr(9)), raw(123.0));
        std::fs::remove_file(&path).ok();
    }
}
