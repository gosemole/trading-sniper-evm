//! Depth estimation that walks initialized ticks instead of assuming the whole
//! move happens at the current in-range liquidity.
//!
//! The math is the standard concentrated-liquidity step: between two prices at
//! constant `L`,
//!   dy = L * (sqrtB - sqrtA)          (token1 side)
//!   dx = L * (1/sqrtA - 1/sqrtB)      (token0 side)
//! and crossing an initialized tick adds `liquidityNet` going up, subtracts it
//! going down. Prices are handled as f64 `sqrt(P)` rather than X96 integers:
//! this is an estimate, and f64 keeps ~1e-12 relative error, far below the
//! uncertainty from reading state a block late.

use anyhow::{Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, Bytes, TransactionRequest, H256, U256};
use ethers::utils::keccak256;
use std::collections::HashMap;
use std::sync::Mutex;

/// `PoolManager._pools` lives at storage slot 6, and within `Pool.State` the
/// `ticks` mapping is member 4 and `tickBitmap` member 5. Verified against the
/// deployed PoolManager on chain 4663 by cross-checking slot0's packed tick and
/// fee against the price implied by sqrtPriceX96.
const POOLS_SLOT: u64 = 6;

/// How many bitmap words to scan for the next initialized tick. One word spans
/// 256 tick spacings, so this covers a wide span while bounding the RPC cost.
const MAX_BITMAP_WORDS: u32 = 32;
const TICKS_OFFSET: u64 = 4;
const BITMAP_OFFSET: u64 = 5;

/// How many slots to ask for in one `extsload`. The whole scan window is 32
/// words plus a slot per initialized tick found in them, which fits comfortably;
/// the cap exists so an unusually dense pool cannot build a request an endpoint
/// refuses.
const SLOTS_PER_CALL: usize = 128;

/// Where to read tick state from.
#[derive(Debug, Clone)]
pub enum Source {
    /// Uniswap V3 pool contract: `ticks(int24)` and `tickBitmap(int16)` views.
    V3 { pool: Address },
    /// Uniswap V4 PoolManager: the same data read straight out of storage.
    V4 { manager: Address, pool_id: H256 },
}

fn selector(sig: &str) -> Vec<u8> {
    keccak256(sig.as_bytes())[..4].to_vec()
}

/// Sign-extend a signed integer into a 32-byte big-endian word.
fn signed_word(v: i64) -> [u8; 32] {
    let mut w = if v >= 0 { [0u8; 32] } else { [0xffu8; 32] };
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn mapping_slot(key: &[u8; 32], base: U256) -> U256 {
    let mut buf = key.to_vec();
    let mut b = [0u8; 32];
    base.to_big_endian(&mut b);
    buf.extend_from_slice(&b);
    U256::from_big_endian(&keccak256(&buf))
}

/// Floor division, so negative ticks compress the same way Solidity does.
fn compress(tick: i32, spacing: i32) -> i32 {
    let mut c = tick / spacing;
    if tick % spacing != 0 && tick < 0 {
        c -= 1;
    }
    c
}

pub struct TickReader<'a> {
    http: &'a Provider<Http>,
    source: Source,
    spacing: i32,
    /// Storage words already read, so a walk asks for each slot once.
    ///
    /// This is a cache, but its first job is consistency rather than speed: an
    /// unpinned walk used to spread thirty reads across three or four blocks of
    /// a chain that produces ten a second, and could combine a bitmap from one
    /// block with the liquidity from another. Reading each slot once means a
    /// walk sees one version of the pool even when no block was pinned.
    words: Mutex<HashMap<(Address, U256), H256>>,
    /// Scan windows already fetched, so a walk that crosses many ticks inside
    /// one window does not re-derive the same request on every crossing.
    windows: Mutex<std::collections::HashSet<(i32, bool)>>,
    /// Whether to fetch storage in batches through the manager's `extsload`.
    /// Only ever turned off to check that batching changed no answers.
    batched: bool,
    /// Block to read at, or `None` for the head. Pinning it matters whenever a
    /// reading is compared against another: a pool that moves between two
    /// "latest" reads turns the difference between them into price movement
    /// rather than whatever was being measured.
    at: Option<u64>,
}

impl<'a> TickReader<'a> {
    pub fn new(http: &'a Provider<Http>, source: Source, spacing: i32) -> Result<Self> {
        anyhow::ensure!(spacing > 0, "tick spacing must be > 0, got {spacing}");
        Ok(Self {
            http,
            source,
            spacing,
            words: Mutex::new(HashMap::new()),
            windows: Mutex::new(std::collections::HashSet::new()),
            batched: true,
            at: None,
        })
    }

    /// Read every value at one fixed block.
    pub fn at_block(mut self, block: Option<u64>) -> Self {
        self.at = block;
        self
    }

    /// Read storage one slot at a time, the way this did before `extsload`.
    /// Exists so the two can be run against each other on the same block and
    /// shown to produce identical numbers; nothing in the bot turns it off.
    pub fn unbatched(mut self) -> Self {
        self.batched = false;
        self
    }

    /// The block tag every read in this reader uses.
    fn tag(&self) -> String {
        match self.at {
            Some(n) => format!("0x{n:x}"),
            None => "latest".to_string(),
        }
    }

    async fn storage(&self, addr: Address, slot: U256) -> Result<H256> {
        if let Some(w) = self.cached(addr, slot) {
            return Ok(w);
        }
        let word: H256 = self
            .http
            .request(
                "eth_getStorageAt",
                (
                    format!("0x{addr:x}"),
                    format!("0x{slot:064x}"),
                    self.tag(),
                ),
            )
            .await
            .context("eth_getStorageAt")?;
        self.remember(addr, &[slot], &[word]);
        Ok(word)
    }

    fn cached(&self, addr: Address, slot: U256) -> Option<H256> {
        self.words.lock().ok()?.get(&(addr, slot)).copied()
    }

    fn remember(&self, addr: Address, slots: &[U256], words: &[H256]) {
        if let Ok(mut map) = self.words.lock() {
            for (s, w) in slots.iter().zip(words) {
                map.insert((addr, *s), *w);
            }
        }
    }

    /// Read a set of slots in as few requests as the manager allows.
    ///
    /// v4's PoolManager serves `extsload(bytes32[])`, which returns a word per
    /// slot in one call - verified against `eth_getStorageAt` on this chain,
    /// same block, same values. That is what turns a tick walk from thirty-odd
    /// sequential round trips into two: the slots a walk needs are scattered
    /// across hash maps, so a generic multicall or a consecutive-range read
    /// cannot serve them, but this can.
    ///
    /// Anything it cannot batch - a v3 pool, or a manager that does not answer -
    /// falls back to reading one slot at a time, which is what this did before
    /// and is never wrong, only slower.
    async fn prefetch(&self, slots: &[U256]) {
        let Source::V4 { manager, .. } = self.source else { return };
        if !self.batched {
            return;
        }
        let wanted: Vec<U256> = {
            let Ok(map) = self.words.lock() else { return };
            let mut seen = std::collections::HashSet::new();
            slots
                .iter()
                .filter(|s| !map.contains_key(&(manager, **s)) && seen.insert(**s))
                .copied()
                .collect()
        };
        for chunk in wanted.chunks(SLOTS_PER_CALL) {
            match self.extsload(manager, chunk).await {
                Ok(words) => self.remember(manager, chunk, &words),
                // Not an error: every one of these slots is still read
                // individually by whoever asked for it. Said once per walk at
                // debug level rather than warned about per slot.
                Err(e) => {
                    tracing::debug!(err = %format!("{e:#}"), "batch storage read unavailable");
                    return;
                }
            }
        }
    }

    /// `extsload(bytes32[])`: one word back per slot, in order.
    async fn extsload(&self, manager: Address, slots: &[U256]) -> Result<Vec<H256>> {
        let mut data = selector("extsload(bytes32[])");
        let mut push = |v: U256| {
            let mut b = [0u8; 32];
            v.to_big_endian(&mut b);
            data.extend_from_slice(&b);
        };
        // One dynamic array: the offset to it, its length, then its contents.
        push(U256::from(0x20));
        push(U256::from(slots.len()));
        for s in slots {
            push(*s);
        }
        let res = self.call(manager, data).await?;
        let body = res.0.get(64..).context("short extsload return")?;
        anyhow::ensure!(
            body.len() >= slots.len() * 32,
            "extsload returned {} bytes for {} slots",
            res.0.len(),
            slots.len()
        );
        Ok((0..slots.len()).map(|i| H256::from_slice(&body[i * 32..(i + 1) * 32])).collect())
    }

    /// Fetch everything one scan could need, in two requests instead of thirty.
    ///
    /// The scan reads bitmap words one at a time until it finds a set bit, and
    /// then the caller reads that tick's `liquidityNet` before scanning on - so
    /// walking a busy pool used to be dozens of sequential round trips, each
    /// one waiting on the last. Both halves are knowable in advance: the window
    /// of words is fixed by where the scan starts and which way it goes, and
    /// the ticks worth reading are exactly the set bits in those words.
    ///
    /// So: one call for the window, then one for the ticks it revealed. The
    /// scan itself is unchanged and still reads word by word - it just finds
    /// every answer already in hand.
    ///
    /// Ticks are capped because a dense pool can have hundreds of initialized
    /// ticks in one window while a swap crosses two or three; reading them all
    /// would trade round trips for bandwidth without being asked to.
    async fn prefetch_scan(&self, start_word: i32, up: bool, max_words: u32) {
        let Source::V4 { manager, .. } = self.source else { return };
        /// Ticks read ahead per scan. Comfortably past what a swap sized by
        /// this bot crosses, and bounded so an unusual pool cannot blow the
        /// request up.
        const TICKS_AHEAD: usize = 32;
        if !self.batched {
            return;
        }
        // Already fetched, or the lock is poisoned - either way the reads below
        // still work one slot at a time, which is what this did before.
        let fresh = match self.windows.lock() {
            Ok(mut seen) => seen.insert((start_word, up)),
            Err(_) => false,
        };
        if !fresh {
            return;
        }
        let positions: Vec<i32> =
            (0..max_words as i32).map(|i| start_word + if up { i } else { -i }).collect();
        let window: Vec<U256> = positions.iter().filter_map(|w| self.bitmap_slot(*w)).collect();
        self.prefetch(&window).await;

        // Set bits, in the order the scan will meet them, so the cap keeps the
        // ticks that are actually about to be crossed.
        let mut ticks = Vec::new();
        for (pos, slot) in positions.iter().zip(&window) {
            let Some(w) = self.cached(manager, *slot) else { continue };
            let word = U256::from_big_endian(w.as_bytes());
            let bits: Box<dyn Iterator<Item = u32>> = match up {
                true => Box::new(0..256u32),
                false => Box::new((0..256u32).rev()),
            };
            for b in bits {
                if word.bit(b as usize) {
                    if let Some(ts) = self.tick_slot(((pos << 8) + b as i32) * self.spacing) {
                        ticks.push(ts);
                        if ticks.len() >= TICKS_AHEAD {
                            break;
                        }
                    }
                }
            }
            if ticks.len() >= TICKS_AHEAD {
                break;
            }
        }
        self.prefetch(&ticks).await;
    }

    /// The storage slot of one bitmap word, for the batch reader.
    fn bitmap_slot(&self, word_pos: i32) -> Option<U256> {
        match &self.source {
            Source::V4 { pool_id, .. } => {
                let base = Self::v4_base(*pool_id) + U256::from(BITMAP_OFFSET);
                Some(mapping_slot(&signed_word(word_pos as i64), base))
            }
            Source::V3 { .. } => None,
        }
    }

    /// The storage slot of one tick's `TickInfo`, for the batch reader.
    fn tick_slot(&self, tick: i32) -> Option<U256> {
        match &self.source {
            Source::V4 { pool_id, .. } => {
                let base = Self::v4_base(*pool_id) + U256::from(TICKS_OFFSET);
                Some(mapping_slot(&signed_word(tick as i64), base))
            }
            Source::V3 { .. } => None,
        }
    }

    async fn call(&self, to: Address, data: Vec<u8>) -> Result<Bytes> {
        let tx = TransactionRequest::new().to(to).data(Bytes::from(data));
        let at = self.at.map(ethers::types::BlockId::from);
        self.http.call(&tx.into(), at).await.context("eth_call")
    }

    /// Base storage slot of `_pools[poolId]` for a v4 pool.
    fn v4_base(pool_id: H256) -> U256 {
        mapping_slot(&pool_id.0, U256::from(POOLS_SLOT))
    }

    /// One 256-bit word of the tick bitmap, covering 256 spacings.
    async fn bitmap_word(&self, word_pos: i32) -> Result<U256> {
        match &self.source {
            Source::V3 { pool } => {
                let mut data = selector("tickBitmap(int16)");
                data.extend_from_slice(&signed_word(word_pos as i64));
                let res = self.call(*pool, data).await?;
                anyhow::ensure!(res.len() >= 32, "short tickBitmap return");
                Ok(U256::from_big_endian(&res[0..32]))
            }
            Source::V4 { manager, pool_id } => {
                let base = Self::v4_base(*pool_id) + U256::from(BITMAP_OFFSET);
                let slot = mapping_slot(&signed_word(word_pos as i64), base);
                Ok(U256::from_big_endian(self.storage(*manager, slot).await?.as_bytes()))
            }
        }
    }

    /// `liquidityNet` of an initialized tick: how `L` changes crossing it upward.
    async fn liquidity_net(&self, tick: i32) -> Result<i128> {
        match &self.source {
            Source::V3 { pool } => {
                let mut data = selector("ticks(int24)");
                data.extend_from_slice(&signed_word(tick as i64));
                let res = self.call(*pool, data).await?;
                // (liquidityGross, liquidityNet, ...): net is the second word.
                anyhow::ensure!(res.len() >= 64, "short ticks() return");
                Ok(i128::from_be_bytes(res[48..64].try_into().unwrap()))
            }
            Source::V4 { manager, pool_id } => {
                let base = Self::v4_base(*pool_id) + U256::from(TICKS_OFFSET);
                let slot = mapping_slot(&signed_word(tick as i64), base);
                let w = self.storage(*manager, slot).await?;
                // TickInfo packs uint128 liquidityGross then int128 liquidityNet,
                // so net occupies the HIGH 128 bits of the word.
                Ok(i128::from_be_bytes(w.0[0..16].try_into().unwrap()))
            }
        }
    }

    /// Next initialized tick from `from` in the given direction, or None if
    /// none was found within `max_words` bitmap words.
    ///
    /// Direction is asymmetric, matching Uniswap: going up the search is
    /// exclusive of the current compressed tick, going down it is inclusive.
    /// Getting that backwards silently skips a tick and understates the cost.
    async fn next_initialized(&self, from: i32, up: bool, max_words: u32) -> Result<Option<i32>> {
        let compressed = compress(from, self.spacing);
        self.prefetch_scan(compressed >> 8, up, max_words).await;
        let mut word_pos = compressed >> 8;
        let mut bit = compressed & 255;
        let mut first = true;
        for _ in 0..max_words {
            let word = self.bitmap_word(word_pos).await?;
            if let Some(b) = scan_word(word, bit, up, first) {
                return Ok(Some(((word_pos << 8) + b as i32) * self.spacing));
            }
            first = false;
            word_pos += if up { 1 } else { -1 };
            bit = 0;
        }
        Ok(None)
    }
}

/// Find a set bit in one bitmap word, searching from `bit` in the given
/// direction. On the first word the starting bit is excluded going up and
/// included going down; later words are scanned end to end.
fn scan_word(word: U256, bit: i32, up: bool, first: bool) -> Option<u32> {
    if up {
        let start = if first { bit + 1 } else { 0 };
        (start.max(0) as u32..256).find(|b| word.bit(*b as usize))
    } else {
        let end = if first { bit } else { 255 };
        if end < 0 {
            return None;
        }
        (0..=end as u32).rev().find(|b| word.bit(*b as usize))
    }
}

/// Every price at which a pool's liquidity changes, near where it is trading.
///
/// `in_range_out` assumes liquidity holds across the whole swap. That is true
/// exactly when no initialized tick lies between where the price starts and
/// where it ends - a question about a stretch of price, which a list of ticks
/// read earlier answers off-line and for nothing. The percentage cap in
/// `executor::modelled_impact_cap` is only ever a guess at the same question,
/// and it guesses wrong in both directions: it refuses swaps that cross nothing
/// and accepts swaps that cross something a tenth of a percent away.
///
/// A WINDOW rather than the two neighbouring ticks, and that is the whole
/// design. A dip is precisely the moment the price jumps a long way, so a pair
/// of neighbours read around the price ten seconds ago has been left behind by
/// the price this is asked about - abandoning the model at the one moment it
/// exists for. A window scanned wide enough still contains both.
#[derive(Debug, Clone, PartialEq)]
pub struct TickWindow {
    /// Sqrt prices of the initialized ticks found, ascending. Every one of them
    /// is a place liquidity changes; between any two it does not.
    edges: Vec<f64>,
    /// What the scan actually covered. Outside this the list means nothing: a
    /// stretch with no edges INSIDE the window is a fact, the same stretch
    /// outside it is merely unread, and the two must never be confused.
    lo: f64,
    hi: f64,
    /// Whether the scan reached every tick the pool could possibly have, in
    /// which case `crosses` can never come back unread however far the price
    /// travels. True for any ordinary spacing on a batched pool.
    whole: bool,
}

impl TickWindow {
    /// Whether a swap from `from` to `to` passes a price where liquidity
    /// changes.
    ///
    /// `None` means the scan did not reach one of the two ends, so the answer
    /// is UNKNOWN rather than "no". Losing that distinction is the one way this
    /// can do harm: "no" is what makes a caller trust the model outright.
    ///
    /// The interval is closed at both ends. A swap stopping exactly on an
    /// initialized tick does not truly cross it, but answering "it does" costs
    /// one trade priced the slow way, while answering "it does not" on a tick
    /// that was in fact crossed costs a quote nobody checked.
    pub fn crosses(&self, from: f64, to: f64) -> Option<bool> {
        if !from.is_finite() || !to.is_finite() {
            return None;
        }
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        if lo < self.lo || hi > self.hi {
            return None;
        }
        // First edge at or above the bottom of the move; it is crossed when it
        // also sits at or below the top.
        let i = self.edges.partition_point(|e| *e < lo);
        Some(self.edges.get(i).is_some_and(|e| *e <= hi))
    }

    /// The sqrt prices the scan covered, for logs and tests.
    pub fn span(&self) -> (f64, f64) {
        (self.lo, self.hi)
    }

    /// How many places liquidity changes inside it.
    pub fn edges(&self) -> usize {
        self.edges.len()
    }

    /// Whether the scan read the pool end to end. For logs: a window that did
    /// can never be escaped by a price move, however violent.
    pub fn whole(&self) -> bool {
        self.whole
    }
}

/// The furthest tick either protocol allows. A pool cannot have an initialized
/// tick beyond it, so a scan reaching this far on both sides has read the pool
/// entirely and can never answer "I did not look there".
const MAX_TICK: i64 = 887_272;

/// Most bitmap words a scan will read on each side when the manager batches
/// them. The whole tick range fits well inside this for every ordinary spacing:
/// one `extsload` call at spacing 60 or above, a handful at spacing 10. The cap
/// only ever bites on a spacing-1 pool, where 512 words still spans a price
/// factor of about half a million.
const MAX_WINDOW_WORDS: i64 = 512;

/// The same for a pool that cannot be batched - a v3 pool, where every word is
/// its own `eth_call` and reading the range would be thousands of them.
///
/// The coverage this buys falls with the spacing, and that is not the problem
/// it looks like: spacing is chosen to match how far the pair moves, so the
/// pools this covers least are the pools that need it least. Sixteen words is
/// a price factor of 200,000 at spacing 60 and still about 50% at spacing 1,
/// which is a stable pair by construction.
const MAX_UNBATCHED_WORDS: i64 = 16;

/// Bitmap words to read on EACH side of the word the price sits in.
///
/// Aimed at the whole tick range, not at a percentage of price. An earlier
/// version scanned a fixed +/-30% and left everything past it unanswerable -
/// which is backwards, because at spacing 60 the entire range is 58 words a
/// side and arrives in a single batched call. Paying one request to never have
/// to guess again is not a trade-off worth thinking about.
///
/// The word the price sits in does not count towards coverage: the price can be
/// hard against either of its edges, so only the words BEYOND it are guaranteed
/// on both sides.
fn window_words(spacing: i32, batched: bool) -> i32 {
    let cap = if batched { MAX_WINDOW_WORDS } else { MAX_UNBATCHED_WORDS };
    let whole_range = MAX_TICK / (spacing.max(1) as i64 * 256) + 1;
    whole_range.clamp(1, cap) as i32
}

/// Whether a scan of this width reaches every tick the pool could have, in
/// which case `TickWindow::crosses` can never answer "unread".
fn covers_whole_range(words: i32, spacing: i32) -> bool {
    (words as i64) * 256 * spacing.max(1) as i64 >= MAX_TICK
}

/// The sqrt prices a run of `2 * words + 1` bitmap words centred on `centre`
/// covers, which is exactly what `TickWindow` may answer questions about.
fn word_span(centre: i32, words: i32, spacing: i32) -> (f64, f64) {
    let bound = |w: i32, plus: i64| {
        let t = ((((w as i64) << 8) + plus) * spacing as i64).clamp(-MAX_TICK, MAX_TICK);
        sqrt_at_tick(t as i32)
    };
    (bound(centre - words, 0), bound(centre + words, 256))
}

/// Read every initialized tick near `sqrt_p`, so a later swap from around there
/// can be judged exactly instead of by a percentage.
///
/// Batched, and meant for the background: see `executor::TickBook`, which keeps
/// the answer current so that no trade ever waits for this.
pub async fn tick_window(reader: &TickReader<'_>, sqrt_p: f64) -> Result<TickWindow> {
    anyhow::ensure!(
        sqrt_p.is_finite() && sqrt_p > 0.0,
        "cannot scan around a non-positive price"
    );
    let spacing = reader.spacing;
    let centre = compress(tick_at_sqrt(sqrt_p), spacing) >> 8;
    let words = window_words(spacing, reader.batched);

    // Only the bitmap, and all of it at once. `prefetch_scan` would do this too
    // but would also fetch each found tick's liquidity, which a walk needs and
    // this does not: the question here is only WHERE liquidity changes, never
    // by how much.
    let positions: Vec<i32> = (centre - words..=centre + words).collect();
    let slots: Vec<U256> = positions.iter().filter_map(|w| reader.bitmap_slot(*w)).collect();
    reader.prefetch(&slots).await;

    let mut edges = Vec::new();
    for w in positions {
        let word = reader.bitmap_word(w).await?;
        for bit in 0..256u32 {
            if !word.bit(bit as usize) {
                continue;
            }
            let tick = (((w as i64) << 8) + bit as i64) * spacing as i64;
            // A bit can only be set for a tick the pool really has, but the
            // arithmetic above runs before that is known.
            if !(-MAX_TICK..=MAX_TICK).contains(&tick) {
                continue;
            }
            edges.push(sqrt_at_tick(tick as i32));
        }
    }

    // Words ascend and bits within them ascend, and `sqrt_at_tick` rises with
    // the tick, so this comes out sorted without sorting it.
    let (lo, hi) = word_span(centre, words, spacing);
    Ok(TickWindow { edges, lo, hi, whole: covers_whole_range(words, spacing) })
}

/// What a swap yields while it stays inside the current tick range, where
/// liquidity is constant and no tick data is needed at all.
///
/// This is the same step `swap_exact_in` takes between two ticks, on its own.
/// It is exact whenever the swap does not reach an initialized tick, and it
/// overstates the output once it would - so callers must check the price move
/// it reports and stop trusting it before that point. Returns the output and
/// the price the pool would be left at.
///
/// `fee_pips` is the WHOLE fee charged on the input, protocol fee included -
/// `PoolState::swap_fee` for this direction, not the pool's bare `lp_fee`.
pub fn in_range_out(
    sqrt_p: f64,
    liquidity: u128,
    fee_pips: u32,
    zero_for_one: bool,
    amount_in: f64,
) -> Option<(f64, f64)> {
    // NaN has to fail these too, hence the explicit finiteness checks rather
    // than negated comparisons.
    if !sqrt_p.is_finite()
        || sqrt_p <= 0.0
        || !amount_in.is_finite()
        || amount_in <= 0.0
        || liquidity == 0
        || fee_pips >= 1_000_000
    {
        return None;
    }
    let l = liquidity as f64;
    // The fee comes off the input before it reaches the curve.
    let net = amount_in * (1.0 - fee_pips as f64 / 1_000_000.0);
    // Paying token1 pushes the raw price up; paying token0 pushes it down.
    let up = !zero_for_one;
    let (sqrt_new, out) = if up {
        let s = sqrt_p + net / l;
        (s, l * (1.0 / sqrt_p - 1.0 / s))
    } else {
        let s = 1.0 / (1.0 / sqrt_p + net / l);
        (s, l * (sqrt_p - s))
    };
    if !out.is_finite() || out <= 0.0 || !sqrt_new.is_finite() || sqrt_new <= 0.0 {
        return None;
    }
    Some((out, sqrt_new))
}

/// sqrt(price) at a tick, in the plain f64 domain (not X96).
pub fn sqrt_at_tick(tick: i32) -> f64 {
    1.0001f64.powf(tick as f64 / 2.0)
}

/// Tick containing the given sqrt(price). The epsilon absorbs the ~1e-10 error
/// of the log/exp round trip, so a price sitting exactly on a tick boundary
/// reports that tick rather than the one below it.
pub fn tick_at_sqrt(sqrt_p: f64) -> i32 {
    (2.0 * sqrt_p.ln() / 1.0001f64.ln() + 1e-9).floor() as i32
}

/// How much of the quote token must be paid IN to move the base token's price
/// up by `move_pct`, walking every tick the swap would cross.
///
/// `base_token` selects which side is the base, exactly as elsewhere:
/// 1 => base is token1 and the quote (paid in) is token0, which pushes the raw
/// price P = y/x DOWN; 0 => the mirror image.
///
/// `lp_fee_pips` is the pool fee in hundredths of a bip (3000 = 0.3%); the swap
/// fee is taken off the input, so the gross amount paid is grossed up by it.
pub async fn pay_to_move(
    reader: &TickReader<'_>,
    sqrt_p_now: f64,
    liquidity_now: u128,
    base_token: u8,
    move_pct: f64,
    lp_fee_pips: u32,
) -> Result<f64> {
    anyhow::ensure!(move_pct > 0.0, "move_pct must be > 0, got {move_pct}");
    anyhow::ensure!(sqrt_p_now > 0.0, "sqrt price must be > 0");
    anyhow::ensure!(lp_fee_pips < 1_000_000, "fee must be < 100%");

    let k = (1.0 + move_pct / 100.0).sqrt();
    // Buying the base pushes the RAW price up when base is token0, down when
    // base is token1.
    let up = base_token == 0;
    let sqrt_target = if up { sqrt_p_now * k } else { sqrt_p_now / k };

    let mut sqrt_cur = sqrt_p_now;
    let mut liquidity = liquidity_now as f64;
    let mut paid = 0.0f64;
    let mut tick = tick_at_sqrt(sqrt_cur);

    // Each iteration covers one tick segment. The bound is generous: a 100%
    // move at spacing 1 is ~6900 ticks, and initialized ticks are far sparser.
    for _ in 0..1_000 {
        let done = if up { sqrt_cur >= sqrt_target } else { sqrt_cur <= sqrt_target };
        if done {
            break;
        }
        let next = reader.next_initialized(tick, up, MAX_BITMAP_WORDS).await?;
        // Price of the next initialized tick, clamped to the target.
        let sqrt_edge = match next {
            Some(t) => {
                let s = sqrt_at_tick(t);
                if up { s.min(sqrt_target) } else { s.max(sqrt_target) }
            }
            None => sqrt_target,
        };
        if liquidity > 0.0 {
            paid += if up {
                // paying token1 (y) to push P up
                liquidity * (sqrt_edge - sqrt_cur)
            } else {
                // paying token0 (x) to push P down
                liquidity * (1.0 / sqrt_edge - 1.0 / sqrt_cur)
            };
        }
        let reached_target = if up { sqrt_edge >= sqrt_target } else { sqrt_edge <= sqrt_target };
        sqrt_cur = sqrt_edge;
        if reached_target {
            break;
        }
        // Cross the tick: L changes, and the walk continues on the far side.
        let t = next.context("no initialized tick but target not reached")?;
        let net = reader.liquidity_net(t).await? as f64;
        liquidity = (if up { liquidity + net } else { liquidity - net }).max(0.0);
        tick = if up { t } else { t - 1 };
    }

    // The pool fee is charged on the input, so the amount actually sent is
    // larger than the amount that reaches the curve.
    Ok(paid / (1.0 - lp_fee_pips as f64 / 1_000_000.0))
}

/// Live state of a pool: everything a swap simulation starts from.
#[derive(Debug, Clone, Copy)]
pub struct PoolState {
    pub sqrt_p: f64,
    pub liquidity: u128,
    /// Current LP fee in hundredths of a bip. For v4 this is read from slot0,
    /// so a hook that changes the fee dynamically is reflected.
    pub lp_fee: u32,
    /// Protocol fee charged on the input of a zeroForOne swap, in hundredths
    /// of a bip. It is taken before the LP fee and never reaches the curve, so
    /// it is invisible in sqrtPriceX96 and has to be read separately or it is
    /// simply lost. Always 0 on v3, where the protocol's cut comes out of the
    /// LPs' share and the swapper pays the same either way.
    pub protocol_fee_0for1: u32,
    /// The same for a oneForZero swap. v4 lets the two halves differ, so the
    /// direction has to be known before a fee can be named.
    pub protocol_fee_1for0: u32,
}

impl PoolState {
    /// The whole fee a swapper pays on the way in, in hundredths of a bip.
    ///
    /// The protocol takes its cut first and the LPs take theirs from what is
    /// left, so the two do not simply add: this is `pf + lp * (1 - pf)`, which
    /// is what v4's `ProtocolFeeLibrary.calculateSwapFee` computes, kept in
    /// integer pips so it agrees with the chain exactly.
    pub fn swap_fee(&self, zero_for_one: bool) -> u32 {
        let pf = match zero_for_one {
            true => self.protocol_fee_0for1,
            false => self.protocol_fee_1for0,
        } as u64;
        let lp = self.lp_fee as u64;
        (pf + lp - pf * lp / PIPS).min(PIPS) as u32
    }
}

/// Denominator every fee on both protocols is quoted against: a fee of 3000 is
/// 3000/1_000_000 = 0.3%.
const PIPS: u64 = 1_000_000;

/// Largest value either half of v4's packed `protocolFee` may hold (0.1%).
/// Anything above it is not a fee the protocol would accept, so reading one is
/// evidence the word was decoded wrongly rather than evidence of a big fee.
const MAX_PROTOCOL_FEE_PIPS: u32 = 1_000;

/// Read sqrt(price), in-range liquidity and the live fee for a pool.
pub async fn read_state(reader: &TickReader<'_>) -> Result<PoolState> {
    match &reader.source {
        Source::V4 { manager, pool_id } => {
            let base = TickReader::v4_base(*pool_id);
            // Both words in one request: slot0 and, three along, liquidity.
            reader.prefetch(&[base, base + U256::from(3)]).await;
            let s0 = reader.storage(*manager, base).await?;
            anyhow::ensure!(!s0.is_zero(), "pool is not initialized");
            let (sqrt_x96, _tick, protocol_fee, lp_fee) = decode_slot0(s0);
            let (pf_0for1, pf_1for0) = protocol_fee_halves(protocol_fee);
            anyhow::ensure!(
                pf_0for1 <= MAX_PROTOCOL_FEE_PIPS && pf_1for0 <= MAX_PROTOCOL_FEE_PIPS,
                "slot0 reports a protocol fee above the protocol's own ceiling \
                 ({pf_0for1}/{pf_1for0} > {MAX_PROTOCOL_FEE_PIPS} pips), which means this \
                 word was decoded wrongly - refusing to price anything from it"
            );
            let liq = reader.storage(*manager, base + U256::from(3)).await?;
            Ok(PoolState {
                sqrt_p: x96_to_f64(sqrt_x96),
                liquidity: u128::from_be_bytes(liq.0[16..32].try_into().unwrap()),
                lp_fee,
                protocol_fee_0for1: pf_0for1,
                protocol_fee_1for0: pf_1for0,
            })
        }
        Source::V3 { pool } => {
            let s0 = reader.call(*pool, selector("slot0()")).await?;
            anyhow::ensure!(s0.len() >= 32, "short slot0() return");
            let sqrt_x96 = U256::from_big_endian(&s0[0..32]);
            let l = reader.call(*pool, selector("liquidity()")).await?;
            anyhow::ensure!(l.len() >= 32, "short liquidity() return");
            let f = reader.call(*pool, selector("fee()")).await?;
            anyhow::ensure!(f.len() >= 32, "short fee() return");
            Ok(PoolState {
                sqrt_p: x96_to_f64(sqrt_x96),
                liquidity: U256::from_big_endian(&l[16..32]).as_u128(),
                lp_fee: U256::from_big_endian(&f[0..32]).low_u32(),
                // v3's protocol fee is carved out of the LP fee rather than
                // charged on top, so the swapper pays `fee()` and nothing more.
                protocol_fee_0for1: 0,
                protocol_fee_1for0: 0,
            })
        }
    }
}

/// Unpack a v4 `slot0` word.
///
/// Fields are packed from the LOW end: sqrtPriceX96 (160 bits), tick (24),
/// protocolFee (24), lpFee (24). In a big-endian 32-byte word that puts
/// sqrtPriceX96 last and lpFee near the front - easy to get backwards, hence
/// the regression test below against a word read off chain.
pub fn decode_slot0(word: H256) -> (U256, i32, u32, u32) {
    let b = word.0;
    let sqrt_x96 = U256::from_big_endian(&b[12..32]);
    let tick_raw = ((b[9] as u32) << 16) | ((b[10] as u32) << 8) | b[11] as u32;
    let tick = if tick_raw & 0x80_0000 != 0 {
        tick_raw as i32 - 0x100_0000
    } else {
        tick_raw as i32
    };
    let protocol_fee = ((b[6] as u32) << 16) | ((b[7] as u32) << 8) | b[8] as u32;
    let lp_fee = ((b[3] as u32) << 16) | ((b[4] as u32) << 8) | b[5] as u32;
    (sqrt_x96, tick, protocol_fee, lp_fee)
}

/// Split v4's packed 24-bit `protocolFee` into `(zeroForOne, oneForZero)`.
/// The low 12 bits apply to a zeroForOne swap, the high 12 to the other
/// direction - a pool may be configured to charge only one way.
pub fn protocol_fee_halves(packed: u32) -> (u32, u32) {
    (packed & 0xfff, (packed >> 12) & 0xfff)
}

fn x96_to_f64(v: U256) -> f64 {
    let f = match u128::try_from(v) {
        Ok(x) => x as f64,
        Err(_) => {
            let l = v.0;
            let lo = (l[0] as u128) | ((l[1] as u128) << 64);
            let hi = (l[2] as u128) | ((l[3] as u128) << 64);
            hi as f64 * 2f64.powi(128) + lo as f64
        }
    };
    f / 2f64.powi(96)
}

/// Result of simulating one exact-input swap.
#[derive(Debug, Clone, Copy)]
pub struct SwapResult {
    pub amount_out: f64,
    /// Input actually consumed. Less than requested only if the pool ran out of
    /// liquidity in that direction entirely.
    pub amount_in_used: f64,
    pub sqrt_p_after: f64,
    pub ticks_crossed: u32,
}

/// Simulate an exact-input swap, walking every tick it crosses.
///
/// `zero_for_one` means token0 goes in and token1 comes out, which pushes the
/// raw price P = y/x DOWN. Amounts are raw token units.
pub async fn swap_exact_in(
    reader: &TickReader<'_>,
    state: PoolState,
    zero_for_one: bool,
    amount_in: f64,
) -> Result<SwapResult> {
    anyhow::ensure!(amount_in > 0.0, "amount_in must be > 0");
    anyhow::ensure!(state.sqrt_p > 0.0, "sqrt price must be > 0");
    let fee_pips = state.swap_fee(zero_for_one);
    anyhow::ensure!(fee_pips < 1_000_000, "fee must be < 100%");

    // Protocol fee first, LP fee on the remainder, and only what survives both
    // reaches the curve - which is why the price the curve reports afterwards
    // knows nothing about either of them.
    let fee_frac = fee_pips as f64 / 1_000_000.0;
    let mut remaining = amount_in * (1.0 - fee_frac);
    let gross_per_net = 1.0 / (1.0 - fee_frac);

    let mut sqrt_cur = state.sqrt_p;
    let mut liquidity = state.liquidity as f64;
    let mut out = 0.0f64;
    let mut tick = tick_at_sqrt(sqrt_cur);
    let mut crossed = 0u32;
    // Going down in price when selling token0.
    let up = !zero_for_one;

    for _ in 0..1_000 {
        if remaining <= 0.0 {
            break;
        }
        let next = reader.next_initialized(tick, up, MAX_BITMAP_WORDS).await?;
        let sqrt_edge = match next {
            Some(t) => sqrt_at_tick(t),
            None => {
                // No further initialized tick means no boundary where liquidity
                // could change, so the rest of the input trades at constant L.
                // A full-range position looks exactly like this: its only ticks
                // sit at MIN_TICK/MAX_TICK, far outside the scanned window.
                if liquidity > 0.0 {
                    let sqrt_new = if up {
                        sqrt_cur + remaining / liquidity
                    } else {
                        1.0 / (1.0 / sqrt_cur + remaining / liquidity)
                    };
                    out += if up {
                        liquidity * (1.0 / sqrt_cur - 1.0 / sqrt_new)
                    } else {
                        liquidity * (sqrt_cur - sqrt_new)
                    };
                    sqrt_cur = sqrt_new;
                    remaining = 0.0;
                }
                break;
            }
        };
        if liquidity <= 0.0 {
            // Empty range: skip straight across it, no input consumed.
            let t = next.context("no tick to cross")?;
            let net = reader.liquidity_net(t).await? as f64;
            liquidity = (if up { liquidity + net } else { liquidity - net }).max(0.0);
            sqrt_cur = sqrt_edge;
            tick = if up { t } else { t - 1 };
            crossed += 1;
            continue;
        }
        // Input needed to walk this whole segment.
        let cap = if up {
            liquidity * (sqrt_edge - sqrt_cur)
        } else {
            liquidity * (1.0 / sqrt_edge - 1.0 / sqrt_cur)
        };
        if remaining < cap {
            // Stops inside the segment: solve for the price actually reached.
            let sqrt_new = if up {
                sqrt_cur + remaining / liquidity
            } else {
                1.0 / (1.0 / sqrt_cur + remaining / liquidity)
            };
            out += if up {
                liquidity * (1.0 / sqrt_cur - 1.0 / sqrt_new)
            } else {
                liquidity * (sqrt_cur - sqrt_new)
            };
            sqrt_cur = sqrt_new;
            remaining = 0.0;
            break;
        }
        out += if up {
            liquidity * (1.0 / sqrt_cur - 1.0 / sqrt_edge)
        } else {
            liquidity * (sqrt_cur - sqrt_edge)
        };
        remaining -= cap;
        sqrt_cur = sqrt_edge;
        let t = next.context("no tick to cross")?;
        let net = reader.liquidity_net(t).await? as f64;
        liquidity = (if up { liquidity + net } else { liquidity - net }).max(0.0);
        tick = if up { t } else { t - 1 };
        crossed += 1;
    }

    let net_used = amount_in * (1.0 - fee_frac) - remaining;
    Ok(SwapResult {
        amount_out: out,
        amount_in_used: net_used * gross_per_net,
        sqrt_p_after: sqrt_cur,
        ticks_crossed: crossed,
    })
}

#[cfg(test)]
mod tests_in_range {
    use super::*;

    /// The in-range step and the full walk have to agree wherever the walk
    /// crosses nothing - they are the same formula, and this is what lets the
    /// cheap one stand in for the expensive one on small trades.
    #[test]
    fn it_is_the_same_step_the_walk_takes() {
        let sqrt_p = 2.0f64;
        let l = 1_000_000_000u128;
        // A trade small enough to stay put: the price barely moves.
        let (out, after) = in_range_out(sqrt_p, l, 3000, false, 1_000.0).unwrap();
        assert!(out > 0.0);
        assert!(after > sqrt_p, "paying token1 lifts the raw price");
        let moved = (after / sqrt_p).powi(2) - 1.0;
        assert!(moved < 1e-5, "moved {moved}");

        // The other direction moves it the other way.
        let (_, after) = in_range_out(sqrt_p, l, 3000, true, 1_000.0).unwrap();
        assert!(after < sqrt_p);
    }

    #[test]
    fn the_fee_comes_off_the_input() {
        let free = in_range_out(2.0, 1_000_000_000, 0, false, 1_000.0).unwrap().0;
        let charged = in_range_out(2.0, 1_000_000_000, 10_000, false, 1_000.0).unwrap().0;
        // 1% of the input never reaches the curve, so ~1% less comes out.
        let ratio = charged / free;
        assert!((ratio - 0.99).abs() < 1e-6, "ratio {ratio}");
    }

    #[test]
    fn nothing_is_quoted_out_of_nothing() {
        assert!(in_range_out(2.0, 0, 3000, false, 1.0).is_none(), "no liquidity");
        assert!(in_range_out(0.0, 1_000, 3000, false, 1.0).is_none(), "no price");
        assert!(in_range_out(2.0, 1_000, 3000, false, 0.0).is_none(), "no input");
        assert!(in_range_out(2.0, 1_000, 1_000_000, false, 1.0).is_none(), "a 100% fee");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(ticks: &[i32], lo: i32, hi: i32) -> TickWindow {
        TickWindow {
            edges: ticks.iter().map(|t| sqrt_at_tick(*t)).collect(),
            lo: sqrt_at_tick(lo),
            hi: sqrt_at_tick(hi),
            whole: false,
        }
    }

    /// The distinction the whole design rests on: a stretch the scan covered
    /// and found empty is a FACT, the same stretch outside it is merely unread.
    /// Answering "no" for the second would tell the model to trust arithmetic
    /// nothing has checked.
    #[test]
    fn unread_is_not_the_same_answer_as_empty() {
        let w = window(&[-600, 600], -6000, 6000);

        // Inside the scan, between two edges: known, and known to cross nothing.
        assert_eq!(w.crosses(sqrt_at_tick(0), sqrt_at_tick(500)), Some(false));
        // Inside the scan with no edges anywhere near: still a fact.
        assert_eq!(w.crosses(sqrt_at_tick(1000), sqrt_at_tick(5000)), Some(false));
        // One end past what was read: not known, and must not read as "no".
        assert_eq!(w.crosses(sqrt_at_tick(0), sqrt_at_tick(9000)), None);
        assert_eq!(w.crosses(sqrt_at_tick(-9000), sqrt_at_tick(0)), None);
        // A window that found nothing at all still answers inside its span.
        assert_eq!(window(&[], -6000, 6000).crosses(sqrt_at_tick(0), sqrt_at_tick(100)), Some(false));
    }

    #[test]
    fn a_swap_reaching_an_edge_is_a_crossing() {
        let w = window(&[-600, 600], -6000, 6000);

        assert_eq!(w.crosses(sqrt_at_tick(0), sqrt_at_tick(700)), Some(true));
        assert_eq!(w.crosses(sqrt_at_tick(0), sqrt_at_tick(-700)), Some(true));
        // Landing exactly on one counts, deliberately: saying "crossed" costs a
        // trade priced the slow way, saying "did not" costs an unchecked quote.
        assert_eq!(w.crosses(sqrt_at_tick(0), sqrt_at_tick(600)), Some(true));
        // Direction is not part of the question.
        assert_eq!(
            w.crosses(sqrt_at_tick(700), sqrt_at_tick(0)),
            w.crosses(sqrt_at_tick(0), sqrt_at_tick(700))
        );
        // A price that is not a number is never an answer.
        assert_eq!(w.crosses(f64::NAN, sqrt_at_tick(0)), None);
    }

    /// Every ordinary spacing reads the WHOLE tick range, so `crosses` can
    /// never come back "unread" for it - and it does so in a request or two,
    /// which is why aiming at a percentage of price was the wrong idea.
    ///
    /// The word the price sits in cannot count towards coverage: the price can
    /// sit hard against either of its edges, so only the words beyond it are
    /// guaranteed on both sides.
    #[test]
    fn an_ordinary_pool_is_scanned_end_to_end() {
        for spacing in [10, 30, 60, 200, 2000] {
            let words = window_words(spacing, true);
            assert!(
                covers_whole_range(words, spacing),
                "spacing {spacing}: {words} words a side leaves part of the range unread"
            );
            // And it stays cheap enough to do every thirty seconds.
            let calls = (2 * words as usize + 1).div_ceil(SLOTS_PER_CALL);
            assert!(calls <= 6, "spacing {spacing}: {calls} extsload calls");
        }
    }

    /// Where the range does not fit, the cap still has to leave a span nothing
    /// this bot trades could walk out of. The floor is deliberately lower for
    /// an unbatched pool at spacing 1: coverage falls with spacing, and so does
    /// how far such a pair moves, so the least-covered pools are the ones that
    /// need it least.
    #[test]
    fn a_capped_scan_still_covers_more_than_any_real_move() {
        for (spacing, batched, least) in
            [(1, true, 2.0), (60, false, 2.0), (1, false, 0.40)]
        {
            let words = window_words(spacing, batched);
            let guaranteed = words as i64 * 256 * spacing as i64;
            let factor = 1.0001f64.powf(guaranteed as f64) - 1.0;
            assert!(
                factor >= least,
                "spacing {spacing} batched={batched}: covers only {:.1}%, wanted {:.0}%",
                factor * 100.0,
                least * 100.0
            );
        }
        // An unbatched pool must not ask for hundreds of eth_calls.
        assert!(window_words(1, false) <= MAX_UNBATCHED_WORDS as i32);
    }

    /// The span a window reports must be the span its words were read from, or
    /// `crosses` would answer about prices nothing was scanned for.
    #[test]
    fn the_reported_span_matches_the_words_read() {
        let (spacing, centre) = (60, 3);
        let words = window_words(spacing, true);
        let (lo, hi) = word_span(centre, words, spacing);

        // Lowest tick of the lowest word read, highest of the highest, both
        // clamped to the range a tick can actually be in.
        let first = ((((centre - words) as i64) << 8) * spacing as i64).clamp(-MAX_TICK, MAX_TICK);
        let last = (((((centre + words) as i64) << 8) + 256) * spacing as i64)
            .clamp(-MAX_TICK, MAX_TICK);
        assert_eq!(lo, sqrt_at_tick(first as i32));
        assert_eq!(hi, sqrt_at_tick(last as i32));
        assert!(lo < hi);
    }


    #[test]
    fn compress_floors_towards_negative_infinity() {
        assert_eq!(compress(200, 60), 3);
        assert_eq!(compress(180, 60), 3);
        assert_eq!(compress(-1, 60), -1);
        assert_eq!(compress(-60, 60), -1);
        assert_eq!(compress(-61, 60), -2);
    }

    #[test]
    fn tick_and_sqrt_round_trip() {
        for t in [-887_220, -60_000, -1, 0, 1, 139_844, 887_220] {
            assert_eq!(tick_at_sqrt(sqrt_at_tick(t)), t, "tick {t}");
        }
    }

    #[test]
    fn sqrt_at_tick_matches_the_price_definition() {
        // P = 1.0001^tick, so sqrt(P) squared must return it.
        let p = sqrt_at_tick(139_844).powi(2);
        assert!((p / 1.0001f64.powi(139_844) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn scan_word_respects_direction_and_inclusivity() {
        // bits 3 and 200 set
        let w = (U256::one() << 3) | (U256::one() << 200);
        // going up from bit 3 must SKIP bit 3 itself
        assert_eq!(scan_word(w, 3, true, true), Some(200));
        assert_eq!(scan_word(w, 2, true, true), Some(3));
        // going down from bit 3 must INCLUDE bit 3
        assert_eq!(scan_word(w, 3, false, true), Some(3));
        assert_eq!(scan_word(w, 2, false, true), None);
        assert_eq!(scan_word(w, 200, false, true), Some(200));
        // continuation words are scanned end to end regardless of `bit`
        assert_eq!(scan_word(w, 0, true, false), Some(3));
        assert_eq!(scan_word(w, 0, false, false), Some(200));
        // empty word finds nothing
        assert_eq!(scan_word(U256::zero(), 128, true, true), None);
        assert_eq!(scan_word(U256::zero(), 128, false, true), None);
    }

    #[test]
    fn scan_word_handles_word_edges() {
        let w = (U256::one() << 0) | (U256::one() << 255);
        // nothing above bit 255
        assert_eq!(scan_word(w, 255, true, true), None);
        // nothing below bit 0 when bit 0 is excluded by direction
        assert_eq!(scan_word(w, 0, true, true), Some(255));
        assert_eq!(scan_word(w, 0, false, true), Some(0));
    }

    /// Real slot0 of the POOLS/ETH pool on chain 4663. Every field is
    /// independently checkable: the tick must match the price implied by
    /// sqrtPriceX96, and the fee must be a plausible tier.
    #[test]
    fn decodes_a_real_slot0_word() {
        let w: H256 = "0x0000000009c4190190022244000000000000043fc544e4e483a607c758a0c251"
            .parse()
            .unwrap();
        let (sqrt_x96, tick, protocol_fee, lp_fee) = decode_slot0(w);
        assert_eq!(
            sqrt_x96,
            U256::from_dec_str("86182064487810775758807691739729").unwrap()
        );
        assert_eq!(lp_fee, 2500, "0.25% tier");
        // protocolFee packs two 12-bit halves, 400 each way
        assert_eq!(protocol_fee & 0xfff, 400);
        assert_eq!(protocol_fee >> 12, 400);
        assert_eq!(tick, 139_844);

        // cross-check: the tick must agree with the price sqrtPriceX96 implies
        let sqrt_p = x96_to_f64(sqrt_x96);
        assert_eq!(tick_at_sqrt(sqrt_p), tick);

        // and the halves are what a swapper on THIS pool actually pays on top
        // of the 0.25% tier, which is the whole reason they are read at all
        assert_eq!(protocol_fee_halves(protocol_fee), (400, 400));
    }

    /// The combined fee has to be the chain's, digit for digit: v4's
    /// `ProtocolFeeLibrary.calculateSwapFee` is
    /// `protocolFee + lpFee - protocolFee * lpFee / 1_000_000`, truncating.
    /// Off by one pip here is off by one pip on every quote the model makes.
    #[test]
    fn the_combined_fee_is_the_protocol_librarys() {
        let s = |pf0, pf1, lp| PoolState {
            sqrt_p: 1.0,
            liquidity: 1,
            lp_fee: lp,
            protocol_fee_0for1: pf0,
            protocol_fee_1for0: pf1,
        };
        // The real POOLS/ETH numbers: 400 + 2500 - 400*2500/1e6 = 2899, and the
        // subtracted pip is exactly what naive addition would miss.
        assert_eq!(s(400, 400, 2500).swap_fee(true), 2899);
        assert_ne!(s(400, 400, 2500).swap_fee(true), 2900, "not a plain sum");
        // No protocol fee leaves the LP fee untouched - the v3 case, and the
        // v4 case before the fee controller ever sets one.
        assert_eq!(s(0, 0, 3000).swap_fee(true), 3000);
        assert_eq!(s(0, 0, 3000).swap_fee(false), 3000);
        // The halves are independent: a pool may charge one way only, and the
        // direction is what picks between them.
        let one_way = s(1000, 0, 500);
        // 1000 + 500, with the cross term truncating away at this size - the
        // same truncation the chain's integer division does.
        assert_eq!(one_way.swap_fee(true), 1_500);
        assert_eq!(one_way.swap_fee(false), 500);
    }

    /// The point of the whole exercise: the protocol's cut is money that never
    /// reaches the curve, so a quote that ignores it is optimistic.
    #[test]
    fn the_protocol_fee_costs_the_swapper_output() {
        let with = in_range_out(
            2.0,
            1_000_000_000,
            PoolState {
                sqrt_p: 2.0,
                liquidity: 1_000_000_000,
                lp_fee: 2500,
                protocol_fee_0for1: 400,
                protocol_fee_1for0: 400,
            }
            .swap_fee(true),
            true,
            1_000_000.0,
        )
        .unwrap()
        .0;
        let without = in_range_out(2.0, 1_000_000_000, 2500, true, 1_000_000.0)
            .unwrap()
            .0;
        assert!(
            with < without,
            "charging the protocol fee must return less, got {with} >= {without}"
        );
    }

    #[test]
    fn decodes_negative_ticks() {
        // tick = -1 is 0xffffff in the int24 field
        let mut b = [0u8; 32];
        b[9] = 0xff;
        b[10] = 0xff;
        b[11] = 0xff;
        let (_, tick, _, _) = decode_slot0(H256::from(b));
        assert_eq!(tick, -1);
    }

    #[test]
    fn signed_word_sign_extends() {
        assert_eq!(signed_word(1)[31], 1);
        assert!(signed_word(1)[..31].iter().all(|b| *b == 0));
        assert!(signed_word(-1).iter().all(|b| *b == 0xff));
        assert_eq!(signed_word(-2)[31], 0xfe);
    }
}
