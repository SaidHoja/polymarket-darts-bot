import sys, runpy, io, contextlib, collections, statistics as st
sys.argv=[sys.argv[0], sys.argv[1], '1.0', '/dev/null']
with contextlib.redirect_stdout(io.StringIO()):
    g=runpy.run_path(sys.argv[0].replace('elapsed','legstudy'), run_name='x')
mid, trades, S, legs = g['mid'], g['trades'], g['S'], g['legs']
st_by=collections.defaultdict(list)
for s,t0,tv,prev,new in legs: st_by[s].append(tv or t0)
for s in st_by: st_by[s].sort()
# leg end hazard: P(leg ends within next 15 s | elapsed in bucket)
durs=[b-a for v in st_by.values() for a,b in zip(v,v[1:])]
durs=[d/S for d in durs]
print('leg durations s: n', len(durs), 'p10 %.0f p25 %.0f p50 %.0f p75 %.0f p90 %.0f' % tuple(sorted(durs)[int(p*len(durs))] for p in (.1,.25,.5,.75,.9)))
for e in range(0,180,20):
    alive=[d for d in durs if d>e]
    if alive: print(f'  elapsed {e:3d}s: P(end within 15s) {sum(1 for d in alive if d<=e+15)/len(alive):.2f}  (n={len(alive)})')
# maker markout by elapsed-in-leg bucket (no hindsight: unsigned real maker pnl at +10s / +30s)
B=collections.defaultdict(list)
for s,v in st_by.items():
    for (t,px,qty,tby) in trades.get(s,[]):
        prev=[x for x in v if x<=t]
        if not prev or t > v[-1]+200*S: continue
        e=(t-prev[-1])/S
        nxt=[x for x in v if x>t]
        to_end=(nxt[0]-t)/S if nxt else None
        for h in (10,30):
            m=mid(s,t+h*S)
            if m is None: continue
            mo=(m-px) if not tby else (px-m)
            B[(min(int(e//30)*30,150),h)].append((mo,qty,to_end))
print('\nmaker markout (qty-weighted, $/contract) by time into leg:')
for b in range(0,180,30):
    a=B[(b,10)]; c=B[(b,30)]
    if a: print(f'  {b:3d}-{b+30}s  n={len(a):3d} qty={sum(x[1] for x in a):5.0f}  mo10 {sum(m*w for m,w,_ in a)/sum(w for _,w,_ in a):+.3f}  mo30 {sum(m*w for m,w,_ in c)/max(1e-9,sum(w for _,w,_ in c)):+.3f}')
# by time to the NEXT leg end (hindsight, but shows where adverse selection concentrates)
print('\nmaker markout by time until the next leg stamp (hindsight view):')
for lo,hi in [(0,3),(3,10),(10,30),(30,60),(60,300)]:
    a=[x for k,v in B.items() if k[1]==10 for x in v if x[2] is not None and lo<=x[2]<hi]
    if a: print(f'  {lo}-{hi}s before stamp: n={len(a)} qty={sum(x[1] for x in a):.0f} mo10 {sum(m*w for m,w,_ in a)/sum(w for _,w,_ in a):+.3f}')
