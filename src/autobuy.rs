//! Buying a dip the moment the monitor sees it.
//!
//! A route marked `auto_buy` is armed against one pool - by default the last
//! pool it swaps through, so the buy happens in the very pool the drop was
//! seen in. When that pool's price drops by its threshold inside a block, the
//! route is bought once. The size is fixed by the route's `amount_in`, so a run
//! of drops cannot escalate into a bigger and bigger position.
//!
//! A drop in a pool no armed route buys through is reported and otherwise
//! ignored: it is a signal with nothing to act on, not an error. The same goes
//! for a route pointed at a pool whose falling side is a different token than
//! the route buys - that is a misconfiguration, and refusing to buy is the only
//! safe reading of it.
//!
//! One round trip stands between a signal and a signed transaction. The quote
//! is taken from the router at that instant - not from the price in the log,
//! and not from the local tick walk - and `amountOutMinimum` is that quote less
//! the route's own slippage tolerance, so what the trade will accept is
//! recomputed for this pair every single time.
//!
//! That single call also *is* the rehearsal. It runs the real batch and is
//! refused only by the final `TAKE_ALL`, which means the input was settled
//! through Permit2 and every hop swapped - balance, allowance and deadline all
//! proven. The minimum it then goes out with is derived from the amount that
//! call reported, so it can only be lower. A second `eth_call` to confirm the
//! exact bytes would re-prove that arithmetic and cost a round trip on the one
//! path where latency is the whole point, so the hot path does without it;
//! `--swap` and `--sell-all`, where nobody is racing, still run it.
//!
//! Routes are resolved once at startup: recovering a PoolKey takes a couple of
//! dozen archive reads, which is fine before the stream opens and far too slow
//! between a drop and a buy.

use crate::config::Config;
use crate::execute;
use crate::pool::Pool;
use crate::route::{format_units, parse_pool_ref, PoolRef, Route};
use crate::swap;
use anyhow::{Context, Result};
use ethers::providers::{Http, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

struct Plan {
    route: Route,
    cooldown: Duration,
}

pub struct AutoBuy {
    http: Provider<Http>,
    router: Address,
    wallet: LocalWallet,
    owner: Address,
    /// Sign and send, rather than only reporting what would have been sent.
    execute: bool,
    /// Trigger pool -> what to buy when it drops.
    plans: HashMap<PoolRef, Plan>,
    last_fire: Mutex<HashMap<PoolRef, Instant>>,
    /// One buy at a time. Two sends racing would read the same nonce and one of
    /// them would be dropped.
    sending: Mutex<()>,
}

impl AutoBuy {
    /// Resolve every armed route, or `None` when none is armed.
    ///
    /// Failing here rather than at the first drop is deliberate: a bad route,
    /// a missing key or a missing approval should stop the process at startup,
    /// while someone is watching, not silently do nothing at the one moment it
    /// was supposed to act.
    pub async fn build(
        http: &Provider<Http>,
        cfg: &Config,
        manager: Address,
        chain_id: u64,
        execute: bool,
    ) -> Result<Option<Arc<Self>>> {
        let armed: Vec<_> = cfg.routes.iter().filter(|r| r.auto_buy).collect();
        if armed.is_empty() {
            return Ok(None);
        }
        let router = swap::resolve_addr(&cfg.universal_router, None, "universal_router")?;
        let permit2 = swap::resolve_addr(&cfg.permit2, Some(swap::PERMIT2_DEFAULT), "permit2")?;
        let wallet = swap::load_wallet(cfg, chain_id).context("auto_buy needs a signing key")?;
        let owner = wallet.address();

        let mut plans = HashMap::new();
        for rc in armed {
            let route = Route::resolve(http, manager, rc, &cfg.tokens)
                .await
                .with_context(|| format!("auto_buy route '{}'", rc.name))?;
            // Default to the pool the route ends in: that is the one whose
            // price the buy is reacting to.
            let raw = rc
                .trigger_pool
                .clone()
                .unwrap_or_else(|| rc.pools[rc.pools.len() - 1].clone());
            let trigger = parse_pool_ref(&raw)
                .with_context(|| format!("route '{}': trigger pool", rc.name))?;
            anyhow::ensure!(
                !plans.contains_key(&trigger),
                "two auto_buy routes are armed against pool {trigger}; only one can fire"
            );

            // A trigger nobody subscribes to is a route that can never fire.
            // Not fatal - the pool may be about to be added - but silent
            // failure is exactly what this is otherwise.
            let watched = cfg.pools.iter().any(|p| {
                let named = match &p.pool_id {
                    Some(id) => parse_pool_ref(id).ok(),
                    None => parse_pool_ref(&p.address).ok(),
                };
                named == Some(trigger)
            });
            if !watched {
                warn!(
                    route = %rc.name, trigger = ?trigger,
                    "trigger pool is not in [[pools]], so nothing watches it - this route \
                     will never fire"
                );
            }

            preflight(http, &route, owner, permit2, router).await?;
            info!(
                route = %rc.name,
                trigger = %trigger,
                spend = format!("{} {}", format_units(route.amount_in, route.input.decimals), route.input.symbol),
                buy = %route.output.symbol,
                slippage_pct = route.max_slippage_pct,
                cooldown_s = rc.cooldown_secs,
                mode = if execute { "LIVE" } else { "dry run" },
                "auto-buy armed"
            );
            plans.insert(
                trigger,
                Plan {
                    route,
                    cooldown: Duration::from_secs(rc.cooldown_secs),
                },
            );
        }

        Ok(Some(Arc::new(Self {
            http: http.clone(),
            router,
            wallet,
            owner,
            execute,
            plans,
            last_fire: Mutex::new(HashMap::new()),
            sending: Mutex::new(()),
        })))
    }

    /// React to one big-sell signal.
    pub async fn on_drop(&self, pool: &Pool, drop_pct: f64) -> Result<()> {
        let fell = pool.base_symbol.clone().unwrap_or_else(|| "?".into());
        let key = pool.pool_ref();
        let Some(plan) = self.plans.get(&key) else {
            warn!(
                pool = %pool.name, token = %fell, id = %key,
                "big sell, but no auto_buy route buys through this pool - not buying"
            );
            return Ok(());
        };

        // The route has to buy what actually fell. A route armed against a pool
        // whose base side is the other token would buy on someone else's dip.
        if let Some(base) = pool.base_currency() {
            if base != plan.route.output.address {
                warn!(
                    pool = %pool.name,
                    route = %plan.route.name,
                    buys = %plan.route.output.symbol,
                    dropped = %fell,
                    "route buys a different token than the one that fell here - not buying; \
                     check base_token on the pool, or trigger_pool on the route"
                );
                return Ok(());
            }
        }

        // Claim the cooldown before doing any work, and keep it claimed even if
        // the buy then fails: a route that reverts every block should not retry
        // every block.
        {
            let mut last = self.last_fire.lock().await;
            if let Some(prev) = last.get(&key) {
                let waited = prev.elapsed();
                if waited < plan.cooldown {
                    info!(
                        pool = %pool.name,
                        route = %plan.route.name,
                        again_in_s = (plan.cooldown - waited).as_secs(),
                        "drop seen, still cooling down"
                    );
                    return Ok(());
                }
            }
            last.insert(key, Instant::now());
        }

        let route = &plan.route;
        let started = Instant::now();
        let deadline = execute::deadline_in(120);
        let quote = execute::verify(
            &self.http,
            self.router,
            self.owner,
            route,
            U256::zero(),
            deadline,
        )
        .await
        .context("quoting the route against the router")?;

        let min_out = execute::apply_slippage(quote.amount_out, route.max_slippage_pct);
        anyhow::ensure!(
            !min_out.is_zero(),
            "route '{}': amountOutMinimum rounds to zero",
            route.name
        );

        let tx = execute::pending_swap(self.router, route, min_out, deadline)?;

        info!(
            pool = %pool.name,
            route = %route.name,
            drop_pct = format!("-{drop_pct:.3}%"),
            spend = format!("{} {}", format_units(route.amount_in, route.input.decimals), route.input.symbol),
            quoted = format!("{} {}", format_units(quote.amount_out, route.output.decimals), route.output.symbol),
            min_out = format!("{} {}", format_units(min_out, route.output.decimals), route.output.symbol),
            slippage_pct = route.max_slippage_pct,
            took_ms = started.elapsed().as_millis(),
            "BUY THE DIP"
        );

        if !self.execute {
            info!(route = %route.name, "dry run - not sent; start with --execute to buy for real");
            return Ok(());
        }

        let _one_at_a_time = self.sending.lock().await;
        swap::send_all(&self.http, self.wallet.clone(), std::slice::from_ref(&tx)).await
    }
}

/// Everything that can be checked before the first drop: is there anything to
/// spend, and may the router spend it.
async fn preflight(
    http: &Provider<Http>,
    route: &Route,
    owner: Address,
    permit2: Address,
    router: Address,
) -> Result<()> {
    let balance = swap::balance_of(http, route.input.address, owner).await?;
    if balance < route.amount_in {
        warn!(
            route = %route.name,
            have = %format_units(balance, route.input.decimals),
            need = %format_units(route.amount_in, route.input.decimals),
            token = %route.input.symbol,
            "not enough to buy with; the first drop will fail unless this is topped up"
        );
    }
    if route.input.address != Address::zero() {
        let (erc20, p2) =
            swap::check_approvals(http, route.input.address, owner, permit2, router).await?;
        anyhow::ensure!(
            erc20 >= route.amount_in && p2 >= route.amount_in,
            "route '{}': {} is not approved for the router (erc20->permit2 {erc20}, \
             permit2->router {p2}); run --approve {} --execute first",
            route.name,
            route.input.symbol,
            route.input.symbol
        );
    }
    Ok(())
}
