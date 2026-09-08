mod cache;
mod config;
mod curve;
mod depth;
mod execute;
mod executor;
mod exit;
mod feed;
mod inventory;
mod journal;
mod launch;
mod operators;
mod pool;
mod price;
mod route;
mod rpc;
mod snipe;
mod strategy;
mod swap;

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
    const VALUE_FLAGS: [&str; 12] = [
        "--quote",
        "--approve",
        "--swap",
        "--sell-all",
        "--amount",
        "--config",
        "--wrap",
        "--unwrap",
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
    let check_routes = args.iter().any(|a| a == "--check-routes");
    let execute = args.iter().any(|a| a == "--execute");
    let depth_check = args.iter().any(|a| a == "--depth-check");
    let depth_report = args.iter().any(|a| a == "--depth");
    let watch_launches = args.iter().any(|a| a == "--watch-launches");
    // Which launchpads to hear from: comma-separated addresses, or "any" to
    // watch the signature wherever it is emitted.
    let launchpads = flag_value("--launchpad");
    // What a launch would be bought with, in the pair token's own units, and
    // how much of the price to give away. Nothing is sent either way - this
    // only decides whether the entries carry a plan.
    let snipe_size = flag_value("--size");
    let slippage_bps = flag_value("--slippage-bps");
    // How long before a step opens the decision is wanted, in milliseconds.
    let lead_ms = flag_value("--lead-ms");
    let quote_route = flag_value("--quote");
    let swap_route = flag_value("--swap");
    let sell_token = flag_value("--sell-all");
    // Overrides a route's configured amount_in, for asking "what would this
    // size do" without editing config.
    let amount = flag_value("--amount");
    let approve_token = flag_value("--approve");
    let wrap_amount = flag_value("--wrap");
    let unwrap_amount = flag_value("--unwrap");
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
    // Kept whole, because a reload is judged against what this process is
    // actually running on - not against the file as it was last saved.
    let running = cfg.clone();
    // Said out loud, and absolute. `config.toml` is resolved against the
    // working directory, so running the binary by its path from somewhere else
    // reads a different file than the one being edited - and every parameter
    // that decides a trade comes out of it.
    tracing::info!(
        config = %std::fs::canonicalize(&path).unwrap_or(path.clone()).display(),
        "config loaded"
    );

    // Opened before anything resolves a pool, so the first lookup already has
    // somewhere to look.
    match cache::open(std::path::Path::new(&cfg.pool_cache_path)) {
        Ok(0) => tracing::info!(file = %cfg.pool_cache_path, "pool cache empty"),
        Ok(n) => tracing::info!(file = %cfg.pool_cache_path, pools = n, "pool cache loaded"),
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "pool cache unusable, ignoring it"),
    }

    // Before any reader exists, because it is one address for the whole run.
    depth::set_multicall(match cfg.multicall.as_deref() {
        Some(a) => Some(a.parse().context("multicall")?),
        None => depth::MULTICALL3_DEFAULT.parse().ok(),
    });

    // Whatever the config already knows, before anything goes looking for it.
    // A pool older than the endpoint's log history cannot be recovered from the
    // chain at all, and this is the only way to hand it over.
    match pool::seed_cache_from_config(&cfg.pools, &cfg.tokens) {
        0 => {}
        n => tracing::info!(pools = n, "PoolKeys taken from the config"),
    }

    // Sanity check via HTTP JSON-RPC before opening WS subscriptions.
    let http =
        ethers::providers::Provider::<ethers::providers::Http>::try_from(cfg.http_url.clone())?;
    // Asked once and kept. This used to be two calls - a warning here and a
    // fatal one inside `Executor::build` - so a moment of rate limiting at the
    // very first request killed the process over a number it had already asked
    // for, and asked for twice.
    let chain_id = chain_id(&http).await?;

    if check_routes {
        return check_all_routes(&http, &cfg).await;
    }
    if depth_check {
        return depth_check_cmd(&http, &cfg).await;
    }
    if depth_report {
        return depth_report_cmd(&http, &cfg).await;
    }
    if watch_launches {
        return watch_launches_cmd(
            &http,
            &cfg,
            launchpads.as_deref(),
            snipe_size.as_deref(),
            slippage_bps.as_deref(),
            lead_ms.as_deref(),
        )
        .await;
    }
    if let Some(name) = quote_route {
        return quote_route_cmd(&http, &cfg, &name, amount.as_deref()).await;
    }
    if let Some(name) = swap_route {
        return swap_cmd(&http, &cfg, &name, amount.as_deref(), execute).await;
    }
    if let Some(token) = sell_token {
        return sell_all_cmd(&http, &cfg, &token, execute).await;
    }
    if let Some(token) = approve_token {
        return approve_cmd(&http, &cfg, &token, execute).await;
    }
    if let Some(amount) = wrap_amount {
        return wrap_cmd(&http, &cfg, &amount, true, chain_id, execute).await;
    }
    if let Some(amount) = unwrap_amount {
        return wrap_cmd(&http, &cfg, &amount, false, chain_id, execute).await;
    }

    // Armed routes are resolved and checked before the first log arrives: a
    // broken route should stop the process here, not at the one moment it was
    // meant to fire.
    // One shared v4 PoolManager for the whole config: routes need it, and so
    // does every v4 pool that no longer writes it out for itself.
    let manager = pool_manager(&cfg)?;
    let auto = match executor::Executor::build(&http, &cfg, manager, chain_id, execute).await {
        Ok(a) => a,
        // Only routes that asked to be armed can fail here, so this is fatal.
        Err(e) => return Err(e.context("arming auto-buy")),
    };

    // Every pool feeds one channel, and the strategy reads it. The two sides
    // never call each other: a pool reports what happened, the strategy decides
    // what it means, and a slow decision cannot hold up the next report.
    let (ticks, rx) = tokio::sync::mpsc::channel(1024);
    // A second, small channel carries receipts back: what the chain decided
    // about a trade belongs in the same place that decided to make it.
    let (reports_tx, reports_rx) = tokio::sync::mpsc::channel(64);
    let inv = inventory::Inventory::load(std::path::Path::new(&cfg.inventory_path))
        .context("loading the inventory")?;
    if !inv.is_empty() {
        tracing::info!("inventory restored from {}", cfg.inventory_path);
    }
    // The reload task arms and retunes routes on this same executor - never a
    // rebuilt one. The nonce, the broadcaster and the fee stream belong to the
    // process, and handing a buy in flight a second nonce counter is how one
    // buy is lost.
    let arming = auto.clone();
    let mut strategy = strategy::Strategy::new(http.clone(), auto, inv, reports_tx);
    // Resolved first, subscribed after: every pool shares one websocket, so
    // there is nothing to open until it is known which pools there are.
    let mut watched: Vec<pool::Pool> = Vec::new();
    let tokens = cfg.tokens.clone();
    for pool_cfg in &cfg.pools {
        let pool = match pool::Pool::resolve(&http, pool_cfg, &tokens, Some(manager)).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(pool = %pool_cfg.name, err = %e, "failed to resolve pool, skipping");
                continue;
            }
        };
        // A take-profit belongs to the route that bought the position, and the
        // route is armed against this pool, so the two meet here.
        let armed = cfg.armed_route(pool.pool_ref());
        strategy.watch(
            pool.clone(),
            pool_cfg.threshold_pct.unwrap_or(cfg.threshold_pct),
            pool_cfg.max_move_pct.unwrap_or(cfg.max_move_pct),
            armed.and_then(|r| r.take_profit_pct),
            armed.and_then(|r| r.exit_after_secs),
        );
        watched.push(pool);
    }
    // Every pool failing to resolve used to leave this empty, which made the
    // process exit 0 in silence.
    anyhow::ensure!(
        strategy.watching() > 0,
        "no pools could be resolved; nothing to watch"
    );
    tracing::info!(pools = strategy.watching(), "watching");

    // What the feed follows. The strategy publishes it after every reload, so
    // a pool added or dropped changes ONE subscription on the live socket
    // instead of reconnecting all of them - a reconnect to add one pool is a
    // gap in every other pool's feed.
    let (pool_set, pool_set_rx) = tokio::sync::watch::channel(watched.clone());
    strategy.publishes_pools_to(pool_set);

    // One task for all of them, and one backoff. Per pool it was a reconnect
    // storm multiplied by their number against an endpoint already refusing.
    let feeds = tokio::spawn({
        let ws = cfg.ws_url.clone();
        async move {
            // Exponential backoff so a persistently failing endpoint is not
            // hammered; reset once the connection has run for a while.
            let mut backoff = std::time::Duration::from_secs(3);
            loop {
                let started = std::time::Instant::now();
                match feed::run_all(pool_set_rx.clone(), ws.clone(), ticks.clone()).await {
                    Ok(()) => tracing::warn!("feed closed, reconnecting"),
                    Err(e) => tracing::error!(err = %format!("{e:#}"), "feed error, reconnecting"),
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

    // A reload is asked for, not watched for: an editor saving a half-written
    // file must not retune a bot holding positions, and `kill -HUP` is somebody
    // saying the file is ready to be read.
    //
    // Depth one, because a second SIGHUP arriving before the first was applied
    // is the same request twice: the loader reads the file at the moment it
    // sends, so what is queued is never staler than the signal behind it.
    let (reloads_tx, reloads_rx) = tokio::sync::mpsc::channel::<strategy::Reload>(1);
    #[cfg(unix)]
    {
        let path = path.clone();
        let http = http.clone();
        let arming = arming.clone();
        tokio::spawn(async move {
            let mut hup =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(err = %e, "cannot listen for SIGHUP; no config reload");
                        return;
                    }
                };
            while hup.recv().await.is_some() {
                // A file that will not parse changes nothing. This process is
                // holding positions; the one thing a typo must not do is take
                // the bot down or leave it running on half a config.
                let next = match config::Config::load(&path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(err = %format!("{e:#}"),
                            "config reload REFUSED; the running config stands");
                        continue;
                    }
                };
                for field in config::restart_only(&running, &next) {
                    tracing::warn!(field, "changed in the file, but only a restart applies it");
                }
                // Resolved here, off the loop that decides trades: this asks
                // the chain for decimals, symbols and PoolKeys, and a tick
                // arriving meanwhile must not wait behind it. One pool that
                // will not resolve is left out and the rest are still applied.
                let mut pools = Vec::with_capacity(next.pools.len());
                for pc in &next.pools {
                    match pool::Pool::resolve(&http, pc, &next.tokens, Some(manager)).await {
                        Ok(p) => pools.push(p),
                        Err(e) => tracing::error!(
                            pool = %pc.name, err = %format!("{e:#}"),
                            "could not resolve on reload; leaving it out"
                        ),
                    }
                }
                // Routes first, and on the executor that is already
                // running: a pool the strategy is about to watch then finds its
                // route already armed, and one it is about to drop cannot fire
                // in between. Each route is applied on its own, so one that
                // will not resolve leaves the others trading.
                if let Some(exec) = &arming {
                    let applied = exec.apply_routes(&next).await;
                    tracing::info!(applied, "routes reloaded");
                } else if next.routes.iter().any(|r| r.auto_buy) {
                    tracing::warn!(
                        "no route was armed at startup, so there is no wallet or broadcaster to \
                         arm one with; arming needs a restart"
                    );
                }
                // Whatever the chain had to be asked for is worth keeping.
                cache::flush();
                if reloads_tx
                    .send(strategy::Reload { cfg: next, pools })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
    }

    // Everything that had to be looked up has been; keep it for next time.
    cache::flush();
    strategy.resolve_pending().await;
    strategy.seed_inventory().await;
    let decisions = tokio::spawn(strategy.run(rx, reports_rx, reloads_rx));

    tokio::select! {
        _ = feeds => {}
        _ = decisions => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received, shutting down");
        }
    }
    Ok(())
}

/// What to spend on this launch: a share of the curve, capped in the pair
/// token's own units when a ceiling is set.
///
/// A share rather than an amount because our own buying and selling is what
/// moves the price against us, and what decides that is the fraction of the
/// curve taken - not the number of tokens spent. The same 0.05 is three
/// percent of a native curve and nothing at all of a USDG one.
fn spend_for(
    size_x100: u64,
    cap: Option<&str>,
    decimals: u8,
    c: &curve::Curve,
) -> ethers::types::U256 {
    let want = c.quote_reserve * ethers::types::U256::from(size_x100)
        / ethers::types::U256::from(10_000u64);
    match cap.and_then(|s| route::parse_units(s, decimals).ok()) {
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
    call: Option<&launch::LaunchCall>,
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
        // Unknown means unknown: a creator tax read as zero would price every
        // step better than it is.
        call?.creator_tax_bps as u64,
    )
    .ok()
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

/// Watch launchpads and say what launched. Nothing else.
///
/// The sniper's eyes before it has hands: it reads no wallet, holds no
/// inventory and cannot send anything, so it is safe to leave running next to
/// the fall bot while what to do about a launch is still being decided.
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
async fn watch_launches_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    pads: Option<&str>,
    size: Option<&str>,
    slippage_bps: Option<&str>,
    lead_ms: Option<&str>,
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
                supply = %route::format_units(c.supply, 18),
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

    /// One followed launch, once its minute is up.
    ///
    /// The trades are in the file. What is worth a line here is where the
    /// curve ended up and how far it got on the way, because that is the
    /// question an entry made at +1s is answered by.
    fn done_line(curve: ethers::types::Address, f: &Followed) -> String {
        let opened = f.opening.map(|o| o.quote_reserve).unwrap_or_default();
        let run = |v: ethers::types::U256| match route::u256_to_f64(opened) {
            o if o > 0.0 => format!("x{:.2}", route::u256_to_f64(v) / o),
            _ => "x?".to_string(),
        };
        format!(
            "    done   {}  {} buys {} sells  peak {}  last {}  {}",
            launch::short_addr(&curve),
            f.buys,
            f.sells,
            run(f.peak_quote),
            run(f.curve.quote_reserve),
            match &f.position {
                snipe::Position::Skipped { why } => format!("skipped: {why}"),
                p => format!("{p:?}"),
            },
        )
    }
    let mut followed: std::collections::HashMap<ethers::types::Address, Followed> =
        std::collections::HashMap::new();
    // Trades that arrived before their curve was being followed. The dev buy
    // is emitted in the launch transaction itself, so it reaches the trade
    // subscription at the same moment the launch reaches the log one - and
    // whichever wins, the reserves have to end up counting it. Missing it left
    // every later quote on that curve one buy stale, which is exactly the
    // MODEL OFF BY it produced.
    let mut early: std::collections::VecDeque<(ethers::types::Address, u64, curve::Trade)> =
        std::collections::VecDeque::new();

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
    let exit_policy = exit::Policy {
        trail_bps: cfg.snipe.trail_bps,
        take_x100: cfg.snipe.take_x100,
        hold_blocks: cfg.snipe.hold_blocks,
        slippage_bps,
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
                // A launch is followed for a minute. Shed the ones whose
                // minute is up here rather than waiting for a trade on them:
                // a curve nobody trades on would otherwise be watched forever,
                // and the set the subscription filters on would only grow.
                followed.retain(|curve, f| {
                    let keep = f.until > now;
                    if !keep {
                        println!("{}", done_line(*curve, f));
                        if f.exempt_known && f.outsiders.is_empty() {
                            ops.note_dead(f.operator);
                            ops_dirty = true;
                        }
                        if let Ok(mut w) = watched_curves.write() {
                            w.remove(curve);
                        }
                    }
                    keep
                });
                // Written on a timer rather than on every change: a busy
                // minute changes it hundreds of times, and the file is only
                // ever read at startup.
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
                        println!(
                            "{}",
                            snipe::render(&signal, &decision, *curve_addr, now.elapsed())
                        );
                        if let Err(e) = journal::append(
                            &f.journal,
                            &journal::decision_line(&signal, &decision),
                        ) {
                            tracing::warn!(err = %format!("{e:#}"), "cannot write the decision");
                        }
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
                            snipe::Decision::Buy { spend, .. } if f.shadow.is_none() => {
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
                                    }
                                    Err(e) => tracing::warn!(
                                        curve = ?curve_addr, err = %format!("{e:#}"),
                                        "the buy this decided on cannot be priced"
                                    ),
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            got = launches.recv() => match got {
                // The sequencer took a launch: everything the calldata says,
                // before the block that will carry it exists.
                Some(launch::Heard::Incoming(i)) => {
                    if reported.contains(&i.tx) {
                        tracing::debug!(tx = ?i.tx, "the feed caught up with a launch already printed");
                        continue;
                    }
                    tracing::debug!(
                        "{}",
                        launch::render_incoming(&i, quotes.get(&i.call.pair_token))
                    );
                    // Asked for AFTER the line is printed, so nothing waits on
                    // it: the answer is for the launches after this one, and
                    // for the entry this one will turn into.
                    if !quotes.contains_key(&i.call.pair_token) {
                        if let Some((q, e)) = quote_of(http, factory, i.call.pair_token).await {
                            if let Some(e) = e {
                                economics.insert(i.call.pair_token, e);
                            }
                            if order.len() >= REMEMBERED {
                                if let Some(old) = order.pop_front() {
                                    quotes.remove(&old);
                                }
                            }
                            order.push_back(i.call.pair_token);
                            quotes.insert(i.call.pair_token, q);
                            cache::flush();
                        }
                    }
                    if seen_order.len() >= REMEMBERED {
                        if let Some(old) = seen_order.pop_front() {
                            sightings.remove(&old);
                        }
                    }
                    seen_order.push_back(i.tx);
                    sightings.insert(i.tx, (i.seen, i.call, i.chain_time));
                }
                Some(launch::Heard::Trade { curve: at, block, trade }) => {
                    // A launch is followed for a minute and then let go. The
                    // sweep below only ran when the map filled up, so a quiet
                    // hour left curves being written to long after anything
                    // about them was still being decided.
                    if followed.get(&at).is_some_and(|f| f.until <= std::time::Instant::now()) {
                        if let Some(f) = followed.remove(&at) {
                            println!("{}", done_line(at, &f));
                        }
                        if let Ok(mut w) = watched_curves.write() {
                            w.remove(&at);
                        }
                        continue;
                    }
                    let Some(f) = followed.get_mut(&at) else {
                        // Not following it yet, but told to watch it: this is
                        // the launch\'s own dev buy, racing its launch log.
                        if early.len() >= 256 {
                            early.pop_front();
                        }
                        early.push_back((at, block, trade));
                        continue;
                    };
                    // Predicted BEFORE applying, which is the order a live
                    // quote would run in.
                    // The model against the chain, on a trade nobody
                    // arranged. Not a log line but an alarm: a buy priced off
                    // reserves that have drifted is a buy sized wrong.
                    if let (Some(p), curve::Trade::Buy { tokens_out, .. }) =
                        (curve::predicted_tokens_out(&f.curve, &trade), &trade)
                    {
                        if p != *tokens_out {
                            tracing::warn!(
                                curve = ?at,
                                off_by = %launch::tokens_of(p.abs_diff(*tokens_out)),
                                "the model and the chain disagree on a fill"
                            );
                        }
                    }
                    if let Err(e) = f.curve.apply(&trade) {
                        // The reserves being followed are not the curve\'s any
                        // more. Nothing priced from them is worth anything, so
                        // the curve is dropped rather than carried on with.
                        tracing::warn!(
                            curve = ?at, err = %format!("{e:#}"),
                            "lost track of a curve; no longer following it"
                        );
                        followed.remove(&at);
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
                    f.peak_quote = f.peak_quote.max(f.curve.quote_reserve);
                    f.last_block = block;
                    newest_block = newest_block.max(block);
                    // The shadow, against the curve this trade just made. The
                    // high is marked BEFORE the question is asked, or the stop
                    // measures a give-back from a peak it has not seen yet.
                    if let Some(h) = f.shadow.as_mut() {
                        h.mark(exit::worth(&f.curve, h.tokens));
                        let decision = exit::decide(h, &f.curve, block, &exit_policy);
                        if let exit::Exit::Sell { worth, why, .. } = &decision {
                            println!(
                                "{}",
                                exit::render(h, &decision, f.quote_decimals, &f.facts.quote_symbol)
                            );
                            if let Err(e) = journal::append(
                                &f.journal,
                                &journal::exit_line(h, *worth, why, block),
                            ) {
                                tracing::warn!(err = %format!("{e:#}"), "cannot write the exit");
                            }
                            ops.record(f.operator, h.x100(*worth));
                            ops_dirty = true;
                            f.shadow = None;
                        }
                    }
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
                        if let Some(f) = followed.remove(&at) {
                            println!("{}  graduated", done_line(at, &f));
                        }
                        if let Ok(mut w) = watched_curves.write() {
                            w.remove(&at);
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
                        "the factory\'s owner changed the snipe tax"
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
                        "the factory\'s owner changed the snipe tax window"
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
                        let pair = launch::pair_of(&p, call.as_ref());
                        println!("{}", launch::render(&launch::Report {
                            created: Some(&p),
                            dev: Some(&l),
                            launched_at,
                            quote: pair.and_then(|t| quotes.get(&t)),
                            call: call.as_ref(),
                            tax,
                            lead,
                            refused: refused.as_deref(),
                        }));
                        continue;
                    }
                    // Anything else means the held launch had no dev buy in it.
                    if let Some(Held { launch: p, call, lead, launched_at, refused }) =
                        pending.take()
                    {
                        let pair = launch::pair_of(&p, call.as_ref());
                        println!("{}", launch::render(&launch::Report {
                            created: Some(&p),
                            quote: pair.and_then(|t| quotes.get(&t)),
                            call: call.as_ref(),
                            tax,
                            lead,
                            launched_at,
                            refused: refused.as_deref(),
                            ..Default::default()
                        }));
                    }

                    // The feed already carried this transaction, so its calldata
                    // is in hand and the request for it is not made at all.
                    let (call, lead, launched_at) = match sightings.remove(&l.tx) {
                        Some((seen, call, at)) => (Some(call), Some(seen.elapsed()), Some(at)),
                        None => (launch_call(http, l.tx).await, None, None),
                    };
                    // The feed stamps every block it carries, not only the ones
                    // whose launches it caught - so a launch it missed still
                    // has a second, and without this one every trade on that
                    // curve loses its place in the tax window.
                    let launched_at = match launched_at.or_else(|| {
                        block_seconds.read().ok().and_then(|s| s.get(&l.block).copied())
                    }) {
                        Some(at) => Some(at),
                        // Neither source had it, which on a healthy feed does
                        // not happen and without one happens every time.
                        None => block_second(http, l.block).await,
                    };

                    match l.what {
                        launch::What::Created { pair_token, .. } => {
                            let mut refused: Option<String> = None;
                            // Both maps, not just the quote: the native pair
                            // is seeded with its decimals at startup, so
                            // gating on the quote alone meant its economics -
                            // and with them every plan for a native launch,
                            // which is most of them - were never read at all.
                            if !quotes.contains_key(&pair_token)
                                || !economics.contains_key(&pair_token)
                            {
                                if let Some((q, e)) = quote_of(http, factory, pair_token).await {
                                    if let Some(e) = e {
                                        economics.insert(pair_token, e);
                                    }
                                    if order.len() >= REMEMBERED {
                                        if let Some(old) = order.pop_front() {
                                            quotes.remove(&old);
                                        }
                                    }
                                    order.push_back(pair_token);
                                    quotes.insert(pair_token, q);
                                    cache::flush();
                                }
                            }
                            // Follow this curve from here, at the reserves it
                            // opens with. Every trade on it after this moves
                            // them exactly, out of its own logs, so what a buy
                            // would get is known without asking anyone.
                            //
                            // Unless its terms rule it out. Those are known
                            // before it has traded and they do not change, so a
                            // launch refused here is not watched, not journalled
                            // and not asked about again.
                            if let Some(opening) = opening_curve(
                                config.as_ref(),
                                economics.get(&pair_token),
                                call.as_ref(),
                                launch::threshold_of(&l).unwrap_or_default(),
                            ) {
                                if followed.len() >= 64 {
                                    let now = std::time::Instant::now();
                                    followed.retain(|_, f| f.until > now);
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
                                    creator_tax_bps: call
                                        .as_ref()
                                        .map(|c| c.creator_tax_bps as u64)
                                        .unwrap_or(0),
                                    exempt: exempt.len(),
                                    via: call.as_ref().map(|c| c.via).unwrap_or(""),
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
                                if let Some(why) = &refused {
                                    // Loud, and on its own line: the entry
                                    // below says it too, but a refusal is what
                                    // the policy DID, and a run of them is how
                                    // a filter set too tight is noticed. One
                                    // greppable line per launch passed over.
                                    tracing::warn!(
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
                                if refused.is_some() {
                                    if let Ok(mut w) = watched_curves.write() {
                                        w.remove(&l.curve);
                                    }
                                    early.retain(|(c, _, _)| *c != l.curve);
                                } else {
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
                                        ),
                                    ) {
                                        tracing::warn!(
                                            err = %format!("{e:#}"),
                                            "cannot write the launch journal"
                                        );
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
                                        outsiders: Default::default(),
                                        peak_quote: opening.quote_reserve,
                                    };
                                    // Whatever already happened on it, in the
                                    // order it happened.
                                    while let Some(i) =
                                        early.iter().position(|(c, _, _)| *c == l.curve)
                                    {
                                        let (_, _, t) = early.remove(i).expect("just found");
                                        if let Err(e) = f.curve.apply(&t) {
                                            tracing::warn!(
                                                curve = ?l.curve, err = %format!("{e:#}"),
                                                "a trade from before we were following does not fit"
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
                                println!("{}", launch::render(&launch::Report {
                                    created: Some(&l),
                                    quote: quotes.get(&pair_token),
                                    call: call.as_ref(),
                                    tax,
                                    lead,
                                    launched_at,
                                    refused: refused.as_deref(),
                                    ..Default::default()
                                }));
                            }
                        }
                        // A dev buy with no launch in front of it: the launch
                        // happened before this process was listening.
                        launch::What::DevBuy { .. } => {
                            println!("{}", launch::render(&launch::Report {
                                dev: Some(&l),
                                quote: launch::pair_of(&l, call.as_ref())
                                    .and_then(|t| quotes.get(&t)),
                                call: call.as_ref(),
                                tax,
                                lead,
                                launched_at,
                                ..Default::default()
                            }));
                        }
                    }
                }
                None => break,
            },
            // Nothing more came in that transaction.
            _ = tokio::time::sleep_until(deadline), if pending.is_some() => {
                let Held { launch: p, call, lead, launched_at, refused } =
                    pending.take().expect("just checked");
                let pair = launch::pair_of(&p, call.as_ref());
                println!("{}", launch::render(&launch::Report {
                    created: Some(&p),
                    quote: pair.and_then(|t| quotes.get(&t)),
                    call: call.as_ref(),
                    tax,
                    lead,
                    launched_at,
                    refused: refused.as_deref(),
                    ..Default::default()
                }));
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

/// Resolve every configured route and print what was recovered, without
/// touching a wallet. Every PoolKey is verified against its pool id, so a wrong
/// id or a broken chain fails here rather than at swap time.
/// Look at what every watched pool's liquidity actually looks like.
///
/// Read-only, manual, and deliberately heavier than anything the bot does on
/// its own: it asks the questions a person asks when a trade was refused, and
/// the answers are worth several requests each when somebody is waiting for
/// them and nothing is racing.
///
/// Three things, in the order they get asked:
///
/// 1. what the pool is right now - price, tick, liquidity, and the fee that is
///    actually charged rather than the one the PoolKey states;
/// 2. what it costs to move it, both ways, at several sizes. This is the
///    number that says whether a route's `amount_in` is sized for the pool;
/// 3. where the liquidity boundaries sit, and - the reason this exists - HOW
///    FAR the ladder the bot keeps in memory actually reaches in price. A quote
///    is refused when a swap walks past that, and `LADDER_TICKS` bounds it in
///    ticks while the swap cares about percent. On a densely provided pool the
///    two are very different numbers.
async fn depth_report_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
) -> anyhow::Result<()> {
    let manager = pool_manager(cfg).ok();
    for pc in &cfg.pools {
        let pool = match pool::Pool::resolve(http, pc, &cfg.tokens, manager).await {
            Ok(p) => p,
            Err(e) => {
                println!("\n{}\n  skipped: {e:#}", pc.name);
                continue;
            }
        };
        let (Some(spacing), Some(source)) = (pool.tick_spacing, pool.tick_source()) else {
            println!("\n{}\n  skipped: no tick source", pool.name);
            continue;
        };
        let reader = depth::TickReader::new(http, source, spacing)?;
        let state = match depth::read_state(&reader).await {
            Ok(s) => s,
            Err(e) => {
                println!("\n{}\n  could not read state: {e:#}", pool.name);
                continue;
            }
        };

        let price =
            |sqrt: f64| price::from_sqrt(sqrt, pool.decimals0, pool.decimals1, pool.base_token);
        let base = pool.base_symbol.clone().unwrap_or_else(|| "base".into());
        let quote = pool.quote_symbol.clone().unwrap_or_else(|| "quote".into());
        let base_decimals = match pool.base_token {
            1 => pool.decimals1,
            _ => pool.decimals0,
        };

        println!("\n{}  {}", pool.name, pool.pool_ref());
        println!(
            "  price      {:.10} {quote} per {base}    tick {}   spacing {spacing}",
            price(state.sqrt_p),
            depth::tick_at_sqrt(state.sqrt_p)
        );
        println!(
            "  liquidity  {}    lp fee {} bps   swap fee {}/{} bps (0->1 / 1->0)",
            state.liquidity,
            state.lp_fee as f64 / 100.0,
            state.swap_fee(true) as f64 / 100.0,
            state.swap_fee(false) as f64 / 100.0,
        );

        // 2. What it costs to move it, in the token actually paid each way.
        println!("\n  cost to move the {base} price");
        println!("     move        buy ({quote} in)          sell ({base} in)");
        for move_pct in [0.5f64, 1.0, 2.0, 5.0, 10.0] {
            // Lifting the base price is paid in the quote token; pushing it
            // down is paid in the base, which is the same walk mirrored - so
            // the two differ only in which side is called the base.
            let mut both = Vec::new();
            for (side, decimals) in [
                (pool.base_token, pool.quote_decimals()),
                (1 - pool.base_token, base_decimals),
            ] {
                both.push(
                    depth::pay_to_move(
                        &reader,
                        state.sqrt_p,
                        state.liquidity,
                        side,
                        move_pct,
                        state.swap_fee(side != 0),
                    )
                    .await
                    .map(|v| v / 10f64.powi(decimals as i32)),
                );
            }
            let down = both.pop().expect("two sides");
            let up = both.pop().expect("two sides");
            let show = |r: &anyhow::Result<f64>| match r {
                Ok(v) => format!("{v:>18.6}"),
                Err(_) => format!("{:>18}", "-"),
            };
            println!("    {:>5.1}%   {}   {}", move_pct, show(&up), show(&down));
        }

        // 3. Where the boundaries are, and how far the bot can walk from here.
        let window = match depth::tick_window(&reader, state.sqrt_p).await {
            Ok(w) => w,
            Err(e) => {
                println!("\n  could not scan ticks: {e:#}");
                continue;
            }
        };
        let (lo, hi) = window.span();
        let (llo, lhi) = window.ladder_span();
        let pct = |s: f64| ((s / state.sqrt_p).powi(2) - 1.0) * 100.0;
        println!(
            "\n  scan       {} tick(s) found, {} with liquidity read",
            window.edges(),
            window.ladder()
        );
        println!(
            "  bitmap     {:+.1}% .. {:+.1}%   (where the ticks ARE)",
            pct(lo),
            pct(hi)
        );
        // An empty ladder is only "nothing" when nothing was read. A pool
        // provided across its whole range has no initialized ticks near its
        // price at all, and the scan reading none of them is the answer, not
        // the absence of one - that pool can be priced anywhere.
        if llo > lhi {
            println!("  walkable   nothing - no quote can be modelled from this pool");
        } else if window.edges() == 0 {
            println!(
                "  walkable   {:+.1}% .. {:+.1}%   (no ticks at all: liquidity is constant \
                 across the whole scan)",
                pct(llo),
                pct(lhi)
            );
        } else {
            println!(
                "  walkable   {:+.1}% .. {:+.1}%   (how far a swap may be PRICED from memory)",
                pct(llo),
                pct(lhi)
            );
        }

        println!("\n     tick        price          from here    liquidityNet");
        let mut shown = 0;
        for (sqrt, net) in window.profile() {
            let away = pct(sqrt);
            // Only the neighbourhood: a wide scan on a busy pool has hundreds,
            // and the ones a trade could reach are the ones worth reading.
            if away.abs() > 25.0 {
                continue;
            }
            println!(
                "  {:>9}   {:>12.10}   {:>+9.2}%   {}",
                depth::tick_at_sqrt(sqrt),
                price(sqrt),
                away,
                match net {
                    Some(n) => n.to_string(),
                    None => "not read".to_string(),
                }
            );
            shown += 1;
        }
        if shown == 0 {
            println!("     (none within 25% of the price)");
        }
    }
    cache::flush();
    Ok(())
}

/// Prove that reading storage in batches changed no number.
///
/// `TickReader` now pulls a whole scan window through the manager's
/// `extsload(bytes32[])` instead of asking for one slot at a time. The
/// arithmetic is untouched, but "untouched" is a claim, and this is a
/// financial system - so both paths are run against the SAME pinned block and
/// their answers compared. A pinned block is the whole point: unpinned, the two
/// walks would read a pool that moved between them and disagree for a reason
/// that has nothing to do with batching.
async fn depth_check_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
) -> anyhow::Result<()> {
    use ethers::providers::Middleware;
    // Two behind the head: a block the node has just announced is not always
    // readable yet, which is the `header not found` calibration keeps hitting.
    let at = http.get_block_number().await?.as_u64().saturating_sub(2);
    let manager = pool_manager(cfg).ok();
    println!("comparing tick walks at block {at}\n");

    let mut checked = 0;
    let mut differed = 0;
    let mut walk_differed = 0;
    let mut walk_checked = 0;
    for pc in &cfg.pools {
        let pool = match pool::Pool::resolve(http, pc, &cfg.tokens, manager).await {
            Ok(p) => p,
            Err(e) => {
                println!("{:24} skipped: {e:#}", pc.name);
                continue;
            }
        };
        let (Some(spacing), Some(source)) = (pool.tick_spacing, pool.tick_source()) else {
            println!("{:24} skipped: no tick source", pool.name);
            continue;
        };
        let move_pct = pc.max_move_pct.unwrap_or(cfg.max_move_pct);

        let mut answers = Vec::new();
        for batched in [true, false] {
            let reader = depth::TickReader::new(http, source.clone(), spacing)?.at_block(Some(at));
            let reader = if batched { reader } else { reader.unbatched() };
            let state = depth::read_state(&reader).await?;
            let started = std::time::Instant::now();
            let pay = depth::pay_to_move(
                &reader,
                state.sqrt_p,
                state.liquidity,
                pool.base_token,
                move_pct,
                state.swap_fee(pool.base_token == 0),
            )
            .await?;
            answers.push((pay, started.elapsed().as_millis()));
        }

        let (batched, t_batched) = answers[0];
        let (plain, t_plain) = answers[1];
        // Exact equality is the right test: the same slots read the same way
        // feed the same f64 arithmetic in the same order. Anything else means
        // the batch read a different pool state, and "close enough" would be
        // exactly the wrong thing to accept.
        let same = batched == plain;
        checked += 1;
        if !same {
            differed += 1;
        }
        println!(
            "{:24} {}  batched {batched:.6} in {t_batched:>5} ms   one slot at a time \
             {plain:.6} in {t_plain:>5} ms",
            pool.name,
            if same { "same " } else { "DIFFER" },
        );

        // The second claim, and the one that decides real trades: a swap walked
        // from the ladder the background scan reads must be the SAME swap the
        // chain walk produces. They share `swap_exact_in_along`, so what this
        // pins is not the arithmetic but the ladder - that the cached rungs are
        // the same ticks, in the same order, that `next_initialized` selects.
        //
        // One reader for both, pinned to one block: the walks must differ over
        // the data or not at all, and an unpinned pair would differ because the
        // pool moved between them.
        let reader = depth::TickReader::new(http, source.clone(), spacing)?.at_block(Some(at));
        let state = depth::read_state(&reader).await?;
        // The size that moves this pool by its own `max_move_pct`, so the walk
        // is asked something the pool can actually feel.
        let amount = batched;
        // Paying to lift the base token's price: token1 in when the base is
        // token0, and the mirror image otherwise.
        let zero_for_one = pool.base_token != 0;
        let window = depth::tick_window(&reader, state.sqrt_p).await?;
        match window.ladder_from(state.sqrt_p, !zero_for_one) {
            Some(ladder) => {
                let cached = depth::swap_exact_in_along(
                    state,
                    zero_for_one,
                    amount,
                    &ladder.rungs,
                    depth::Beyond::HoldsUntil(ladder.bound),
                )?;
                let chain = depth::swap_exact_in(&reader, state, zero_for_one, amount).await?;
                walk_checked += 1;
                match cached {
                    depth::Walk::Done(local) if local == chain => println!(
                        "{:24} same   walk from the cached ladder matches the chain: \
                         {:.6} out, {} tick(s) crossed",
                        "", local.amount_out, local.ticks_crossed
                    ),
                    depth::Walk::Done(local) => {
                        walk_differed += 1;
                        println!(
                            "{:24} DIFFER cached {:.6} ({} ticks) vs chain {:.6} ({} ticks)",
                            "",
                            local.amount_out,
                            local.ticks_crossed,
                            chain.amount_out,
                            chain.ticks_crossed
                        );
                    }
                    // Not a disagreement: the ladder is read near the price and
                    // this size walked past it. The model refuses exactly here.
                    depth::Walk::NeedsRung => println!(
                        "{:24} n/a    this size walks past the cached ladder ({} rung(s))",
                        "",
                        ladder.rungs.len()
                    ),
                }
            }
            None => println!("{:24} n/a    no ladder covers this pool's price", ""),
        }
    }

    println!(
        "\n{checked} pool(s) checked, {differed} differed on batching; \
         {walk_checked} walk(s) compared, {walk_differed} differed"
    );
    anyhow::ensure!(
        differed == 0,
        "batched reads changed an answer - do not ship this"
    );
    anyhow::ensure!(
        walk_differed == 0,
        "the cached ladder walks a swap differently from the chain - do not ship this"
    );
    Ok(())
}

async fn check_all_routes(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
) -> anyhow::Result<()> {
    anyhow::ensure!(!cfg.routes.is_empty(), "no [[routes]] configured");
    // The same resolution the rest of the process uses; it was a second copy of
    // it here, which is one place for the two to drift apart.
    let manager = pool_manager(cfg)?;
    tracing::info!(?manager, routes = cfg.routes.len(), "checking routes");

    let mut failed = 0;
    for rc in &cfg.routes {
        match route::Route::resolve(http, manager, rc, &cfg.tokens, weth(cfg)?).await {
            Ok(r) => {
                println!("\nroute \"{}\"  OK", r.name);
                println!(
                    "  aim to move the trigger pool {}%  spending {}  ->  receive {} (slippage cap {}%)",
                    r.impact_pct,
                    r.input.symbol,
                    r.output.symbol,
                    r.max_slippage_pct
                );
                for (i, h) in r.hops.iter().enumerate() {
                    println!(
                        "  hop {i}: {}  zeroForOne={}",
                        h.describe(),
                        h.zero_for_one()
                    );
                    println!("         {:?} -> {:?}", h.input, h.output);
                }
            }
            Err(e) => {
                failed += 1;
                println!("\nroute \"{}\"  FAILED\n  {e:#}", rc.name);
            }
        }
    }
    cache::flush();
    anyhow::ensure!(failed == 0, "{failed} route(s) failed to resolve");
    Ok(())
}

/// Move between the native currency and its wrapper, by hand.
///
/// A route that trades a native pool while holding the wrapped token needs the
/// wrapped token to exist first, and the native balance kept for gas has to be
/// refilled from somewhere. Both are plain calls to the wrapper - no router, no
/// pool, nothing this chain's fork could have changed.
async fn wrap_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    amount: &str,
    wrapping: bool,
    chain_id: u64,
    execute: bool,
) -> anyhow::Result<()> {
    let weth = weth(cfg)?.context("set `weth` in the config to wrap or unwrap")?;
    // The wrapper matches the native currency it wraps, and every one of them
    // has eighteen decimals.
    let amount_raw = route::parse_units(amount, 18)
        .with_context(|| format!("--{} {amount}", if wrapping { "wrap" } else { "unwrap" }))?;

    let wallet = swap::load_wallet(cfg, chain_id)?;
    let owner = ethers::signers::Signer::address(&wallet);
    let native = swap::balance_of(http, ethers::types::Address::zero(), owner).await?;
    let wrapped = swap::balance_of(http, weth, owner).await?;

    println!(
        "\n{} {} for {owner:?}",
        if wrapping { "wrap" } else { "unwrap" },
        amount
    );
    println!("  weth     {weth:?}");
    println!("  native   {}", route::format_units(native, 18));
    println!("  wrapped  {}", route::format_units(wrapped, 18));

    // Said here rather than discovered as a revert, and said about the side
    // that is actually short.
    let (have, what) = match wrapping {
        true => (native, "native"),
        false => (wrapped, "wrapped"),
    };
    anyhow::ensure!(
        have >= amount_raw,
        "{what} balance is {} but this moves {amount}",
        route::format_units(have, 18)
    );
    if wrapping {
        // Wrapping every last wei leaves nothing to pay for the transaction
        // doing it, which is a call that cannot be made.
        anyhow::ensure!(
            native > amount_raw,
            "wrapping the whole native balance leaves nothing for the gas to send it with"
        );
    }

    let tx = match wrapping {
        true => swap::build_wrap(weth, amount_raw)?,
        false => swap::build_unwrap(weth, amount_raw)?,
    };
    println!("\ntransaction:");
    tx.print(0);

    if !execute {
        println!("\ndry run - nothing sent. Re-run with --execute to submit.");
        return Ok(());
    }
    println!("\nsubmitting from {owner:?}");
    swap::send_all(http, wallet, std::slice::from_ref(&tx)).await
}

/// The wrapped native token, when one is configured.
fn weth(cfg: &config::Config) -> anyhow::Result<Option<ethers::types::Address>> {
    cfg.weth
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(Into::into)
}

fn pool_manager(cfg: &config::Config) -> anyhow::Result<ethers::types::Address> {
    match &cfg.pool_manager {
        Some(a) => Ok(a.parse()?),
        // Still inferred from a v4 pool that names one, so a config written
        // before `pool_manager` existed keeps working untouched.
        None => cfg
            .pools
            .iter()
            .filter(|p| p.version == "v4")
            .find_map(|p| p.address.as_ref())
            .map(|a| a.parse())
            .transpose()?
            .context("set pool_manager, or give at least one v4 pool an address to infer it from"),
    }
}

/// Simulate a route end to end and print what it would produce. Read-only:
/// no wallet is loaded and nothing is sent.
async fn quote_route_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    name: &str,
    amount: Option<&str>,
) -> anyhow::Result<()> {
    let rc = cfg
        .routes
        .iter()
        .find(|r| r.name == name)
        .with_context(|| format!("no route named '{name}' in config"))?;
    let manager = pool_manager(cfg)?;
    let r = route::Route::resolve(http, manager, rc, &cfg.tokens, weth(cfg)?).await?;
    let amount_in = manual_amount(&r, amount)?;
    let q = r.quote(http, manager, None, amount_in).await?;

    println!("\nroute \"{}\"", r.name);
    println!(
        "  spend  {} {}",
        route::format_units(amount_in, r.input.decimals),
        r.input.symbol
    );
    for (i, h) in q.hops.iter().enumerate() {
        println!(
            "  hop {i}: {} -> {}  fee={}bps (lp {}bps + protocol {}bps) \
             ticks_crossed={} impact={:+.3}%",
            route::format_units(route::f64_to_u256_pub(h.amount_in), h.input_decimals),
            route::format_units(route::f64_to_u256_pub(h.amount_out), h.output_decimals),
            h.swap_fee() as f64 / 100.0,
            h.lp_fee as f64 / 100.0,
            h.protocol_fee_paid() as f64 / 100.0,
            h.ticks_crossed,
            h.price_impact_pct
        );
        println!("         via {}", h.pool);
    }
    println!(
        "  expect {} {}",
        route::format_units(q.amount_out, r.output.decimals),
        r.output.symbol
    );
    println!(
        "  min    {} {}   (amountOutMinimum at {}% slippage)",
        route::format_units(q.min_out, r.output.decimals),
        r.output.symbol,
        r.max_slippage_pct
    );
    println!(
        "\n  NOTE: in-range + tick-walk simulation, hooks not simulated. Treat as an\n  \
         estimate; amountOutMinimum is what actually protects the trade."
    );
    cache::flush();
    Ok(())
}

/// Build (and optionally send) unlimited approvals so the router can spend a
/// token. Prints the exact calldata and only sends with --execute.
async fn approve_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    token_ref: &str,
    execute: bool,
) -> anyhow::Result<()> {
    let token = pool::resolve_token(&cfg.tokens, token_ref)?;
    let router = swap::resolve_addr(&cfg.universal_router, None, "universal_router")?;
    let permit2 = swap::resolve_addr(&cfg.permit2, Some(swap::PERMIT2_DEFAULT), "permit2")?;

    for (label, addr) in [("universal_router", router), ("permit2", permit2)] {
        let code = http.get_code(addr, None).await?;
        anyhow::ensure!(
            !code.0.is_empty(),
            "{label} {addr:?} has no code on this chain"
        );
        tracing::info!(label, ?addr, code_bytes = code.0.len(), "contract present");
    }

    let chain_id = http.get_chainid().await?.as_u64();
    let wallet = swap::load_wallet(cfg, chain_id)?;
    let owner = ethers::signers::Signer::address(&wallet);

    let (erc20_now, permit2_now) =
        swap::check_approvals(http, token, owner, permit2, router).await?;
    println!("\ncurrent allowances for {token_ref} ({token:?})");
    println!("  erc20 -> permit2 : {erc20_now}");
    println!("  permit2 -> router: {permit2_now}");

    let txs = swap::build_unlimited_approval(token, permit2, router)?;
    println!("\nunlimited approval, {} transaction(s):", txs.len());
    for (i, tx) in txs.iter().enumerate() {
        tx.print(i);
    }

    if !execute {
        println!("\ndry run - nothing sent. Re-run with --execute to submit.");
        return Ok(());
    }
    println!("\nsubmitting from {owner:?}");
    swap::send_all(http, wallet, &txs).await
}

/// Build the swap, check it against the real contracts with `eth_call`, print
/// it, and send it only when asked.
///
/// The on-chain check is the point of this command: the local simulation does
/// not model hooks, and a hook that charges a fee (or a pool that has moved
/// since the quote) shows up here as a smaller output rather than as a failed
/// transaction later.
/// The size a manual command trades at.
///
/// Required, because a route no longer carries one: it describes a path, and
/// the bot sizes each buy from the drop that triggered it. A command run by
/// hand has no drop to size from, so the size comes from the hand that ran it.
fn manual_amount(r: &route::Route, amount: Option<&str>) -> anyhow::Result<ethers::types::U256> {
    let raw = amount.with_context(|| {
        format!(
            "give a size with --amount: route '{}' is sized per signal when the bot runs, \
             so it has none of its own",
            r.name
        )
    })?;
    let parsed =
        route::parse_units(raw, r.input.decimals).with_context(|| format!("--amount {raw}"))?;
    anyhow::ensure!(!parsed.is_zero(), "--amount {raw} is zero");
    Ok(parsed)
}

async fn swap_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    name: &str,
    amount: Option<&str>,
    execute: bool,
) -> anyhow::Result<()> {
    use ethers::types::U256;

    let rc = cfg
        .routes
        .iter()
        .find(|r| r.name == name)
        .with_context(|| format!("no route named '{name}' in config"))?;
    let manager = pool_manager(cfg)?;
    let router = swap::resolve_addr(&cfg.universal_router, None, "universal_router")?;
    let permit2 = swap::resolve_addr(&cfg.permit2, Some(swap::PERMIT2_DEFAULT), "permit2")?;

    let r = route::Route::resolve(http, manager, rc, &cfg.tokens, weth(cfg)?).await?;
    let amount_in = manual_amount(&r, amount)?;
    let chain_id = http.get_chainid().await?.as_u64();
    let wallet = swap::load_wallet(cfg, chain_id)?;
    let owner = ethers::signers::Signer::address(&wallet);

    println!("\nroute \"{}\"  {} hop(s)", r.name, r.hops.len());
    println!(
        "  spend {} {}  from {owner:?}",
        route::format_units(amount_in, r.input.decimals),
        r.input.symbol
    );

    // Fail on the obvious things here, where the message can say what is wrong,
    // rather than as an opaque revert inside the router.
    let balance = swap::balance_of(http, r.input.address, owner).await?;
    anyhow::ensure!(
        balance >= amount_in,
        "balance is {} {} but the route spends {}",
        route::format_units(balance, r.input.decimals),
        r.input.symbol,
        route::format_units(amount_in, r.input.decimals)
    );
    if r.input.address != ethers::types::Address::zero() {
        let (erc20_now, permit2_now) =
            swap::check_approvals(http, r.input.address, owner, permit2, router).await?;
        anyhow::ensure!(
            erc20_now >= amount_in && permit2_now >= amount_in,
            "{} is not approved for the router (erc20->permit2 {erc20_now}, permit2->router \
             {permit2_now}); run --approve {} --execute first",
            r.input.symbol,
            r.input.symbol
        );
    }

    // The local walk only seeds the bracket; a wrong hint costs a few extra
    // eth_calls and nothing else.
    let hint = match r.quote(http, manager, None, amount_in).await {
        Ok(q) => {
            println!(
                "  local  {} {}   (tick walk, hooks not modelled)",
                route::format_units(q.amount_out, r.output.decimals),
                r.output.symbol
            );
            q.amount_out
        }
        Err(e) => {
            println!("  local  simulation failed ({e:#}); searching from scratch");
            U256::zero()
        }
    };

    let deadline = execute::deadline_in(600);
    let onchain = execute::verify(http, router, owner, &r, amount_in, hint, deadline, None).await?;
    println!(
        "  actual {} {}   ({}, {} eth_call{})",
        route::format_units(onchain.amount_out, r.output.decimals),
        r.output.symbol,
        if onchain.exact {
            "reported by the router"
        } else {
            "lower bound, bisected"
        },
        onchain.probes,
        if onchain.probes == 1 { "" } else { "s" }
    );
    // Both sides are parsed rather than cast: U256::as_u128 panics above
    // u128::MAX, and a wild local estimate must not take the command down on
    // what is only a line of commentary.
    let as_f64 = |v: ethers::types::U256| v.to_string().parse::<f64>().unwrap_or(f64::NAN);
    let diff = (as_f64(onchain.amount_out) / as_f64(hint) - 1.0) * 100.0;
    if diff.is_finite() {
        println!("         {diff:+.2}% against the local estimate");
    }

    let min_out = execute::apply_slippage(onchain.amount_out, r.max_slippage_pct);
    anyhow::ensure!(!min_out.is_zero(), "amountOutMinimum rounds to zero");
    println!(
        "  min    {} {}   (amountOutMinimum at {}% slippage)",
        route::format_units(min_out, r.output.decimals),
        r.output.symbol,
        r.max_slippage_pct
    );

    // The last check is the transaction itself: if this call goes through, the
    // only thing left that can move against it is the chain.
    execute::dry_run(http, router, owner, &r, amount_in, min_out, deadline).await?;

    let tx = execute::pending_swap(router, &r, amount_in, min_out, deadline)?;
    println!("\ntransaction:");
    tx.print(0);
    println!("  deadline {deadline} (unix seconds)");

    if !execute {
        println!("\ndry run - nothing sent. Re-run with --execute to submit.");
        cache::flush();
        return Ok(());
    }
    println!("\nsubmitting from {owner:?}");
    cache::flush();
    swap::send_all(http, wallet, std::slice::from_ref(&tx)).await
}

/// Sell the entire balance of `token_ref` back down the route that buys it.
///
/// The route is not re-resolved in reverse: `Route::reversed` reuses the very
/// PoolKeys already recovered and checked against their pool ids, so selling
/// crosses exactly the pools the buy crossed, in the opposite order.
async fn sell_all_cmd(
    http: &ethers::providers::Provider<ethers::providers::Http>,
    cfg: &config::Config,
    token_ref: &str,
    execute: bool,
) -> anyhow::Result<()> {
    use ethers::types::U256;

    let token = pool::resolve_token(&cfg.tokens, token_ref)?;
    let manager = pool_manager(cfg)?;
    let router = swap::resolve_addr(&cfg.universal_router, None, "universal_router")?;
    let permit2 = swap::resolve_addr(&cfg.permit2, Some(swap::PERMIT2_DEFAULT), "permit2")?;

    // Every route is resolved, not just up to the first match: two routes
    // ending in the same token is exactly the ambiguity worth refusing, and it
    // cannot be seen without looking at all of them.
    let mut buy = None;
    let mut also_end_here = Vec::new();
    for rc in &cfg.routes {
        let r = route::Route::resolve(http, manager, rc, &cfg.tokens, weth(cfg)?)
            .await
            .with_context(|| format!("route '{}'", rc.name))?;
        if r.output.address == token {
            match buy {
                None => buy = Some(r),
                Some(_) => also_end_here.push(rc.name.clone()),
            }
        }
    }
    let buy = buy.with_context(|| {
        format!("no route buys {token_ref}, so there is no path to sell it back down")
    })?;
    anyhow::ensure!(
        also_end_here.is_empty(),
        "routes {:?} and {:?} both end in {token_ref}; nothing says which one to sell down",
        buy.name,
        also_end_here
    );

    let chain_id = http.get_chainid().await?.as_u64();
    let wallet = swap::load_wallet(cfg, chain_id)?;
    let owner = ethers::signers::Signer::address(&wallet);

    let balance = swap::balance_of(http, token, owner).await?;
    anyhow::ensure!(
        !balance.is_zero(),
        "{token_ref} balance is zero; there is nothing to sell"
    );
    let sell = buy.reversed();

    println!(
        "\nsell all {} {}",
        route::format_units(balance, sell.input.decimals),
        sell.input.symbol
    );
    println!("  from   {owner:?}");
    println!(
        "  down   \"{}\"  {} hop(s), reversed",
        buy.name,
        sell.hops.len()
    );
    for (i, h) in sell.hops.iter().enumerate() {
        println!(
            "  hop {i}: {}  zeroForOne={}",
            h.describe(),
            h.zero_for_one()
        );
    }

    if sell.input.address != ethers::types::Address::zero() {
        let (erc20_now, permit2_now) =
            swap::check_approvals(http, sell.input.address, owner, permit2, router).await?;
        anyhow::ensure!(
            erc20_now >= balance && permit2_now >= balance,
            "{} is not approved for the router (erc20->permit2 {erc20_now}, permit2->router \
             {permit2_now}); run --approve {} --execute first",
            sell.input.symbol,
            sell.input.symbol
        );
    }

    let deadline = execute::deadline_in(600);

    // TODO: amountOutMinimum is 0, so this sale accepts ANY price - including
    // one moved against it inside the block it lands in. `--swap` already does
    // the right thing (quote the route on chain, floor it by max_slippage_pct);
    // do the same here with the reversed route before this is used on a size
    // worth stealing.
    let min_out = U256::zero();

    // Quoting is only to show what the sale is worth. It does not protect it,
    // and a failure here must not stop the sale being built.
    match execute::verify(
        http,
        router,
        owner,
        &sell,
        balance,
        U256::zero(),
        deadline,
        None,
    )
    .await
    {
        Ok(q) => println!(
            "  worth  {} {}   (quoted on chain, not enforced)",
            route::format_units(q.amount_out, sell.output.decimals),
            sell.output.symbol
        ),
        Err(e) => println!("  worth  could not be quoted: {e:#}"),
    }
    println!(
        "  min    {} {}   *** amountOutMinimum is 0: no slippage protection ***",
        route::format_units(min_out, sell.output.decimals),
        sell.output.symbol
    );

    execute::dry_run(http, router, owner, &sell, balance, min_out, deadline).await?;
    let tx = execute::pending_swap(router, &sell, balance, min_out, deadline)?;
    println!("\ntransaction:");
    tx.print(0);
    println!("  deadline {deadline} (unix seconds)");

    if !execute {
        println!("\ndry run - nothing sent. Re-run with --execute to submit.");
        return Ok(());
    }
    println!("\nsubmitting from {owner:?}");
    swap::send_all(http, wallet, std::slice::from_ref(&tx)).await
}
