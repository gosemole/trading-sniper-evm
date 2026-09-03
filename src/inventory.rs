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
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Inventory {
    #[serde(default)]
    positions: HashMap<String, Position>,
    /// Reservations, keyed by transaction hash.
    #[serde(default)]
    pending: HashMap<String, Pending>,
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

    /// Set a trade aside until the chain confirms it.
    ///
    /// Returns false for a size or price that cannot mean anything, so a
    /// caller can tell "reserved" from "ignored".
    pub fn reserve(&mut self, hash: ethers::types::H256, t: Trade) -> bool {
        if t.side == Side::Buy && (!t.price.is_finite() || t.price <= 0.0 || t.raw.is_zero()) {
            return false;
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
            },
        );
        true
    }

    /// The chain confirmed it.
    ///
    /// `moved` is what the receipt says actually changed hands - the amount
    /// bought, or the amount sold. It is preferred over the quote in every
    /// case, because the quote is what was expected and this is what happened.
    /// Without it the reservation's own figure is used, which is the best that
    /// can be done and is why it is kept.
    pub fn settle(
        &mut self,
        hash: ethers::types::H256,
        moved: Option<ethers::types::U256>,
    ) -> Option<Side> {
        let p = self.pending.remove(&format!("{hash:?}").to_lowercase())?;
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

    /// It did not happen: forget the reservation and leave the position exactly
    /// as it was.
    pub fn rollback(&mut self, hash: ethers::types::H256) -> Option<Side> {
        self.pending
            .remove(&format!("{hash:?}").to_lowercase())
            .map(|p| p.side)
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
        inv.settle(hash(h), None);
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
        inv.settle(hash(1), Some(raw(97.0)));
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(97.0));
        assert_eq!(p.qty, 97.0);
        assert_eq!(p.avg_price, 10.0);
    }

    #[test]
    fn an_unreadable_receipt_falls_back_to_the_quote() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        inv.settle(hash(1), None);
        assert_eq!(inv.get(addr(1)).unwrap().held(), raw(100.0));
    }

    #[test]
    fn a_buy_that_delivered_nothing_is_not_a_position() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        assert_eq!(inv.settle(hash(1), Some(U256::zero())), Some(Side::Buy));
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
            assert!(inv.settle(hash(n), None).is_none(), "and nothing to settle");
        }
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.held(), raw(100.0));
        assert_eq!(p.avg_price, 10.0);
        assert_eq!(p.buys, 1);
    }

    #[test]
    fn a_reservation_changes_nothing_until_it_settles() {
        let mut inv = Inventory::default();
        assert!(inv.reserve(hash(1), buy(addr(1), 100.0, 10.0)));
        assert!(inv.get(addr(1)).is_none(), "a sent transaction is not a fill");
        assert_eq!(inv.settle(hash(1), None), Some(Side::Buy));
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
        assert_eq!(inv.settle(hash(4), None), Some(Side::Sell));
        assert!(inv.get(addr(1)).is_none());
    }

    #[test]
    fn a_partial_sale_leaves_the_rest_of_the_position() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        // Capped by the wallet balance, only 40 went. The remaining 60 are
        // still ours and still carry the price they were bought at.
        inv.reserve(hash(2), sale(addr(1), 40.0));
        inv.settle(hash(2), None);
        let p = inv.get(addr(1)).expect("the rest is still held");
        assert_eq!(p.held(), raw(60.0));
        assert_eq!(p.qty, 60.0);
        assert_eq!(p.avg_price, 10.0, "selling does not change what it cost");
    }

    #[test]
    fn settling_the_same_transaction_twice_does_nothing() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), buy(addr(1), 100.0, 10.0));
        assert_eq!(inv.settle(hash(1), None), Some(Side::Buy));
        assert_eq!(inv.settle(hash(1), None), None, "the reservation is gone");
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
        inv.settle(hash(7), None);
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
        inv.settle(hash(2), None);
        assert!(inv.get(addr(1)).is_none());
        assert!(inv.settle(hash(2), None).is_none(), "and stays forgotten");
    }
}
