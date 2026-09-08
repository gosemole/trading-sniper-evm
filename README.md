# trading-sniper

Watches the PonsV2 launchpad on the Robinhood chain (`chain_id 4663`), buys
into a new bonding curve inside its snipe-tax window through a wrapper
contract, and sells the position back on a trailing stop, a target, or a
minute of nobody trading it.

Nothing is ever sent without `--execute`, and with it the run refuses to start
unless the wrapper it is pointed at answers that this wallet owns it.

Every threshold in [`live.toml`](live.toml) was measured on journals this bot
wrote; [`analysis/`](analysis) is how, and the numbers are quoted in the commit
that set each one.

## Setup

Secrets come from the environment, never from a file in the repo:

```bash
export WS_URL="wss://..."
export HTTP_URL="https://..."
export PRIVATE_KEY="0x..."   # only for --execute
```

## Running

```bash
cargo build --release

# Watch, journal and shadow everything. Spends nothing, ever.
./target/release/trading-sniper --watch-launches --config collect.toml

# The same, with the entry filters on: still watches and journals every
# launch, and would buy only the ones that pass.
./target/release/trading-sniper --watch-launches --config live.toml

# And with money behind it. Refuses to start unless the wrapper in the config
# answers `owner()` with this wallet and `factory()` with the launchpad.
./target/release/trading-sniper --watch-launches --config live.toml --execute
```

`--verbose` puts every launch entry, decision and close back on the console.
Without it the console reports the run as a whole every thirty seconds, and
everything else lives in the journals.

## What it does

A PonsV2 launch opens a bonding curve whose snipe tax falls in whole seconds -
9900 bps in the launch second, then 618, then 19, then nothing. The bot hears
the launch from its log, prices the curve from the same numbers the curve
prices itself with, and decides at each step whether to buy. It follows every
curve it hears about for a minute, records every trade on it, and closes the
position it took on a trailing stop, a target, or a minute of holding.

A curve it has money in is followed past that minute, however quiet it goes,
until the sale has landed - the exit is asked on every block and not only on a
trade, because the rule written for a curve nobody is trading is one that
nothing but the clock can reach.

Nothing about the launch second is configurable: the wrapper refuses to buy in
it structurally, because that second costs 99% and no argument should be able
to reach it.

## Layout

| | |
|---|---|
| `src/launch.rs` | hearing launches and curve trades, and the chain's clock |
| `src/curve.rs` | the bonding curve, ported and checked against 36,151 real trades |
| `src/snipe.rs` | whether to buy, and on what terms |
| `src/exit.rs` | when to let a position go |
| `src/operators.rs` | who is behind a launch, when they keep changing address |
| `src/wrapper.rs` | the two calls made to the contract |
| `src/journal.rs` | one file per launch, everything that happened to it |
| `contracts/` | `PonsSniper`, the wrapper a buy and a sale go through |
| `analysis/` | replaying the journals, and what they say |

## The journals

Every launch writes `launches/<block>-<curve>.jsonl`: the launch and its terms,
every trade on the curve for a minute, every decision taken, and what was sent
if anything was. That directory is the only record of why a number is what it
is, and every threshold in `live.toml` came out of it:

```bash
python3 analysis/replay.py launches/ > rows.jsonl
python3 analysis/stats.py rows.jsonl
```

`replay.py` checks itself against the chain on every trade - the reserves it
rebuilds must match the ones recorded, and the fill its model predicts must
match the one the curve paid - and throws out any journal that disagrees.
