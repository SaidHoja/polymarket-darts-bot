"""Shared loading and top-of-book helpers for the taker study. Run extract.py first:
    python3 data-analysis/strategy-research/extract.py data-analysis/backtest data-analysis/strategy-research/markets.pkl
"""
import pickle, bisect
from datetime import datetime
DEV_END = datetime.fromisoformat('2026-08-20T00:00:00+00:00').timestamp()*1e9
HO = datetime.fromisoformat('2026-09-29T00:00:00+00:00').timestamp()*1e9
import os
M = pickle.load(open(os.path.join(os.path.dirname(__file__), 'markets.pkl'), 'rb'))
for m in M:
    m['split'] = 'dev' if m['start'] < DEV_END else ('holdout' if m['start'] >= HO else 'gap')
    m['_t'] = [x[0] for x in m['tob']]
def tob_at(m, t):
    i = bisect.bisect_right(m['_t'], t) - 1
    return m['tob'][i] if i >= 0 else None
def mid_at(m, t, maxspr=1.0):
    x = tob_at(m, t)
    if not x or x[1] is None or x[3] is None or x[3]-x[1] > maxspr: return None
    return (x[1]+x[3])/2
def fee(theta, p, q=1.0): return theta*q*p*(1-p)
H = 3600e9; MIN = 60e9
