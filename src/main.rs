mod cache;
mod config;
mod curve;
mod exit;
mod journal;
mod launch;
mod operators;
mod pool;
mod units;
mod rpc;
mod snipe;
mod swap;
mod wrapper;

use anyhow::Context;
use ethers::providers::Middleware;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // Flags that take a value, so their value is not mistaken for the config
    // path: `--quote "test buy CAMELTOE"` must not try to open the route name.
    const VALUE_FLAGS: [&str; 5] = [
        "--config",
        "--launchpad",
        "--size",
        "--slippage-bps",
        "--lead-ms",
    ];
    let consumed: std::collections::HashSet<usize> = args
        .iter()
        .enumerate()
        .filter(|(_, a)| VALUE_FLAGS.contains(&a.as_str()))
        .map(|(i, _)| i + 1)
        .collect();
    let flag_value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .filter(|v| !v.starts_with("--"))
            .cloned()
    };
    // Nothing is sent without it. Everything else runs the same way, minus
    // the money.
    let execute = args.iter().any(|a| a == "--execute");
    let watch_launches = args.iter().any(|a| a == "--watch-launches");
    // Every entry the console used to print is a record in the journal - the
    // entry is `launch`, a step is `decision`, a close is `exit` - so by
    // default the console says what no record can: how the whole run is
    // doing. This puts the entries back for a person watching one launch.
    let verbose = args.iter().any(|a| a == "--verbose");
    // Which launchpads to hear from: comma-separated addresses, or "any" to
    // watch the signature wherever it is emitted.
    let launchpads = flag_value("--launchpad");
    // What a launch would be bought with, in the pair token's own units, and
    // how much of the price to give away. Both override the config.
    let snipe_size = flag_value("--size");
    // The ENTRY allowance only. The exit has its own and always has had a
    // different job; see `[snipe] exit_slippage_bps`.
    let slippage_bps = flag_value("--slippage-bps");
    // How long before a step opens the decision is wanted, in milliseconds.
    let lead_ms = flag_value("--lead-ms");
    let path = flag_value("--config")
        .or_else(|| {
            args.iter()
                .enumerate()
                .find(|(i, a)| !a.starts_with("--") && !consumed.contains(i))
                .map(|(_, a)| a.clone())
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let cfg = config::Config::load(&path)?;
    // Said out loud, and absolute. `config.toml` is resolved against the
    // working directory, so running the binary by its path from somewhere else
    // reads a different file than the one being edited - and every parameter
    // that decides a trade comes out of it.
    tracing::info!(
        config = %std::fs::canonicalize(&path).unwrap_or(path.clone()).display(),
        "config loaded"
    );

    // What is already known about the pair tokens, before a launch needs it.
    // Decimals and a symbol are fixed at deployment, so a cache of them is
    // honest rather than merely convenient.
    match cache::open(std::path::Path::new(&cfg.token_cache_path)) {
        Ok(0) => tracing::info!(file = %cfg.token_cache_path, "token cache empty"),
        Ok(n) => tracing::info!(file = %cfg.token_cache_path, tokens = n, "token cache loaded"),
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "token cache unusable, ignoring it"),
    }

    // Sanity check via HTTP JSON-RPC before opening WS subscriptions.
    let http =
        ethers::providers::Provider::<ethers::providers::Http>::try_from(cfg.http_url.clone())?;
    // Says out loud which chain this is pointed at, and fails here rather than
    // at the first launch if the endpoint is not answering.
    chain_id(&http).await?;

    if watch_launches {
        return watch_launches_cmd(
            &http,
            &cfg,
            launchpads.as_deref(),
            snipe_size.as_deref(),
            slippage_bps.as_deref(),
            lead_ms.as_deref(),
            verbose,
            execute,
        )
        .await;
    }

    anyhow::bail!(
        "nothing to do: this is the launch sniper - see --watch-launches, and \
         live.toml for what it needs"
    );
}

fn spend_for(
    size_x100: u64,
    cap: Option<&str>,
    decimals: u8,
    c: &curve::Curve,
) -> ethers::types::U256 {
    let want = c.quote_reserve * ethers::types::U256::from(size_x100)
        / ethers::types::U256::from(10_000u64);
    match cap.and_then(|s| units::parse_units(s, decimals).ok()) {
        Some(ceiling) => want.min(ceiling),
        None => want,
    }
}

/// What a pair token is, asked once and cached with everything else.
///
/// Both answers are immutable and kept in `pools.json`, so this is one pair of
/// calls the first time a pair token is ever seen and nothing at all after
/// that. A token that will not answer is not a reason to drop a launch: its
/// amounts print raw instead.
async fn quote_of(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    factory: ethers::types::Address,
    token: ethers::types::Address,
) -> Option<(launch::Quote, Option<launch::PairEconomics>)> {
    // The factory knows the decimals it approved this pair token under, along
    // with the reserves a curve quoted in it opens at - three answers for one
    // call, and the two that matter for pricing are not on the token at all.
    let economics = match launch::read_pair_economics(http, factory, token).await {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::debug!(?token, err = %format!("{e:#}"), "no pair economics on the factory");
            None
        }
    };
    let decimals = match economics.map(|e| e.decimals) {
        // Zero is not an answer, it is the absence of one - and the factory
        // keeps no economics for the native pair at all, so taking its zero
        // printed every ETH amount as raw wei.
        Some(d) if d > 0 => Ok(d),
        _ => pool::decimals_of(http, token).await,
    };
    match (decimals, pool::symbol_of(http, token).await) {
        (Ok(decimals), Ok(symbol)) => Some((launch::Quote { decimals, symbol }, economics)),
        (d, sym) => {
            let err = d
                .err()
                .map(|e| format!("{e:#}"))
                .or_else(|| sym.err().map(|e| format!("{e:#}")))
                .unwrap_or_default();
            tracing::warn!(
                ?token, err = %err,
                "cannot read the pair token; printing raw amounts"
            );
            None
        }
    }
}

/// What this launch would be bought with, priced at every second of its tax
/// window - when there is a size to plan for and everything it needs is known.
///
/// Everything it needs: the size, the launch second (which only the feed
/// gives), the factory's configuration, the pair token's own reserves, and
/// what the creator taxes - which is in the calldata and nowhere else. Missing
/// any of them, the entry simply carries no plan rather than a guess.
/// The curve a launch opens with, from what the chain says rather than from
/// any relation between the numbers.
fn opening_curve(
    config: Option<&launch::LaunchConfig>,
    economics: Option<&launch::PairEconomics>,
    // The creator's cut. Unknown means unknown: read as zero it would price
    // every step better than it is, so a launch whose terms nobody could name
    // is not priced at all.
    creator_tax_bps: Option<u64>,
    // What the launch itself said it graduates at, which is the one number
    // here that is never in doubt: it is in the log.
    threshold: ethers::types::U256,
) -> Option<curve::Curve> {
    let c = config?;
    // The pair token's own terms where the factory keeps them. Where it does
    // not, the config's - but only when the launch graduates at exactly what
    // the config says, which is what makes them this launch's terms rather
    // than another pair's borrowed.
    let phantom = match economics.filter(|e| !e.phantom_quote.is_zero()) {
        Some(e) if e.graduation_threshold == threshold => e.phantom_quote,
        _ if c.graduation_threshold == threshold => c.phantom_quote,
        _ => return None,
    };
    curve::at_launch(
        c.supply,
        phantom,
        threshold,
        c.curve_fee_bps,
        creator_tax_bps?,
    )
    .ok()
}

/// What sends transactions, and everything that stops it.
///
/// Built only when `--execute` is given AND a contract is configured AND that
/// contract answers that this wallet owns it. Absent, nothing below sends
/// anything and the run is what it has been all along: a shadow.
struct Trader {
    wallet: ethers::signers::LocalWallet,
    to: std::sync::Arc<swap::Broadcaster>,
    wrapper: ethers::types::Address,
    /// Ours, not the node's. A buy is sent from a task, so the loop cannot ask
    /// what the next one is without waiting - and two buys in the same second
    /// asking the same node get the same answer and collide.
    ///
    /// TODO(safety): this assumes nothing else spends from this key. Anything
    /// that does - a second copy of this bot, a manual `cast send`, the fall
    /// bot in this same repo - takes a nonce this counter still believes is
    /// free, and every transaction after it is replaced or rejected. Either
    /// refuse to start when the pending count has moved under us, or take the
    /// key exclusively. (The fall bot is gone from this repo, but a second
    /// copy of this one is not a hypothesis.)
    nonce: u64,
    /// How many of ours are signed and not yet settled.
    ///
    /// The node's pending count is the truth about this key, but only when
    /// nothing of ours is in the air: while a transaction is pending it is
    /// counted, and while it is dropped it is not, and the same number means
    /// opposite things in the two cases. Zero here is what makes the node's
    /// answer safe to take in both directions.
    inflight: u32,
    fees: std::sync::Arc<std::sync::RwLock<(ethers::types::U256, ethers::types::U256)>>,
    gas: ethers::types::U256,
    /// Positions sent and not yet closed. Every one is money in a contract,
    /// and the exit rules were measured one position at a time.
    open: usize,
    max_open: usize,
    /// The most this run may LOSE, in the pair token: everything sent, plus
    /// gas, less everything that came back.
    ///
    /// It used to cap gross sends, and that measured the wrong thing twice
    /// over. The same 1.68 mETH going out and coming back thirty times an hour
    /// is not thirty times the risk, so the cap fired on a run that was
    /// working; and the case it exists for - something is wrong, every buy
    /// reverts - moves gas and returns nothing, which the old counter did not
    /// see at all. Now a cap of 0.05 means what a person means by it: stop
    /// after losing that much.
    max_spend: Option<ethers::types::U256>,
}

impl Trader {
    /// Why this buy must not be sent, if it must not.
    /// Whether to refuse this buy. `lost` is what the run is down so far -
    /// sent plus gas, less what came back - which only the caller can know,
    /// because it is the curves' own logs that say what came back.
    fn refuses(
        &self,
        spend: ethers::types::U256,
        native: bool,
        lost: ethers::types::U256,
    ) -> Option<String> {
        if !native {
            return Some("not a native pair, and only native is traded".to_string());
        }
        if self.open >= self.max_open {
            return Some(format!("{} positions already open", self.open));
        }
        if let Some(cap) = self.max_spend {
            // Not "already past it" but "could be past it after this": the
            // worst a buy can do is lose all of it, and a cap that only stops
            // once it has been broken is not a cap.
            if lost.saturating_add(spend) > cap {
                return Some(format!(
                    "this run is down {} and this would risk {}, against a cap of {}",
                    units::format_units(lost, 18),
                    units::format_units(spend, 18),
                    units::format_units(cap, 18)
                ));
            }
        }
        None
    }

    fn take_nonce(&mut self) -> u64 {
        let n = self.nonce;
        self.nonce += 1;
        self.inflight += 1;
        n
    }
}

/// Sign, broadcast, wait for the receipt, and say what happened.
///
/// All of it in its own task. The loop that decided this has a step of
/// somebody else's tax window to aim at within the next hundred milliseconds,
/// and a signature plus a round trip plus a receipt is none of its business.
#[allow(clippy::too_many_arguments)]
async fn fire(
    http: ethers::providers::Provider<ethers::providers::Http>,
    to: std::sync::Arc<swap::Broadcaster>,
    wallet: ethers::signers::LocalWallet,
    tx: swap::PendingTx,
    nonce: u64,
    fees: (ethers::types::U256, ethers::types::U256),
    gas: ethers::types::U256,
    curve: ethers::types::Address,
    leg: launch::Leg,
    out: tokio::sync::mpsc::Sender<launch::Heard>,
) {
    let owner = ethers::signers::Signer::address(&wallet);
    let mut cost = ethers::types::U256::zero();
    let (ok, pending, hash, why) =
        match swap::send_nowait(&to, &wallet, &tx, nonce.into(), fees, gas).await {
            Ok(hash) => {
                tracing::info!(?leg, ?curve, ?hash, nonce, "sent");
                let landed = swap::await_receipt(&http, hash, &tx.label).await;
                let ok = landed.outcome == swap::Outcome::Confirmed;
                cost = landed.cost;
                // Not knowing is its own answer. Everything treats it as "did
                // not happen", but a replacement sent against a transaction
                // that may still land is a second trade, not a retry.
                let pending = landed.outcome == swap::Outcome::Unknown;
                (ok, pending, Some(hash), format!("{:?}", landed.outcome))
            }
            // Never broadcast, so the nonce it was given was never used - which
            // the re-read below picks up.
            Err(e) => (false, false, None, format!("{e:#}")),
        };
    // Only when something went wrong. A confirmed transaction consumed exactly
    // the nonce it was given and the count we keep is already right.
    let resync_nonce = if ok {
        None
    } else {
        swap::pending_nonce(&http, owner).await.ok()
    };
    if !ok {
        tracing::warn!(?leg, ?curve, ?hash, why = %why, "did not land");
    }
    let _ = out
        .send(launch::Heard::Landed(Box::new(launch::Settled {
            curve,
            leg,
            hash: hash.unwrap_or_default(),
            ok,
            why,
            nonce,
            resync_nonce,
            pending,
            cost,
        })))
        .await;
}

/// One launch entry, for a person watching. Off unless asked for: it is the
/// same thing the journal's `launch` line already holds, and forty of them a
/// minute is how the one line that needed reading got lost.
fn print_entry(verbose: bool, r: &launch::Report) {
    if verbose {
        println!("{}", launch::render(r));
    }
}

/// The second a block carries, asked of the endpoint.
///
/// The feed gives this earlier and for free, and while it is up nothing here
/// is called at all. But the second is what the whole tax window is measured
/// from: without it no step can be aimed at, no trade can be placed in the
/// window, and the loop below decides nothing whatsoever. A run that lost the
/// feed lost the bot with it - one overnight run caught 3% of launches and
/// made not one decision - and a number that is sitting in a block we already
/// know the number of is not a number worth being inert over.
///
/// One request per launch, and only for launches the feed did not stamp.
async fn block_second(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    block: u64,
) -> Option<u64> {
    let got = rpc::retrying("eth_getBlockByNumber", || async {
        http.get_block(block)
            .await
            .map_err(anyhow::Error::from)
            .context("get_block")
    })
    .await;
    match got {
        Ok(Some(b)) => Some(b.timestamp.as_u64()),
        Ok(None) => {
            tracing::warn!(block, "the endpoint does not have this block yet");
            None
        }
        Err(e) => {
            tracing::warn!(block, err = %format!("{e:#}"), "cannot read the launch second");
            None
        }
    }
}

/// What the launch transaction asked for, or nothing.
///
/// One request per launch, and never on anything's critical path: this is a
/// report. A transaction that cannot be fetched or does not decode costs the
/// entry its extra lines and nothing else - the logs already said the parts a
/// trade would be built from.
async fn launch_call(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    tx: ethers::types::H256,
) -> Option<launch::LaunchCall> {
    let got = rpc::retrying("eth_getTransactionByHash", || async {
        http.get_transaction(tx)
            .await
            .context("eth_getTransactionByHash")
    })
    .await;
    let (input, to) = match got {
        Ok(Some(t)) => (t.input, t.to),
        Ok(None) => {
            tracing::debug!(?tx, "the launch transaction is not there yet");
            return None;
        }
        Err(e) => {
            tracing::warn!(?tx, err = %format!("{e:#}"), "cannot fetch the launch transaction");
            return None;
        }
    };
    match launch::decode_call(&input) {
        Ok(c) => Some(c),
        Err(e) => {
            // Loud, because this is a launch that happened through something
            // this does not know - a wrapper, a router, a new entry point - and
            // the selector below is what says which. Silence here is the tool
            // quietly reporting less than it saw.
            //
            // Once per selector, though. The unknown entry point is the news;
            // the hundredth launch through it is not, and on a busy chain that
            // one line becomes most of the log.
            static SAID: std::sync::OnceLock<
                std::sync::Mutex<std::collections::HashSet<[u8; 4]>>,
            > = std::sync::OnceLock::new();
            let selector: [u8; 4] = input.get(..4).unwrap_or_default().try_into().unwrap_or([0; 4]);
            let first = SAID
                .get_or_init(Default::default)
                .lock()
                .map(|mut s| s.insert(selector))
                .unwrap_or(true);
            if !first {
                return None;
            }
            tracing::warn!(
                ?tx, ?to,
                selector = %format!("0x{}", hex::encode(input.get(..4).unwrap_or_default())),
                err = %format!("{e:#}"),
                "a launch arrived through calldata this does not decode"
            );
            None
        }
    }
}

/// The chain's second right now, from the last turnover we were shown.
///
/// Used for the deadline on a transaction, which is why a missing anchor
/// answers zero rather than guessing: a deadline built on a guess either
/// refuses a good transaction or lets a stale one through, and both are worse
/// than the wrapper refusing an obviously expired one outright.
fn chain_second(
    anchor: &std::sync::RwLock<Option<(u64, std::time::Instant)>>,
) -> u64 {
    anchor
        .read()
        .ok()
        .and_then(|a| *a)
        .map(|(s, at)| s + at.elapsed().as_secs())
        .unwrap_or(0)
}

/// The fees to sign with: the tip as last suggested, the base fee as of the
/// newest block we have seen.
///
/// The cached pair is refreshed on a twenty-second timer, and the headroom in
/// it - twice the base fee - was reasoned about as "roughly six blocks of
/// continuous growth". That reasoning is for a chain with twelve-second
/// blocks. **This one runs 9.8 blocks a second**, so six blocks is 0.6
/// seconds and the timer is 196 blocks wide. A base fee that climbs while the
/// timer sleeps leaves every transaction signed in the meantime with a
/// `max_fee` below it, and a transaction below the base fee is not slow, it is
/// unmineable - on exactly the launches busy enough to be worth buying.
///
/// The header stream already carries the base fee of every block and is
/// already subscribed to, so the current one costs nothing. The tip stays on
/// the timer: it is a suggestion rather than a consensus rule, and nothing is
/// stranded by a stale one.
///
/// Falls back to the cached pair entirely until the first header arrives.
fn fees_now(
    cached: (ethers::types::U256, ethers::types::U256),
    base_fee: &std::sync::RwLock<ethers::types::U256>,
) -> (ethers::types::U256, ethers::types::U256) {
    let (max_fee, tip) = cached;
    let base = base_fee.read().map(|b| *b).unwrap_or_default();
    if base.is_zero() {
        return (max_fee, tip);
    }
    // The same shape the cached figure has, against a base fee one block old
    // instead of up to two hundred.
    (base * ethers::types::U256::from(2u64) + tip, tip)
}

/// What to set the nonce counter to after a transaction did not land, if
/// anything.
///
/// A transaction that did not land leaves a gap, and every nonce after it
/// waits behind that gap forever - so the node is asked rather than assumed.
///
/// **Backwards is the case that matters**, and the one the rule here used to
/// refuse. A dropped transaction makes the node's pending count LOWER than our
/// counter, never higher: we counted a nonce as used and the chain never saw
/// it. A forward-only rule therefore never fired on the failure it was written
/// for, and a single drop stopped the wallet for the rest of the run.
///
/// Going back is safe exactly when nothing of ours is in the air and this one
/// is known not to be - then the node's count describes a key nobody here is
/// racing. While anything is pending, only forward: reusing a nonce that may
/// still land is a second trade wearing the first one's number, and on a buy
/// that is the double-buy the whole position machinery exists to prevent.
fn resync_to(ours: u64, node: u64, inflight: u32, pending: bool) -> Option<u64> {
    if node > ours {
        // Somebody else spent from this key, or a transaction we gave up on
        // landed after all. Either way the chain has moved past us.
        return Some(node);
    }
    if node < ours && inflight == 0 && !pending {
        return Some(node);
    }
    None
}

/// Whether money of ours is still in a curve.
///
/// `position` alone does not answer it. It reads `Bought` from the moment the
/// buy decision is made, wallet or no wallet - every launch that passes the
/// filters has one, and without `--execute` not one of them ever had a
/// transaction behind it. What says money left is `sent`, and what says it
/// came back is `Sold`, written only when a sale of ours lands.
///
/// A buy still in flight counts as held. The receipt has not arrived, so the
/// position may well exist on chain, and forgetting the launch now means never
/// learning that it does - the receipt is delivered to the launch it belongs
/// to and nowhere else.
///
/// This is what keeps a followed launch alive past its minute, so it must not
/// answer yes to a position that is only on paper: every shadow would then be
/// followed forever.
fn holding(sent: ethers::types::U256, position: &snipe::Position) -> bool {
    !sent.is_zero()
        && matches!(
            position,
            snipe::Position::InFlight { .. } | snipe::Position::Bought { .. }
        )
}

/// What the exit rules have returned so far.
///
/// One place, because closing a position is one event that now happens from
/// two: a trade that moves the price, and a tick that moves only the clock.
/// Eight counters bumped by hand in two arms of the same loop is how the two
/// paths come to disagree about the same run.
#[derive(Default)]
struct Tally {
    /// Positions opened and closed in the last window.
    open: u64,
    closed: u64,
    /// Buys the chain refused. Not counted as outcomes - no trade happened,
    /// and calling that break-even would dilute the average with a number
    /// nobody earned - but counted somewhere, because the shadow beside them
    /// goes on reporting what the rules WOULD have made. A run where every buy
    /// reverts otherwise looks exactly like a healthy one.
    failed: u64,
    all_failed: u64,
    x100: u64,
    wins: u64,
    /// The same, for the launches the filters would actually have bought. Two
    /// numbers rather than one, because the whole flow and the traded subset
    /// are different questions and one process answers both.
    kept_closed: u64,
    kept_x100: u64,
    /// The same, never reset. Thirty seconds holds a handful of closes and a
    /// handful says nothing; the run as a whole is the number worth reading,
    /// and it is the one that cannot be recovered from a window that scrolled
    /// past an hour ago.
    all_closed: u64,
    all_x100: u64,
    all_wins: u64,
}

impl Tally {
    fn opened(&mut self) {
        self.open += 1;
    }

    /// The chain refused a buy we sent.
    fn refused(&mut self) {
        self.failed += 1;
        self.all_failed += 1;
    }

    /// One position closed at `x100` hundredths of what it cost. `kept` says
    /// the entry filters would have bought this one.
    fn close(&mut self, x100: u64, kept: bool) {
        self.closed += 1;
        self.x100 += x100;
        self.all_closed += 1;
        self.all_x100 += x100;
        if kept {
            self.kept_closed += 1;
            self.kept_x100 += x100;
        }
        if x100 > 100 {
            self.wins += 1;
            self.all_wins += 1;
        }
    }

    /// A new reporting window. The run's own totals are not touched.
    fn window(&mut self) {
        self.open = 0;
        self.closed = 0;
        self.failed = 0;
        self.x100 = 0;
        self.wins = 0;
        self.kept_closed = 0;
        self.kept_x100 = 0;
    }

    /// The average close of `n` of them, as a multiple, for a log line.
    fn average(x100: u64, n: u64) -> String {
        match n {
            0 => "-".to_string(),
            n => format!("{}.{:02}x", x100 / n / 100, x100 / n % 100),
        }
    }
}

/// Watch launchpads and say what launched. Nothing else.
///
/// Without `--execute` it reads no wallet and cannot send anything: the same
/// watching, the same journals, the same shadow positions, and no money.
///
/// One socket, reconnected on the same backoff as the pool feed - an endpoint
/// that has just dropped everyone is not helped by being dialled in a tight
/// loop, and a launch missed while backing off is a launch that was already
/// missed.
///
/// Three things are asked of the chain, and only one of them per launch:
///
/// * the snipe tax, twice at startup and never again - it is kept current from
///   the factory's own settings events on the same subscription;
/// * the pair token's decimals and symbol, once per token ever seen and cached
///   with everything else. Without them the amounts in these logs are
///   unreadable: USDG launches quote a graduation of "8090", and a six-decimal
///   token printed at eighteen says 0.00000000809;
/// * the launch transaction, once per launch, for what the calldata says and
///   no log does - the name, the creator's tax, and who was exempted from the
///   snipe tax.
#[allow(clippy::too_many_arguments)]
async fn watch_launches_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    pads: Option<&str>,
    size: Option<&str>,
    slippage_bps: Option<&str>,
    lead_ms: Option<&str>,
    // Print every launch entry, every decision and every close. Off by
    // default: all three are already records in the journal, and forty of them
    // a minute is how the one line that needed reading got lost.
    verbose: bool,
    // Actually send. Everything works the same without it, minus the money.
    sending: bool,
) -> anyhow::Result<()> {
    // How long before a step opens a decision is wanted.
    //
    // The transaction still has to be signed and sent after it: submission is
    // ~50ms on a warm connection and a block is ~103ms, so a decision handed
    // over at the boundary itself lands a block or two into the step. A
    // hundred milliseconds ahead puts the send just before it.
    //
    // Aiming early is the safe direction to be wrong in, and only because the
    // minimum makes it so: a fill one block early is a fill at the previous,
    // dearer step, and that minimum refuses it. The transaction reverts and
    // costs gas rather than buying at six percent when two tenths were meant.
    let lead = std::time::Duration::from_millis(match lead_ms {
        Some(v) => v.parse().context("--lead-ms")?,
        None => cfg.snipe.lead_ms,
    });
    // The flags override the config, and the config is where this normally
    // lives. Reading only the flag meant a size set in `[snipe]` did nothing -
    // and with no size nothing is followed, so the journal held launches with
    // no trades under them.
    // The size is a share of the curve, and the config holds it. `--size` is
    // now only the ceiling on that share, in the pair token's own units.
    let size_x100 = cfg.snipe.size_x100;
    let cap: Option<String> = size
        .map(str::to_string)
        .or_else(|| Some(cfg.snipe.size.trim().to_string()).filter(|s| !s.is_empty()));
    let cap = cap.as_deref();
    let size = (size_x100 > 0).then_some("on");
    let slippage_bps: u64 = match slippage_bps {
        Some(v) => v.parse().context("--slippage-bps")?,
        None => cfg.snipe.slippage_bps,
    };
    anyhow::ensure!(slippage_bps < 10_000, "--slippage-bps is the whole trade");
    // The launchpad's settings, read once. Not fatal: without them every launch
    // still prints, one line shorter.
    let factory: ethers::types::Address = launch::PONS_V2_FACTORY
        .parse()
        .context("the built-in factory address")?;
    let mut tax = match launch::read_snipe_tax(http, factory).await {
        Ok(t) => {
            // The schedule rather than the two settings: what a buy pays is a
            // step per whole second, and those steps are the entire decision
            // about when to buy. Said once, because it is one setting for the
            // whole launchpad - and said again the moment it changes.
            tracing::info!(schedule = %launch::snipe_tax_line(&t), "snipe tax");
            Some(t)
        }
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "cannot read the snipe tax");
            None
        }
    };

    let pads: Vec<ethers::types::Address> = match pads.map(str::trim) {
        // Deliberate and spelled out: an event signature belongs to nobody, so
        // this trusts whoever emits it.
        Some("any") => {
            tracing::warn!(
                "watching every contract that emits these events; anyone can emit them, so \
                 treat what comes back as a claim rather than a launch"
            );
            Vec::new()
        }
        Some(list) => list
            .split(',')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(|a| a.parse().with_context(|| format!("launchpad \"{a}\"")))
            .collect::<anyhow::Result<_>>()?,
        None => launch::KNOWN_PADS
            .iter()
            .map(|a| a.parse().context("a built-in launchpad address"))
            .collect::<anyhow::Result<_>>()?,
    };

    // The launch configuration, read rather than inferred. Everything the
    // local pricing in `curve.rs` does rests on these three numbers, and until
    // now they came from watching launches agree with a guess.
    let mut config: Option<launch::LaunchConfig> = None;
    match launch::read_launch_config(http, factory, 0).await {
        Ok(c) => {
            config = Some(c);
            tracing::info!(
                supply = %units::format_units(c.supply, 18),
                curve_fee_bps = c.curve_fee_bps,
                phantom_quote = %c.phantom_quote,
                graduation_threshold = %c.graduation_threshold,
                pool_fee = c.pool_fee,
                tick_spacing = c.tick_spacing,
                enabled = c.enabled,
                "launch config #0"
            );
            // The relation the local pricing was built on, checked against the
            // value itself: phantom = two fifths of the threshold. A config
            // whose own threshold is zero says nothing either way - the pair
            // token's economics override both, and that is a separate read.
            if !c.graduation_threshold.is_zero() {
                let expected = c.graduation_threshold * ethers::types::U256::from(2u64)
                    / ethers::types::U256::from(5u64);
                if c.phantom_quote == expected {
                    tracing::info!("phantom quote is two fifths of the threshold, as assumed");
                } else {
                    tracing::warn!(
                        phantom = %c.phantom_quote, expected = %expected,
                        "the phantom quote is NOT two fifths of the threshold; local pricing \
                         built on that ratio is wrong for this config"
                    );
                }
            }
            if !c.enabled {
                tracing::warn!("launch config #0 is disabled; launches are using another");
            }
        }
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "cannot read launch config #0"),
    }
    match launch::read_launch_config_count(http, factory).await {
        Ok(1) => {}
        Ok(n) => tracing::warn!(
            configs = n,
            "the factory holds more than one launch config; a launch names its own by id, \
             and only #0 has been checked"
        ),
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "cannot read the launch config count"),
    }

    // What second each block carries, filled in by the feed. Without it the
    // second a trade fell in has to be guessed from block numbers, and a guess
    // that says "+1s" next to a tax of 19 bps is a guess that contradicts the
    // chain in the same line.
    let block_seconds: std::sync::Arc<std::sync::RwLock<std::collections::BTreeMap<u64, u64>>> =
        Default::default();

    // When the chain's second last turned over, in our own clock. Everything
    // about aiming at a step depends on this one pair of numbers.
    let second_anchor: std::sync::Arc<std::sync::RwLock<Option<(u64, std::time::Instant)>>> =
        Default::default();

    // The newest block's base fee, kept current by the header watcher below.
    // Zero until the first header, which is what makes the cached figure the
    // fallback rather than this.
    let base_fee: std::sync::Arc<std::sync::RwLock<ethers::types::U256>> = Default::default();

    let (tx, mut launches) = tokio::sync::mpsc::channel(64);
    let ws = cfg.ws_url.clone();
    let watching = tokio::spawn({
        let tx = tx.clone();
        let pads = pads.clone();
        async move {
            let mut backoff = std::time::Duration::from_secs(3);
            loop {
                let started = std::time::Instant::now();
                match launch::watch(&ws, &pads, tx.clone()).await {
                    Ok(()) => tracing::warn!("launch feed closed, reconnecting"),
                    Err(e) => {
                        tracing::error!(err = %format!("{e:#}"), "launch feed error, reconnecting")
                    }
                }
                if started.elapsed() >= std::time::Duration::from_secs(60) {
                    backoff = std::time::Duration::from_secs(3);
                }
                tracing::info!(delay_s = backoff.as_secs(), "backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
            }
        }
    });

    // The chain's clock, from its own headers. Not an optimisation: without an
    // anchor the tick that decides things gives up on its first line, and
    // until now the only thing that set one was the sequencer feed - so a run
    // where the feed faltered decided nothing at all and said nothing about
    // it.
    let heading = tokio::spawn({
        let ws = cfg.ws_url.clone();
        let anchor = second_anchor.clone();
        let seconds = block_seconds.clone();
        let base = base_fee.clone();
        async move {
            let mut backoff = std::time::Duration::from_secs(3);
            loop {
                let started = std::time::Instant::now();
                match launch::watch_heads(&ws, anchor.clone(), seconds.clone(), base.clone()).await
                {
                    Ok(()) => tracing::warn!(
                        lived_s = started.elapsed().as_secs(),
                        "block headers ended, reconnecting"
                    ),
                    Err(e) => tracing::error!(
                        lived_s = started.elapsed().as_secs(),
                        err = %format!("{e:#}"),
                        "block headers failed, reconnecting"
                    ),
                }
                if started.elapsed() >= std::time::Duration::from_secs(60) {
                    backoff = std::time::Duration::from_secs(3);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
            }
        }
    });

    // The sequencer's own feed, when there is one. A second source for the same
    // launches, heard earlier: it carries signed transactions rather than logs,
    // so it says a launch is coming before the block that carries it exists.
    // Same channel, same reconnect shape, and entirely optional - without FEED
    // everything below works exactly as it did.
    let feeding = match std::env::var("FEED")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        Some(url) => {
            let tx = tx.clone();
            let pads = pads.clone();
            let seconds = block_seconds.clone();
            let anchor = second_anchor.clone();
            Some(tokio::spawn(async move {
                let mut backoff = std::time::Duration::from_secs(3);
                loop {
                    let started = std::time::Instant::now();
                    match launch::watch_feed(
                        &url,
                        &pads,
                        anchor.clone(),
                        seconds.clone(),
                        tx.clone(),
                    )
                    .await
                    {
                        Ok(()) => tracing::warn!(
                            lived_s = started.elapsed().as_secs(),
                            "feed closed, reconnecting"
                        ),
                        Err(e) => tracing::error!(
                            lived_s = started.elapsed().as_secs(),
                            err = %format!("{e:#}"),
                            "feed error, reconnecting"
                        ),
                    }
                    if started.elapsed() >= std::time::Duration::from_secs(60) {
                        backoff = std::time::Duration::from_secs(3);
                    }
                    tracing::info!(delay_s = backoff.as_secs(), "backing off");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
                }
            }))
        }
        None => {
            tracing::info!("no FEED set; launches are heard from logs only");
            None
        }
    };

    // Curve trades, when there is a size to follow them for. Without one there
    // is nothing to price against them, and the subscription would be a
    // stream of somebody else's business.
    // Which curves are worth hearing about, shared with the subscription so
    // that everything else is dropped before it reaches this loop.
    let watched_curves: std::sync::Arc<
        std::sync::RwLock<std::collections::HashSet<ethers::types::Address>>,
    > = Default::default();
    let following = match size {
        Some(_) => {
            let tx = tx.clone();
            let ws = cfg.ws_url.clone();
            let watched = watched_curves.clone();
            Some(tokio::spawn(async move {
                let mut backoff = std::time::Duration::from_secs(3);
                loop {
                    let started = std::time::Instant::now();
                    match launch::watch_curves(&ws, watched.clone(), tx.clone()).await {
                        Ok(()) => tracing::warn!("curve feed closed, reconnecting"),
                        Err(e) => tracing::error!(
                            err = %format!("{e:#}"), "curve feed error, reconnecting"
                        ),
                    }
                    if started.elapsed() >= std::time::Duration::from_secs(60) {
                        backoff = std::time::Duration::from_secs(3);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
                }
            }))
        }
        None => None,
    };

    // Curves being followed, and the reserves they are at. Kept for as long as
    // a launch is still interesting: the tax window is three seconds and the
    // flow that decides a price is the first few after it.
    // A launch is decided in three seconds and its shape is clear inside a
    // minute: the exempt wallets are in and out, and what is left is ordinary
    // trading.
    const FOLLOW_FOR: std::time::Duration = std::time::Duration::from_secs(60);
    // One file per launch, kept for reading later. None of this can be
    // recovered afterwards without asking the chain for every log again.
    let journal_dir = std::path::PathBuf::from("launches");
    struct Followed {
        curve: curve::Curve,
        journal: std::path::PathBuf,
        /// Everyone the launch declared exempt from the snipe tax: the list in
        /// the calldata, plus the deployer and the creator fee recipient, whom
        /// the factory exempts whether or not they were named. Empty when the
        /// calldata was never decoded, and the journal then says nothing about
        /// exemption rather than saying "no".
        exempt: std::collections::HashSet<ethers::types::Address>,
        exempt_known: bool,
        quote_decimals: u8,
        /// The curve as it opened, kept only so the journal can record how far
        /// a plan made at launch had drifted by the time a step opened.
        opening: Option<curve::Curve>,
        /// What this launch is, for deciding about it.
        facts: snipe::Facts,
        /// Where we stand with it.
        position: snipe::Position,
        /// Steps already decided about. A step is asked once: asking twice is
        /// how a launch gets bought twice.
        decided: std::collections::HashSet<u64>,
        /// Steps of the tax window whose opening block has been written out
        /// already. The block is what a buy aims at, and it is only knowable
        /// once the feed has carried the blocks around the boundary.
        windows_written: std::collections::HashSet<u64>,
        /// The second the launch landed in, as the feed stamped it. Without
        /// it - no feed, or a launch heard only from its log - a trade's
        /// place in the tax window is not known and is not guessed at.
        launched_at: Option<u64>,
        until: std::time::Instant,
        /// What happened while it was followed, for the one line that stands
        /// in for the trades themselves. The high water mark and not the last
        /// reserve, because what a position was worth at its best is the half
        /// of the minute an exit rule is judged against.
        buys: u32,
        sells: u32,
        peak_quote: ethers::types::U256,
        /// The last block we saw a trade on this curve in, so a position can
        /// be aged in the chain's time rather than in ours.
        last_block: u64,
        /// Which trades have been applied, by their place in the chain. The
        /// backfill asks for logs the subscription may already have carried,
        /// and applying a trade twice moves the reserves twice.
        seen: std::collections::HashSet<(u64, u64)>,
        /// Whether the model has already been caught disagreeing here. Once
        /// the reserves have parted from the chain's they stay parted, so
        /// every later fill disagrees too and saying so again says nothing.
        model_off: bool,
        /// What was actually sent for this position, when something was.
        sent: ethers::types::U256,
        /// Its closing line is already in the journal. A launch can now leave
        /// by more than one door - its minute, graduation, or a position that
        /// finally sold - and two of them writing `done` is a file that says
        /// the launch ended twice.
        finished: bool,
        /// The reserves stopped being the curve's and nothing may be priced
        /// from them. Set only for a launch that is still holding: anything
        /// else is dropped outright, which is what this used to do to both.
        lost: bool,
        /// What the chain actually filled our buy at, once its log has come
        /// back. `None` until then, and `None` forever without `--execute`.
        ///
        /// Everything that decides money prefers this to the model: the model
        /// is what a threshold was measured on, and this is what the wallet
        /// holds. The two are journalled side by side so the difference is a
        /// running measurement rather than an assumption.
        filled: Option<ethers::types::U256>,
        /// What the buy we sent was priced to fill at. Kept beside `sent`
        /// rather than read off the shadow when the receipt lands, because the
        /// exit can fire on a trade while that buy is still in the air - and
        /// then the shadow is gone by the time there is a position to write it
        /// into, which left the position recording nothing and no price to
        /// sell it at.
        expected_tokens: ethers::types::U256,
        /// A sale is in flight, so the exit rules do not send another.
        selling: bool,
        /// Why the filters would not buy this one, when they would not. The
        /// launch is followed and journalled either way; this decides only
        /// whether money goes into it.
        refused: Option<String>,
        /// The exit fired and we mean to be out. Kept because a sale that does
        /// not land has to be tried again: the reason for leaving does not
        /// stop being true because a transaction was dropped.
        leaving: bool,
        /// The earliest the next sale may be sent, and how many have gone
        /// already. A retry driven by the tick would otherwise go out fifty
        /// times a second at whatever the endpoint charges for a revert, and
        /// a sale reported as still pending is one that may yet land - so the
        /// gap is wide enough for the first answer to arrive before a second
        /// is asked.
        sell_next: std::time::Instant,
        sell_tries: u32,
        /// The pair token. Execution refuses anything but the native one, and
        /// a symbol is not proof: this is the address the curve named.
        pair_token: ethers::types::Address,
        /// Whoever is behind this launch, for filing its outcome under.
        operator: operators::OpId,
        /// Distinct wallets that bought and were NOT exempt. A launch that
        /// never gets one is a launch nobody outside the bundle wanted, and
        /// 15% of them end that way.
        outsiders: std::collections::HashSet<ethers::types::Address>,
        /// What the buy decision would have bought, followed to its exit.
        ///
        /// Nothing was sent, so this is not a position - it is the record of
        /// what one would have done, kept because the exit rules cannot be
        /// judged any other way until there is a wallet behind this. It is
        /// also the only honest input to an operator's history: the curve's
        /// own peak correlates with our outcome at 0.62, and a shadow
        /// position correlates with it at 1.
        shadow: Option<exit::Held>,
    }

    impl Followed {
        /// Whether money of ours is still in this curve. See [`holding`].
        fn holding(&self) -> bool {
            holding(self.sent, &self.position)
        }
    }

    /// Our own leg of a curve's trade log, when this one is ours.
    ///
    /// The receipt says a transaction landed and nothing about what it did.
    /// The curve's log says exactly what it did, to the wei, and it is already
    /// being subscribed to for every launch - so the real fill costs nothing
    /// but recognising the wrapper in a field that is already decoded.
    ///
    /// On a buy this replaces the modelled fill everywhere it matters: the
    /// position, the shadow the exit rules read, and through them the `min_out`
    /// the sale is sent with - which until now rested on a number the chain had
    /// never confirmed. On a sale it is the realised result, the first figure
    /// in this program that is not a model of anything.
    fn ours(
        f: &mut Followed,
        trade: &curve::Trade,
        wrapper: ethers::types::Address,
        block: u64,
        realized: &mut (ethers::types::U256, ethers::types::U256),
    ) {
        match trade {
            curve::Trade::Buy {
                recipient,
                quote_in,
                tokens_out,
                fee,
                creator_tax,
            } if *recipient == wrapper => {
                if let Err(e) = journal::append(
                    &f.journal,
                    &journal::fill_line(
                        "buy",
                        block,
                        *tokens_out,
                        *quote_in,
                        *fee,
                        *creator_tax,
                        Some((f.expected_tokens, f.sent)),
                        f.quote_decimals,
                    ),
                ) {
                    tracing::warn!(err = %format!("{e:#}"), "cannot write the fill");
                }
                // What the wallet actually parted with, and not merely what
                // it sent. A buy past the allocation is clamped: the curve
                // keeps what it needs, and the rest comes back through the
                // wrapper to the owner. `quote_in` is the kept half - the
                // curve reports the refund in a separate event, and
                // `Curve::apply` adds `quote_in` less fees straight to the
                // reserve, which reconciles to the wei against every recorded
                // reserve in the journals. A gross figure there would not.
                // So `sent` minus this is the refund, and this is the outlay.
                realized.0 += *quote_in;
                f.filled = Some(*tokens_out);
                // The position and the rules that will sell it, corrected to
                // what is actually held. The high is re-marked rather than
                // carried: it was measured against a token count that has just
                // been found wrong, and a trailing stop compared with a peak
                // computed from the wrong quantity is not a stop.
                if let snipe::Position::Bought { step, spend, .. } = f.position {
                    f.position = snipe::Position::Bought {
                        step,
                        spend,
                        tokens: *tokens_out,
                    };
                }
                if let Some(h) = f.shadow.as_mut() {
                    h.tokens = *tokens_out;
                    h.cost = *quote_in;
                    h.high = exit::worth(&f.curve, *tokens_out);
                }
            }
            curve::Trade::Sell {
                seller,
                tokens_in,
                quote_out,
                fee,
                creator_tax,
                ..
            } if *seller == wrapper => {
                if let Err(e) = journal::append(
                    &f.journal,
                    &journal::fill_line(
                        "sell",
                        block,
                        *tokens_in,
                        *quote_out,
                        *fee,
                        *creator_tax,
                        // Nothing to compare a sale against: it is sent for
                        // the whole balance rather than for a priced quantity.
                        None,
                        f.quote_decimals,
                    ),
                ) {
                    tracing::warn!(err = %format!("{e:#}"), "cannot write the fill");
                }
                realized.1 += *quote_out;
                tracing::info!(
                    curve = ?f.facts.curve,
                    out = %launch::amount_of(*quote_out, f.quote_decimals),
                    r#in = %launch::amount_of(f.sent, f.quote_decimals),
                    symbol = %f.facts.quote_symbol,
                    "position closed on chain"
                );
            }
            _ => {}
        }
    }

    /// The exit, asked and answered. Returns whether a sale should now be sent.
    ///
    /// Called from a trade on the curve and from the tick, because the rules
    /// do not share a trigger: the target and the stop are about a price,
    /// which only a trade moves, and `hold_blocks` is about the block, which
    /// moves whether or not anybody trades. Asked only on a trade, the rule
    /// written for a curve that goes quiet was the one rule a quiet curve
    /// could never reach - and the position went unsold until the launch was
    /// dropped out from under it.
    fn ask_exit(
        f: &mut Followed,
        block: u64,
        p: &exit::Policy,
        verbose: bool,
        tally: &mut Tally,
        ops: &mut operators::Operators,
        ops_dirty: &mut bool,
    ) {
        if f.lost {
            return;
        }
        let Some(h) = f.shadow.as_mut() else { return };
        // The high is marked BEFORE the question is asked, or the stop
        // measures a give-back from a peak it has not seen yet.
        h.mark(exit::worth(&f.curve, h.tokens));
        let decision = exit::decide(h, &f.curve, block, p);
        let exit::Exit::Sell { worth, why, .. } = &decision else {
            return;
        };
        if verbose {
            println!(
                "{}",
                exit::render(h, &decision, f.quote_decimals, &f.facts.quote_symbol)
            );
        }
        close_shadow(f, *worth, why, block, tally, ops, ops_dirty);
        f.leaving = true;
    }

    /// One position closed: recorded, counted, and filed under its operator.
    ///
    /// Shared because a position now stops being one for two reasons - a rule
    /// fired, or the curve graduated out from under it - and a launch counted
    /// in one place and not the other is a statistic that quietly excludes its
    /// own best outcomes.
    #[allow(clippy::too_many_arguments)]
    fn close_shadow(
        f: &mut Followed,
        worth: ethers::types::U256,
        why: &str,
        block: u64,
        tally: &mut Tally,
        ops: &mut operators::Operators,
        ops_dirty: &mut bool,
    ) {
        let Some(h) = f.shadow.take() else { return };
        if let Err(e) = journal::append(&f.journal, &journal::exit_line(&h, worth, why, block)) {
            tracing::warn!(err = %format!("{e:#}"), "cannot write the exit");
        }
        tally.close(h.x100(worth), f.refused.is_none());
        ops.record(f.operator, h.x100(worth));
        *ops_dirty = true;
    }

    /// How many reverted sales are chased at once before the gap applies.
    /// A price that moved under the floor is retried immediately, because the
    /// next quote is the one that fills; anything still failing after this is
    /// not about the price.
    const CHASE_TRIES: u32 = 4;

    /// Out, and keep trying until we are.
    ///
    /// The reason for leaving does not stop being true because a transaction
    /// was dropped, so the retry hangs on `leaving` rather than on another
    /// decision - the shadow that raised it has already closed and will not
    /// answer again. Also called from both the trade and the tick: a sale
    /// that reverted on a curve nobody trades again would otherwise never get
    /// its second attempt.
    #[allow(clippy::too_many_arguments)]
    fn send_exit(
        f: &mut Followed,
        at: ethers::types::Address,
        trader: Option<&mut Trader>,
        http: &ethers::providers::Provider<ethers::providers::Http>,
        tx: &tokio::sync::mpsc::Sender<launch::Heard>,
        base_fee: &std::sync::RwLock<ethers::types::U256>,
        second: u64,
        // The EXIT allowance. Recomputed here rather than carried from the
        // `Exit::Sell` that ordered this, because a retry is signed blocks
        // after the stop fired and the floor has to be about the price now.
        exit_slippage_bps: u64,
    ) {
        /// Wide enough that a sale reported as still pending has landed or
        /// been dropped before the next one is signed.
        const SELL_GAP: std::time::Duration = std::time::Duration::from_secs(5);
        /// After this many the chain is refusing for a reason no retry will
        /// change, and the tokens need a person: `rescue` on the wrapper.
        const SELL_TRIES: u32 = 12;

        if !f.leaving || f.selling || !f.holding() || f.lost {
            return;
        }
        let Some(t) = trader else { return };
        let snipe::Position::Bought { tokens, .. } = f.position else {
            // In flight. The receipt settles it, and until it does there is
            // nothing to sell and no figure to sell it at.
            return;
        };
        if tokens.is_zero() || std::time::Instant::now() < f.sell_next {
            return;
        }
        if f.sell_tries >= SELL_TRIES {
            // Once, not once a tick: the counter is pushed one past the cap so
            // this arm is never taken again. The launch is still followed and
            // still counts as held, which is the point - it holds `max_open`
            // down and keeps the line above saying so, rather than quietly
            // forgetting the money.
            if f.sell_tries == SELL_TRIES {
                f.sell_tries += 1;
                tracing::error!(
                    curve = ?at, tries = SELL_TRIES,
                    "cannot sell this position; it is still in the wrapper and \
                     needs rescue(token, owner) by hand"
                );
            }
            return;
        }
        f.sell_tries += 1;
        // The sale goes out for the whole balance, so `tokens` sets only the
        // price we refuse to go below. Once the buy's own log has come back
        // that is the real holding; before then it is what the model said the
        // fill would be, and a `min_out` built on a fill the chain never
        // confirmed can sit above what the sale can actually return - which
        // reverts, and reverts again on every retry for the same reason.
        //
        // Not worth waiting for: an exit that waits for a log is an exit a
        // block later, and a block is worth more here than the difference
        // usually is. Said out loud instead, because a run where this appears
        // next to a stuck position has its explanation in one line.
        if f.filled.is_none() {
            tracing::debug!(
                curve = ?at,
                "selling before the buy's own log came back; the floor is the \
                 model's fill and not the wallet's"
            );
        }
        let worth = exit::worth(&f.curve, tokens);
        let min_out = worth * ethers::types::U256::from(10_000 - exit_slippage_bps)
            / ethers::types::U256::from(10_000u64);
        let nonce = t.take_nonce();
        let fees = fees_now(t.fees.read().map(|f| *f).unwrap_or_default(), base_fee);
        let call = wrapper::unwind(
            t.wrapper,
            at,
            // The whole balance, whatever it turned out to be. An exact
            // figure a wei off reverts, and a revert here keeps a position
            // while the price it is leaving falls.
            ethers::types::U256::zero(),
            min_out,
            // No deadline when the chain's clock is not known, rather than a
            // deadline built on zero - which the wrapper reads as already
            // expired and refuses, so a run that lost the header socket could
            // not sell at all. What bounds an exit is `min_out`, enforced by
            // the curve itself; the deadline only keeps a queued sale from
            // arriving at a price nobody meant, and a late exit still gets the
            // money back. That is the opposite of a late entry, which is why
            // the buy above has no such fallback.
            match second {
                0 => 0,
                s => s + 12,
            },
        );
        f.selling = true;
        f.sell_next = std::time::Instant::now() + SELL_GAP;
        tokio::spawn(fire(
            http.clone(),
            t.to.clone(),
            t.wallet.clone(),
            call,
            nonce,
            fees,
            t.gas,
            at,
            launch::Leg::Sell,
            tx.clone(),
        ));
    }

    /// Let go of every launch whose minute is up and whose money is back.
    ///
    /// One place, because there were three - the tick, a trade arriving on an
    /// expired launch, and a bulk sweep when the map filled up - and they did
    /// not agree. The bulk one dropped a launch still holding a position,
    /// wrote no closing line, and left its address in the set the subscription
    /// filters on, which nothing ever removed it from again.
    ///
    /// A launch still holding is kept whatever its age: the record is all that
    /// knows the position exists, and the only address a receipt still in
    /// flight can land on.
    fn sweep(
        followed: &mut std::collections::HashMap<ethers::types::Address, Followed>,
        watched: &std::sync::RwLock<std::collections::HashSet<ethers::types::Address>>,
        ops: &mut operators::Operators,
        ops_dirty: &mut bool,
        now: std::time::Instant,
    ) {
        followed.retain(|curve, f| {
            if f.until > now || f.holding() {
                return true;
            }
            finish(f);
            // Nobody outside the bundle ever bought this one. Only worth
            // saying when the exemption list is known - an empty one makes
            // every buyer look like an outsider and none of them like one.
            if f.exempt_known && f.outsiders.is_empty() {
                ops.note_dead(f.operator);
                *ops_dirty = true;
            }
            if let Ok(mut w) = watched.write() {
                w.remove(curve);
            }
            false
        });
    }

    /// One followed launch, once its minute is up: into the file, not onto
    /// the console. Everything else the console used to say about a launch is
    /// already a record - the entry is `launch`, a step is `decision`, a close
    /// is `exit` - and this was the one line that was not.
    fn finish(f: &mut Followed) {
        if f.finished {
            return;
        }
        f.finished = true;
        let opened = f.opening.map(|o| o.quote_reserve).unwrap_or_default();
        let position = match &f.position {
            snipe::Position::Watching => "never bought".to_string(),
            snipe::Position::Skipped { why } => format!("skipped: {why}"),
            snipe::Position::InFlight { step } => format!("in flight from +{step}s"),
            snipe::Position::Failed { step, why } => format!("failed at +{step}s: {why}"),
            snipe::Position::Bought { step, .. } => format!("bought at +{step}s"),
            snipe::Position::Sold { step } => format!("bought at +{step}s and sold"),
        };
        if let Err(e) = journal::append(
            &f.journal,
            &journal::done_line(
                f.buys,
                f.sells,
                f.outsiders.len(),
                f.peak_quote,
                f.curve.quote_reserve,
                opened,
                f.quote_decimals,
                &position,
            ),
        ) {
            tracing::warn!(err = %format!("{e:#}"), "cannot write the closing line");
        }
    }

    let mut followed: std::collections::HashMap<ethers::types::Address, Followed> =
        std::collections::HashMap::new();
    // Trades that arrived before their curve was being followed. The dev buy
    // is emitted in the launch transaction itself, so it reaches the trade
    // subscription at the same moment the launch reaches the log one - and
    // whichever wins, the reserves have to end up counting it. Missing it left
    // every later quote on that curve one buy stale, which is exactly the
    // MODEL OFF BY it produced.
    let mut early: std::collections::VecDeque<(
        ethers::types::Address,
        u64,
        u64,
        curve::Trade,
    )> = std::collections::VecDeque::new();

    // Pair token -> what it is, so the amounts in a launch can be printed in the
    // units the chain meant. Bounded, because this process is meant to be left
    // running: the oldest go, and a launch whose pair token has been forgotten
    // is looked up again rather than printed wrong.
    const REMEMBERED: usize = 512;
    let mut quotes: std::collections::HashMap<ethers::types::Address, launch::Quote> =
        std::collections::HashMap::new();
    // The reserves a curve quoted in this token opens at, as the factory has
    // them. Read rather than derived from the launch log's threshold: a pair
    // token overrides the config, and the ratio between the two is a setting
    // rather than a law.
    let mut economics: std::collections::HashMap<ethers::types::Address, launch::PairEconomics> =
        std::collections::HashMap::new();
    // The native currency needs no lookup and is not evicted with the rest: a
    // launch quoted in it arrives before any log has taught this map anything,
    // and "8000000000000000 raw" is not a number anybody reads.
    quotes.insert(
        ethers::types::Address::zero(),
        launch::Quote {
            decimals: 18,
            symbol: "ETH".to_string(),
        },
    );
    let mut order: std::collections::VecDeque<ethers::types::Address> =
        std::collections::VecDeque::new();

    // What the feed saw, by transaction hash, until its log turns up. Two things
    // come out of it: the calldata, so the log needs no request of its own, and
    // when the sequencer took it, which is what the lead time is measured from.
    type Sighting = (std::time::Instant, launch::LaunchCall, u64);
    let mut sightings: std::collections::HashMap<ethers::types::H256, Sighting> =
        std::collections::HashMap::new();
    let mut seen_order: std::collections::VecDeque<ethers::types::H256> =
        std::collections::VecDeque::new();
    // Launches already reported from a log. The feed is a second source, not a
    // faster one by construction: on a fresh connection it can hand over a
    // transaction whose log has already been printed, and announcing that as
    // "incoming" would be a launch reported twice, the second time as news.
    let mut reported: std::collections::HashSet<ethers::types::H256> =
        std::collections::HashSet::new();
    let mut reported_order: std::collections::VecDeque<ethers::types::H256> =
        std::collections::VecDeque::new();

    // A launch is held back for as long as its own transaction could still have
    // more to say. Both logs are emitted by one call and arrive together, so
    // this is a few milliseconds in practice - but it has to be a wait rather
    // than "print when the next log arrives", or the newest launch would sit
    // unprinted until an unrelated one turned up.
    const SAME_TX_GRACE: std::time::Duration = std::time::Duration::from_millis(250);
    /// A launch held back for the moment its dev buy could still arrive.
    struct Held {
        launch: launch::Launch,
        call: Option<launch::LaunchCall>,
        lead: Option<std::time::Duration>,
        launched_at: Option<u64>,
        /// Why this launch is not being followed, when it is not. Part of the
        /// entry rather than a line of its own: printed separately it lands
        /// next to somebody else's launch and says nothing about either.
        refused: Option<String>,
    }
    let mut pending: Option<Held> = None;
    let mut deadline = tokio::time::Instant::now();

    // How a position ends, refused at startup rather than at the moment one
    // has to be closed.
    // The exit's own allowance, not the entry's. `--slippage-bps` overrides the
    // entry only: it exists to aim a buy at a tax step, and the exit has never
    // been what it was for.
    let exit_slippage_bps = cfg.snipe.exit_slippage_bps;
    let exit_policy = exit::Policy {
        trail_bps: cfg.snipe.trail_bps,
        take_x100: cfg.snipe.take_x100,
        hold_blocks: cfg.snipe.hold_blocks,
        slippage_bps: exit_slippage_bps,
    };
    exit_policy.check().context("[snipe] exit rules")?;

    // What this is willing to do, from the config. The size is filled in per
    // launch, because it is in the pair token's own units and those differ.
    let policy = snipe::Policy {
        spend: ethers::types::U256::zero(),
        slippage_bps,
        max_tax_bps: cfg.snipe.max_tax_bps,
        max_creator_tax_bps: cfg.snipe.max_creator_tax_bps,
        max_run_x100: cfg.snipe.max_run_x100,
        max_exempt: cfg.snipe.max_exempt,
        require_dev_buy: cfg.snipe.require_dev_buy,
        min_dev_buy_x100: cfg.snipe.min_dev_buy_x100,
        operator_needs: cfg.snipe.operator_needs,
        min_exempt: cfg.snipe.min_exempt,
        pairs: cfg.snipe.pairs.clone(),
        max_dev_buy_x100: cfg.snipe.max_dev_buy_x100,
    };

    // Who has launched before, and how it went. The only state this keeps
    // between runs: everything else can be rebuilt from the chain, and this
    // cannot.
    let ops_path = std::path::PathBuf::from(&cfg.snipe.operators);
    let mut ops = operators::Operators::load(&ops_path)
        .with_context(|| format!("reading {}", ops_path.display()))?;
    if ops.is_empty() {
        tracing::info!(
            path = %ops_path.display(),
            "no operator history yet; every launch is a first sighting until \
             one is built - seed it from the journals with analysis/seed.py"
        );
    } else {
        tracing::info!(
            operators = ops.len(),
            wallets = ops.wallets(),
            needs = cfg.snipe.operator_needs,
            path = %ops_path.display(),
            "operator history"
        );
    }
    let mut ops_dirty = false;
    let mut ops_saved = std::time::Instant::now();
    // A week of this chain, at 9.8 blocks to the second. An operator nobody
    // has heard from in a week is not one we are about to meet again, and the
    // store is the one thing here that grows without an upper bound.
    const REMEMBER_BLOCKS: u64 = 5_927_040;
    let mut newest_block = 0u64;
    // The block the exit was last asked about, so it is asked once per block.
    let mut asked_at = 0u64;
    // What the wallet actually paid and actually got back, in the pair token,
    // from the curves' own logs. Everything else this program reports is the
    // model's opinion; this pair is the chain's. Only ETH pairs are traded, so
    // the two add up.
    let mut realized = (ethers::types::U256::zero(), ethers::types::U256::zero());
    // And what the chain charged to do it, reverts included.
    let mut gas_paid = ethers::types::U256::zero();

    // The wrapper is checked whenever one is configured, whether or not this
    // run may spend anything. A dry run that does not verify the contract is a
    // dry run that proves nothing about the live one, and both answers cost
    // two calls at startup rather than one reverted trade at a time.
    let mut trader: Option<Trader> = None;
    if let Some(addr) = cfg.snipe.contract.as_ref() {
        let wrapper: ethers::types::Address = addr.parse().context("[snipe] contract")?;
        let owner = pool::call_address(
            http,
            wrapper,
            &ethers::types::Bytes::from(pool::selector("owner()").to_vec()),
        )
        .await
        .context("reading the wrapper's owner")?;
        // That it is the wrapper at all. An address that answers `owner()`
        // could be anything we ever deployed; only one pointed at this
        // launchpad will let a buy through its own factory check.
        let its_factory = pool::call_address(
            http,
            wrapper,
            &ethers::types::Bytes::from(pool::selector("factory()").to_vec()),
        )
        .await
        .context("reading the wrapper's factory - is this a PonsSniper?")?;
        let ours: ethers::types::Address = launch::PONS_V2_FACTORY.parse()?;
        anyhow::ensure!(
            its_factory == ours,
            "the wrapper at {wrapper:?} trades against factory {its_factory:?}, not {ours:?}"
        );
        tracing::info!(?wrapper, ?owner, "wrapper checked");

        // TODO(money): nothing is recovered at startup. A bot killed holding a
        // position comes back knowing nothing about it: the wrapper still
        // holds the tokens, no `Followed` refers to them, and the curve they
        // came from is not being watched. They sit until somebody notices and
        // calls `rescue`. The wrapper knows - its balance of each launched
        // token is the position - so a sweep at startup over the curves in
        // recent journals would find them and either sell or report them.
        if sending {
            let chain = http.get_chainid().await.context("chain id")?.as_u64();
            let wallet = swap::load_wallet(cfg, chain)?;
            let me = ethers::signers::Signer::address(&wallet);
            anyhow::ensure!(
                owner == me,
                "the wrapper at {wrapper:?} is owned by {owner:?}, not by this wallet ({me:?})"
            );
            let balance = http.get_balance(me, None).await.context("wallet balance")?;
            let urls = if cfg.submit_urls.is_empty() {
                vec![cfg.http_url.clone()]
            } else {
                cfg.submit_urls.clone()
            };
            let fees = std::sync::Arc::new(std::sync::RwLock::new(swap::fee_params(http).await?));
            {
                // The tip only, and on a timer. The base fee comes from the
                // header subscription, one block old rather than up to twenty
                // seconds - see `fees_now` - so asking for the latest block
                // again here was a round trip for a number already in hand,
                // and the heavier of the two this used to make. The pair below
                // is what stands until the first header arrives.
                let fees = fees.clone();
                let http = http.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                        if let Ok(tip) = swap::tip_now(&http).await {
                            if let Ok(mut w) = fees.write() {
                                // The stale max_fee is kept as the fallback it
                                // is: `fees_now` rebuilds it from the live base
                                // whenever there is one.
                                w.1 = tip;
                            }
                        }
                    }
                });
            }
            let max_spend = match cfg.snipe.max_spend.trim() {
                "" => None,
                v => Some(units::parse_units(v, 18).context("[snipe] max_spend")?),
            };
            tracing::warn!(
                wallet = ?me,
                balance = %units::format_units(balance, 18),
                max_open = cfg.snipe.max_open,
                max_spend = %cfg.snipe.max_spend,
                gas_limit = cfg.snipe.gas_limit,
                "EXECUTING: this run sends transactions and spends real money"
            );
            let to = std::sync::Arc::new(swap::Broadcaster::new(&urls)?);
            // Where a buy will actually go, said once at startup. It is the
            // one round trip a buy waits on, it is set from the environment
            // rather than from the config, and until now nothing said whether
            // that setting had been picked up at all - a run submitting to one
            // node looked exactly like a run submitting to five. Labels are
            // scheme and host only: these URLs carry API keys.
            tracing::info!(
                endpoints = ?to.labels(),
                count = to.width(),
                from = if cfg.submit_urls.is_empty() { "http_url" } else { "SUBMIT_URLS" },
                "submitting through"
            );
            trader = Some(Trader {
                nonce: swap::pending_nonce(http, me).await?,
                wallet,
                to,
                wrapper,
                inflight: 0,
                fees,
                gas: ethers::types::U256::from(cfg.snipe.gas_limit),
                open: 0,
                max_open: cfg.snipe.max_open,

                max_spend,
            });
        }
    } else if sending {
        anyhow::bail!("--execute needs [snipe] contract, the wrapper to trade through");
    }

    // How the feed is actually doing, against the logs it is supposed to beat.
    // What is happening, in numbers, because the entries themselves are all in
    // the journals and reading them go past says nothing about the whole. Set
    // `--verbose` to get the entries back on the console.
    let mut seen_launches: u64 = 0;
    let mut refused_launches: u64 = 0;
    let mut all_launches: u64 = 0;
    let mut tally = Tally::default();
    let started = std::time::Instant::now();
    let mut heard_from_feed: u64 = 0;
    let mut feed_wait_ms: u64 = 0;
    let mut feed_wait_worst: u64 = 0;
    let mut feed_was_late: u64 = 0;
    let mut feed_told = std::time::Instant::now();

    // The requests a launch needs before anything can be decided about it,
    // made off the loop that decides. ONE task working a queue, not one task
    // per launch: a `TokenLaunched` and the `Launched` of the same transaction
    // resolving out of order would print a dev buy with no launch in front of
    // it, and pair the wrong two together. In order, and the loop never waits.
    let (wanting, mut wanted) = tokio::sync::mpsc::channel::<launch::Wanted>(256);
    let resolving = tokio::spawn({
        let http = http.clone();
        let tx = tx.clone();
        let seconds = block_seconds.clone();
        async move {
            while let Some(w) = wanted.recv().await {
                let launch::Wanted {
                    launch,
                    mut call,
                    lead,
                    mut launched_at,
                    want_quote,
                    want_terms,
                } = w;
                if call.is_none() {
                    call = launch_call(&http, launch.tx).await;
                }
                // Only when the calldata really did not decode, which is what
                // `want_terms` was set for - a decoded call already says both.
                let terms = match want_terms.filter(|_| call.is_none()) {
                    Some(curve) => launch::read_curve_terms(&http, curve).await,
                    None => None,
                };
                if launched_at.is_none() {
                    // The header carrying this block's timestamp is on its way
                    // through a subscription of its own, racing the log that
                    // brought the launch - so the answer is usually moments
                    // away and asking for it is a request per launch, two
                    // thousand an hour, against an endpoint the journals show
                    // refusing reads several times an hour already.
                    //
                    // Waited for here rather than in the loop: this task is
                    // where everything slow already lives, and half a second
                    // is nothing against the three-second window it feeds.
                    for _ in 0..10 {
                        launched_at = seconds
                            .read()
                            .ok()
                            .and_then(|s| s.get(&launch.block).copied());
                        if launched_at.is_some() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                if launched_at.is_none() {
                    launched_at = block_second(&http, launch.block).await;
                }
                let quote = match want_quote {
                    Some(t) => quote_of(&http, factory, t).await.map(|(q, e)| (t, q, e)),
                    None => None,
                };
                if tx
                    .send(launch::Heard::Ready(Box::new(launch::Resolved {
                        launch,
                        call,
                        lead,
                        launched_at,
                        quote,
                        terms,
                    })))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    });
    if size.is_some() {
        tracing::info!(
            max_tax_bps = policy.max_tax_bps,
            max_creator_tax_bps = policy.max_creator_tax_bps,
            max_run_x100 = policy.max_run_x100,
            max_exempt = policy.max_exempt,
            require_dev_buy = policy.require_dev_buy,
            min_dev_buy_x100 = policy.min_dev_buy_x100,
            slippage_bps = policy.slippage_bps,
            lead_ms = lead.as_millis() as u64,
            "snipe policy"
        );
    }

    // Fine enough that the lead above is respected rather than rounded up to
    // it: a scan over at most sixty-four curves costs nothing next to being a
    // block late.
    let mut ticks = tokio::time::interval(std::time::Duration::from_millis(20));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            // A step of some launch's tax window is about to open. Nothing is
            // planned ahead: the question is asked here, against the reserves
            // the cache holds at this moment, and asked once per step.
            _ = ticks.tick(), if size.is_some() => {
                let Some((second, at)) = second_anchor.read().ok().and_then(|a| *a) else {
                    continue;
                };
                let Some(t) = tax else { continue };
                let now = std::time::Instant::now();

                // The exit, on the clock rather than on a trade. The stop and
                // the target cannot have moved - only a trade moves the price
                // and a trade would have asked already - so what this reaches
                // is `hold_blocks`, which is about the block and is the one
                // rule a curve that went quiet can satisfy. Asked against the
                // chain's own head, not ours: the last block we saw a trade in
                // stops advancing the moment the curve does, which is exactly
                // when this has to keep counting.
                let head = block_seconds
                    .read()
                    .ok()
                    .and_then(|s| s.keys().next_back().copied())
                    .unwrap_or(0);
                newest_block = newest_block.max(head);
                // Once per block, not once per tick. Nothing the exit reads
                // moves in between - the curve only changes on a trade, and a
                // trade asks for itself - so the other four ticks of every
                // block would ask the same question and get the same answer,
                // on the same path that has 100ms to aim an entry.
                if head > asked_at {
                    asked_at = head;
                    let chain_now = chain_second(&second_anchor);
                    for (curve, f) in followed.iter_mut() {
                        ask_exit(
                            f, head, &exit_policy, verbose, &mut tally, &mut ops, &mut ops_dirty,
                        );
                        send_exit(
                            f,
                            *curve,
                            trader.as_mut(),
                            http,
                            &tx,
                            &base_fee,
                            chain_now,
                            exit_slippage_bps,
                        );
                    }
                }

                // A launch is followed for a minute. Shed the ones whose
                // minute is up here rather than waiting for a trade on them:
                // a curve nobody trades on would otherwise be watched forever,
                // and the set the subscription filters on would only grow.
                //
                // Except one that is still holding. Dropping a `Followed`
                // drops the only record of what is in the wrapper and the
                // only thing that would sell it, and it drops the receipt of a
                // buy still in flight along with it - so a launch whose money
                // has not come back is kept past its minute however quiet it
                // goes. `hold_blocks` above is what ends it; if the chain
                // refuses the sale twelve times over, `send_exit` says so and
                // the tokens want a person.
                sweep(&mut followed, &watched_curves, &mut ops, &mut ops_dirty, now);
                // Written on a timer rather than on every change: a busy
                // minute changes it hundreds of times, and the file is only
                // ever read at startup.
                if feed_told.elapsed() >= std::time::Duration::from_secs(30) {
                    feed_told = std::time::Instant::now();
                    tracing::info!(
                        launches = seen_launches,
                        refused = refused_launches,
                        following = followed.len(),
                        // Followed past their minute because money of ours is
                        // still in them. Anything but zero for long is a sale
                        // that is not going through.
                        holding = followed.values().filter(|f| f.holding()).count(),
                        bought = tally.open,
                        closed = tally.closed,
                        // Anything but zero here and the shadow beside it is
                        // describing a run that did not happen.
                        buys_refused = tally.failed,
                        // The shadow so far, as a multiple of what it cost.
                        // Without `--execute` nothing was sent: this is what
                        // the rules would have returned, and the only running
                        // answer there is.
                        shadow = %Tally::average(tally.x100, tally.closed),
                        win_pct = %match tally.closed {
                            0 => "-".to_string(),
                            n => (100 * tally.wins / n).to_string(),
                        },
                        kept = %match tally.kept_closed {
                            0 => "-".to_string(),
                            n => format!("{} on {n}", Tally::average(tally.kept_x100, n)),
                        },
                        feed_heard = heard_from_feed,
                        feed_late = feed_was_late,
                        // Ours, not theirs: decode to handled.
                        feed_queued_ms = match heard_from_feed {
                            0 => 0,
                            n => feed_wait_ms / n,
                        },
                        feed_queued_worst_ms = feed_wait_worst,
                        operators = ops.len(),
                        "the last thirty seconds"
                    );
                    tracing::info!(
                        minutes = started.elapsed().as_secs() / 60,
                        launches = all_launches,
                        closed = tally.all_closed,
                        buys_refused = tally.all_failed,
                        shadow = %Tally::average(tally.all_x100, tally.all_closed),
                        win_pct = %match tally.all_closed {
                            0 => "-".to_string(),
                            n => (100 * tally.all_wins / n).to_string(),
                        },
                        "the whole run"
                    );
                    // What the wallet actually did, from the curves' own logs.
                    // Printed only once something has been sent, because a
                    // line of zeros beside the shadow reads as a result and is
                    // not one - it is the absence of a wallet.
                    if !realized.0.is_zero() {
                        let (paid, back) = realized;
                        // Trading and gas as two numbers and then as one.
                        // Separately because they answer different questions -
                        // whether the rules work, and whether they work at
                        // this size - and together because only the last one
                        // is the wallet.
                        let gross = |a: ethers::types::U256, b: ethers::types::U256| match a >= b {
                            true => format!("+{}", launch::amount_of(a - b, 18)),
                            false => format!("-{}", launch::amount_of(b - a, 18)),
                        };
                        tracing::info!(
                            paid = %launch::amount_of(paid, 18),
                            back = %launch::amount_of(back, 18),
                            trading = %gross(back, paid),
                            gas = %launch::amount_of(gas_paid, 18),
                            net = %gross(back, paid + gas_paid),
                            "on chain"
                        );
                    }
                    seen_launches = 0;
                    refused_launches = 0;
                    tally.window();
                    heard_from_feed = 0;
                    feed_was_late = 0;
                    feed_wait_ms = 0;
                    feed_wait_worst = 0;
                }
                if ops_dirty && ops_saved.elapsed() >= std::time::Duration::from_secs(30) {
                    let gone = ops.forget_before(newest_block.saturating_sub(REMEMBER_BLOCKS));
                    if gone > 0 {
                        tracing::info!(gone, "forgot operators nobody has heard from");
                    }
                    if let Err(e) = ops.save(&ops_path) {
                        tracing::warn!(err = %format!("{e:#}"), "cannot write the operator history");
                    }
                    ops_dirty = false;
                    ops_saved = std::time::Instant::now();
                }
                for (curve_addr, f) in followed.iter_mut() {
                    let Some(launched_at) = f.launched_at else { continue };
                    // The launch second is not asked about. Its tax is 99%,
                    // the wrapper refuses it outright, and a decision line
                    // saying so on every launch is a line nobody reads.
                    for step in 1..=t.seconds {
                        if f.decided.contains(&step) {
                            continue;
                        }
                        let opens_at = launched_at + step;
                        // Where that second falls in our own clock, from the
                        // last turnover the feed showed us.
                        let opens_in_ms = (opens_at as i64 - second as i64) * 1000
                            - at.elapsed().as_millis() as i64;
                        if opens_in_ms > lead.as_millis() as i64 {
                            continue;
                        }
                        // More than a step behind: the moment passed while
                        // nothing was arriving, and it is not a decision to
                        // make late.
                        if opens_in_ms < -1000 {
                            f.decided.insert(step);
                            continue;
                        }
                        f.decided.insert(step);
                        let tax_bps = launch::snipe_tax_bps(&t, step);
                        // Against the curve as it OPENED, so the size is the
                        // same statement whether or not somebody has traded
                        // since - a share of what the launch started with.
                        let spend = spend_for(
                            size_x100,
                            cap,
                            f.quote_decimals,
                            f.opening.as_ref().unwrap_or(&f.curve),
                        );
                        if spend.is_zero() {
                            continue;
                        }
                        let signal = snipe::Signal {
                            step,
                            tax_bps,
                            opens_at,
                            in_ms: opens_in_ms,
                            now: &f.curve,
                            facts: &f.facts,
                            position: &f.position,
                        };
                        let mut p = policy.clone();
                        p.spend = spend;
                        let decision = snipe::decide(&signal, &p);
                        // Only the answers that are answers. `wait` at 618 bps
                        // is the policy's own ceiling restated, identical on
                        // every launch and on every step of every launch - two
                        // lines of it per launch buried the ones that decide
                        // something. The journal keeps them all.
                        if verbose && !matches!(decision, snipe::Decision::Wait { .. }) {
                            println!(
                                "{}",
                                snipe::render(&signal, &decision, *curve_addr, now.elapsed())
                            );
                        }
                        // Built here, where the signal is still borrowable,
                        // and written after the transaction has gone. Opening
                        // a file, appending and closing it is not free, and it
                        // sat between deciding to buy and signing the buy - on
                        // the one path with a hundred milliseconds to spend
                        // and a step of somebody else's tax window to hit. The
                        // journal is read hours later; the send is not.
                        let record = journal::decision_line(&signal, &decision);
                        // Nothing is sent yet, so a buy is recorded as the
                        // decision it is and the position stays open. When
                        // there is a wallet behind this, the position becomes
                        // InFlight here and the receipt settles it.
                        match &decision {
                            snipe::Decision::Skip { why } => {
                                f.position = snipe::Position::Skipped { why: why.clone() };
                            }
                            // The shadow opens on the first buy decision and
                            // never reopens: a second one would be averaging
                            // into a position we are already carrying, which
                            // is a different strategy than the one measured.
                            snipe::Decision::Buy { spend, min_tokens_out, .. }
                                if f.shadow.is_none() =>
                            {
                                match curve::buy(&f.curve, *spend, tax_bps) {
                                    Ok(fill) => {
                                        let mut h = exit::Held {
                                            tokens: fill.tokens_out,
                                            cost: fill.spent,
                                            high: ethers::types::U256::zero(),
                                            opened_at: f.last_block,
                                        };
                                        h.mark(exit::worth(&f.curve, h.tokens));
                                        f.shadow = Some(h);
                                        tally.opened();
                                        // The position the decision implies.
                                        // Without this every later step of the
                                        // same window asks again and answers
                                        // BUY again - which on paper is two
                                        // lines and with a wallet behind it is
                                        // two buys.
                                        f.position = snipe::Position::Bought {
                                            step,
                                            spend: fill.spent,
                                            tokens: fill.tokens_out,
                                        };
                                        // And with a wallet behind it, this is
                                        // where the money leaves.
                                        let stopped = f
                                            .refused
                                            .clone()
                                            .or_else(|| {
                                                trader.as_ref().and_then(|t| {
                                                    // Sent plus gas, less what
                                                    // came back. Saturating,
                                                    // because a run in profit
                                                    // is not a run that has
                                                    // lost a negative amount.
                                                    let lost = (realized.0 + gas_paid)
                                                        .saturating_sub(realized.1);
                                                    t.refuses(
                                                        *spend,
                                                        f.pair_token.is_zero(),
                                                        lost,
                                                    )
                                                })
                                            });
                                        if let Some(t) = trader.as_mut() {
                                            match stopped {
                                                Some(why) => tracing::debug!(
                                                    curve = ?curve_addr, why,
                                                    "decided to buy and did not send"
                                                ),
                                                None => {
                                                    let nonce = t.take_nonce();
                                                    let fees = fees_now(
                                                        t.fees
                                                            .read()
                                                            .map(|f| *f)
                                                            .unwrap_or_default(),
                                                        &base_fee,
                                                    );
                                                    let call = wrapper::snipe(
                                                        t.wrapper,
                                                        *curve_addr,
                                                        *spend,
                                                        *min_tokens_out,
                                                        tax_bps,
                                                        // Two seconds past the
                                                        // step it aims at. A
                                                        // buy that arrives
                                                        // later is aimed at a
                                                        // launch that has moved
                                                        // on, and the tax it
                                                        // would pay is not the
                                                        // one this decided on.
                                                        opens_at + 2,
                                                    );
                                                    t.open += 1;
                                                    f.sent = *spend;
                                                    f.expected_tokens = fill.tokens_out;
                                                    f.position =
                                                        snipe::Position::InFlight { step };
                                                    tokio::spawn(fire(
                                                        http.clone(),
                                                        t.to.clone(),
                                                        t.wallet.clone(),
                                                        call,
                                                        nonce,
                                                        fees,
                                                        t.gas,
                                                        *curve_addr,
                                                        launch::Leg::Buy,
                                                        tx.clone(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => tracing::warn!(
                                        curve = ?curve_addr, err = %format!("{e:#}"),
                                        "the buy this decided on cannot be priced"
                                    ),
                                }
                            }
                            _ => {}
                        }
                        // The transaction is away; now the file.
                        if let Err(e) = journal::append(&f.journal, &record) {
                            tracing::warn!(err = %format!("{e:#}"), "cannot write the decision");
                        }
                    }
                }
            }
            got = launches.recv() => match got {
                // The sequencer took a launch: everything the calldata says,
                // before the block that will carry it exists.
                Some(launch::Heard::Incoming(i)) => {
                    heard_from_feed += 1;
                    // How long this sighting sat between being decoded off the
                    // feed and being looked at here. It shares one channel with
                    // every trade on every followed curve, which is hundreds a
                    // minute, so a feed that looks slow may only be queued
                    // behind our own work.
                    let waited = i.seen.elapsed().as_millis() as u64;
                    feed_wait_ms += waited;
                    feed_wait_worst = feed_wait_worst.max(waited);
                    if reported.contains(&i.tx) {
                        // The feed found it, and found it too late to be worth
                        // anything: the log for the same launch had already
                        // arrived. Counted, because "the feed sees nothing" and
                        // "the feed is behind the logs" need entirely different
                        // fixes and look identical from the entries.
                        feed_was_late += 1;
                        tracing::debug!(tx = ?i.tx, "the feed caught up with a launch already printed");
                        continue;
                    }
                    tracing::debug!(
                        "{}",
                        launch::render_incoming(&i, quotes.get(&i.call.pair_token))
                    );
                    // The pair token is NOT looked up here any more. It was
                    // a prefetch - the feed sees a launch before its log, so
                    // the answer would be warm by the time the launch needed
                    // it - but it was a round trip inside the loop that has to
                    // aim at a step of the tax window, and the resolver fetches
                    // the same thing off it now.
                    if seen_order.len() >= REMEMBERED {
                        if let Some(old) = seen_order.pop_front() {
                            sightings.remove(&old);
                        }
                    }
                    seen_order.push_back(i.tx);
                    sightings.insert(i.tx, (i.seen, i.call, i.chain_time));
                }
                Some(launch::Heard::Trade(t)) => {
                    let launch::TradeAt { curve: at, block, index, trade } = *t;
                    // A launch is followed for a minute and then let go. The
                    // sweep below only ran when the map filled up, so a quiet
                    // hour left curves being written to long after anything
                    // about them was still being decided.
                    //
                    // Except one still holding, exactly as the sweep does: the
                    // same rule through a different door, and a door that
                    // opens on a TRADE - which is the one moment the position
                    // might be about to sell itself.
                    if followed
                        .get(&at)
                        .is_some_and(|f| f.until <= std::time::Instant::now() && !f.holding())
                    {
                        sweep(
                            &mut followed,
                            &watched_curves,
                            &mut ops,
                            &mut ops_dirty,
                            std::time::Instant::now(),
                        );
                        continue;
                    }
                    let Some(f) = followed.get_mut(&at) else {
                        // Not following it yet, but told to watch it: this is
                        // the launch's own dev buy, racing its launch log.
                        if early.len() >= 256 {
                            early.pop_front();
                        }
                        early.push_back((at, block, index, trade));
                        continue;
                    };
                    // Already applied, from whichever source got here first.
                    if !f.seen.insert((block, index)) {
                        continue;
                    }
                    // Predicted BEFORE applying, which is the order a live
                    // quote would run in.
                    // The model against the chain, on a trade nobody
                    // arranged. Not a log line but an alarm: a buy priced off
                    // reserves that have drifted is a buy sized wrong.
                    if let (Some(p), curve::Trade::Buy { tokens_out, .. }) =
                        (curve::predicted_tokens_out(&f.curve, &trade), &trade)
                    {
                        // Off by enough to matter. The check used to be exact,
                        // which is how it found the missing bundle buys - those
                        // were 8% to 200% out. But a few wei at a clamp
                        // boundary is arithmetic, not drift, and three false
                        // alarms an hour reading "everything priced from this
                        // curve is now a guess" teaches the reader to skip the
                        // line that means it.
                        let off = p.abs_diff(*tokens_out);
                        let matters = off.saturating_mul(ethers::types::U256::from(1_000_000u64))
                            > *tokens_out;
                        if matters && !f.model_off {
                            f.model_off = true;
                            tracing::warn!(
                                curve = ?at,
                                block,
                                off_by = %launch::tokens_of(off),
                                // In parts per million of the fill, because
                                // the absolute figure means nothing without
                                // the size beside it.
                                off_ppm = %(off.saturating_mul(
                                    ethers::types::U256::from(1_000_000u64)
                                ) / (*tokens_out).max(ethers::types::U256::one())),
                                "the model and the chain disagree on a fill; \
                                 everything priced from this curve is now a guess"
                            );
                        }
                    }
                    if let Err(e) = f.curve.apply(&trade) {
                        // The reserves being followed are not the curve's any
                        // more. Nothing priced from them is worth anything, so
                        // the curve is dropped rather than carried on with -
                        // unless money of ours is in it. Then dropping the
                        // record is the one thing that must not happen: it is
                        // all that knows the position exists, and the only
                        // address a receipt still in flight can land on. Kept,
                        // and marked unpriceable, which stops it being sold at
                        // a floor computed from reserves that are no longer
                        // the curve's.
                        //
                        // TODO(money): re-reading the curve's reserves from
                        // the chain would make it priceable again and let the
                        // exit finish on its own. One call, off this path, and
                        // the position leaves instead of waiting for a person.
                        if f.holding() {
                            if !f.lost {
                                f.lost = true;
                                tracing::error!(
                                    curve = ?at, err = %format!("{e:#}"),
                                    "lost track of a curve holding a position of ours; it \
                                     cannot be priced and will not be sold - rescue(token, \
                                     owner) on the wrapper"
                                );
                            }
                            continue;
                        }
                        tracing::warn!(
                            curve = ?at, err = %format!("{e:#}"),
                            "lost track of a curve; no longer following it"
                        );
                        // The file has to say why it stops, or a journal that
                        // ends mid-launch is indistinguishable from a bot that
                        // died. And out of the set the subscription filters
                        // on: this was the one exit that left an address in it
                        // with nothing on the other side, so the filter only
                        // ever grew.
                        finish(f);
                        followed.remove(&at);
                        if let Ok(mut w) = watched_curves.write() {
                            w.remove(&at);
                        }
                        continue;
                    }
                    match &trade {
                        curve::Trade::Buy { recipient, .. } => {
                            f.buys += 1;
                            // Only when the exemption list is actually known:
                            // an empty one would make every buyer an outsider.
                            if f.exempt_known && !f.exempt.contains(recipient) {
                                f.outsiders.insert(*recipient);
                            }
                        }
                        curve::Trade::Sell { .. } => f.sells += 1,
                        _ => {}
                    }
                    // Ours, when it is ours. The wrapper buys to itself and
                    // sells from itself, so our own leg arrives through the
                    // same subscription as everybody else's and carries the
                    // exact amounts - no receipt to parse and no extra
                    // request, on a path that has none to spare. This is the
                    // only place the real fill is ever learnt.
                    if let Some(mine) = trader.as_ref().map(|t| t.wrapper) {
                        ours(f, &trade, mine, block, &mut realized);
                    }
                    f.peak_quote = f.peak_quote.max(f.curve.quote_reserve);
                    f.last_block = block;
                    newest_block = newest_block.max(block);
                    // The exit, against the curve this trade just made.
                    ask_exit(f, block, &exit_policy, verbose, &mut tally, &mut ops, &mut ops_dirty);
                    send_exit(
                        f,
                        at,
                        trader.as_mut(),
                        http,
                        &tx,
                        &base_fee,
                        chain_second(&second_anchor),
                        exit_slippage_bps,
                    );
                    let elapsed = match (
                        f.launched_at,
                        block_seconds.read().ok().and_then(|s| s.get(&block).copied()),
                    ) {
                        (Some(at), Some(now)) => Some(now as i64 - at as i64),
                        _ => None,
                    };
                    let fee_bps = config.map(|c| c.curve_fee_bps).unwrap_or(0);
                    // Which block this step of the window actually opened at.
                    // Written once per step, the first time a trade lands in
                    // it: by then the feed has carried the blocks around the
                    // boundary, and the answer is a range scan over what it
                    // left behind.
                    if let (Some(at), Some(e), Some(t)) = (f.launched_at, elapsed, tax) {
                        let step = e.max(0) as u64;
                        if e >= 0 && step <= t.seconds && f.windows_written.insert(step) {
                            let second = at + step;
                            let first = block_seconds.read().ok().and_then(|s| {
                                s.iter()
                                    .find(|(_, &ts)| ts == second)
                                    .map(|(&b, _)| b)
                            });
                            let tax_bps = launch::snipe_tax_bps(&t, step);
                            if let Some(first) = first {
                                let line =
                                    journal::window_line(step, tax_bps, second, first);
                                if let Err(e) = journal::append(&f.journal, &line) {
                                    tracing::warn!(
                                        err = %format!("{e:#}"), "cannot write the window line"
                                    );
                                }
                            }
                        }
                    }
                    if let Err(e) = journal::append(
                        &f.journal,
                        &journal::trade_line(
                            &trade,
                            block,
                            elapsed,
                            fee_bps,
                            f.quote_decimals,
                            f.exempt_known.then(|| match &trade {
                                curve::Trade::Buy { recipient, .. } => f.exempt.contains(recipient),
                                curve::Trade::Sell { seller, .. } => f.exempt.contains(seller),
                                _ => false,
                            }),
                            &f.curve,
                        ),
                    ) {
                        tracing::warn!(err = %format!("{e:#}"), "cannot write to the journal");
                    }
                    // The trades themselves go to the journal and not to the
                    // console. A busy launch makes hundreds of them in its
                    // minute, and a hundred lines nobody reads buries the
                    // handful that ask for a decision.
                    // Graduated: the curve is done and there is nothing left to
                    // decide about it. It gets the same closing line as a
                    // launch whose minute simply ran out.
                    if matches!(trade, curve::Trade::Completed) {
                        if let Some(f) = followed.get_mut(&at) {
                            // Valued at the last price the curve ever had.
                            // Doing nothing dropped it uncounted, and the
                            // launches that graduate are the ones that ran -
                            // so the statistics lost precisely their best
                            // outcomes. It is also what the measurement does:
                            // `stats.py` walks the value path and takes its
                            // last point when no rule fired.
                            let worth = f
                                .shadow
                                .map(|h| exit::worth(&f.curve, h.tokens))
                                .unwrap_or_default();
                            close_shadow(
                                f, worth, "graduated", block, &mut tally, &mut ops,
                                &mut ops_dirty,
                            );
                            finish(f);
                            // And a position of ours cannot leave through a
                            // curve that has closed - `sell` on it reverts,
                            // and no retry changes that. The record is kept so
                            // the count still shows it and so a receipt still
                            // in flight has somewhere to land; the tokens need
                            // `rescue` and then the graduated pool.
                            if f.holding() {
                                f.lost = true;
                                f.leaving = false;
                                tracing::error!(
                                    curve = ?at,
                                    "the curve graduated holding a position of ours; it \
                                     cannot be sold through the curve - rescue(token, owner) \
                                     on the wrapper, then the graduated pool"
                                );
                            }
                        }
                        let holding = followed.get(&at).is_some_and(|f| f.holding());
                        if !holding {
                            followed.remove(&at);
                            if let Ok(mut w) = watched_curves.write() {
                                w.remove(&at);
                            }
                        }
                    }
                }
                Some(launch::Heard::SnipeTaxStartBps(bps)) => {
                    let next = launch::SnipeTax {
                        start_bps: bps,
                        seconds: tax.map(|t| t.seconds).unwrap_or_default(),
                    };
                    tracing::warn!(
                        schedule = %launch::snipe_tax_line(&next),
                        "the factory's owner changed the snipe tax"
                    );
                    tax = Some(next);
                }
                Some(launch::Heard::SnipeTaxSeconds(seconds)) => {
                    let next = launch::SnipeTax {
                        start_bps: tax.map(|t| t.start_bps).unwrap_or_default(),
                        seconds,
                    };
                    tracing::warn!(
                        schedule = %launch::snipe_tax_line(&next),
                        "the factory's owner changed the snipe tax window"
                    );
                    tax = Some(next);
                }
                Some(launch::Heard::Launch(l)) => {
                    // FIRST, before anything that waits. The dev buy and the
                    // whole opening bundle are already on their way, and every
                    // await between here and this line is time their logs
                    // spend in a buffer that the rest of the chain is filling.
                    // It used to sit after the calldata request, which is a
                    // round trip of a few hundred milliseconds.
                    if let launch::What::Created { .. } = l.what {
                        if let Ok(mut w) = watched_curves.write() {
                            w.insert(l.curve);
                        }
                    }
                    // Everything the endpoint would have to be asked for,
                    // asked for somewhere else. Made here, the three requests
                    // behind a launch - its calldata, the second its block
                    // carries, what its pair token is - spend this loop's
                    // whole budget: it has a hundred milliseconds to aim at a
                    // step of the tax window and one round trip is a third of
                    // that. The paper runs showed steps decided after they had
                    // already opened.
                    let sighted = sightings.remove(&l.tx);
                    let cached_at = sighted.as_ref().map(|(_, _, at)| *at).or_else(|| {
                        block_seconds.read().ok().and_then(|s| s.get(&l.block).copied())
                    });
                    let want_quote = launch::pair_of(&l, sighted.as_ref().map(|(_, c, _)| c))
                        .filter(|p| !quotes.contains_key(p) || !economics.contains_key(p));
                    // A launch through an entry point this does not decode
                    // still has a curve, and the curve knows its own terms.
                    // Without them it was not followed at all.
                    let want_terms = (sighted.is_none()
                        && matches!(l.what, launch::What::Created { .. }))
                    .then_some(l.curve);
                    if wanting
                        .send(launch::Wanted {
                            launch: *l,
                            call: sighted.as_ref().map(|(_, c, _)| c.clone()),
                            lead: sighted.as_ref().map(|(seen, _, _)| seen.elapsed()),
                            launched_at: cached_at,
                            want_quote,
                            want_terms,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                // The same launch, once nothing is left to ask about. In the
                // order it arrived: the resolver is one task working through a
                // queue rather than one task per launch, because a `Launched`
                // and the `TokenLaunched` of the same transaction resolving
                // out of order is a dev buy printed without its launch.
                // A transaction of ours ended. Nothing here waited for it.
                Some(launch::Heard::Landed(l)) => {
                    let launch::Settled {
                        curve, leg, ok, why, hash, nonce, resync_nonce, pending, cost,
                    } = *l;
                    // Every transaction, landed or reverted. A reverted buy
                    // costs this and returns nothing, and a run that leaves
                    // those out of its total is reporting a cheaper run than
                    // it had. This is also the number the whole live run is
                    // for: the strategy's edge at this size is measured in
                    // hundreds of microETH, and so is a round trip's gas.
                    gas_paid += cost;
                    if let Some(t) = trader.as_mut() {
                        t.inflight = t.inflight.saturating_sub(1);
                    }
                    if let (Some(t), Some(n)) = (trader.as_mut(), resync_nonce) {
                        match resync_to(t.nonce, n, t.inflight, pending) {
                            Some(to) => {
                                tracing::warn!(
                                    from = t.nonce, to, inflight = t.inflight, pending,
                                    "nonce re-read after a miss"
                                );
                                t.nonce = to;
                            }
                            // Behind the node and not allowed to go back: a gap
                            // is open and something of ours may still fill it.
                            None if n < t.nonce => tracing::warn!(
                                ours = t.nonce, node = n, inflight = t.inflight,
                                "a gap in the nonce sequence, and something still in the air; \
                                 nothing of ours will land until it settles"
                            ),
                            None => {}
                        }
                    }
                    if let Some(f) = followed.get_mut(&curve) {
                        if let Err(e) = journal::append(
                            &f.journal,
                            &journal::sent_line(
                                match leg {
                                    launch::Leg::Buy => "buy",
                                    launch::Leg::Sell => "sell",
                                },
                                &format!("{hash:?}"),
                                ok,
                                &why,
                                nonce,
                            ),
                        ) {
                            tracing::warn!(err = %format!("{e:#}"), "cannot write what was sent");
                        }
                        match (leg, ok) {
                            (launch::Leg::Buy, true) => {
                                if let snipe::Position::InFlight { step } = f.position {
                                    f.position = snipe::Position::Bought {
                                        step,
                                        spend: f.sent,
                                        // The real fill when its log has come
                                        // back, which it usually has: the
                                        // subscription carries it the moment
                                        // the block exists, and this receipt
                                        // waited on a poll to find it. When it
                                        // has not, the model stands in and
                                        // `ours` corrects the position the
                                        // moment the log lands.
                                        tokens: f.filled.unwrap_or(f.expected_tokens),
                                    };
                                }
                            }
                            (launch::Leg::Buy, false) => {
                                // The position never existed. Drop the shadow
                                // with it, or the exit rules would sell a
                                // holding that is not there.
                                if let Some(t) = trader.as_mut() {
                                    t.open = t.open.saturating_sub(1);
                                }
                                // The shadow is dropped with it: a later step
                                // may still buy this launch, and the arm that
                                // opens one refuses to while another stands.
                                // What must not go with it is the fact that
                                // the chain refused - the statistics carry on
                                // reporting what the rules would have made,
                                // and nothing else would say they did not get
                                // the chance.
                                tally.refused();
                                f.shadow = None;
                                // The step it failed at, not zero. Zero is the
                                // launch second, which is structurally
                                // unbuyable - a record saying a buy failed
                                // there describes something that cannot
                                // happen, and `decide` reads this field to
                                // refuse the step that has already failed.
                                f.position = snipe::Position::Failed {
                                    step: match f.position {
                                        snipe::Position::InFlight { step } => step,
                                        _ => 0,
                                    },
                                    why: why.clone(),
                                };
                            }
                            (launch::Leg::Sell, true) => {
                                if let Some(t) = trader.as_mut() {
                                    t.open = t.open.saturating_sub(1);
                                }
                                // The money is back. This is the one state
                                // that says so, and without it the launch is
                                // held past its minute forever waiting for a
                                // sale that already happened.
                                if let snipe::Position::Bought { step, .. } = f.position {
                                    f.position = snipe::Position::Sold { step };
                                }
                                f.selling = false;
                                f.leaving = false;
                            }
                            (launch::Leg::Sell, false) => {
                                f.selling = false;
                                // Nothing of this token is in the wrapper, so
                                // there is nothing to try again for. Either
                                // the buy never landed or a sale already did -
                                // and a run that keeps retrying this one ends
                                // up reporting a stuck position that is not
                                // there, which is worse than saying nothing.
                                if why.contains("NothingHeld") {
                                    tracing::warn!(
                                        curve = ?curve,
                                        "the wrapper holds none of this; nothing left to sell"
                                    );
                                    if let snipe::Position::Bought { step, .. } = f.position {
                                        f.position = snipe::Position::Sold { step };
                                    }
                                    f.leaving = false;
                                } else if !pending && f.sell_tries <= CHASE_TRIES {
                                    // A revert is final and its nonce is spent,
                                    // so the replacement is a retry and not a
                                    // race. Sent on the next block rather than
                                    // after the gap, because the usual reason
                                    // is a floor set above a price that has
                                    // moved - and it is still moving, so the
                                    // next quote is the one that fills.
                                    //
                                    // Only for the first few. A receipt comes
                                    // back inside a few hundred milliseconds,
                                    // so chasing without a limit would spend
                                    // every attempt in a couple of seconds and
                                    // then give up on a launch that might still
                                    // have been sellable a minute later. After
                                    // the chase, whatever is wrong is not the
                                    // price, and the gap applies.
                                    f.sell_next = std::time::Instant::now();
                                }
                            }
                        }
                    }
                }
                Some(launch::Heard::Ready(r)) => {
                    let launch::Resolved { launch, call, lead, launched_at, quote, terms } = *r;
                    // Followed and priced like any other, and said so on the
                    // entry: what is missing about it is the exemption list and
                    // the maker's own buy, which is what the analysis leans on.
                    let marked = call.is_none().then_some("unknown entry point");
                    let l = Box::new(launch);
                    if let Some((token, q, e)) = quote {
                        if let Some(e) = e {
                            economics.insert(token, e);
                        }
                        if order.len() >= REMEMBERED {
                            if let Some(old) = order.pop_front() {
                                quotes.remove(&old);
                            }
                        }
                        order.push_back(token);
                        quotes.insert(token, q);
                        cache::flush();
                    }
                    // Whatever this log turns into below, the launch behind it
                    // has now been said out loud.
                    if reported.insert(l.tx) {
                        if reported_order.len() >= REMEMBERED {
                            if let Some(old) = reported_order.pop_front() {
                                reported.remove(&old);
                            }
                        }
                        reported_order.push_back(l.tx);
                    }
                    // The dev buy belonging to the launch in hand.
                    if pending.as_ref().is_some_and(|h| {
                        h.launch.tx == l.tx && matches!(l.what, launch::What::DevBuy { .. })
                    }) {
                        let Held { launch: p, call, lead, launched_at, refused } =
                            pending.take().expect("just checked");
                        let marked = call.is_none().then_some("unknown entry point");
                        let pair = launch::pair_of(&p, call.as_ref());
                        print_entry(verbose, &launch::Report {
                            created: Some(&p),
                            dev: Some(&l),
                            via: marked,
                            launched_at,
                            quote: pair.and_then(|t| quotes.get(&t)),
                            call: call.as_ref(),
                            tax,
                            lead,
                            refused: refused.as_deref(),
                        });
                        continue;
                    }
                    // Anything else means the held launch had no dev buy in it.
                    if let Some(Held { launch: p, call, lead, launched_at, refused }) =
                        pending.take()
                    {
                        let marked = call.is_none().then_some("unknown entry point");
                        let pair = launch::pair_of(&p, call.as_ref());
                        print_entry(verbose, &launch::Report {
                            created: Some(&p),
                            quote: pair.and_then(|t| quotes.get(&t)),
                            call: call.as_ref(),
                            tax,
                            lead,
                            launched_at,
                            refused: refused.as_deref(),
                            via: marked,
                            ..Default::default()
                        });
                    }

                    match l.what {
                        launch::What::Created { pair_token, .. } => {
                            let mut refused: Option<String> = None;
                            // The pair token was looked up by the resolver on
                            // the way here, if it needed looking up: both maps
                            // are checked there, not just the quote, because
                            // the native pair is seeded with its decimals at
                            // startup and gating on the quote alone left its
                            // economics - and with them every native launch,
                            // which is most of them - never read at all.
                            // Follow this curve from here, at the reserves it
                            // opens with. Every trade on it after this moves
                            // them exactly, out of its own logs, so what a buy
                            // would get is known without asking anyone.
                            //
                            // Unless its terms rule it out. Those are known
                            // before it has traded and they do not change, so a
                            // launch refused here is not watched, not journalled
                            // and not asked about again.
                            // From the calldata when it decoded, and from the
                            // curve itself when it did not.
                            let creator_tax_bps = call
                                .as_ref()
                                .map(|c| c.creator_tax_bps as u64)
                                .or(terms.map(|(t, _)| t));
                            if let Some(opening) = opening_curve(
                                config.as_ref(),
                                economics.get(&pair_token),
                                creator_tax_bps,
                                launch::threshold_of(&l).unwrap_or_default(),
                            ) {
                                if followed.len() >= 64 {
                                    sweep(
                                        &mut followed,
                                        &watched_curves,
                                        &mut ops,
                                        &mut ops_dirty,
                                        std::time::Instant::now(),
                                    );
                                }
                                let mut exempt: std::collections::HashSet<_> = call
                                    .as_ref()
                                    .map(|c| {
                                        let mut e: std::collections::HashSet<_> =
                                            c.exemptions.iter().copied().collect();
                                        e.insert(c.creator_fee_recipient);
                                        e
                                    })
                                    .unwrap_or_default();
                                if let launch::What::Created { deployer, .. } = l.what {
                                    if call.is_some() {
                                        exempt.insert(deployer);
                                    }
                                }
                                // Everything this launch names, which is what
                                // an operator is recognised by. Asked BEFORE
                                // the launch is filed, so the history it
                                // returns is the history and not this.
                                let mut who: Vec<ethers::types::Address> =
                                    exempt.iter().copied().collect();
                                if let launch::What::Created { deployer, .. } = l.what {
                                    who.push(deployer);
                                }
                                if let Some(c) = call.as_ref() {
                                    who.push(c.creator_fee_recipient);
                                }
                                let verdict = ops.verdict(&who);
                                let operator = ops.join(&who, l.block);
                                ops_dirty = true;
                                let facts = snipe::Facts {
                                    operator: verdict,
                                    curve: l.curve,
                                    name: call.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
                                    symbol: call
                                        .as_ref()
                                        .map(|c| c.symbol.clone())
                                        .unwrap_or_default(),
                                    quote_symbol: quotes
                                        .get(&pair_token)
                                        .map(|q| q.symbol.clone())
                                        .unwrap_or_default(),
                                    quote_decimals: quotes
                                        .get(&pair_token)
                                        .map(|q| q.decimals)
                                        .unwrap_or(18),
                                    creator_tax_bps: creator_tax_bps.unwrap_or(0),
                                    exempt: exempt.len(),
                                    // Marked, not hidden. A launch through an
                                    // entry point this does not decode is
                                    // followed like any other now, but what is
                                    // missing about it is real: who was
                                    // exempted from the snipe tax, and whether
                                    // the maker bought their own launch.
                                    via: match call.as_ref() {
                                        Some(c) => c.via,
                                        None if terms.is_some() => "unknown entry point",
                                        None => "",
                                    },
                                    // The maker's own money, from the calldata
                                    // and so known before the block exists.
                                    dev_buy_x100: snipe::dev_buy_x100(
                                        call.as_ref()
                                            .and_then(|c| c.quote_in)
                                            .unwrap_or_default(),
                                        &opening,
                                    ),
                                    opening,
                                };
                                // A launch whose terms rule it out is not
                                // followed, and no file is opened for it: a
                                // journal holding a launch line and nothing
                                // else reads as data lost rather than as a
                                // launch deliberately passed over.
                                refused = snipe::refuse_outright(&facts, &policy);
                                seen_launches += 1;
                                all_launches += 1;
                                if refused.is_some() {
                                    refused_launches += 1;
                                }
                                if let Some(why) = &refused {
                                    // At debug, because a refusal is now the
                                    // ordinary case: the filters keep about
                                    // one launch in twenty, and twenty warning
                                    // lines a minute is not a warning. The
                                    // count that matters - how many were
                                    // passed over - is in the summary, and the
                                    // reason for each is in its journal.
                                    tracing::debug!(
                                        launch = %if facts.symbol.is_empty() {
                                            format!("{:?}", l.token)
                                        } else {
                                            facts.symbol.clone()
                                        },
                                        curve = %format!("{:?}", l.curve),
                                        pair = %facts.quote_symbol,
                                        via = %facts.via,
                                        dev_buy_x100 = facts.dev_buy_x100,
                                        creator_tax_bps = facts.creator_tax_bps,
                                        exempt = facts.exempt,
                                        why = %why,
                                        "launch passed over"
                                    );
                                }
                                // Followed and journalled whatever the filters
                                // said. They gate the BUY, not the watching:
                                // the only way to know when the flow that the
                                // filters are looking for comes back is to
                                // have kept the flow that is not it, and a
                                // launch passed over leaves no record at all
                                // if it is never followed.
                                {
                                    let journal =
                                        journal::path_for(&journal_dir, l.block, l.curve);
                                    if let Err(e) = journal::append(
                                        &journal,
                                        &journal::launch_line(
                                            &l,
                                            call.as_ref(),
                                            quotes.get(&pair_token),
                                            Some(&opening),
                                            tax,
                                            launched_at,
                                            &exempt,
                                            refused.as_deref(),
                                        ),
                                    ) {
                                        tracing::warn!(
                                            err = %format!("{e:#}"),
                                            "cannot write the launch journal"
                                        );
                                    }
                                    // The blocks the subscription could not
                                    // have covered: the address only entered
                                    // its filter a moment ago, and the launch
                                    // block is already gone. Five is well past
                                    // where a bundle finishes buying.
                                    {
                                        let http = http.clone();
                                        let tx = tx.clone();
                                        let curve = l.curve;
                                        let from = l.block;
                                        tokio::spawn(async move {
                                            launch::backfill(&http, curve, from, tx).await
                                        });
                                    }
                                    let mut f = Followed {
                                        curve: opening,
                                        opening: Some(opening),
                                        facts,
                                        position: snipe::Position::Watching,
                                        decided: Default::default(),
                                        journal,
                                        exempt,
                                        exempt_known: call.is_some(),
                                        windows_written: Default::default(),
                                        quote_decimals: quotes
                                            .get(&pair_token)
                                            .map(|q| q.decimals)
                                            .unwrap_or(18),
                                        launched_at,
                                        until: std::time::Instant::now() + FOLLOW_FOR,
                                        buys: 0,
                                        sells: 0,
                                        last_block: l.block,
                                        shadow: None,
                                        operator,
                                        pair_token,
                                        sent: ethers::types::U256::zero(),
                                        refused: refused.clone(),
                                        selling: false,
                                        leaving: false,
                                        finished: false,
                                        lost: false,
                                        filled: None,
                                        expected_tokens: Default::default(),
                                        sell_next: std::time::Instant::now(),
                                        sell_tries: 0,
                                        outsiders: Default::default(),
                                        seen: Default::default(),
                                        model_off: false,
                                        peak_quote: opening.quote_reserve,
                                    };
                                    // Whatever already happened on it, in the
                                    // order it happened.
                                    while let Some(i) =
                                        early.iter().position(|(c, _, _, _)| *c == l.curve)
                                    {
                                        let (_, b, ix, t) = early.remove(i).expect("just found");
                                        if !f.seen.insert((b, ix)) {
                                            continue;
                                        }
                                        if let Err(e) = f.curve.apply(&t) {
                                            tracing::warn!(
                                                curve = ?l.curve, err = %format!("{e:#}"),
                                                "a trade from before we were following does not fit"
                                            );
                                            continue;
                                        }
                                        // Written like any other. These moved
                                        // the reserves, and a journal whose
                                        // first recorded reserve already
                                        // includes trades it does not contain
                                        // cannot be replayed from the opening
                                        // curve at all - every fill after them
                                        // is priced off a state the file never
                                        // shows. It was a third of the
                                        // journals once the backfill started
                                        // feeding this buffer.
                                        let elapsed = launched_at.and_then(|at| {
                                            block_seconds
                                                .read()
                                                .ok()
                                                .and_then(|s| s.get(&b).copied())
                                                .map(|now| now as i64 - at as i64)
                                        });
                                        if let Err(e) = journal::append(
                                            &f.journal,
                                            &journal::trade_line(
                                                &t,
                                                b,
                                                elapsed,
                                                config.map(|c| c.curve_fee_bps).unwrap_or(0),
                                                f.quote_decimals,
                                                f.exempt_known.then(|| match &t {
                                                    curve::Trade::Buy { recipient, .. } => {
                                                        f.exempt.contains(recipient)
                                                    }
                                                    curve::Trade::Sell { seller, .. } => {
                                                        f.exempt.contains(seller)
                                                    }
                                                    _ => false,
                                                }),
                                                &f.curve,
                                            ),
                                        ) {
                                            tracing::warn!(
                                                err = %format!("{e:#}"),
                                                "cannot write a trade from before we were following"
                                            );
                                        }
                                    }
                                    followed.insert(l.curve, f);
                                }
                            }
                            if launch::may_carry_dev_buy(call.as_ref()) {
                                deadline = tokio::time::Instant::now() + SAME_TX_GRACE;
                                pending = Some(Held {
                                    launch: *l,
                                    call,
                                    lead,
                                    launched_at,
                                    refused,
                                });
                            } else {
                                // Nothing else is coming in that transaction,
                                // so nothing is waited for.
                                print_entry(verbose, &launch::Report {
                                    created: Some(&l),
                                    via: marked,
                                    quote: quotes.get(&pair_token),
                                    call: call.as_ref(),
                                    tax,
                                    lead,
                                    launched_at,
                                    refused: refused.as_deref(),
                                    ..Default::default()
                                });
                            }
                        }
                        // A dev buy with no launch in front of it: the launch
                        // happened before this process was listening.
                        launch::What::DevBuy { .. } => {
                            print_entry(verbose, &launch::Report {
                                dev: Some(&l),
                                via: marked,
                                quote: launch::pair_of(&l, call.as_ref())
                                    .and_then(|t| quotes.get(&t)),
                                call: call.as_ref(),
                                tax,
                                lead,
                                launched_at,
                                ..Default::default()
                            });
                        }
                    }
                }
                None => break,
            },
            // Nothing more came in that transaction.
            _ = tokio::time::sleep_until(deadline), if pending.is_some() => {
                let Held { launch: p, call, lead, launched_at, refused } =
                    pending.take().expect("just checked");
                let marked = call.is_none().then_some("unknown entry point");
                let pair = launch::pair_of(&p, call.as_ref());
                print_entry(verbose, &launch::Report {
                    created: Some(&p),
                    via: marked,
                    quote: pair.and_then(|t| quotes.get(&t)),
                    call: call.as_ref(),
                    tax,
                    lead,
                    launched_at,
                    refused: refused.as_deref(),
                    ..Default::default()
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("ctrl-c received, shutting down");
                break;
            }
        }
    }
    // Last, and unconditionally: a history lost to a shutdown is a history
    // that has to be rebuilt from the journals.
    if let Err(e) = ops.save(&ops_path) {
        tracing::error!(err = %format!("{e:#}"), "cannot write the operator history");
    } else {
        tracing::info!(operators = ops.len(), wallets = ops.wallets(), "operator history saved");
    }
    resolving.abort();
    heading.abort();
    watching.abort();
    if let Some(f) = feeding {
        f.abort();
    }
    if let Some(f) = following {
        f.abort();
    }
    Ok(())
}

/// The chain id, with a few tries before giving up.
///
/// A trading process must not refuse to start because one request was rate
/// limited or landed on a cold endpoint. The answer here is a constant: it will
/// be the same number in two seconds as it is now, so waiting for it costs
/// nothing and dying on it costs the whole session - which is exactly what
/// happened when `fullnode request limit exceeded` came back on the first
/// request of a restart.
async fn chain_id(
    http: &ethers::providers::Provider<ethers::providers::Http>,
) -> anyhow::Result<u64> {
    const TRIES: u32 = 5;
    let mut wait = std::time::Duration::from_secs(2);
    let mut last = String::new();
    for attempt in 1..=TRIES {
        match http.get_chainid().await {
            Ok(id) => {
                tracing::info!(chain_id = %id, attempt, "connected via http");
                return Ok(id.as_u64());
            }
            Err(e) => {
                last = e.to_string();
                tracing::warn!(
                    err = %last, attempt, of = TRIES, retry_in_s = wait.as_secs(),
                    "the http endpoint would not answer"
                );
                if attempt < TRIES {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(std::time::Duration::from_secs(30));
                }
            }
        }
    }
    anyhow::bail!("the http endpoint would not answer eth_chainId after {TRIES} tries: {last}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    fn spent() -> U256 {
        U256::from(1_000u64)
    }

    /// The predicate that keeps a followed launch alive past its minute. Yes
    /// where it should be no strands nothing but leaks a watcher; no where it
    /// should be yes strands the tokens themselves, which is the expensive
    /// direction and the one this exists for.
    #[test]
    fn only_a_launch_that_money_went_into_counts_as_held() {
        // The shadow writes `Bought` on every launch the filters pass, and
        // nothing was sent for any of them.
        let on_paper = snipe::Position::Bought {
            step: 2,
            spend: spent(),
            tokens: U256::from(5u64),
        };
        assert!(!holding(U256::zero(), &on_paper));
        assert!(holding(spent(), &on_paper));

        // A buy in the air is held: the receipt has nowhere else to go.
        assert!(holding(spent(), &snipe::Position::InFlight { step: 2 }));

        // And everything that is over is over.
        for done in [
            snipe::Position::Watching,
            snipe::Position::Skipped { why: "no".into() },
            snipe::Position::Failed { step: 2, why: "reverted".into() },
            snipe::Position::Sold { step: 2 },
        ] {
            assert!(!holding(spent(), &done), "{done:?} read as still held");
        }
    }

    /// A sale that landed has to be the thing that lets go, or the launch is
    /// kept forever waiting for one that already happened.
    #[test]
    fn a_landed_sale_is_what_releases_a_launch() {
        let bought = snipe::Position::Bought {
            step: 1,
            spend: spent(),
            tokens: U256::from(5u64),
        };
        assert!(holding(spent(), &bought));
        let snipe::Position::Bought { step, .. } = bought else { unreachable!() };
        assert!(!holding(spent(), &snipe::Position::Sold { step }));
    }

    /// The window resets and the run does not. The run's total is the number
    /// that cannot be recovered once a window has scrolled past.
    #[test]
    fn a_new_window_keeps_the_run_and_forgets_the_window() {
        let mut t = Tally::default();
        t.opened();
        t.close(250, true);
        t.close(50, false);
        t.refused();
        assert_eq!((t.closed, t.wins, t.kept_closed), (2, 1, 1));
        // A refused buy is not an outcome and must never reach the average.
        assert_eq!((t.failed, t.all_failed, t.closed), (1, 1, 2));
        assert_eq!((t.all_closed, t.all_wins, t.all_x100), (2, 1, 300));
        t.window();
        assert_eq!((t.open, t.closed, t.wins, t.kept_closed, t.x100), (0, 0, 0, 0, 0));
        assert_eq!((t.failed, t.all_failed), (0, 1), "the run forgot a refused buy");
        assert_eq!((t.all_closed, t.all_wins, t.all_x100), (2, 1, 300));
    }

    /// Exactly at cost is not a win. Half the launches close near enough to it
    /// that a `>=` here would report a losing run as a winning one.
    #[test]
    fn breaking_even_is_not_counted_as_a_win() {
        let mut t = Tally::default();
        t.close(100, true);
        assert_eq!(t.wins, 0);
        t.close(101, true);
        assert_eq!(t.wins, 1);
    }

    #[test]
    fn an_average_of_nothing_is_not_a_number() {
        assert_eq!(Tally::average(0, 0), "-");
        assert_eq!(Tally::average(250, 1), "2.50x");
        assert_eq!(Tally::average(300, 2), "1.50x");
    }

    /// The failure this rule exists for: one transaction we counted as used
    /// that the chain never saw. Refusing to go back here is what left the
    /// wallet unable to send anything for the rest of a run.
    #[test]
    fn a_dropped_transaction_gives_its_nonce_back() {
        // Counter at 6, five confirmed and the sixth gone. Nothing else of
        // ours is in the air, so the node's answer is the whole truth.
        assert_eq!(resync_to(6, 5, 0, false), Some(5));
    }

    /// And the failure the old rule was protecting against, which is real: a
    /// nonce that may still land must not be handed out twice.
    #[test]
    fn a_nonce_that_may_still_land_is_not_reused() {
        // Reported pending - it may yet be mined under this very number.
        assert_eq!(resync_to(6, 5, 0, true), None);
        // Or something else of ours is still out there holding a number.
        assert_eq!(resync_to(6, 5, 1, false), None);
        assert_eq!(resync_to(6, 5, 3, true), None);
    }

    /// Forward is always taken: the chain having moved past us means somebody
    /// else spent from this key, or one we gave up on landed after all.
    #[test]
    fn the_chain_moving_ahead_is_always_adopted() {
        assert_eq!(resync_to(6, 9, 0, false), Some(9));
        assert_eq!(resync_to(6, 9, 2, true), Some(9));
    }

    /// Agreement is not a change.
    #[test]
    fn a_count_that_agrees_changes_nothing() {
        assert_eq!(resync_to(6, 6, 0, false), None);
        assert_eq!(resync_to(6, 6, 2, true), None);
    }

    /// The base fee has to come from the newest block, not from a cache that
    /// is up to two hundred blocks old on this chain. The tip does not: it is
    /// a suggestion, and a stale one strands nothing.
    #[test]
    fn the_base_fee_comes_from_the_newest_block() {
        let cached = (U256::from(1_000u64), U256::from(7u64));
        let head = std::sync::RwLock::new(U256::from(100u64));
        // 100 * 2 + 7, and not the 1000 the timer last wrote.
        assert_eq!(fees_now(cached, &head), (U256::from(207u64), U256::from(7u64)));
    }

    /// Before the first header there is nothing better than the cache, and a
    /// zero base fee must not be mistaken for a real one - signing with a
    /// max_fee of the tip alone is a transaction nothing will include.
    #[test]
    fn without_a_header_the_cached_pair_stands() {
        let cached = (U256::from(1_000u64), U256::from(7u64));
        let none = std::sync::RwLock::new(U256::zero());
        assert_eq!(fees_now(cached, &none), cached);
    }

    /// A base fee that has climbed past what the cache allowed for is exactly
    /// the case this exists for: the signed max_fee has to climb with it.
    #[test]
    fn a_risen_base_fee_raises_what_we_sign() {
        let cached = (U256::from(207u64), U256::from(7u64));
        let risen = std::sync::RwLock::new(U256::from(400u64));
        let (max_fee, _) = fees_now(cached, &risen);
        assert!(max_fee > U256::from(400u64), "signed below the base fee");
        assert_eq!(max_fee, U256::from(807u64));
    }

    /// Without an anchor the deadline is zero, which the wrapper refuses. A
    /// guess here would either expire a good transaction or let a stale one
    /// through.
    #[test]
    fn a_deadline_without_a_clock_is_not_guessed_at() {
        let none = std::sync::RwLock::new(None);
        assert_eq!(chain_second(&none), 0);
        let known = std::sync::RwLock::new(Some((1_700u64, std::time::Instant::now())));
        assert_eq!(chain_second(&known), 1_700);
    }
}
