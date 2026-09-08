#!/usr/bin/env python3
"""Slice the replayed rows, honestly.

Reads what `replay.py` wrote and answers "does this trait separate the
launches worth buying". Three things here exist because the naive version of
that question lies:

  clustered by deployer  - one operator launches dozens of tokens in a night
                           and they are not independent observations. Every
                           mean is over deployers, and the bootstrap resamples
                           deployers rather than launches. Counting per launch
                           makes a bot that launched forty times look like
                           forty pieces of evidence.
  split in half          - by deployer, so no operator lands in both halves.
                           Rules are chosen on the first half; the number that
                           gets quoted comes from the second. With enough
                           splits tried, something always separates on the data
                           it was chosen from.
  the noise floor        - the best moment of a random walk is above where it
                           started, so "how much upside was ahead" is above 1
                           for any series at all. The same path with its steps
                           shuffled says what that is worth here.

    python3 analysis/replay.py launches/ > rows.jsonl
    python3 analysis/stats.py rows.jsonl
"""

import argparse
import json
import random
import statistics as st
from collections import defaultdict


# This chain runs 9.8 blocks to the second, measured off the launches where the
# feed gave both. The paths are indexed by block offset, because a block number
# is on every trade and the launch second often is not.
BLOCKS_PER_SECOND = 9.8


def value_at(path, block):
    """What the position is worth at `block`, from the last trade at or before."""
    v = path[0][1]
    for e, x in path:
        if e is not None and e <= block:
            v = x
    return v


def hold(sec):
    """Hold for `sec` seconds after entry, in blocks."""
    return lambda r: value_at(r["path"], r["enter_block"] + sec * BLOCKS_PER_SECOND)


def trail(pct, since=None):
    """Sell when the position gives back `pct` from its high."""
    def rule(r):
        p = [v for e, v in r["path"] if since is None or e is None or e >= since]
        if not p:
            return value_at(r["path"], since or 0)
        hi = p[0]
        for v in p:
            hi = max(hi, v)
            if v < hi * (1 - pct / 100):
                return v
        return p[-1]
    return rule


def by_deployer(rows, rule):
    """One number per operator, not per launch."""
    per = defaultdict(list)
    for r in rows:
        per[r["deployer"]].append(rule(r))
    return [st.mean(v) for v in per.values()]


def band(vals, draws=4000, seed=7):
    """Where the mean would land if this night happened again."""
    if len(vals) < 2:
        return float("nan"), float("nan"), float("nan")
    rng = random.Random(seed)
    b = sorted(st.mean(rng.choices(vals, k=len(vals))) for _ in range(draws))
    lo, hi = b[int(0.05 * draws)], b[int(0.95 * draws)]
    return lo, hi, 100 * sum(1 for x in b if x > 1) / draws


def report(title, groups, rule):
    print(f"\n  {title}")
    print(f"    {'':<24}{'наб':>5}{'зап':>5}{'средн':>9}{'медиана':>10}"
          f"{'win%':>6}   {'bootstrap 90%':<15}{'P>1':>4}")
    for name, sel in groups:
        g = [r for r in ROWS if sel(r)]
        if not g:
            print(f"    {name:<24}{0:>5}")
            continue
        v = by_deployer(g, rule)
        lo, hi, p = band(v)
        print(f"    {name:<24}{len(v):>5}{len(g):>5}{st.mean(v):>8.3f}x"
              f"{st.median(v):>9.3f}x{100*sum(1 for x in v if x>1)/len(v):>6.0f}"
              f"   {lo:.2f} .. {hi:<7.2f}{p:>4.0f}%")


def halves(rows, seed=7):
    """Split by deployer, so an operator never appears in both sides."""
    who = sorted({r["deployer"] for r in rows})
    rng = random.Random(seed)
    rng.shuffle(who)
    left = set(who[: len(who) // 2])
    return ([r for r in rows if r["deployer"] in left],
            [r for r in rows if r["deployer"] not in left])


def noise_floor(rows, after_blocks, seed=7):
    """What 'upside still ahead' is worth when there is no structure at all."""
    rng = random.Random(seed)
    out = []
    for r in rows:
        fut = [v for e, v in r["path"] if e is not None and e > after_blocks]
        if len(fut) < 3:
            continue
        steps = [fut[i + 1] / fut[i] for i in range(len(fut) - 1) if fut[i] > 0]
        if not steps:
            continue
        peaks = []
        for _ in range(200):
            rng.shuffle(steps)
            v = hi = 1.0
            for s in steps:
                v *= s
                hi = max(hi, v)
            peaks.append(hi)
        out.append(st.mean(peaks))
    return st.mean(out) if out else float("nan")


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("rows", help="the jsonl replay.py wrote")
    ap.add_argument("--exit", default="trail10",
                    help="trail10 | trail20 | hold10 | hold20 | hold30")
    args = ap.parse_args()

    global ROWS
    ROWS = [json.loads(l) for l in open(args.rows)]
    rules = {"trail10": trail(10), "trail20": trail(20),
             "hold10": hold(10), "hold20": hold(20), "hold30": hold(30)}
    rule = rules[args.exit]

    who = {r["deployer"] for r in ROWS}
    print(f"{len(ROWS)} запусков, {len(who)} деплойеров, выход: {args.exit}")
    serial = sorted(((sum(1 for r in ROWS if r['deployer'] == d), d) for d in who),
                    reverse=True)[:3]
    print("  самые частые: " + ", ".join(f"{d[:10]}… x{n}" for n, d in serial))
    peak = by_deployer(ROWS, lambda r: max(v for _, v in r["path"]))
    print(f"  средний пик {st.mean(peak):.3f}x     "
          f"шум {noise_floor(ROWS, 30):.3f}x  (всё ниже этого — не сигнал)")

    report("выход, без всякого отбора", [(args.exit, lambda r: True)], rule)
    report("dev buy, % фантома", [
        ("< 2%", lambda r: r["dev_pct"] < 2),
        ("2 - 5%", lambda r: 2 <= r["dev_pct"] < 5),
        ("5 - 15%", lambda r: 5 <= r["dev_pct"] < 15),
        (">= 15%", lambda r: r["dev_pct"] >= 15)], rule)
    report("creator fee, bps", [
        ("0", lambda r: r["creator_tax_bps"] == 0),
        ("1 - 200", lambda r: 0 < r["creator_tax_bps"] <= 200),
        ("> 200", lambda r: r["creator_tax_bps"] > 200)], rule)
    report("exempt count", [
        ("1", lambda r: r["exempt"] <= 1),
        ("2 - 5", lambda r: 2 <= r["exempt"] <= 5),
        (">= 6", lambda r: r["exempt"] >= 6)], rule)
    report("покупки в окне налога", [
        ("платившие: 0", lambda r: r["window_taxed"] == 0),
        ("платившие: 1", lambda r: r["window_taxed"] == 1),
        ("платившие: >= 2", lambda r: r["window_taxed"] >= 2),
        ("бандл: 2 - 4", lambda r: 2 <= r["window_bundled"] <= 4),
        ("бандл: >= 5", lambda r: r["window_bundled"] >= 5)], rule)

    a, b = halves(ROWS)
    print(f"\n  выборка пополам по деплойеру: {len(a)} / {len(b)} запусков")
    print("  правило выбирается слева, число называется справа:")
    for name, sel in (("exempt >= 6", lambda r: r["exempt"] >= 6),
                      ("exempt 2-5 (пропускать)", lambda r: 2 <= r["exempt"] <= 5),
                      ("dev buy >= 15%", lambda r: r["dev_pct"] >= 15)):
        va = by_deployer([r for r in a if sel(r)], rule)
        vb = by_deployer([r for r in b if sel(r)], rule)
        f = lambda v: f"{st.mean(v):.3f}x (n={len(v)})" if v else "нет данных"
        print(f"    {name:<26} слева {f(va):<20} справа {f(vb)}")


if __name__ == "__main__":
    main()
