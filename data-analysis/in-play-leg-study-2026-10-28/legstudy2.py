import sys, runpy, statistics as st, math, collections
sys.argv = [sys.argv[0], sys.argv[1], sys.argv[2], '/dev/null']
import io, contextlib
buf = io.StringIO()
with contextlib.redirect_stdout(buf):
    g = runpy.run_path(sys.argv[0].replace('legstudy2', 'legstudy'), run_name='x')
rows, mid, books, trades, S, q, fmt = g['rows'], g['mid'], g['books'], g['trades'], g['S'], g['q'], g['fmt']
legs = g['legs']

# 1. fine timing around t0 (when we saw it) and around venue updatedAt
print('Fine timing: median / mean signed move vs mid at -20s, and share of moved legs (|x|>=2c)')
for ref in ['seen', 'venue']:
    print(' ref =', ref)
    for o in [-8,-6,-4,-3,-2,-1.5,-1,-0.5,0,0.5,1,2,3,5,8]:
        xs=[]
        for r in rows:
            t = r['t0'] if ref=='seen' else r['t0'] - int((r['poll_lag'] or 0)*S)
            a, b = mid(r['slug'], t - 20*S), mid(r['slug'], t + int(o*S))
            if a is not None and b is not None: xs.append(r['sign']*(b-a))
        print(f'   {o:+5.1f}s  mean {st.mean(xs):+.3f}  moved>=2c {sum(1 for x in xs if x>=0.02)/len(xs):.2f}  wrong<=-2c {sum(1 for x in xs if x<=-0.02)/len(xs):.2f}')

# 2. first time the mid moves >=2c in leg-winner direction, relative to venue updatedAt
print('\nFirst >=2c move toward the winner, relative to venue updatedAt (search −40..+60 s, baseline mid at −40 s):')
firsts=[]
for r in rows:
    s=r['slug']; tv = r['t0'] - int((r['poll_lag'] or 0)*S)
    base = mid(s, tv-40*S)
    if base is None: continue
    hit=None
    for (t,b,a,*_) in books[s]:
        if t < tv-40*S or b is None or a is None: continue
        if t > tv+60*S: break
        if r['sign']*((a+b)/2-base) >= 0.02: hit=(t-tv)/S; break
    firsts.append(hit)
got=[x for x in firsts if x is not None]
print(f'  n={len(firsts)} never={len(firsts)-len(got)}  p10 {q(got,.1):+.1f}  p25 {q(got,.25):+.1f}  p50 {q(got,.5):+.1f}  p75 {q(got,.75):+.1f}  p90 {q(got,.9):+.1f}')
print('  share before venue stamp:', f'{sum(1 for x in got if x<0)/len(got):.2f}', ' before -5s:', f'{sum(1 for x in got if x<-5)/len(got):.2f}')

# 3. settle: first time after t0 the mid is within 1c of its +90s value and stays there through +90
print('\nSettle time after we see the leg (mid within 1c of +90s value through +90s):')
sts=[]
for r in rows:
    s=r['slug']; t0=r['t0']; fin = mid(s, t0+90*S)
    if fin is None: continue
    pts=[(t,(b+a)/2) for (t,b,a,*_) in books[s] if t0<=t<=t0+90*S and b is not None and a is not None]
    last_out = 0.0
    for t,m in pts:
        if abs(m-fin)>0.01: last_out=(t-t0)/S
    sts.append(last_out)
print(f'  n={len(sts)} p25 {q(sts,.25):.1f}  p50 {q(sts,.5):.1f}  p75 {q(sts,.75):.1f}  p90 {q(sts,.9):.1f}')

# 4. step vs model using settled baseline: mid 45 s after the previous leg (or 60 s before)
print('\nRealised step from the settled previous state (prev leg +45 s) to +60 s, vs model:')
prev_t = {}
pairs=[]
for r in sorted(rows, key=lambda r:(r['slug'], r['t0'])):
    s=r['slug']
    pb = mid(s, prev_t[s]+45*S) if s in prev_t else None
    prev_t[s]=r['t0']
    if pb is None or r['m'][60] is None: continue
    pairs.append((r['sign']*r['model_step'], r['sign']*(r['m'][60]-pb), r))
ms=[p[0] for p in pairs]; rs=[p[1] for p in pairs]
print(f'  n={len(pairs)} model p50 {q(ms,.5):+.3f}  realised p50 {q(rs,.5):+.3f}  mean realised/model {st.mean(rs)/st.mean(ms):.2f}  corr {st.correlation(ms,rs):.2f}')

# 5. hold-rate MLE over sequences, starter unknown (mixture), common tau, no strength diff
seqs = collections.defaultdict(list)
for s, t0, tv, prev, new in sorted(legs, key=lambda x:(x[0],x[1])):
    seqs[s].append(new[0] > prev[0])
def ll(tau):
    tot=0
    for s, w in seqs.items():
        L=0
        for h1 in (True, False):
            p=0.5
            for i, hw in enumerate(w):
                home_throws = h1 if i%2==0 else not h1
                hold = (hw == home_throws)
                p *= tau if hold else 1-tau
            L+=p
        tot+=math.log(L)
    return tot
best=max((ll(t/100),t/100) for t in range(30,90))
print(f'\nHold-rate MLE (no strength term), {sum(len(v) for v in seqs.values())} legs in {len(seqs)} matches: tau={best[1]:.2f}')
for t in (0.5,0.55,0.6,0.65,0.7): print(f'  ll({t}) = {ll(t):.2f}')

# 6. stale-quote exposure: how often does the mid move more than h within 10 s of the venue stamp
print('\nJump size |mid(+10s) − mid(−10s)| around the venue stamp:')
js=[]
for r in rows:
    tv = r['t0'] - int((r['poll_lag'] or 0)*S)
    a,b = mid(r['slug'], tv-10*S), mid(r['slug'], tv+10*S)
    if a is not None and b is not None: js.append(abs(b-a))
for h in (0.05,0.10,0.15,0.20): print(f'  > {h:.2f}: {sum(1 for x in js if x>h)/len(js):.2f}')
print('  p50', f'{q(js,.5):.3f}', 'p90', f'{q(js,.9):.3f}', 'max', f'{max(js):.3f}')

# 7. trades: prices relative to the prevailing mid, by window around venue stamp
print('\nTrades by window around the venue stamp: n, qty, taker-with-winner share, mean |px − mid(−20s)|:')
for lo,hi in [(-60,-10),(-10,-2),(-2,2),(2,10),(10,30),(30,60)]:
    xs=[]
    for r in rows:
        tv = r['t0'] - int((r['poll_lag'] or 0)*S)
        m0 = mid(r['slug'], tv-20*S)
        for (t,px,qty,tby) in trades[r['slug']]:
            if tv+lo*S <= t < tv+hi*S: xs.append((qty, tby==(r['sign']>0), abs(px-m0) if m0 else 0))
    if xs: print(f'  [{lo:+d},{hi:+d}) n={len(xs)} qty={sum(x[0] for x in xs):.0f} with-winner={sum(x[1] for x in xs)/len(xs):.2f} |dpx|={st.mean(x[2] for x in xs):.3f}')
