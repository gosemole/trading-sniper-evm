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
SUBMIT_URLS  comma-separated https:// endpoints to broadcast through, all at
             once. Optional; empty means just HTTP_URL
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

Running the bot with `--execute` does this by itself for every armed route, so
this command is only needed for the manual ones (`--swap`, `--sell-all`).

Running the bot:

```bash
cargo run --release
```

Watches every `[[pools]]`, and for routes marked `auto_buy` prints what it would
buy. Add `--execute` to trade for real:

```bash
./target/release/trading-mm-fall --execute
```

Arming a route checks both tokens it touches - the one it spends and the one it
sells back down - and, with `--execute`, approves either of them that is not
already approved without limit. That happens at startup, before the first log
arrives, and costs two transactions once per token. Without `--execute` it
prints those transactions and carries on, because a dry run has no trade to fail
at later.

`--config path/to/other.toml` points any command at a different config.

## Config

Full reference with comments: `config.example.toml`.

Global:

| key | |
|---|---|
| `threshold_pct` | drop inside one block that counts as a signal |
| `max_move_pct` | what the depth line in a signal is measured against |
| `calibrate_secs` | how often to re-measure what a route takes on top of its pools' stated fees, and how often the pool snapshot a buy is priced from is refreshed. Both a price input and a safety check: an unmeasured route is not bought, and one keeping over 5% is refused. `0` disables it, so nothing is ever bought. The snapshot is trusted for twice this interval, so raising it makes buys price off older state |
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
| `take_profit_pct` | sell the whole position once it is worth this much more than it cost, **net of both fees, the hook and impact** |
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
| `inventory.json` | positions, trades in flight, and the tracked spendable balance of whatever a route spends. Deleting it loses the entry prices (and with them the take-profit targets) and the tracked balance - the next start just re-reads the real one |
| `pools.json` | recovered PoolKeys, decimals, symbols. All immutable; deleting it only costs a slow start (about 5s instead of 0.2s) |

Both are gitignored and written through a temporary file, so a crash mid-write
leaves the previous state rather than half of the new one.

## Worth knowing

- A signal is a drop against the pool's **quote token**, not against the dollar.
  On `CAMELTOE/LULU` a 5% drop means 5% cheaper in LULU; if LULU itself moved,
  the dollar price may have gone the other way. The same goes for
  `take_profit_pct`.
- `take_profit_pct` is net. The entry price on the books is what the buy really
  paid - the quote token that left the wallet divided by the token that
  arrived - so the LP fee, the protocol fee, the hook's cut and our own impact
  are all already in it. The target then assumes the sale costs the same
  fraction again, because it has not happened yet and the way back out is the
  same pools and the same hook. So `5.0` fires later than a naive 5% move in
  the pool price, by roughly the cost of one round trip, and what it clears is
  five percent actually kept.
- A position records **two** prices, in two currencies, and they are never
  compared to each other. `entry_in_pool` is what the pool actually charged in
  its own quote token, read out of the buy's `Swap` log - that is the one a
  take-profit target is built from, because it is the currency the feed quotes
  prices in. `cost_in_route_token` is what left the wallet on the whole route,
  which is the real money but means nothing to a single pool: on a multi-hop
  route it is a different token entirely.
- Positions recorded before this existed carry the mid price as their entry and
  assume a free exit. They stay slightly optimistic until the next buy averages
  in a real fill; there is nothing to recompute them from.
- Broadcasting fans out and nothing else does. Every endpoint in `submit_urls`
  is handed the identical signed transaction at the same moment, and the first
  one to take it decides the answer; the others keep going, because having the
  transaction in more than one mempool is the point rather than a leftover. The
  hash is computed from the signed bytes rather than taken from a reply, so it
  is the same hash whoever accepts. Reads - calls, gas, receipts - still go to
  `http_url` alone: an answer fetched twice is the same answer.
- Every submission connection is kept warm on its own timer. A reused
  connection answers in about 50ms and a cold one pays a TLS handshake for
  350-400ms, landing on exactly the request a buy waits for.
- A sale is priced against the pool as the feed last saw it, never against a
  snapshot alone. Without live state for the pool being sold into, the sale
  asks the router instead, and a sale that has already reverted once asks the
  router whatever happens - the chain has just disagreed with the model, and
  the retry is not the place to argue. A sale is not racing anyone, so paying
  for an honest quote costs nothing that matters.
- A buy is priced entirely from memory and never asks the router, so nothing
  rehearses it. The pool that dropped is priced from the very log that raised
  the signal - price, liquidity and the fee it actually charged; any other hop
  on the route from the last calibration snapshot. What calibration measures on
  top of that is only a check: an unmeasured route is not bought, one keeping
  over 5% of a swap is refused. Nor is the wallet balance read from the chain
  per buy: it is read once at startup and kept as a running total from there,
  debited the moment a buy is decided and credited back if it never lands -
  accurate because nothing but this bot spends from the wallet while it runs.
  Allowance is settled once when the route is armed - approved without limit
  then if it is not already - and every buy after that relies on it. Selling and
  every manual command
  (`--swap`, `--sell-all`, `--quote`) still ask the router when the model cannot
  answer, which is slower but proves the trade first.
- `cooldown_secs = 0` buys on every signal, so a dip lasting ten blocks buys ten
  times.

## Development

```bash
cargo test
cargo clippy --all-targets
```
