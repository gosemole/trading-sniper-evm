"""What the rules would have got, beside what the position actually got.

Every other number here is a counterfactual: the replay invents an entry, walks
the recorded trades and reports what a rule would have returned. This one does
not invent anything. It takes the launches the bot really bought - the ones
whose journal carries a `fill` for our own buy and a `closed` for what came
back - and asks the same exit rule the same question against the same curve,
from the same entry, with the same tokens.

So the two numbers differ for exactly one reason at a time:

* the model's view of the curve was wrong, or
* the rule's answer was right and the fill was not, which is what a revert and
  a retry three blocks later look like.

Live, seven positions came back at 2.02x, 0.90x, 0.55x, 0.47x, 0.35x, 0.33x
and 0.31x while a hundred and sixty replayed launches averaged 1.115x. One of
those two is describing something the other is not, and until this ran there
was no way to say which.

    python3 analysis/versus.py launches/
"""

import argparse
import json
import pathlib
import statistics as st
import sys

from replay import units, sell

# The exit as live.toml has it, and the lag the chain actually has. These have
# to be kept level with the config: a comparison run at a stop the bot is not
# using answers a question nobody asked, and it answers it plausibly, which is
# worse than failing.
TRAIL_PCT = 3
TAKE = 2.0
LAG = 2


def one(path):
    """One journal, or None when the bot never bought this launch."""
    rows = []
    for raw in open(path):
        raw = raw.strip()
        if raw:
            try:
                rows.append(json.loads(raw))
            except json.JSONDecodeError:
                # A half-written last line is what a killed process leaves.
                break
    if not rows or rows[0].get("kind") != "launch":
        return None
    head = rows[0]
    qd = int(head.get("pair_decimals", 18))
    fee = int(head.get("curve_fee_bps", 0))
    ctax = int(head.get("creator_tax_bps", 0))

    bought = next(
        (r for r in rows if r.get("kind") == "fill" and r.get("leg") == "buy"), None
    )
    closed = next((r for r in rows if r.get("kind") == "closed"), None)
    if not bought or not closed:
        return None

    tokens = units(bought["tokens"], 18)
    cost = units(bought["quote"], qd)
    from_block = int(bought["block"])
    if tokens <= 0 or cost <= 0:
        return None

    # What the position was worth after every trade from our buy onward, priced
    # the way the bot prices it: the whole position sold into the curve, our own
    # impact included. The reserves are the ones the journal recorded, which
    # `replay.py` checks against the chain's own fills.
    path_pts = []
    for r in rows:
        if r.get("kind") not in ("buy", "sell", "buyback"):
            continue
        b = int(r.get("block", 0))
        if b < from_block:
            continue
        qr = units(r["quote_reserve"], qd)
        tr = units(r["token_reserve"], 18)
        worth, _, _ = sell(qr, tr, fee, ctax, tokens)
        path_pts.append((b, worth / cost))
    if not path_pts:
        return None

    # The same three rules, in the same order, at the lag the chain has.
    hi = path_pts[0][1]
    armed = None
    model = path_pts[-1][1]
    for b, v in path_pts:
        if armed is not None and b >= armed + LAG:
            model = v
            break
        hi = max(hi, v)
        if armed is None and (v >= TAKE or v < hi * (1 - TRAIL_PCT / 100)):
            armed = b
    return {
        "file": pathlib.Path(path).name,
        "why": closed.get("why", ""),
        "model": model,
        "actual": int(closed["x100"]) / 100,
        "points": len(path_pts),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("dir", help="the directory of .jsonl journals")
    args = ap.parse_args()

    got = []
    for f in sorted(pathlib.Path(args.dir).glob("*.jsonl")):
        try:
            r = one(f)
        except (KeyError, ValueError, ZeroDivisionError) as e:
            print(f"  skipped {f.name}: {e}", file=sys.stderr)
            continue
        if r:
            got.append(r)
    if not got:
        print("no journal here carries both a fill of ours and a close", file=sys.stderr)
        return

    print(f"\n  {len(got)} positions the bot actually took\n")
    print(f"    {'launch':<24}{'модель':>9}{'факт':>9}{'разница':>10}  почему")
    for r in got:
        d = r["actual"] - r["model"]
        print(
            f"    {r['file'][:23]:<24}{r['model']:>8.2f}x{r['actual']:>8.2f}x"
            f"{d:>+10.2f}  {r['why']}"
        )
    m = [r["model"] for r in got]
    a = [r["actual"] for r in got]
    print(f"\n    средняя модель  {st.mean(m):.3f}x")
    print(f"    средний факт    {st.mean(a):.3f}x")
    print(f"    портфель модель {sum(m) - len(m):+.2f} ставок")
    print(f"    портфель факт   {sum(a) - len(a):+.2f} ставок")
    # Which of the two questions this answers.
    close = sum(1 for r in got if abs(r["actual"] - r["model"]) < 0.05)
    print(
        f"\n    сошлись в пределах 0.05x: {close} из {len(got)}"
        f" - если это большинство, модель права и вопрос к порогам;"
        f" если нет, вопрос к исполнению"
    )


if __name__ == "__main__":
    main()
