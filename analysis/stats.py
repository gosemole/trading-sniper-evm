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


def trail(pct, take=None, lag=1):
    """Sell on a give-back from the high, or at a multiple of cost.

    `lag` is blocks between seeing a price and selling into it, and it is not
    a detail: zero means selling into the very trade that broke the stop,
    which is not a fast reaction but an impossible one - that trade IS the
    price move, and seeing it means its block is already made. On the launches
    the live filter keeps, zero reports +76.5 stakes and one reports +31.9.
    Everything past the first block is nearly flat: two is +27.4, five +24.1.
    """
    def rule(r):
        p = r["path"]
        hi = p[0][1]
        armed = None
        for b, v in p:
            if armed is not None and b >= armed + lag:
                return v
            hi = max(hi, v)
            if armed is None and ((take and v >= take) or v < hi * (1 - pct / 100)):
                armed = b
        return p[-1][1]
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


def totals(title, groups, rule):
    """The portfolio, not the average.

    One unit into every launch in the group, and what comes back out. A mean
    hides which half of the ledger it came from: the same 1.03x is a business
    if the profit is spread over hundreds of launches and a lottery ticket if
    two of them carry it. These columns say which.

    Amounts are multiples of the stake, so launches in different pair tokens
    add up - a fixed fraction of each curve is the same bet in each currency.
    """
    print(f"\n  {title}")
    print(f"    {'':<24}{'зап':>5}{'итого':>9}{'прибыль':>10}{'убыток':>9}"
          f"{'лучший':>8}{'пик':>9}{'от пика':>8}{'top10':>7}")
    for name, sel in groups:
        g = [r for r in ROWS if sel(r)]
        if not g:
            print(f"    {name:<24}{0:>5}")
            continue
        out = [rule(r) - 1 for r in g]
        peak = [max(v for _, v in r["path"]) - 1 for r in g]
        win = sorted((x for x in out if x > 0), reverse=True)
        loss = sum(x for x in out if x < 0)
        net = sum(out)
        pk = sum(peak)
        top10 = sum(win[:10]) / sum(win) * 100 if win else 0.0
        print(f"    {name:<24}{len(g):>5}{net:>+9.1f}{sum(win):>+10.1f}{loss:>+9.1f}"
              f"{max(out, default=0):>+8.1f}{pk:>+9.1f}"
              # Meaningless unless there was a peak to capture: a share of a
              # negative number reads as a percentage and is not one.
              f"{(100*net/pk if pk > 0 else 0):>7.0f}%{top10:>6.0f}%")


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
    ap.add_argument("--lag", type=int, default=1,
                    help="blocks between seeing a price and selling into it "
                         "(default 1; zero is not achievable, see trail())")
    ap.add_argument("--exit", default="live",
                    help="live | trail5 | trail10 | trail20 | hold10 | hold20 | hold30")
    args = ap.parse_args()

    global ROWS
    ROWS = [json.loads(l) for l in open(args.rows)]
    rules = {
        "live": trail(5, take=2.0, lag=args.lag),
        "trail5": trail(5, lag=args.lag),
        "trail10": trail(10, lag=args.lag),
        "trail20": trail(20, lag=args.lag),
        "hold10": hold(10),
        "hold20": hold(20),
        "hold30": hold(30),
    }
    rule = rules[args.exit]

    who = {r["deployer"] for r in ROWS}
    print(f"{len(ROWS)} запусков, {len(who)} деплойеров, выход: {args.exit}")
    serial = sorted(((sum(1 for r in ROWS if r['deployer'] == d), d) for d in who),
                    reverse=True)[:3]
    print("  самые частые: " + ", ".join(f"{d[:10]}… x{n}" for n, d in serial))
    # What a number in this report is. Every column below is one of these two
    # and nothing else, and reading a stake as ETH or a multiple as a total is
    # how a result gets quoted at ten times what it was.
    # From an ETH row specifically. Taken from whichever row came first it
    # printed an NVDA curve's stake and called it ETH, which is a factor of ten
    # in the one line whose whole job is to say what the numbers mean.
    stake = next((r.get("stake") for r in ROWS
                  if r.get("pair") == "ETH" and r.get("stake")), None)
    print("\n  единицы:")
    print("    СТАВКА  — то, что кладётся в один запуск: доля фантомного резерва")
    print("              его кривой, заданная size_x100."
          f"{' На ETH-парах это ' + stake + ' ETH.' if stake else ''}")
    print("    итого   — ставок в плюсе, если положить ПО ОДНОЙ в каждый запуск")
    print("              группы. +43.8 значит: вложено 296 ставок, вернулось 339.8.")
    print("    на зап  — то же, делённое на число запусков. +0.148 это +14.8%.")
    print("    1.07x   — сколько вернулось на единицу вложенного. 1.00x это ноль,")
    print("              а не прибыль.")
    print("    win     — доля запусков, закрывшихся выше вложенного.")
    peak = by_deployer(ROWS, lambda r: max(v for _, v in r["path"]))
    # Two numbers about the same paths, and NOT a comparison. The floor is
    # what the best moment of a shuffled path is worth - it applies to "how
    # much upside was still ahead", which is a statistic this report no longer
    # uses. The P&L below is measured by rules that could actually be run, and
    # nothing about it needs clearing a noise floor.
    print(f"  средний пик {st.mean(peak):.3f}x")
    print(f"  для справки: у пути с перемешанными шагами лучший момент стоит "
          f"{noise_floor(ROWS, 30):.3f}x —")
    print(f"  это про \"сколько было впереди\", а не про итог правил ниже")

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

    totals("портфель: одна единица в каждый запуск", [
        ("всё", lambda r: True),
        ("exempt = 1", lambda r: r["exempt"] <= 1),
        ("exempt 2 - 5", lambda r: 2 <= r["exempt"] <= 5),
        ("exempt >= 6", lambda r: r["exempt"] >= 6),
        ("бандл >= 5", lambda r: r["window_bundled"] >= 5),
        ("creator fee = 0", lambda r: r["creator_tax_bps"] == 0),
        ("dev buy >= 15%", lambda r: r["dev_pct"] >= 15),
        # What live.toml actually refuses down to, in one row. The single
        # columns above say which field separates; this says whether the
        # combination pays for the launches it throws away, which is the only
        # question a config answers.
        # Pair included, because live.toml names one: on the night's data ETH
        # carried +43.0 stakes over 266 launches and every other token
        # together carried +0.8 over 30.
        ("live.toml (без пары)",
         lambda r: r["exempt"] >= 6
         and r["creator_tax_bps"] == 0
         and 2 <= r["dev_pct"] <= 15),
        ("live.toml + pairs=[ETH]",
         lambda r: r["exempt"] >= 6
         and r["creator_tax_bps"] == 0
         and 2 <= r["dev_pct"] <= 15
         and r["pair"] == "ETH")], rule)

    # The live filter, against the delay it will actually run at and the
    # widths it might run at. Two tables, because the answer to "is this worth
    # doing" and the answer to "at what settings" are different questions and
    # reading one for the other is how a number gets quoted that nobody
    # measured.
    live = [r for r in ROWS if r["exempt"] >= 6 and r["creator_tax_bps"] == 0
            and 2 <= r["dev_pct"] <= 15 and r["pair"] == "ETH"]
    if live:
        print(f"\n  live.toml: {len(live)} запусков из {len(ROWS)}")
        print(f"    {'задержка':<24}" + "".join(f"{'стоп '+str(p)+'%':>14}" for p in (3, 5, 10)))
        for lag in (0, 1, 2, 3):
            row = f"    +{lag} блок{'а' if lag in (2, 3) else 'ов' if lag != 1 else ''}".ljust(28)
            for pct in (3, 5, 10):
                o = [trail(pct, take=2.0, lag=lag)(r) - 1 for r in live]
                row += f"{sum(o):>+9.1f} ({100*sum(1 for x in o if x>0)/len(o):>2.0f}%)"
            print(row)
        print(f"\n    {'выход при задержке 1':<30}{'итого':>9}{'на зап':>9}{'медиана':>10}{'win':>6}")
        for name, pct, take in (("трейлинг 5%", 5, None), ("трейлинг 3%", 3, None),
                                ("трейлинг 10%", 10, None),
                                ("трейлинг 5% + тейк 1.5x", 5, 1.5),
                                ("трейлинг 5% + тейк 2x", 5, 2.0),
                                ("трейлинг 5% + тейк 3x", 5, 3.0)):
            o = [trail(pct, take=take, lag=1)(r) - 1 for r in live]
            print(f"    {name:<30}{sum(o):>+9.1f}{sum(o)/len(o):>+9.3f}"
                  f"{st.median(o):>+10.3f}{100*sum(1 for x in o if x>0)/len(o):>5.0f}%")
        # The two questions a portfolio of 69 launches cannot answer by
        # standing there being positive: how much of it is luck, and how much
        # of it is a handful of launches.
        #
        # The interval resamples the launches themselves rather than the
        # deployers, because this group is small enough that most deployers in
        # it appear once - and a portfolio is a sum, so it is the sum that has
        # to be resampled. Read the low end: that is the run this could have
        # been.
        o = [trail(5, take=2.0, lag=1)(r) - 1 for r in live]
        rng = random.Random(7)
        draws = sorted(sum(rng.choices(o, k=len(o))) for _ in range(4000))
        lo, hi = draws[200], draws[3800]
        above = 100 * sum(1 for x in draws if x > 0) / len(draws)
        print(f"\n    портфель {sum(o):+.1f} ставок, 90% между {lo:+.1f} и {hi:+.1f}, "
              f"выше нуля в {above:.0f}% пересборок")
        # Positive skew is the shape of this whole strategy, so the question is
        # not whether the best launches carry it - they do - but whether
        # anything is left when they do not arrive.
        drop = sorted(o, reverse=True)
        for k in (5, 10, 20):
            if k < len(drop):
                rest = drop[k:]
                print(f"    без {k:>2} лучших: {sum(rest):+.1f} ставок "
                      f"на {len(rest)} запусках ({sum(rest)/len(rest):+.3f} на зап)")

        # And in money, because a stake is a share of each curve and they
        # differ - the sum of multiples is not what a wallet would show.
        staked = sum(float(r.get("stake", 0)) for r in live)
        if staked > 0:
            got = sum(float(r["stake"]) * trail(5, take=2.0, lag=1)(r) for r in live)
            print(f"\n    в ETH: поставлено {staked:.3f}, вернулось {got:.3f}, "
                  f"итого {got - staked:+.3f} ETH ({100*(got-staked)/staked:+.1f}%)")
        else:
            print("\n    в ETH: пересоберите rows.jsonl - в этих строках нет ставки")

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
