#!/usr/bin/env python3
"""Build the bot's operator history out of the journals it already wrote.

The bot keeps one thing between runs: who has launched before and how it went.
A fresh store means every launch is a first sighting, and the rule that passes
over an operator with a bad record cannot fire until enough launches have been
seen through it - which, on a chain where 80% of deployers launch exactly once,
takes a long time to accumulate honestly.

The journals already hold all of it. This replays them, joins the launches that
share a wallet, closes each shadow position by the same rules the bot uses, and
writes the store in the format `src/operators.rs` reads.

**The exit is simulated the way the bot simulates it, not the way the analysis
measured it.** The bot's shadow sells into the trade that triggered it, with no
delay; the analysis allowed two blocks and got numbers a third of the size.
Seeding with the realistic ones would put the history on a different scale from
everything recorded after it, and the rule compares a median against break-even
- so consistency matters more here than realism.

    python3 analysis/seed.py launches/ -o operators.json
"""

import argparse
import json
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import replay  # noqa: E402

# Kept in step with src/operators.rs. A join that would take an operator past
# this is refused there, so it has to be refused here too or the seeded store
# would hold a blob the bot would never have built.
MAX_WALLETS = 5_000
KEEP_OUTCOMES = 64


def outcome(row, trail_bps, take_x100, hold_blocks):
    """Where the shadow position ends, in hundredths of what it cost.

    The path is already in multiples of cost, so this is `exit::decide` with
    the arithmetic done in floats: mark the high, take the target, then the
    stop, then the age. Marking BEFORE deciding is what makes the stop measure
    a give-back rather than a fall from wherever it happens to be.
    """
    path = row["path"]
    opened = path[0][0]
    high = 0.0
    for block, v in path:
        high = max(high, v)
        if take_x100 and v * 100 >= take_x100:
            return round(v * 100)
        if v < high * (1 - trail_bps / 10_000):
            return round(v * 100)
        if block >= opened + hold_blocks:
            return round(v * 100)
    return round(path[-1][1] * 100)


class Store:
    """The same union-find `src/operators.rs` keeps, in the same shape."""

    def __init__(self):
        self.of = {}
        self.alias = {}
        self.ops = {}
        self.next = 0

    def resolve(self, i):
        for _ in range(64):
            j = self.alias.get(i)
            if j is None or j == i:
                return i
            i = j
        return i

    def join(self, wallets, block):
        found = sorted({self.resolve(self.of[w]) for w in wallets if w in self.of})
        refused = set()
        if not found:
            i = self.next
            self.next += 1
            self.ops[i] = dict(launches=0, wallets=0, outcomes=[], dead=0,
                               first_block=block, last_block=block)
        else:
            i = max(found, key=lambda k: self.ops[k]["launches"])
            for other in found:
                if other == i:
                    continue
                a, b = self.ops[i], self.ops[other]
                if a["wallets"] + b["wallets"] > MAX_WALLETS:
                    refused.add(other)
                    continue
                a["launches"] += b["launches"]
                a["dead"] += b["dead"]
                a["wallets"] += b["wallets"]
                a["outcomes"] = (a["outcomes"] + b["outcomes"])[-KEEP_OUTCOMES:]
                a["first_block"] = min(a["first_block"], b["first_block"])
                a["last_block"] = max(a["last_block"], b["last_block"])
                del self.ops[other]
                self.alias[other] = i
        for w in wallets:
            held = self.of.get(w)
            if held is not None and self.resolve(held) in refused:
                continue
            if held != i:
                self.of[w] = i
                self.ops[i]["wallets"] += 1
        r = self.ops[i]
        r["launches"] += 1
        r["last_block"] = max(r["last_block"], block)
        if not r["first_block"]:
            r["first_block"] = block
        return i

    def record(self, i, x100):
        r = self.ops[self.resolve(i)]
        r["outcomes"] = (r["outcomes"] + [max(0, int(x100))])[-KEEP_OUTCOMES:]

    def note_dead(self, i):
        self.ops[self.resolve(i)]["dead"] += 1

    def as_json(self):
        return {
            "of": self.of,
            "alias": {str(k): v for k, v in self.alias.items()},
            "ops": {str(k): v for k, v in self.ops.items()},
            "next": self.next,
        }


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("dir", help="the directory of .jsonl journals")
    ap.add_argument("-o", "--out", default="operators.json")
    ap.add_argument("--size-x100", type=int, default=100)
    ap.add_argument("--enter-block", type=int, default=30)
    ap.add_argument("--trail-bps", type=int, default=500)
    ap.add_argument("--take-x100", type=int, default=200)
    ap.add_argument("--hold-blocks", type=int, default=588)
    args = ap.parse_args()

    files = sorted(pathlib.Path(args.dir).glob("*.jsonl"),
                   key=lambda p: int(p.name.split("-")[0]))
    if not files:
        sys.exit(f"no journals in {args.dir}")

    store = Store()
    kept = thrown = dead = 0
    for f in files:
        try:
            row = replay.replay(f, args.size_x100, args.enter_block)
        except replay.Bad:
            # The same journals the analysis refuses. A launch whose reserves
            # do not reconcile would file a made-up outcome under a real
            # operator, which is worse than filing none.
            thrown += 1
            continue
        op = store.join(row["wallets"], row["block"])
        store.record(op, outcome(row, args.trail_bps, args.take_x100, args.hold_blocks))
        if row["outsiders"] == 0:
            store.note_dead(op)
            dead += 1
        kept += 1

    out = pathlib.Path(args.out)
    out.write_text(json.dumps(store.as_json(), indent=2))
    ops = store.ops
    multi = [r for r in ops.values() if r["launches"] > 1]
    print(f"{kept} launches, {thrown} thrown out, {dead} with no outside buyer")
    print(f"{len(ops)} operators, {len(store.of)} wallets -> {out}")
    print(f"{len(multi)} operators launched more than once, "
          f"largest {max((r['launches'] for r in ops.values()), default=0)}")
    ranked = sorted(ops.values(), key=lambda r: -r["launches"])[:8]
    print(f"\n  {'launches':>9}{'wallets':>9}{'dead':>6}{'median':>9}")
    for r in ranked:
        o = sorted(r["outcomes"])
        med = f"{o[len(o)//2]/100:.2f}x" if o else "-"
        print(f"  {r['launches']:>9}{r['wallets']:>9}{r['dead']:>6}{med:>9}")


if __name__ == "__main__":
    main()
