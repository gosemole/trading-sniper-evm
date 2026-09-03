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
    /// Human units acquired, as quoted at the time of each buy.
    pub qty: f64,
    /// Pool price of this token, weighted by quantity across every buy.
    pub avg_price: f64,
    pub buys: u32,
    /// Unix seconds of the last change.
    pub updated: u64,
}

impl Position {
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
    /// Quoted size, folded into the position only once this settles.
    pub qty: f64,
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
    pub fn reserve(
        &mut self,
        hash: ethers::types::H256,
        side: Side,
        token: ethers::types::Address,
        symbol: &str,
        qty: f64,
        price: f64,
    ) -> bool {
        if side == Side::Buy
            && (!qty.is_finite() || !price.is_finite() || qty <= 0.0 || price <= 0.0)
        {
            return false;
        }
        self.pending.insert(
            format!("{hash:?}").to_lowercase(),
            Pending {
                side,
                token: key(token),
                symbol: symbol.to_string(),
                qty,
                price,
                at: now_secs(),
            },
        );
        true
    }

    /// The chain confirmed it: a buy joins the average, a sale ends the
    /// position outright.
    pub fn settle(&mut self, hash: ethers::types::H256) -> Option<Side> {
        let p = self.pending.remove(&format!("{hash:?}").to_lowercase())?;
        match p.side {
            Side::Buy => {
                let e = self
                    .positions
                    .entry(p.token.clone())
                    .or_insert_with(|| Position {
                        symbol: p.symbol.clone(),
                        qty: 0.0,
                        avg_price: p.price,
                        buys: 0,
                        updated: 0,
                    });
                let total = e.qty + p.qty;
                e.avg_price = (e.avg_price * e.qty + p.price * p.qty) / total;
                e.qty = total;
                e.buys += 1;
                e.updated = now_secs();
            }
            Side::Sell => {
                self.positions.remove(&p.token);
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
    use ethers::types::Address;

    fn addr(b: u8) -> Address {
        Address::from([b; 20])
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
        for (n, qty, price) in [
            (2u8, 0.0, 5.0),
            (3, -5.0, 5.0),
            (4, f64::NAN, 5.0),
            (5, 10.0, 0.0),
            (6, 10.0, f64::INFINITY),
        ] {
            assert!(
                !inv.reserve(hash(n), Side::Buy, addr(1), "TKN", qty, price),
                "qty {qty} at {price} should be refused"
            );
            assert!(inv.settle(hash(n)).is_none(), "and nothing to settle");
        }
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.qty, 100.0);
        assert_eq!(p.avg_price, 10.0);
        assert_eq!(p.buys, 1);
    }

    #[test]
    fn positions_survive_a_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("mmfall-inv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inventory.json");
        let _ = std::fs::remove_file(&path);

        let mut inv = Inventory::load(&path).unwrap();
        assert!(inv.is_empty(), "a missing file is a first run, not a failure");
        inv.reserve(hash(7), Side::Buy, addr(7), "CAMELTOE", 500.0, 0.0000123);
        inv.settle(hash(7));
        inv.save().unwrap();

        let back = Inventory::load(&path).unwrap();
        assert_eq!(back.get(addr(7)), inv.get(addr(7)));
        assert_eq!(back.get(addr(7)).unwrap().symbol, "CAMELTOE");

        std::fs::remove_file(&path).ok();
    }

    fn hash(b: u8) -> ethers::types::H256 {
        ethers::types::H256::from([b; 32])
    }

    /// A buy that reached the chain, which is the only way a position grows.
    fn filled(inv: &mut Inventory, h: u8, token: Address, qty: f64, price: f64) {
        inv.reserve(hash(h), Side::Buy, token, "TKN", qty, price);
        inv.settle(hash(h));
    }

    #[test]
    fn a_reservation_changes_nothing_until_it_settles() {
        let mut inv = Inventory::default();
        assert!(inv.reserve(hash(1), Side::Buy, addr(1), "TKN", 100.0, 10.0));
        assert!(inv.get(addr(1)).is_none(), "a sent transaction is not a fill");
        assert_eq!(inv.settle(hash(1)), Some(Side::Buy));
        assert_eq!(inv.get(addr(1)).unwrap().qty, 100.0);
    }

    #[test]
    fn a_rolled_back_buy_leaves_the_average_untouched() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), Side::Buy, addr(1), "TKN", 100.0, 10.0);
        inv.settle(hash(1));
        // A second buy that never lands must not drag the average down with it.
        inv.reserve(hash(2), Side::Buy, addr(1), "TKN", 900.0, 1.0);
        assert_eq!(inv.rollback(hash(2)), Some(Side::Buy));
        let p = inv.get(addr(1)).unwrap();
        assert_eq!(p.qty, 100.0);
        assert_eq!(p.avg_price, 10.0);
        assert_eq!(p.buys, 1);
    }

    #[test]
    fn a_sale_ends_the_position_only_once_it_confirms() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        inv.reserve(hash(3), Side::Sell, addr(1), "TKN", 0.0, 0.0);
        assert!(inv.get(addr(1)).is_some(), "still held while the sale is in flight");
        // A sale that fails leaves the position exactly as it was, so the next
        // attempt still knows what it is selling and at what average.
        assert_eq!(inv.rollback(hash(3)), Some(Side::Sell));
        assert_eq!(inv.get(addr(1)).unwrap().avg_price, 10.0);

        inv.reserve(hash(4), Side::Sell, addr(1), "TKN", 0.0, 0.0);
        assert_eq!(inv.settle(hash(4)), Some(Side::Sell));
        assert!(inv.get(addr(1)).is_none());
    }

    #[test]
    fn settling_the_same_transaction_twice_does_nothing() {
        let mut inv = Inventory::default();
        inv.reserve(hash(1), Side::Buy, addr(1), "TKN", 100.0, 10.0);
        assert_eq!(inv.settle(hash(1)), Some(Side::Buy));
        assert_eq!(inv.settle(hash(1)), None, "the reservation is gone");
        assert_eq!(inv.get(addr(1)).unwrap().qty, 100.0, "and was counted once");
    }

    #[test]
    fn reservations_outlive_a_restart_and_can_be_looked_up() {
        let dir = std::env::temp_dir().join(format!("mmfall-pend-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inventory.json");
        let _ = std::fs::remove_file(&path);

        let mut inv = Inventory::load(&path).unwrap();
        inv.reserve(hash(9), Side::Buy, addr(2), "TKN", 5.0, 2.0);
        inv.save().unwrap();

        let back = Inventory::load(&path).unwrap();
        let un = back.unsettled();
        assert_eq!(un.len(), 1, "a restart must not lose a trade in flight");
        assert_eq!(un[0].0, hash(9));
        assert_eq!(un[0].1.side, Side::Buy);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_position_is_only_forgotten_by_a_sale_that_landed() {
        let mut inv = Inventory::default();
        filled(&mut inv, 1, addr(1), 100.0, 10.0);
        inv.reserve(hash(2), Side::Sell, addr(1), "TKN", 0.0, 0.0);
        inv.settle(hash(2));
        assert!(inv.get(addr(1)).is_none());
        assert!(inv.settle(hash(2)).is_none(), "and stays forgotten");
    }
}
