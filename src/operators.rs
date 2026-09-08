//! Who is behind a launch, when they keep changing address.
//!
//! A deployer address says almost nothing: 2797 of one night's 3473 launches
//! came from an address that never launched again. The wallets around it say a
//! great deal, because they are the expensive part - a bundle is funded, and
//! funding a fresh set for every launch costs real money, so operators reuse
//! them. On that night 1421 bundle wallets appeared under more than one
//! deployer, and joining launches through them collapsed 728 deployers into 88
//! operators covering a quarter of the flow. The largest was 108 deployers
//! across 108 launches: a new deployer every single time, the same wallets
//! underneath.
//!
//! That is worth knowing because operators are not alike. Measured over that
//! night, one ran 35 launches with nothing to show on any of them, another 43
//! with two percent worth holding; a third ran 26 of which 62% were. The
//! deployer address hid all of it.
//!
//! What is kept is the outcome of the shadow position, not something easier to
//! measure. The curve's own peak correlates with what a position would have
//! returned at 0.62 and the number of outside buyers at 0.20, so a history
//! built on either would be a history of the wrong thing.
//!
//! Nothing here is inferred from money movements: the funding that would join
//! the other three quarters is not in a launch's logs and would need a pass
//! over each new deployer's first incoming transfer. This is the part that is
//! free.

use anyhow::{Context, Result};
use ethers::types::Address;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

pub type OpId = u32;

/// The most wallets one operator may collect before a join is refused.
///
/// The guard against a single shared address - a service, a router, a fee
/// collector that happens to sit in an exemption list - welding every operator
/// on the chain into one blob. The largest real cluster on a night of 3540
/// launches held a few hundred wallets, so this is far above anything genuine
/// and far below a runaway.
const MAX_WALLETS: u32 = 5_000;

/// How many outcomes are kept per operator. Enough to say what they usually
/// do, few enough that the file does not grow without bound.
const KEEP_OUTCOMES: usize = 64;

/// What an operator has done, as far as we have seen.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub launches: u32,
    pub wallets: u32,
    /// Closed shadow positions, in hundredths of what they cost. 100 is break
    /// even. Oldest first, and only the last [`KEEP_OUTCOMES`] are kept.
    #[serde(default)]
    pub outcomes: Vec<u64>,
    /// Launches of theirs where nobody outside the bundle ever bought.
    #[serde(default)]
    pub dead: u32,
    pub first_block: u64,
    pub last_block: u64,
}

/// What is known about an operator before their next launch is decided on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub launches: u32,
    pub dead: u32,
    /// How many of their positions we have actually seen closed. A history of
    /// launches with no closed positions says nothing about outcomes.
    pub closed: usize,
    /// The middle one, in hundredths of cost. Absent until something closed.
    pub median_x100: Option<u64>,
}

impl Verdict {
    /// Whether their record is bad enough to pass on the next one.
    ///
    /// Deliberately conservative: it takes several closed positions before a
    /// verdict is allowed at all, because two bad launches are two bad
    /// launches and not a pattern.
    pub fn is_poor(&self, need: usize) -> bool {
        self.closed >= need && self.median_x100.is_some_and(|m| m < 100)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct Operators {
    /// Wallet -> the operator it was last filed under. The value may have been
    /// merged since; [`Operators::resolve`] follows that.
    of: HashMap<String, OpId>,
    /// An operator that was merged into another, and which. Kept instead of
    /// rewriting every wallet at merge time, which is what makes joining two
    /// large operators cheap.
    #[serde(default)]
    alias: HashMap<OpId, OpId>,
    ops: HashMap<OpId, Record>,
    next: OpId,
}

fn key(a: &Address) -> String {
    format!("{a:?}")
}

impl Operators {
    /// Read the store, or start an empty one.
    ///
    /// A file that cannot be parsed is an error rather than a fresh start: the
    /// history is the only thing here that cannot be recomputed from the
    /// journals, and silently discarding it would look exactly like a first
    /// run.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write it out, through a temporary file so a kill mid-write cannot leave
    /// a truncated store behind.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    /// Follow a merged operator to the one it became.
    fn resolve(&self, mut id: OpId) -> OpId {
        // Bounded rather than trusting the chain to be acyclic: a hand-edited
        // file must not hang the process.
        for _ in 0..64 {
            match self.alias.get(&id) {
                Some(&next) if next != id => id = next,
                _ => return id,
            }
        }
        id
    }

    /// Which operator these wallets belong to, joining them if they were
    /// previously thought to be different people.
    ///
    /// `wallets` is everything a launch names: the deployer, whoever collects
    /// the creator fee, and every address exempted from the snipe tax.
    pub fn join(&mut self, wallets: &[Address], block: u64) -> OpId {
        let keys: Vec<String> = wallets.iter().map(key).collect();
        let mut found: Vec<OpId> = keys
            .iter()
            .filter_map(|k| self.of.get(k).copied())
            .map(|id| self.resolve(id))
            .collect();
        found.sort_unstable();
        found.dedup();

        // The one that keeps its identity is the one with the most launches,
        // so the surviving id is the one a person would recognise.
        // Operators a join was refused for. Their wallets must keep pointing
        // at them: repointing those would weld the two together through the
        // wallet map, which is the very thing the refusal is for.
        let mut refused: std::collections::HashSet<OpId> = Default::default();
        let id = match found.first().copied() {
            None => {
                let id = self.next;
                self.next += 1;
                self.ops.insert(id, Record { first_block: block, ..Default::default() });
                id
            }
            Some(first) => {
                let keep = found
                    .iter()
                    .copied()
                    .max_by_key(|i| self.ops.get(i).map(|r| r.launches).unwrap_or(0))
                    .unwrap_or(first);
                for other in found.iter().copied().filter(|i| *i != keep) {
                    let Some(rec) = self.ops.remove(&other) else { continue };
                    let Some(into) = self.ops.get(&keep) else { continue };
                    // A join this large is not an operator, it is a wallet
                    // that everybody happens to use. Refuse it and say so:
                    // welding two crowds together loses both.
                    if into.wallets.saturating_add(rec.wallets) > MAX_WALLETS {
                        tracing::warn!(
                            keep, other,
                            wallets = into.wallets + rec.wallets,
                            "refusing to join two operators through a shared address"
                        );
                        self.ops.insert(other, rec);
                        refused.insert(other);
                        continue;
                    }
                    let into = self.ops.get_mut(&keep).expect("just read");
                    into.launches += rec.launches;
                    into.dead += rec.dead;
                    into.wallets += rec.wallets;
                    into.outcomes.extend(rec.outcomes);
                    let n = into.outcomes.len();
                    if n > KEEP_OUTCOMES {
                        into.outcomes.drain(..n - KEEP_OUTCOMES);
                    }
                    into.first_block = into.first_block.min(rec.first_block);
                    into.last_block = into.last_block.max(rec.last_block);
                    self.alias.insert(other, keep);
                }
                keep
            }
        };

        for k in keys {
            if self
                .of
                .get(&k)
                .is_some_and(|held| refused.contains(&self.resolve(*held)))
            {
                continue;
            }
            // Only wallets that are new to this operator count towards its
            // size, or a bundle seen fifty times would look like fifty times
            // the wallets.
            if self.of.insert(k, id) != Some(id) {
                if let Some(r) = self.ops.get_mut(&id) {
                    r.wallets += 1;
                }
            }
        }
        if let Some(r) = self.ops.get_mut(&id) {
            r.launches += 1;
            r.last_block = r.last_block.max(block);
            if r.first_block == 0 {
                r.first_block = block;
            }
        }
        id
    }

    /// A closed shadow position, in hundredths of what it cost.
    pub fn record(&mut self, id: OpId, x100: u64) {
        let id = self.resolve(id);
        let Some(r) = self.ops.get_mut(&id) else { return };
        r.outcomes.push(x100);
        let n = r.outcomes.len();
        if n > KEEP_OUTCOMES {
            r.outcomes.drain(..n - KEEP_OUTCOMES);
        }
    }

    /// A launch of theirs that nobody outside the bundle ever bought into.
    pub fn note_dead(&mut self, id: OpId) {
        let id = self.resolve(id);
        if let Some(r) = self.ops.get_mut(&id) {
            r.dead += 1;
        }
    }

    /// What is known about whoever these wallets belong to, **without**
    /// filing this launch under them.
    ///
    /// Asked before deciding, so it must not count the launch being decided
    /// about - a history that includes the present is not a history.
    pub fn verdict(&self, wallets: &[Address]) -> Option<Verdict> {
        let id = wallets
            .iter()
            .find_map(|w| self.of.get(&key(w)).copied())
            .map(|id| self.resolve(id))?;
        let r = self.ops.get(&id)?;
        let mut sorted = r.outcomes.clone();
        sorted.sort_unstable();
        Some(Verdict {
            launches: r.launches,
            dead: r.dead,
            closed: sorted.len(),
            median_x100: (!sorted.is_empty()).then(|| sorted[sorted.len() / 2]),
        })
    }

    /// Drop operators nothing has been heard from since `before`.
    ///
    /// A process that runs for weeks would otherwise keep every wallet it has
    /// ever seen. Their wallets go with them, so a name that comes back starts
    /// a fresh operator rather than pointing at a record that is gone.
    pub fn forget_before(&mut self, before: u64) -> usize {
        let going: std::collections::HashSet<OpId> = self
            .ops
            .iter()
            .filter(|(_, r)| r.last_block < before)
            .map(|(id, _)| *id)
            .collect();
        if going.is_empty() {
            return 0;
        }
        // Resolved first: `retain` holds a mutable borrow, and following an
        // alias needs an immutable one.
        let stale: Vec<String> = self
            .of
            .iter()
            .filter(|(_, id)| going.contains(&self.resolve(**id)))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.of.remove(&k);
        }
        self.alias.retain(|from, to| !going.contains(to) && !going.contains(from));
        self.ops.retain(|id, _| !going.contains(id));
        going.len()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn wallets(&self) -> usize {
        self.of.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> Address {
        let mut b = [0u8; 20];
        b[19] = n;
        Address::from(b)
    }

    /// The whole point: two launches from different deployers that share a
    /// bundle wallet are one operator.
    #[test]
    fn a_shared_wallet_joins_two_deployers() {
        let mut o = Operators::default();
        let first = o.join(&[a(1), a(10), a(11)], 100);
        let second = o.join(&[a(2), a(10), a(12)], 200);
        assert_eq!(first, second, "a shared wallet did not join them");
        let v = o.verdict(&[a(2)]).unwrap();
        assert_eq!(v.launches, 2);
    }

    /// And transitively: A shares with B, B shares with C, so all three are
    /// one operator even though A and C have nothing in common.
    #[test]
    fn the_join_is_transitive() {
        let mut o = Operators::default();
        o.join(&[a(1), a(10)], 100);
        o.join(&[a(2), a(20)], 100);
        assert_ne!(
            o.verdict(&[a(1)]).map(|_| o.resolve(o.of[&key(&a(1))])),
            o.verdict(&[a(2)]).map(|_| o.resolve(o.of[&key(&a(2))])),
        );
        // A launch naming a wallet from each welds them together.
        o.join(&[a(3), a(10), a(20)], 200);
        let x = o.resolve(o.of[&key(&a(1))]);
        let y = o.resolve(o.of[&key(&a(2))]);
        assert_eq!(x, y, "the join did not carry through");
        assert_eq!(o.verdict(&[a(1)]).unwrap().launches, 3);
    }

    /// A merge must not lose what either side had done.
    #[test]
    fn outcomes_survive_a_merge() {
        let mut o = Operators::default();
        let one = o.join(&[a(1), a(10)], 100);
        o.record(one, 300);
        let two = o.join(&[a(2), a(20)], 100);
        o.record(two, 50);
        o.join(&[a(3), a(10), a(20)], 200);
        let v = o.verdict(&[a(1)]).unwrap();
        assert_eq!(v.closed, 2, "an outcome went missing in the merge");
        assert_eq!(v.launches, 3);
        // Recording against the id that was merged away still lands.
        o.record(two, 400);
        assert_eq!(o.verdict(&[a(2)]).unwrap().closed, 3);
    }

    /// The history must not include the launch being asked about.
    #[test]
    fn a_verdict_is_about_the_past_only() {
        let mut o = Operators::default();
        assert!(o.verdict(&[a(1)]).is_none(), "a stranger has a history");
        o.join(&[a(1), a(10)], 100);
        assert_eq!(o.verdict(&[a(1)]).unwrap().launches, 1);
        assert_eq!(o.verdict(&[a(99)]), None);
    }

    /// A verdict needs enough closed positions to be one.
    #[test]
    fn a_poor_record_takes_more_than_one_bad_launch() {
        let mut o = Operators::default();
        let id = o.join(&[a(1)], 100);
        o.record(id, 40);
        let v = o.verdict(&[a(1)]).unwrap();
        assert!(!v.is_poor(3), "one launch is not a pattern");
        o.record(id, 50);
        o.record(id, 60);
        assert!(o.verdict(&[a(1)]).unwrap().is_poor(3));
        // A median above break-even is not poor however many there are.
        let mut o = Operators::default();
        let id = o.join(&[a(2)], 100);
        for x in [40, 250, 300] {
            o.record(id, x);
        }
        assert!(!o.verdict(&[a(2)]).unwrap().is_poor(3));
    }

    /// One address in everybody's exemption list must not weld the chain into
    /// a single operator.
    #[test]
    fn a_shared_address_cannot_weld_two_crowds_together() {
        let mut o = Operators::default();
        // Two operators, each already enormous.
        let big = o.join(&[a(1)], 100);
        o.ops.get_mut(&big).unwrap().wallets = MAX_WALLETS - 1;
        let other = o.join(&[a(2)], 100);
        o.ops.get_mut(&other).unwrap().wallets = MAX_WALLETS - 1;
        assert_ne!(o.resolve(big), o.resolve(other));
        // A launch naming one wallet from each does NOT join them.
        o.join(&[a(1), a(2)], 200);
        assert_ne!(
            o.resolve(o.of[&key(&a(1))]),
            o.resolve(o.of[&key(&a(2))]),
            "a shared address welded two crowds together"
        );
    }

    /// Counting the same bundle again must not inflate how big it looks.
    #[test]
    fn seeing_the_same_wallets_again_does_not_grow_the_operator() {
        let mut o = Operators::default();
        let id = o.join(&[a(1), a(10), a(11)], 100);
        let before = o.ops[&id].wallets;
        o.join(&[a(1), a(10), a(11)], 200);
        assert_eq!(o.ops[&id].wallets, before);
        assert_eq!(o.ops[&id].launches, 2);
    }

    /// Only the recent past is carried, and what is dropped is dropped whole.
    #[test]
    fn old_operators_are_forgotten_with_their_wallets() {
        let mut o = Operators::default();
        o.join(&[a(1), a(10)], 100);
        o.join(&[a(2), a(20)], 5_000);
        assert_eq!(o.forget_before(1_000), 1);
        assert_eq!(o.len(), 1);
        assert!(o.verdict(&[a(1)]).is_none(), "a forgotten wallet still points somewhere");
        assert!(o.verdict(&[a(2)]).is_some());
        assert_eq!(o.wallets(), 2);
    }

    /// Only the last of a long history is kept, and it is the last rather than
    /// the first.
    #[test]
    fn the_history_is_bounded_and_keeps_the_recent_end() {
        let mut o = Operators::default();
        let id = o.join(&[a(1)], 100);
        for i in 0..(KEEP_OUTCOMES as u64 + 20) {
            o.record(id, 100 + i);
        }
        let r = &o.ops[&o.resolve(id)];
        assert_eq!(r.outcomes.len(), KEEP_OUTCOMES);
        assert_eq!(*r.outcomes.last().unwrap(), 100 + KEEP_OUTCOMES as u64 + 19);
    }

    /// The file is the only thing here that cannot be recomputed.
    #[test]
    fn it_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("ops-{}", std::process::id()));
        let path = dir.join("operators.json");
        let mut o = Operators::default();
        let id = o.join(&[a(1), a(10)], 100);
        o.record(id, 275);
        o.note_dead(id);
        o.join(&[a(2), a(10)], 200);
        o.save(&path).unwrap();
        let back = Operators::load(&path).unwrap();
        assert_eq!(back, o);
        assert_eq!(back.verdict(&[a(2)]).unwrap().launches, 2);
        assert_eq!(back.verdict(&[a(2)]).unwrap().dead, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store that cannot be read is not a store that is empty.
    #[test]
    fn a_broken_file_is_an_error_and_not_a_fresh_start() {
        let dir = std::env::temp_dir().join(format!("ops-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("operators.json");
        std::fs::write(&path, "{not json").unwrap();
        assert!(Operators::load(&path).is_err());
        // A file that is simply not there is a first run.
        assert!(Operators::load(&dir.join("nothing.json")).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// The store is written by two programs and read by one, and the format is
    /// the contract between them. This is exactly what `analysis/seed.py`
    /// emits, byte for byte in shape: string keys for the operator ids and for
    /// the aliases, lowercase 0x addresses for the wallets.
    ///
    /// Getting this wrong does not fail loudly - a store that will not parse
    /// looks like a first run, and the bot would go on treating every operator
    /// as a stranger while a perfectly good history sat on disk.
    #[test]
    fn it_reads_what_the_seeding_script_writes() {
        let seeded = r#"{
          "of": {
            "0x1af0263775791236c57d9517102487afb11f692a": 0,
            "0x5d16c21fd043bc66fc8a6e12823ad848dad00764": 3
          },
          "alias": { "78": 3 },
          "ops": {
            "0": {"launches":2,"wallets":1,"outcomes":[96,82],"dead":1,
                  "first_block":57324543,"last_block":57351591},
            "3": {"launches":38,"wallets":5,"outcomes":[92,140,71,88,95],"dead":0,
                  "first_block":57324000,"last_block":57727268}
          },
          "next": 2148
        }"#;
        let o: Operators = serde_json::from_str(seeded).expect("the seeded format changed");
        assert_eq!(o.len(), 2);
        assert_eq!(o.wallets(), 2);

        let one: Address = "0x1af0263775791236c57d9517102487afb11f692a".parse().unwrap();
        let v = o.verdict(&[one]).expect("a seeded wallet is a stranger");
        assert_eq!(v.launches, 2);
        assert_eq!(v.dead, 1);
        assert_eq!(v.median_x100, Some(96));
        assert!(v.is_poor(2), "0.96x over two closed positions is poor");

        // The alias has to be followed, or a merged operator reads as missing.
        let big: Address = "0x5d16c21fd043bc66fc8a6e12823ad848dad00764".parse().unwrap();
        let v = o.verdict(&[big]).unwrap();
        assert_eq!(v.launches, 38);
        assert_eq!(v.closed, 5);
        assert_eq!(v.median_x100, Some(92));
        assert_eq!(o.resolve(78), 3, "an alias was not followed");

        // And a launch filed after loading lands on the same operator rather
        // than starting a new one beside it.
        let mut o = o;
        let id = o.join(&[big], 57_800_000);
        assert_eq!(id, 3);
        assert_eq!(o.verdict(&[big]).unwrap().launches, 39);
    }

    /// A real store, when there is one on disk. Not run by default because it
    /// depends on a file no checkout has, but the one check that covers a
    /// store of thousands rather than a hand-written pair:
    ///
    ///     python3 analysis/seed.py launches/ -o operators.json
    ///     cargo test operators -- --ignored --nocapture
    #[test]
    #[ignore = "needs a seeded operators.json"]
    fn a_real_seeded_store_loads() {
        let path = Path::new("operators.json");
        let o = Operators::load(path).expect("the seeded store did not parse");
        assert!(!o.is_empty(), "a store with no operators in it");
        println!("{} operators, {} wallets", o.len(), o.wallets());
        // Every wallet must point at an operator that is actually there.
        for (w, id) in &o.of {
            assert!(
                o.ops.contains_key(&o.resolve(*id)),
                "{w} points at an operator that is gone"
            );
        }
    }

}
