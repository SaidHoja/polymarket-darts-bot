"""Rebuild L3 books from data-analysis/backtest/*.csv; dump top-of-book series + trades per settled market."""
import csv, json, pickle, sys
from datetime import datetime

ROOT = sys.argv[1]
OUT = sys.argv[2]
HOLDOUT = datetime.fromisoformat('2026-09-29T00:00:00+00:00').timestamp() * 1e9
markets = json.load(open(f'{ROOT}/markets.json'))['markets']
res = []
for m in markets:
    if m['settlement'] is None:
        continue
    orders = {}  # id -> (side, px, qty)
    tob = []     # (ns, bid, bidsz, ask, asksz)
    trades = []  # (ns, px, qty, aggressor)
    last_key = None
    def top():
        bids = {}; asks = {}
        for s, p, q in orders.values():
            d = bids if s == 'b' else asks
            d[p] = d.get(p, 0) + q
        b = max(bids) if bids else None
        a = min(asks) if asks else None
        return b, bids.get(b, 0), a, asks.get(a, 0)
    cur_msg = None
    with open(f"{ROOT}/{m['slug']}.csv") as f:
        r = csv.reader(f); next(r)
        pending = False
        for row in r:
            ns = int(row[0]); kind = row[4]
            key = (row[0], row[2])
            if key != cur_msg and pending:
                t = top()
                if not tob or tob[-1][1:] != t:
                    tob.append((prev_ns, *t))
                pending = False
            cur_msg = key; prev_ns = ns
            if kind == 'M':
                orders.clear()
            elif kind in ('S', 'A'):
                orders[row[8]] = (row[5], float(row[6]), float(row[7]))
            elif kind == 'U':
                if row[8] in orders:
                    s, p, _ = orders[row[8]]
                    orders[row[8]] = (row[5] or s, float(row[6]) if row[6] else p, float(row[7]))
            elif kind == 'D':
                orders.pop(row[8], None)
            elif kind == 'T':
                trades.append((ns, float(row[6]), float(row[7]), row[9]))
            pending = True
        if pending:
            t = top()
            if not tob or tob[-1][1:] != t:
                tob.append((prev_ns, *t))
    start = datetime.fromisoformat(m['game_start']).timestamp() * 1e9
    res.append(dict(slug=m['slug'], league=m['league'], settle=m['settlement'], start=start,
                    split='holdout' if (tob and tob[0][0] >= HOLDOUT) else 'dev',
                    fee=m['fee_coefficient'], yes_ordering=m['yes_ordering'], tob=tob, trades=trades))
pickle.dump(res, open(OUT, 'wb'))
import collections
print(len(res), collections.Counter((x['split'], x['league']) for x in res))
