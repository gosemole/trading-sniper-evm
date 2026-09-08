#!/usr/bin/env python3
"""Replay the launch journals and emit one row per launch.

The bot writes a file per launch: the launch line, then every trade the curve
made in the minute after it. This reads those back, rebuilds the curve, buys
into it with money we did not spend, and records what the position would have
been worth after every subsequent trade.

**The curve arithmetic here is a second implementation of `src/curve.rs`, and
it does not get to be trusted.** Two things are checked against the journal on
every single trade, and a file that fails either is thrown out rather than
quietly analysed:

  reserves  - the reserves rebuilt from the trade deltas must equal the ones
              the bot recorded, to the wei. This catches a mistake in how a
              trade moves the curve.
  fills     - the tokens the model says a buy receives must equal the tokens
              it actually received. This catches a mistake in the pricing, and
              it is a check against the chain rather than against other code.

Everything is integer arithmetic in the token's own smallest unit, the way the
chain does it. Floats appear only in the value path, where they are ratios and
the last bit does not decide anything.

Time is measured in blocks after the launch block, not in seconds. The launch
second comes from the sequencer feed and the feed is not always up - it caught
three percent of one overnight run - and a journal without it has no `elapsed`
on any trade at all. Block numbers are on every trade unconditionally. This
chain runs 9.8 blocks to the second, measured off the launches where both are
known, and the seconds overlap at their edges: second 1 spans offsets 1 to 18
and second 2 spans 11 to 23, so an offset cannot say which of those two a trade
fell in. It can say a trade is past the tax window, which is what matters: the
window is three seconds, second 3 starts by offset 23, and everything from
offset 30 on pays no snipe tax whatever the feed did.

So the entry is "the first trade at least `--enter-block` blocks after the
launch", at zero tax. One definition that holds whether or not the feed was up,
which is also the only way a night and a day are comparable.

The output is one JSON object per launch with the launch's traits and the full
value path indexed by block offset, so exit rules can be tried in `stats.py`
without replaying again.

    python3 analysis/replay.py launches/ > rows.jsonl
"""

import argparse
import json
import pathlib
import sys
from decimal import Decimal

BPS = 10_000


def units(s, decimals):
    """A decimal string from the journal, back to the integer it came from."""
    return int(Decimal(str(s)).scaleb(decimals))


def buy(qr, tr, reserved, fee_bps, creator_bps, snipe_bps, quote_in):
    """A buy priced the way the curve prices one.

    The fees come off the input and the swap that follows is priced at zero
    fee. A fill that would take more than the curve has spare is clamped to
    what is there and re-priced from the token side, which is the only part of
    this that is not obvious from the formula.
    """
    net = quote_in
    for rate in (fee_bps, creator_bps, snipe_bps):
        net -= quote_in * rate // BPS
    if net <= 0:
        return 0, qr, tr
    spare = tr - reserved
    out = tr * net // (qr + net)
    if out > spare:
        out = spare
        net = qr * out // (tr - out) + 1 if tr > out else net
    return out, qr + net, tr - out


def sell(qr, tr, fee_bps, creator_bps, tokens_in):
    """A sell, with both fees taken off the proceeds."""
    gross = qr * tokens_in // (tr + tokens_in)
    out = gross - gross * fee_bps // BPS - gross * creator_bps // BPS
    return out, qr - gross, tr + tokens_in


class Bad(Exception):
    """This file cannot be trusted, and the reason a person can act on."""


def replay(path, size_x100, enter_block):
    lines = []
    for n, raw in enumerate(open(path), 1):
        raw = raw.strip()
        if not raw:
            continue
        try:
            lines.append(json.loads(raw))
        except json.JSONDecodeError as e:
            # A half-written last line is what a killed process leaves behind.
            raise Bad(f"line {n} is not json ({e})")
    if not lines:
        raise Bad("empty")
    head = lines[0]
    if head.get("kind") != "launch":
        raise Bad("does not start with a launch line")
    for key in ("opening_quote_reserve", "opening_token_reserve", "reserved_tokens",
                "curve_fee_bps", "creator_tax_bps", "deployer"):
        if key not in head:
            raise Bad(f"launch line has no {key}")

    qd = head.get("pair_decimals", 18)
    fee_bps = head["curve_fee_bps"]
    creator_bps = head["creator_tax_bps"]
    reserved = int(head["reserved_tokens"])
    qr0 = int(head["opening_quote_reserve"])
    tr0 = int(head["opening_token_reserve"])
    if qr0 <= 0 or tr0 <= 0:
        raise Bad("a curve that opened with no reserves")

    # Every trade, with the deltas it applied and what it should have paid out.
    trades = []
    launch_block = head.get("block")
    if not launch_block:
        raise Bad("launch line has no block")
    qr, tr = qr0, tr0
    graduated = False
    last_block = 0
    last_elapsed = None
    for row in lines[1:]:
        kind = row.get("kind")
        if kind in ("window", "decision", "quote"):
            continue
        if kind == "graduated":
            graduated = True
            continue
        if kind not in ("buy", "sell", "buyback"):
            raise Bad(f"unknown record {kind!r}")
        if graduated:
            raise Bad("a trade after the curve graduated")
        block = row.get("block", 0)
        if block < last_block:
            raise Bad(f"block {block} after {last_block}")
        last_block = block
        e = row.get("elapsed")
        if e is not None:
            if last_elapsed is not None and e < last_elapsed:
                raise Bad(f"elapsed {e}s after {last_elapsed}s")
            last_elapsed = e

        if kind == "buy":
            q_in = units(row["quote_in"], qd)
            out = units(row["tokens_out"], 18)
            fee = units(row["fee"], qd)
            ctax = units(row["creator_tax"], qd)
            net = q_in - fee - ctax
            if net < 0:
                raise Bad("a buy whose fees are more than it spent")
            # The model, against what the chain actually paid out. The fees are
            # taken from the record rather than re-derived, so this tests the
            # pricing and not our reading of the tax schedule.
            spare = tr - reserved
            want = tr * net // (qr + net)
            if want > spare:
                want = spare
            if want != out:
                raise Bad(
                    f"model off by {abs(want - out)} on a buy at +{e}s "
                    f"(said {want}, chain paid {out})"
                )
            qr, tr = qr + net, tr - out
        elif kind == "sell":
            t_in = units(row["tokens_in"], 18)
            gross = (units(row["quote_out"], qd) + units(row["fee"], qd)
                     + units(row["creator_tax"], qd))
            if gross > qr:
                raise Bad("a sell took more quote than the curve held")
            qr, tr = qr - gross, tr + t_in
        else:
            qr += units(row["quote_spent"], qd)
            tr -= units(row["tokens_locked"], 18)
            if tr < 0:
                raise Bad("a buyback locked more tokens than the curve held")

        # The reserves the bot recorded, which came from its own `apply`.
        for name, got, want in (("quote", qr, units(row["quote_reserve"], qd)),
                                ("token", tr, units(row["token_reserve"], 18))):
            if got != want:
                raise Bad(
                    f"{name} reserve off by {abs(got - want)} after a {kind} "
                    f"at +{e}s"
                )
        trades.append((row, e, block - launch_block))

    # Now the counterfactual: buy in at `enter_at`, then let the same trades
    # run against a curve that carries our position.
    spend = qr0 * size_x100 // (100 * 100)
    if spend <= 0:
        raise Bad("a size that rounds to nothing")
    qr, tr = qr0, tr0
    i = 0
    while i < len(trades) and trades[i][2] < enter_block:
        qr, tr = _apply(trades[i][0], qr, tr, qd)
        i += 1
    run_at_entry = qr / qr0
    # Zero tax: `enter_block` is past the window by construction. Anything
    # earlier would need the launch second, which is exactly what is missing.
    held, qr, tr = buy(qr, tr, reserved, fee_bps, creator_bps, 0, spend)
    if held == 0:
        raise Bad("our own buy fills nothing")
    path = [(enter_block, sell(qr, tr, fee_bps, creator_bps, held)[0] / spend)]
    for row, _, off in trades[i:]:
        qr, tr = _apply(row, qr, tr, qd)
        path.append((off, sell(qr, tr, fee_bps, creator_bps, held)[0] / spend))

    # What the launch itself was, all of it knowable before the buy.
    # The tax window, in blocks: three seconds is 30 blocks, and the last of
    # them is the first offset that is certainly past it.
    window_taxed = window_bundled = 0
    dev = 0
    for row, _, off in trades:
        if row["kind"] != "buy" or off > 30:
            continue
        dev = max(dev, units(row["quote_in"], qd))
        if row.get("exempt"):
            window_bundled += 1
        else:
            window_taxed += 1
    return {
        "symbol": head.get("symbol", ""),
        "deployer": head["deployer"],
        "pair": head.get("pair_symbol", ""),
        "block": head.get("block"),
        "launched_at": head.get("launched_at"),
        "enter_block": enter_block,
        "via": head.get("via", ""),
        "creator_tax_bps": creator_bps,
        "curve_fee_bps": fee_bps,
        "exempt": len(head.get("exempt", [])),
        "declared_exempt": len(head.get("declared_exempt", [])),
        "dev_pct": 100 * dev / qr0,
        "window_taxed": window_taxed,
        "window_bundled": window_bundled,
        "run_at_entry": run_at_entry,
        "graduated": graduated,
        "trades": len(trades),
        "last_seen_at": last_elapsed,
        "path": path,
    }


def _apply(row, qr, tr, qd):
    """One recorded trade's effect on the reserves, from the record alone."""
    if row["kind"] == "buy":
        return (qr + units(row["quote_in"], qd) - units(row["fee"], qd)
                - units(row["creator_tax"], qd),
                tr - units(row["tokens_out"], 18))
    if row["kind"] == "sell":
        return (qr - units(row["quote_out"], qd) - units(row["fee"], qd)
                - units(row["creator_tax"], qd),
                tr + units(row["tokens_in"], 18))
    return qr + units(row["quote_spent"], qd), tr - units(row["tokens_locked"], 18)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("dir", help="the directory of .jsonl journals")
    ap.add_argument("--size-x100", type=int, default=100,
                    help="our size, in hundredths of a percent of the opening "
                         "quote reserve (default 100 = 1%%)")
    ap.add_argument("--enter-block", type=int, default=30,
                    help="blocks after the launch block that we buy at "
                         "(default 30, the first offset certainly past the "
                         "three-second tax window)")
    ap.add_argument("-o", "--out", default="-", help="where to write the rows")
    args = ap.parse_args()

    files = sorted(pathlib.Path(args.dir).glob("*.jsonl"))
    if not files:
        sys.exit(f"no journals in {args.dir}")
    out = sys.stdout if args.out == "-" else open(args.out, "w")
    kept = 0
    thrown = {}
    for f in files:
        try:
            row = replay(f, args.size_x100, args.enter_block)
        except Bad as e:
            # Grouped by the shape of the reason, not the numbers in it, so a
            # hundred truncated files read as one line rather than a hundred.
            key = str(e).split("(")[0].split(" at +")[0]
            key = " ".join(w for w in key.split() if not w.lstrip("-").isdigit())[:60]
            thrown.setdefault(key, []).append(f.name)
            continue
        row["file"] = f.name
        out.write(json.dumps(row) + "\n")
        kept += 1
    if out is not sys.stdout:
        out.close()
    print(f"{kept} launches replayed, {sum(len(v) for v in thrown.values())} thrown out",
          file=sys.stderr)
    for why, which in sorted(thrown.items(), key=lambda kv: -len(kv[1])):
        print(f"  {len(which):>4}  {why}", file=sys.stderr)
        for name in which[:3]:
            print(f"        {name}", file=sys.stderr)


if __name__ == "__main__":
    main()
