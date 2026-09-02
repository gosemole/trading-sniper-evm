mod config;
mod monitor;
mod pool;
mod price;

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

    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config.toml"));
    let cfg = config::Config::load(&path)?;

    // Sanity check via HTTP JSON-RPC before opening WS subscriptions.
    let http = ethers::providers::Provider::<ethers::providers::Http>::try_from(
        cfg.http_url.clone(),
    )?;
    match http.get_chainid().await {
        Ok(id) => tracing::info!(chain_id = %id, "connected via http"),
        Err(e) => tracing::warn!(err = %e, "http check failed (continuing with ws)"),
    }

    let mut tasks = Vec::new();
    for pool_cfg in cfg.pools {
        let pool = match pool::Pool::resolve(&http, &pool_cfg).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(pool = %pool_cfg.name, err = %e, "failed to resolve pool, skipping");
                continue;
            }
        };
        let ws = cfg.ws_url.clone();
        let http = http.clone();
        let thresh = pool_cfg.threshold_pct.unwrap_or(cfg.threshold_pct);
        let max_move = pool_cfg.max_move_pct.unwrap_or(cfg.max_move_pct);
        tasks.push(tokio::spawn(async move {
            loop {
                if let Err(e) =
                    monitor::run_pool(pool.clone(), http.clone(), thresh, max_move, ws.clone()).await
                {
                    tracing::error!(pool = %pool.name, err = %e, "monitor error, reconnecting");
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }));
    }

    tokio::select! {
        _ = futures_util::future::join_all(tasks) => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("ctrl-c received, shutting down");
        }
    }
    Ok(())
}
