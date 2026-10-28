"""Event study: book behaviour around in-play leg changes (captures 10-27, 10-28)."""
import json, sys, bisect, collections, statistics as st
from datetime import datetime, timezone
from functools import lru_cache

ROOT = sys.argv[1]
DAYS = ['2026-10-27', '2026-10-28']
MIN_SIZE = float(sys.argv[2]) if len(sys.argv) > 2 else 1.0
S = 1_000_000_000

def iso_ns(s):
    s = s.rstrip('Z')
    main, _, frac = s.partition('.')
    dt = datetime.fromisoformat(main).replace(tzinfo=timezone.utc)
    return int(dt.timestamp()) * S + int((frac + '000000000')[:9])

# ---- game
legs = []  # (slug, t_seen_ns, t_venue_ns, prev, new)
for d in DAYS:
    last = {}
    for l in open(f'{ROOT}/captures/game-{d}.jsonl'):
        r = json.loads(l); g = r['game']
        e = g['event_slug']; sc = g['score']
        if sc and e in last and last[e] and tuple(sc) != tuple(last[e]):
            legs.append(('aec-' + e, r['recv_at_ns'], g['updated_ns'], tuple(last[e]), tuple(sc)))
        if sc: last[e] = sc
slugs = {l[0] for l in legs}

# ---- books and trades
books = collections.defaultdict(list)   # slug -> [(t, bid, ask, bidsz, asksz)]
trades = collections.defaultdict(list)  # slug -> [(t, px, qty, taker_buys_yes)]
def touch(levels, best):
    for lv in sorted(levels, key=lambda x: -float(x['px']['value']) if best == 'bid' else float(x['px']['value'])):
        if float(lv['qty']) >= MIN_SIZE:
            return float(lv['px']['value']), float(lv['qty'])
    return None, 0
for d in DAYS:
    for l in open(f'{ROOT}/captures/{d}.jsonl'):
        if not any(s in l for s in slugs): continue
        r = json.loads(l); m = json.loads(r['raw'])
        t = iso_ns(r['recv_at'])
        if 'marketData' in m:
            md = m['marketData']; s = md['marketSlug']
            if s not in slugs: continue
            b, bs = touch(md.get('bids', []), 'bid'); a, as_ = touch(md.get('offers', []), 'ask')
            books[s].append((t, b, a, bs, as_))
        elif 'trade' in m:
            tr = m['trade']; s = tr['marketSlug']
            if s not in slugs: continue
            # maker side BUY on YES => taker sold YES
            mk = tr['maker']
            maker_buys_yes = (mk['side'] == 'ORDER_SIDE_BUY') == (mk.get('outcomeSide') != 'OUTCOME_SIDE_NO')
            trades[s].append((iso_ns(tr['tradeTime']), float(tr['price']['value']), float(tr['quantity']['value']), not maker_buys_yes))
for s in books: books[s].sort()
times = {s: [x[0] for x in v] for s, v in books.items()}

def at(s, t):
    i = bisect.bisect_right(times[s], t) - 1
    if i < 0: return None
    _, b, a, *_ = books[s][i]
    return (b, a)

def mid(s, t):
    q = at(s, t)
    if not q or q[0] is None or q[1] is None: return None
    return (q[0] + q[1]) / 2

def spread(s, t):
    q = at(s, t)
    if not q or q[0] is None or q[1] is None: return None
    return q[1] - q[0]

# ---- model: first to 4 legs, alternating throw, hold prob for thrower
N = 4
@lru_cache(None)
def W(a, b, home_throws, hh, ha):
    """P(home wins match) from (a,b), home throwing first in this leg."""
    if a == N: return 1.0
    if b == N: return 0.0
    p = hh if home_throws else 1 - ha  # P(home wins this leg)
    return p * W(a + 1, b, not home_throws, hh, ha) + (1 - p) * W(a, b + 1, not home_throws, hh, ha)

def Wmix(a, b, ph1, tau, dlt):
    hh, ha = tau + dlt / 2, tau - dlt / 2
    # who throws first in the current leg: home if (home threw leg 1) xor (a+b odd)
    odd = (a + b) % 2 == 1
    w_h1 = W(a, b, not odd, hh, ha); w_a1 = W(a, b, odd, hh, ha)
    return ph1 * w_h1 + (1 - ph1) * w_a1

def solve_delta(a, b, ph1, tau, m):
    lo, hi = -0.8, 0.8
    for _ in range(50):
        md = (lo + hi) / 2
        if Wmix(a, b, ph1, tau, md) < m: lo = md
        else: hi = md
    return (lo + hi) / 2

OFFS = [-60, -30, -15, -10, -5, -2, 0, 2, 5, 10, 15, 20, 30, 45, 60, 90, 120]
rows = []
# prior over who threw first in leg 1, updated by Bayes per match
ph1_by = {}
legs.sort(key=lambda x: (x[0], x[1]))
prev_seen = {}
for s, t0, tv, prev, new in legs:
    if s not in books: continue
    home_won = new[0] > prev[0]
    if sum(new) - sum(prev) != 1: continue
    m = {o: mid(s, t0 + o * S) for o in OFFS}
    sp = {o: spread(s, t0 + o * S) for o in OFFS}
    if m[-30] is None or m[60] is None: continue
    sign = 1 if home_won else -1
    # model step from mid at -30 (before most in-leg leakage? check) using prev score
    tau = 0.6
    ph1 = ph1_by.get(s, 0.5)
    base = m[-60] if m[-60] is not None else m[-30]
    dlt = solve_delta(*prev, ph1, tau, min(max(base, 0.02), 0.98))
    model_new = Wmix(*new, ph1, tau, dlt)
    model_step = model_new - Wmix(*prev, ph1, tau, dlt)
    # bayes update of ph1 on the leg outcome
    hh, ha = tau + dlt / 2, tau - dlt / 2
    odd = sum(prev) % 2 == 1
    p_if_h1 = hh if not odd else 1 - ha
    p_if_a1 = (1 - ha) if not odd else hh
    lh = p_if_h1 if home_won else 1 - p_if_h1
    la = p_if_a1 if home_won else 1 - p_if_a1
    ph1_by[s] = ph1 * lh / (ph1 * lh + (1 - ph1) * la)
    gap = (t0 - prev_seen[s]) / S if s in prev_seen else None
    prev_seen[s] = t0
    tr_pre = [x for x in trades[s] if t0 - 30 * S <= x[0] < t0]
    tr_post = [x for x in trades[s] if t0 <= x[0] < t0 + 30 * S]
    rows.append(dict(slug=s, t0=t0, poll_lag=(t0 - tv) / S if tv else None, prev=prev, new=new, sign=sign,
                     m=m, sp=sp, model_step=model_step, base=base, gap=gap,
                     tr_pre=tr_pre, tr_post=tr_post, deciding=max(new) == N))

json.dump([{**r, 'm': {str(k): v for k, v in r['m'].items()}, 'sp': {str(k): v for k, v in r['sp'].items()}} for r in rows],
          open(sys.argv[3] if len(sys.argv) > 3 else '/dev/null', 'w'))

def q(xs, p):
    xs = sorted(x for x in xs if x is not None)
    if not xs: return None
    return xs[min(len(xs) - 1, int(p * len(xs)))]
def fmt(x): return '—' if x is None else f'{x:+.3f}'

print(f'MIN_SIZE={MIN_SIZE}  leg changes used: {len(rows)}  (matches {len({r["slug"] for r in rows})})')
print('poll lag (seen − venue updatedAt) s: p50', q([r['poll_lag'] for r in rows], .5), 'p90', q([r['poll_lag'] for r in rows], .9))
print('leg gap s: p10', q([r['gap'] for r in rows], .1), 'p50', q([r['gap'] for r in rows], .5), 'p90', q([r['gap'] for r in rows], .9))

nd = [r for r in rows if not r['deciding']]
print(f'\nnon-deciding legs n={len(nd)}. Signed mid move (toward leg winner) relative to mid at -60s:')
print(' off   p25    p50    p75   mean   spread_p50')
for o in OFFS:
    xs = [r['sign'] * (r['m'][o] - r['base']) for r in nd if r['m'][o] is not None]
    print(f'{o:+4d} {fmt(q(xs,.25))} {fmt(q(xs,.5))} {fmt(q(xs,.75))} {fmt(st.mean(xs) if xs else None)}  {q([r["sp"][o] for r in nd], .5)}')

# share of the eventual (+60) move already done at t0, and at +5/+15/+30
print('\nshare of move (−60 → +60) done by offset, legs with |move| >= 3c:')
big = [r for r in nd if abs(r['m'][60] - r['base']) >= 0.03]
for o in [-30, -10, -5, 0, 2, 5, 10, 15, 30]:
    xs = [(r['m'][o] - r['base']) / (r['m'][60] - r['base']) for r in big if r['m'][o] is not None]
    print(f'  {o:+4d}s  median {q(xs,.5):.2f}  p25 {q(xs,.25):.2f}  p75 {q(xs,.75):.2f}  (n={len(xs)})')

# model step vs realised
print('\nmodel step (tau=0.6, delta from −60 mid) vs realised −60→+60 signed move:')
pairs = [(r['sign'] * r['model_step'], r['sign'] * (r['m'][60] - r['base'])) for r in nd]
ms = [p[0] for p in pairs]; rs = [p[1] for p in pairs]
print('  model p50', fmt(q(ms, .5)), ' realised p50', fmt(q(rs, .5)), ' mean ratio realised/model', f"{st.mean(rs)/st.mean(ms):.2f}")
print('  realised wrong-way (< −1c):', sum(1 for x in rs if x < -0.01), '/', len(rs))
# residual after t0: what's left to capture if we reprice at t0
res = [r['sign'] * (r['m'][60] - r['m'][0]) for r in nd if r['m'][0] is not None]
print('  remaining move t0→+60: p25', fmt(q(res, .25)), 'p50', fmt(q(res, .5)), 'p75', fmt(q(res, .75)), 'mean', fmt(st.mean(res)))
res5 = [r['sign'] * (r['m'][60] - r['m'][5]) for r in nd if r['m'][5] is not None]
print('  remaining move +5→+60: p50', fmt(q(res5, .5)), 'mean', fmt(st.mean(res5)))
# model vs market at t0: does model step predict remaining move?
mrem = [(r['sign'] * (r['base'] + r['model_step'] - r['m'][0]), r['sign'] * (r['m'][60] - r['m'][0])) for r in nd if r['m'][0] is not None]
if len(mrem) > 3:
    xs, ys = zip(*mrem)
    try: print('  corr(model gap at t0, remaining move):', f'{st.correlation(xs, ys):.2f}')
    except Exception: pass

# trades
tp = [x for r in nd for x in r['tr_pre']]; tq = [x for r in nd for x in r['tr_post']]
def dir_share(rs_, key):
    n = agree = 0
    for r in rs_:
        for x in r[key]:
            n += 1; agree += (x[3] == (r['sign'] > 0))
    return n, agree
print('\ntrades in [−30,0): n=%d, taker in leg-winner direction=%d' % dir_share(nd, 'tr_pre'))
print('trades in [0,+30): n=%d, taker in leg-winner direction=%d' % dir_share(nd, 'tr_post'))

# deciding legs
dl = [r for r in rows if r['deciding']]
print(f'\ndeciding legs n={len(dl)}: signed mid at −30/−10/0/+5 rel −60:',
      [fmt(q([r['sign'] * (r['m'][o] - r['base']) for r in dl if r['m'][o] is not None], .5)) for o in [-30, -10, 0, 5]])
