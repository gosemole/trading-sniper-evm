# trading-mm-fall

Watches Uniswap v3/v4 pools on the Robinhood chain (`chain_id 4663`), buys a
token when it falls hard inside a single block, and sells it again on a target
or a timeout.

Nothing is ever sent without `--execute`. Every command runs read-only by
default and prints the exact transaction it would submit.

## Setup

Secrets come from the environment, never from a file in the repo:

```bash
chmod 600 ~/.config/mm-fall.env
set -a; source ~/.config/mm-fall.env; set +a
```

```
WS_URL       wss:// endpoint, for live swap logs
HTTP_URL     https:// endpoint, for calls
PRIVATE_KEY  32-byte hex signing key, needed only to trade
```

Then copy `config.example.toml` to `config.toml` and uncomment what you need.
`config.toml` is gitignored.

```bash
cargo build --release
```

## Commands

Read-only:

```bash
cargo run -- --check-routes
```

Resolves every route, recovers each pool's key and verifies it against its id.
Run this after editing `[[routes]]`; a broken chain of pools fails here rather
than at trade time.

```bash
cargo run -- --quote "buy CAMELTOE"
```

Simulates the route locally by walking ticks. Hooks are not modelled, so treat
it as an estimate.

```bash
cargo run -- --quote "buy CAMELTOE" --amount 5
```

Same, at a size other than the route's own `amount_in`. Works with `--swap` too.

```bash
cargo run -- --swap "buy CAMELTOE"
```

Asks the router what the route really pays, applies the route's slippage, and
prints the transaction. Add `--execute` to send it:

```bash
cargo run -- --swap "buy CAMELTOE" --execute
```

```bash
cargo run -- --sell-all CAMELTOE
```

Sells the entire wallet balance of a token back down the route that buys it.
**`amountOutMinimum` is 0 here** - there is no slippage protection on this
command yet. Add `--execute` to send.

```bash
cargo run -- --approve USDG --execute
```

One-time, two transactions: the ERC-20 approves Permit2, and Permit2 approves
the router. Needed once per token you spend - including a token you intend to
sell. Without `--execute` it only prints them.

Running the bot:

```bash
cargo run --release
```

Watches every `[[pools]]`, and for routes marked `auto_buy` prints what it would
buy. Add `--execute` to trade for real:

```bash
./target/release/trading-mm-fall --execute
```

`--config path/to/other.toml` points any command at a different config.

## Config

Full reference with comments: `config.example.toml`.

Global:

| key | |
|---|---|
| `threshold_pct` | drop inside one block that counts as a signal |
| `max_move_pct` | what the depth line in a signal is measured against |
| `calibrate_secs` | how often to re-measure what pools take on top of their stated fee; `0` disables |
| `fast_quote` | price a buy from the measured model instead of asking the router - one round trip instead of two, at the cost of the router's rehearsal |
| `pool_cache_path` | where recovered PoolKeys, decimals and symbols are kept |
| `inventory_path` | where positions and unsettled trades are kept |
| `universal_router`, `pool_manager`, `permit2` | contracts |

Per route:

| key | |
|---|---|
| `input`, `amount_in` | what to spend and how much |
| `pools` | ordered list; a 32-byte v4 pool id or a 20-byte v3 pool address, mixed freely |
| `max_slippage_pct` | percent, not basis points |
| `auto_buy` | arm the route |
| `trigger_pool` | which pool's drop fires it; defaults to the last pool in `pools` |
| `cooldown_secs` | shortest gap between buys; `0` means every signal buys |
| `take_profit_pct` | sell the whole position once the pool price is this far above the average entry |
| `exit_after_secs` | sell it anyway once held this long since the last buy |

Per pool: `name`, `version`, `address`, `pool_id`, and optionally `base_token`,
`threshold_pct`, `max_move_pct`. `base_token` is normally omitted - a pool named
`BASE/QUOTE` says which side is which, and the index is derived from the name
and checked against the chain.

## How it works

```
feed ──ticks──▶ strategy ──▶ executor ──▶ chain
                   ▲                        │
                   └────── reports ─────────┘
```

- **feed** - one task per pool. Turns each swap log into a tick and has no
  opinion about what it means. Price, liquidity and the fee actually charged all
  come out of the log itself, so the freshest state costs nothing.
- **strategy** - runs the drop meter, holds the inventory, decides to buy or to
  sell. Nothing in its loop waits on the network: trades and tick walks go to
  tasks of their own and report back over a channel.
- **executor** - prices a route, signs, sends. Knows nothing about why.
- **inventory** - a trade is reserved when broadcast and only joins the average
  once the chain confirms it. Reverted or dropped, it rolls back. Reservations
  survive a restart and are resolved against their receipts at startup.

A sale that fails is retried twice more; after three failures the pair is halted
and said so, with the position still held and still recorded.

## Files it writes

| | |
|---|---|
| `inventory.json` | positions and trades in flight. Deleting it loses the entry prices, and with them the take-profit targets |
| `pools.json` | recovered PoolKeys, decimals, symbols. All immutable; deleting it only costs a slow start (about 5s instead of 0.2s) |

Both are gitignored and written through a temporary file, so a crash mid-write
leaves the previous state rather than half of the new one.

## Worth knowing

- A signal is a drop against the pool's **quote token**, not against the dollar.
  On `CAMELTOE/LULU` a 5% drop means 5% cheaper in LULU; if LULU itself moved,
  the dollar price may have gone the other way. The same goes for
  `take_profit_pct`.
- Trades are priced by asking the router, which also proves the balance,
  allowances and deadline for free. `fast_quote` gives that up for one round
  trip of latency.
- `cooldown_secs = 0` buys on every signal, so a dip lasting ten blocks buys ten
  times.

## Development

```bash
cargo test
cargo clippy --all-targets
```
