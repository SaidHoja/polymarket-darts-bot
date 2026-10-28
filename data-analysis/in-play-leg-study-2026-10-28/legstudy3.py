import sys, runpy, statistics as st, collections, io, contextlib, json
sys.argv=[sys.argv[0], sys.argv[1], '1.0', '/dev/null']
with contextlib.redirect_stdout(io.StringIO()):
    g=runpy.run_path(sys.argv[0].replace('legstudy3','legstudy'), run_name='x')
rows, mid, trades, S, q, legs, books = g['rows'], g['mid'], g['trades'], g['S'], g['q'], g['legs'], g['books']
# settlement per slug: who reached 4
final={}
for s,t0,tv,prev,new in legs:
    if max(new)==4: final[s]=1.0 if new[0]==4 else 0.0
print('settled matches:', len(final))
# maker markout per trade = maker's PnL per contract at horizon; maker buys YES iff not taker_buys_yes
def mk(s,t,px,tby,h):
    ref = final.get(s) if h=='settle' else mid(s, t+h*S)
    if ref is None: return None
    return (ref-px) if not tby else (px-ref)
print('\nMaker markout per contract (qty-weighted mean) by window around venue stamp, split by whether the maker sold the leg winner')
for lo,hi in [(-60,-10),(-10,-2),(-2,2),(2,10),(10,30),(30,60)]:
    out=collections.defaultdict(list)
    for r in rows:
        tv=r['t0']-int((r['poll_lag'] or 0)*S); s=r['slug']
        for (t,px,qty,tby) in trades[s]:
            if not (tv+lo*S<=t<tv+hi*S): continue
            side='maker sold winner' if tby==(r['sign']>0) else 'maker bought winner'
            for h in (10,60,'settle'):
                m=mk(s,t,px,tby,h)
                if m is not None: out[(side,h)].append((m,qty))
    line=f'[{lo:+d},{hi:+d})'
    for side in ('maker sold winner','maker bought winner'):
        parts=[]
        for h in (10,60,'settle'):
            xs=out[(side,h)]
            if xs: parts.append(f"{h}:{sum(m*w for m,w in xs)/sum(w for m,w in xs):+.3f}")
        n=len(out[(side,10)])
        line+=f'   {side} n={n} '+' '.join(parts)
    print(line)
# overshoot check per leg: mid +5 vs +120
xs=[r['sign']*(r['m'][120]-r['m'][5]) for r in rows if r['m'][120] is not None and r['m'][5] is not None]
print('\nsigned drift +5 → +120: mean %+.3f, p25 %+.3f p50 %+.3f p75 %+.3f, share reverting >2c %.2f' % (st.mean(xs), q(xs,.25), q(xs,.5), q(xs,.75), sum(1 for x in xs if x<-0.02)/len(xs)))
# total in-play volume vs pre-match volume
inplay=collections.Counter(); pre=collections.Counter()
first_live={}
for s,t0,tv,prev,new in sorted(legs,key=lambda x:x[1]):
    first_live.setdefault(s, t0-150*S)
for s in trades:
    for (t,px,qty,tby) in trades[s]:
        (inplay if t>=first_live.get(s,1<<62) else pre)[s]+=qty*px
print(f'\nnotional: in-play ${sum(inplay.values()):.0f}, pre-match ${sum(pre.values()):.0f} over {len(trades)} markets')
