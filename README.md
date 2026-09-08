# trading-sniper

Two bots on the Robinhood chain (`chain_id 4663`), sharing one tree because
they share the endpoints, the wallet handling and the execution layer.

**The launch sniper** is the current work: it watches the PonsV2 launchpad,
buys into a new bonding curve inside its snipe-tax window through a wrapper
contract, and sells the position back on a trailing stop or a target. Start at
[`live.toml`](live.toml) and `--watch-launches`; the analysis that chose every
number in it is in [`analysis/`](analysis).

**The fall bot** came first: it watches Uniswap v3/v4 pools, buys a token when
it falls hard inside a single block, and sells it again on a target or a
timeout. Everything below this line is about that one.

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
FEED         wss:// Nitro sequencer feed. Optional; --watch-launches hears
             launches from it before the block exists
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
cargo run -- --depth
```

Prints, for every `[[pools]]` entry: its price, liquidity and the fee actually
charged; what it costs to move that price by 0.5 to 10 percent in either
direction; where the initialized ticks sit around it; and how far the ladder the
bot keeps in memory reaches in **percent**. That last line is the one to read
when a buy was refused with "walks past the last tick the scan read" - the
ladder is bounded in ticks, and how much price that buys depends on how densely
the pool is provided.

Read-only, and heavier than anything the bot does by itself - it asks the whole
question at once because somebody is waiting for the answer and nothing is
racing.

```bash
cargo run -- --watch-launches
```

Watches the PonsV2 launchpad and prints every new launch. It reads no wallet,
holds nothing and cannot send anything, so it is safe to leave running next to
the bot.

Two logs, because they answer different questions and only one of them always
happens:

| | |
|---|---|
| `TokenLaunched` | the factory. The token, its curve, who deployed it, the **pair token** the curve is bought with, and what it has to take before it graduates. Every launch has exactly one |
| `Launched` | the launcher in front of it, whose `launchAndBuy` mints and buys in one transaction: what the dev buy paid and received. A launch made straight through the factory has none |

Both logs of one transaction print as **one** entry - two would read as two
launches - so a launch is held for 250ms in case its dev buy is still coming.

```
21:04:56.427 launch   Zcash Mascot (ZEBRIGRADE)  block 57136763  feed +32ms  tax 9900bps/3s
  token      0x6abee1…956d   curve 0x60ef1c…c4ab
  pair       ETH   config #0, graduates at 4.2 ETH
  deployer   0x508211…a031   creator tax 0 bps
  dev buy    0.086 ETH -> 48.23M -> 0x508211…a031 (min 47.75M)
  exempt     0x531d1b…57c0, 0xe924fa…2c91
  tx         0xbee1a66d…2adb37   via launchAndBuy
```

Every entry is stamped, in UTC to the millisecond, like the log lines around
it - with two sources the whole question is which arrived first, and without a
stamp the order in a terminal is a guess. Addresses are shortened to something
that can be recognised and pasted into a search; the whole of one is in the
transaction. Amounts are cut to six decimals and supply-sized numbers to
`48.23M`, because no decision is made on the eighteenth decimal of either.

A line appears only when it says something: `exempt` when somebody was
exempted, `min` when the launcher set one, `launcher` when whoever paid is not
the deployer, and the emitting contract only when it is **not** one of the two
known ones - the ordinary case would be the same two addresses on every entry,
which is a line nobody reads.

`name`, `creator`, `exempt` and `via` come from the transaction's **calldata**,
which is the only place they exist - no log carries them. `exempt` is who does
not pay the snipe tax; a sniper is not on that list. That costs one
`eth_getTransactionByHash` per launch, and a transaction that cannot be fetched
or does not decode costs the entry those lines and nothing else.

`snipe tax` is the launchpad's setting, not the launch's: two `eth_call`s at
startup, and then **zero** requests, because the factory announces every change
to it (`SnipeTaxStartBpsUpdated`, `SnipeTaxSecondsUpdated`) on the same
subscription the launches arrive on. Where it says `decay unknown` it means
just that: the ABI gives the tax at launch and the window it decays over, but
the shape of the decay lives in the hook, so what a buy at second three would
actually pay is not computed here rather than guessed.

Amounts in the pair token are printed in **its** decimals, which costs one
`decimals()` and one `symbol()` the first time a pair token is ever seen and
nothing after that - they are cached in `pools.json` with everything else. A
pair token that will not answer prints its amounts raw rather than guessed at:
USDG has six decimals, and 8090 USDG printed at eighteen reads
`0.00000000809`. Amounts of the launched token are printed at 18 decimals,
which is what these mint.

The ABIs of both contracts are checked in under `abi/`, and a test rebuilds the
event signatures from them - see `abi/README.md`.

### The sequencer feed

With `FEED` set to the Nitro sequencer feed's `wss://` URL, launches are heard
from a second source: the feed carries **signed transactions**, so it says a
launch is coming before the block that carries it exists.

```
21:04:56.395 feed    Zcash Mascot (ZEBRIGRADE)  seq 57136763  pair ETH  dev buy 0.086 ETH  from 0x508211…a031  launchAndBuy  tx 0xbee1a66d…2adb37
```

`launchedAt` is the second the sequencer stamped the message with, and it is
the `block.timestamp` the block built from it will carry - so it is the
`launchedAt` the snipe tax counts from, known **before the block exists** and
without asking anyone for a block. The entry for the launch then carries the
whole tax window in absolute seconds:

```
  window     618 bps at 1788814628, 19 bps at 1788814629, free at 1788814630
```

Blocks are about 100ms and the timestamp is a whole second, so ten of them
share each step: what a buy pays is decided by which second it lands in, not by
which block. The feed also says WHERE in our own second the chain's second
turns over - it is logged whenever it moves - which is the difference between
sending now and sending in half a second.

`seq` is the feed's own sequence number, which on this chain is the block the
transaction is heading for. It is printed because a relay that hands out
history on connect would otherwise be indistinguishable from one that is
merely fast: the first sequence number of each connection is logged too.

The entry for the same launch then arrives from the logs as usual, with one
line more:

```
  feed       412ms before this log
```

Two things come of it. The lead time is measured rather than assumed - both
sources are read in one process against one clock, so it compares endpoints and
not machines. And the calldata is already in hand when the log arrives, so the
`eth_getTransactionByHash` above is **not made at all** for a launch the feed
saw first.

What the feed cannot say is what the launch became: the token and its curve are
created inside the transaction, so those still come from the factory's log.
Without `FEED` nothing changes - launches are heard from logs alone.

### The journal

With `--size` set, every launch also gets a file under `launches/`, named
`<block>-<curve>.jsonl`: one JSON line for the launch and one for each trade on
its curve for the first minute. Who bought, in which second of the tax window,
what they were really charged, and the reserves each trade left behind.

It is written for reading afterwards. A launch is decided in three seconds, and
none of this can be recovered later without asking the chain for every log
again.

Every amount appears twice: in the token's own units (`quote_in`,
`tokens_out`, `quote_reserve`) and as the integer the chain moved
(`quote_in_wei`, `tokens_out_wei`). Both are **strings**, never JSON numbers -
a token amount runs to twenty-seven digits and a JSON number is a double, which
would round away the last nine of them in the file that exists to record them.
The readable form is exact as well: the same integer with a decimal point put
in, not a rounding of it.

`exempt` says whether the wallet was declared free of the snipe tax at launch -
the list in the calldata plus the deployer and the creator fee recipient, whom
the factory exempts whether or not they were named. It is **absent** rather
than `false` when the launch's calldata was never decoded: not knowing is not
the same as knowing they were not. `snipe_tax` on the same line says what was
actually paid, and the two together separate a wallet that was let in free from
one that merely arrived after the window closed.

```bash
jq -r 'select(.kind=="buy") | [.elapsed, .snipe_tax_bps, .exempt, .who, .quote_in] | @tsv' launches/*.jsonl
```

`launches/` is gitignored.

`--launchpad 0x...` watches a different address instead of the two built in
(comma-separated for several). `--launchpad any` drops the address filter and
takes the events from whoever emits them - **an event signature belongs to
nobody**, so anything that comes back that way is a claim about a token, not a
launch.

```bash
cargo run -- --quote "buy CAMELTOE"
```

Simulates the route locally by walking ticks. Hooks are not modelled, so treat
it as an estimate.

```bash
cargo run -- --quote "buy CAMELTOE" --amount 5
```

`--amount` is required: a route carries no size of its own, because the bot
sizes each buy from the drop that triggered it. Works with `--swap` too.

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

```bash
cargo run -- --wrap 0.5
cargo run -- --unwrap 0.2 --execute
```

Move between the native currency and its wrapper, for a setup that trades a
native pool while holding the wrapped token. Needs `weth` in the config. Prints
both balances and the exact call; sends only with `--execute`. Wrapping refuses
to take the whole native balance, because the transaction doing it has to be
paid for out of what is left.

Running the bot:

```bash
cargo run --release
```

Watches every `[[pools]]`, and for routes marked `auto_buy` prints what it would
buy. Add `--execute` to trade for real:

```bash
./target/release/trading-sniper --execute
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
| `gas_reserve` | native currency that must be on hand for gas, e.g. `"0.05"`. Below it **nothing is bought**: a position bought with the last of the gas cannot be sold, and that is worse than a dip not caught. Defaults to `0.05` |
| `pool_cache_path` | where recovered PoolKeys, decimals and symbols are kept |
| `inventory_path` | where positions and unsettled trades are kept |
| `weth` | wrapped native token, and the **only** way to trade a pool that holds the native one: a route writes `input = "WETH"`, and the router unwraps on the way in and wraps on the way out inside the same transaction. A route spending the native currency directly is refused - that balance is what gas is paid from, and trading it makes one pot of the money for a swap and the money for the sale after it |
| `multicall` | Multicall3, used to read a v3 pool in one request rather than two dozen - v4 has `extsload` for this, v3 has only its own view functions. Defaults to the canonical deterministic deployment, and is never required: a chain without it simply costs more requests |
| `universal_router`, `pool_manager`, `permit2` | contracts. `pool_manager` is shared by every v4 pool and every v4 route, so it is written once here and omitted from the pools themselves |

Per route:

| key | |
|---|---|
| `input` | what to spend |
| `impact_pct` | size each buy to move the **trigger pool's** price by this much. A route carries no size of its own, and there is no ceiling here: the ceiling is the tracked balance, and if the whole of it still cannot move the pool that far the buy is skipped rather than shrunk. Worked out per signal from its own log and the tick ladder already in memory, so it costs no requests and nothing waits for it. Must be below `max_slippage_pct` |
| `pools` | ordered list; a 32-byte v4 pool id or a 20-byte v3 pool address, mixed freely |
| `max_slippage_pct` | percent, not basis points. Sets `amountOutMinimum`, and with it how far the model may be trusted: an auto-buy is priced from memory rather than by the router, so this budget covers the model being wrong as well as the market moving, and a trade is only modelled at all while it moves its pool by less than a third of it |
| `auto_buy` | arm the route |
| `trigger_pool` | which pool's drop fires it; defaults to the last pool in `pools` |
| `cooldown_secs` | shortest gap between buys; `0` means every signal buys |
| `take_profit_pct` | sell the whole position once it is worth this much more than it cost, **net of both fees, the hook and impact** |
| `exit_after_secs` | sell it anyway once held this long since the last buy |

Spelling out `token0`, `token1`, `fee`, `tick_spacing` and `hooks` for a v4 pool
does more than save a lookup: the key is filed under its own hash and used
everywhere, **routes included**. That is the only way to trade a pool created
further back than the endpoint keeps logs, since a route recovers a PoolKey from
the `Initialize` log and there is none to be had. A pool that also declares a
`pool_id` is checked against the one its fields hash to, and refused if they
disagree.

Per pool: `name`, `version`, `pool_id`, and optionally `base_token`,
`threshold_pct`, `max_move_pct`. `address` is required for a v3 pool - that is
what a v3 pool is - and omitted for a v4 one, which takes the global
`pool_manager`. `base_token` is normally omitted - a pool named
`BASE/QUOTE` says which side is which, and the index is derived from the name
and checked against the chain.

## How it works

```
feed ──ticks──▶ strategy ──▶ executor ──▶ chain
                   ▲                        │
                   └────── reports ─────────┘
```

- **feed** - one websocket for every pool, with a subscription and a task each.
  Turns each swap log into a tick and has no opinion about what it means. Price,
  liquidity and the fee actually charged all come out of the log itself, so the
  freshest state costs nothing.
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
| `pools.json` | recovered PoolKeys, decimals, symbols. All immutable; deleting it only costs a slower start - each v4 pool's PoolKey is found again by walking the chain's logs back from the head, one `eth_getLogs` per ten million blocks until its `Initialize` turns up |

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
