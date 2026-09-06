mod cache;
mod config;
mod executor;
mod feed;
mod inventory;
mod depth;
mod execute;
mod strategy;
mod pool;
mod price;
mod route;
mod swap;

use anyhow::Context;
use ethers::providers::Middleware;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    // Flags that take a value, so their value is not mistaken for the config
    // path: `--quote "test buy CAMELTOE"` must not try to open the route name.
    const VALUE_FLAGS: [&str; 6] =
        ["--quote", "--approve", "--swap", "--sell-all", "--amount", "--config"];
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
    let quote_route = flag_value("--quote");
    let swap_route = flag_value("--swap");
    let sell_token = flag_value("--sell-all");
    // Overrides a route's configured amount_in, for asking "what would this
    // size do" without editing config.
    let amount = flag_value("--amount");
    let approve_token = flag_value("--approve");
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

    // Opened before anything resolves a pool, so the first lookup already has
    // somewhere to look.
    match cache::open(std::path::Path::new(&cfg.pool_cache_path)) {
        Ok(0) => tracing::info!(file = %cfg.pool_cache_path, "pool cache empty"),
        Ok(n) => tracing::info!(file = %cfg.pool_cache_path, pools = n, "pool cache loaded"),
        Err(e) => tracing::warn!(err = %format!("{e:#}"), "pool cache unusable, ignoring it"),
    }

    // Sanity check via HTTP JSON-RPC before opening WS subscriptions.
    let http = ethers::providers::Provider::<ethers::providers::Http>::try_from(
        cfg.http_url.clone(),
    )?;
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

    // Armed routes are resolved and checked before the first log arrives: a
    // broken route should stop the process here, not at the one moment it was
    // meant to fire.
    // One shared v4 PoolManager for the whole config: routes need it, and so
    // does every v4 pool that no longer writes it out for itself.
    let manager = pool_manager(&cfg)?;
    let auto = match executor::Executor::build(
        &http,
        &cfg,
        manager,
        chain_id,
        execute,
    )
    .await
    {
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
    let mut strategy = strategy::Strategy::new(http.clone(), auto, inv, reports_tx);
    let mut feeds = Vec::new();
    let tokens = cfg.tokens.clone();
    for pool_cfg in cfg.pools {
        let pool = match pool::Pool::resolve(&http, &pool_cfg, &tokens, Some(manager)).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(pool = %pool_cfg.name, err = %e, "failed to resolve pool, skipping");
                continue;
            }
        };
        // A take-profit belongs to the route that bought the position, and the
        // route is armed against this pool, so the two meet here.
        let armed = cfg
            .routes
            .iter()
            .find(|r| {
                r.auto_buy
                    && r.trigger_pool
                        .clone()
                        .or_else(|| r.pools.last().cloned())
                        .and_then(|p| route::parse_pool_ref(&p).ok())
                        == Some(pool.pool_ref())
            });
        strategy.watch(
            pool.clone(),
            pool_cfg.threshold_pct.unwrap_or(cfg.threshold_pct),
            pool_cfg.max_move_pct.unwrap_or(cfg.max_move_pct),
            armed.and_then(|r| r.take_profit_pct),
            armed.and_then(|r| r.exit_after_secs),
        );
        let ws = cfg.ws_url.clone();
        let out = ticks.clone();
        feeds.push(tokio::spawn(async move {
            // Exponential backoff so a persistently failing endpoint is not
            // hammered; reset once a subscription has run for a while.
            let mut backoff = std::time::Duration::from_secs(3);
            loop {
                let started = std::time::Instant::now();
                match feed::run_pool(pool.clone(), ws.clone(), out.clone()).await {
                    Ok(()) => tracing::warn!(pool = %pool.name, "stream closed, reconnecting"),
                    Err(e) => {
                        tracing::error!(pool = %pool.name, err = %e, "feed error, reconnecting")
                    }
                }
                if started.elapsed() >= std::time::Duration::from_secs(60) {
                    backoff = std::time::Duration::from_secs(3);
                }
                tracing::info!(pool = %pool.name, delay_s = backoff.as_secs(), "backing off");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(60));
            }
        }));
    }
    // The last sender in this scope has to go, or the strategy would wait on a
    // channel nobody can ever write to again.
    drop(ticks);

    // Every pool failing to resolve used to leave this empty, which made the
    // process exit 0 in silence.
    anyhow::ensure!(
        strategy.watching() > 0,
        "no pools could be resolved; nothing to watch"
    );
    tracing::info!(pools = strategy.watching(), "watching");
    // Everything that had to be looked up has been; keep it for next time.
    cache::flush();
    strategy.resolve_pending().await;
    strategy.seed_inventory().await;
    let decisions = tokio::spawn(strategy.run(rx, reports_rx));

    tokio::select! {
        _ = futures_util::future::join_all(feeds) => {}
        _ = decisions => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received, shutting down");
        }
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
    println!("comparing batched and unbatched tick walks at block {at}\n");

    let mut checked = 0;
    let mut differed = 0;
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
    }

    println!("\n{checked} pool(s) checked, {differed} differed");
    anyhow::ensure!(differed == 0, "batched reads changed an answer - do not ship this");
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
        match route::Route::resolve(http, manager, rc, &cfg.tokens).await {
            Ok(r) => {
                println!("\nroute \"{}\"  OK", r.name);
                println!(
                    "  spend {} {}  ->  receive {} (slippage cap {}%)",
                    route::format_units(r.amount_in, r.input.decimals),
                    r.input.symbol,
                    r.output.symbol,
                    r.max_slippage_pct
                );
                for (i, h) in r.hops.iter().enumerate() {
                    println!("  hop {i}: {}  zeroForOne={}", h.describe(), h.zero_for_one());
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
    let mut r = route::Route::resolve(http, manager, rc, &cfg.tokens).await?;
    override_amount(&mut r, amount)?;
    let q = r.quote(http, manager, None).await?;

    println!("\nroute \"{}\"", r.name);
    println!(
        "  spend  {} {}",
        route::format_units(r.amount_in, r.input.decimals),
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
        anyhow::ensure!(!code.0.is_empty(), "{label} {addr:?} has no code on this chain");
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
/// Replace a route's configured size, when the caller named one.
fn override_amount(r: &mut route::Route, amount: Option<&str>) -> anyhow::Result<()> {
    let Some(raw) = amount else { return Ok(()) };
    let parsed = route::parse_units(raw, r.input.decimals)
        .with_context(|| format!("--amount {raw}"))?;
    anyhow::ensure!(!parsed.is_zero(), "--amount {raw} is zero");
    r.amount_in = parsed;
    Ok(())
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

    let mut r = route::Route::resolve(http, manager, rc, &cfg.tokens).await?;
    override_amount(&mut r, amount)?;
    let chain_id = http.get_chainid().await?.as_u64();
    let wallet = swap::load_wallet(cfg, chain_id)?;
    let owner = ethers::signers::Signer::address(&wallet);

    println!("\nroute \"{}\"  {} hop(s)", r.name, r.hops.len());
    println!(
        "  spend {} {}  from {owner:?}",
        route::format_units(r.amount_in, r.input.decimals),
        r.input.symbol
    );

    // Fail on the obvious things here, where the message can say what is wrong,
    // rather than as an opaque revert inside the router.
    let balance = swap::balance_of(http, r.input.address, owner).await?;
    anyhow::ensure!(
        balance >= r.amount_in,
        "balance is {} {} but the route spends {}",
        route::format_units(balance, r.input.decimals),
        r.input.symbol,
        route::format_units(r.amount_in, r.input.decimals)
    );
    if r.input.address != ethers::types::Address::zero() {
        let (erc20_now, permit2_now) =
            swap::check_approvals(http, r.input.address, owner, permit2, router).await?;
        anyhow::ensure!(
            erc20_now >= r.amount_in && permit2_now >= r.amount_in,
            "{} is not approved for the router (erc20->permit2 {erc20_now}, permit2->router \
             {permit2_now}); run --approve {} --execute first",
            r.input.symbol,
            r.input.symbol
        );
    }

    // The local walk only seeds the bracket; a wrong hint costs a few extra
    // eth_calls and nothing else.
    let hint = match r.quote(http, manager, None).await {
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
    let onchain = execute::verify(http, router, owner, &r, hint, deadline, None).await?;
    println!(
        "  actual {} {}   ({}, {} eth_call{})",
        route::format_units(onchain.amount_out, r.output.decimals),
        r.output.symbol,
        if onchain.exact { "reported by the router" } else { "lower bound, bisected" },
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
    execute::dry_run(http, router, owner, &r, min_out, deadline).await?;

    let tx = execute::pending_swap(router, &r, min_out, deadline)?;
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
        let r = route::Route::resolve(http, manager, rc, &cfg.tokens)
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
    let sell = buy.reversed(balance);

    println!("\nsell all {} {}", route::format_units(balance, sell.input.decimals), sell.input.symbol);
    println!("  from   {owner:?}");
    println!("  down   \"{}\"  {} hop(s), reversed", buy.name, sell.hops.len());
    for (i, h) in sell.hops.iter().enumerate() {
        println!("  hop {i}: {}  zeroForOne={}", h.describe(), h.zero_for_one());
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
    match execute::verify(http, router, owner, &sell, U256::zero(), deadline, None).await {
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

    execute::dry_run(http, router, owner, &sell, min_out, deadline).await?;
    let tx = execute::pending_swap(router, &sell, min_out, deadline)?;
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
