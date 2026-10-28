"""Taker strategies on rebuilt top-of-book. One trade per market max, 1 contract,
fill at the touch 1 s after the signal (needs >= 1 contract there), taker fee
theta*p*(1-p), held to settlement unless the strategy exits at the touch."""
import random, sys
from common import *

LAT = 1e9  # signal -> fill

def buy(m, t, side):
    """Fill one contract of side ('yes'/'no') at t+LAT. Returns (cost, side, t_fill) or None."""
    x = tob_at(m, t + LAT)
    if not x: return None
    _, b, bs, a, as_ = x
    if side == 'yes':
        if a is None or as_ < 1: return None
        return a + fee(m['fee'], a), side
    if b is None or bs < 1: return None
    return (1 - b) + fee(m['fee'], b), side

def sell(m, t, side):
    """Exit one contract at t+LAT; proceeds net of fee, or None if no bid."""
    x = tob_at(m, t + LAT)
    if not x: return None
    _, b, bs, a, as_ = x
    if side == 'yes':
        return None if b is None else b - fee(m['fee'], b)
    return None if a is None else (1 - a) - fee(m['fee'], a)

def payout(m, side): return m['settle'] if side == 'yes' else 1 - m['settle']

def settle_pnl(m, entry):
    if entry is None: return None
    cost, side = entry
    return payout(m, side) - cost

# ---------- strategies: each returns pnl (float) or None (no trade) ----------

def prematch_fav(m, lo, hi, off=5*MIN, maxspr=0.10):
    t = m['start'] - off; p = mid_at(m, t, maxspr)
    if p is None: return None
    if lo <= p < hi: return settle_pnl(m, buy(m, t, 'yes'))
    if lo <= 1 - p < hi: return settle_pnl(m, buy(m, t, 'no'))

def prematch_side(m, side, off=5*MIN, maxspr=0.10):
    t = m['start'] - off
    if mid_at(m, t, maxspr) is None: return None
    return settle_pnl(m, buy(m, t, side))

def prematch_drift(m, d, follow, frm=6*H, off=5*MIN, maxspr=0.10):
    p0 = mid_at(m, m['start'] - frm, 0.15); t = m['start'] - off; p1 = mid_at(m, t, maxspr)
    if p0 is None or p1 is None or abs(p1 - p0) < d: return None
    up = p1 > p0
    side = ('yes' if up else 'no') if follow else ('no' if up else 'yes')
    return settle_pnl(m, buy(m, t, side))

def inplay_iter(m, maxspr):
    """(t, mid) for each in-play top-of-book change with a tight enough book."""
    for x in m['tob']:
        if x[0] < m['start']: continue
        if x[1] is None or x[3] is None or x[3] - x[1] > maxspr: continue
        yield x[0], (x[1] + x[3]) / 2

def inplay_fav(m, th, maxspr=0.05, max_px=0.99):
    """First in-play moment one side's mid >= th: buy it (if ask <= max_px)."""
    for t, p in inplay_iter(m, maxspr):
        if p >= th or 1 - p >= th:
            side = 'yes' if p >= th else 'no'
            e = buy(m, t, side)
            if e and e[0] <= max_px + 0.01: return settle_pnl(m, e)
            return None

def inplay_shock(m, k, window, follow, hold=None, maxspr=0.06):
    """In-play mid moves >= k within `window`: follow or fade. Hold to settlement or `hold` ns."""
    hist = []
    for t, p in inplay_iter(m, maxspr):
        hist.append((t, p))
        while hist and hist[0][0] < t - window: hist.pop(0)
        dp = p - hist[0][1]
        if abs(dp) >= k:
            up = dp > 0
            side = ('yes' if up else 'no') if follow else ('no' if up else 'yes')
            e = buy(m, t, side)
            if e is None: return None
            if hold is None: return settle_pnl(m, e)
            out = sell(m, t + hold, side)
            return None if out is None else out - e[0]

def prematch_stale_open(m, off=12*H):
    """Control: buy the side the first quotes favour, early."""
    t = m['tob'][0][0] + MIN
    p = mid_at(m, t, 0.2)
    if p is None or t > m['start']: return None
    return settle_pnl(m, buy(m, t, 'yes' if p >= 0.5 else 'no'))

STRATS = {
    'T1 prematch fav 0.55-0.70': lambda m: prematch_fav(m, 0.55, 0.70),
    'T1 prematch fav 0.70-0.85': lambda m: prematch_fav(m, 0.70, 0.85),
    'T1 prematch fav 0.85-0.99': lambda m: prematch_fav(m, 0.85, 0.99),
    'T2 prematch dog 0.15-0.30': lambda m: prematch_fav(m, 0.15, 0.30),
    'T2 prematch dog 0.30-0.45': lambda m: prematch_fav(m, 0.30, 0.45),
    'T3 always YES (home) T-5m': lambda m: prematch_side(m, 'yes'),
    'T3 always NO (away) T-5m': lambda m: prematch_side(m, 'no'),
    'T4 drift follow >=5c': lambda m: prematch_drift(m, 0.05, True),
    'T4 drift fade >=5c': lambda m: prematch_drift(m, 0.05, False),
    'T4 drift follow >=10c': lambda m: prematch_drift(m, 0.10, True),
    'T4 drift fade >=10c': lambda m: prematch_drift(m, 0.10, False),
    'T5 open-quote fav, hold': prematch_stale_open,
    'T6 inplay fav >=0.80': lambda m: inplay_fav(m, 0.80),
    'T6 inplay fav >=0.85': lambda m: inplay_fav(m, 0.85),
    'T6 inplay fav >=0.90': lambda m: inplay_fav(m, 0.90),
    'T6 inplay fav >=0.95': lambda m: inplay_fav(m, 0.95),
    'T7 shock fade 10c/2m, settle': lambda m: inplay_shock(m, 0.10, 2*MIN, False),
    'T7 shock fade 10c/2m, 10m exit': lambda m: inplay_shock(m, 0.10, 2*MIN, False, 10*MIN),
    'T7 shock fade 20c/2m, 10m exit': lambda m: inplay_shock(m, 0.20, 2*MIN, False, 10*MIN),
    'T8 shock follow 10c/2m, settle': lambda m: inplay_shock(m, 0.10, 2*MIN, True),
    'T8 shock follow 10c/2m, 10m exit': lambda m: inplay_shock(m, 0.10, 2*MIN, True, 10*MIN),
    'T8 shock follow 20c/2m, 10m exit': lambda m: inplay_shock(m, 0.20, 2*MIN, True, 10*MIN),
}

def boot(xs, n=2000, seed=1):
    if not xs: return (0, 0)
    rnd = random.Random(seed); s = []
    for _ in range(n):
        s.append(sum(rnd.choice(xs) for _ in xs))
    s.sort(); return s[int(0.025*n)], s[int(0.975*n)]

if __name__ == '__main__':
    print(f"{'strategy':36} {'split':8} {'n':>4} {'pnl$':>7} {'$/trade':>8} {'win%':>5} {'95% CI':>17}")
    for name, f in STRATS.items():
        for split in ('dev', 'holdout'):
            xs = [r for m in M if m['split'] == split for r in [f(m)] if r is not None]
            lo, hi = boot(xs)
            win = 100 * sum(x > 0 for x in xs) / len(xs) if xs else 0
            print(f"{name:36} {split:8} {len(xs):4} {sum(xs):7.2f} {sum(xs)/max(len(xs),1):8.4f} {win:5.0f} [{lo:6.2f},{hi:6.2f}]")
