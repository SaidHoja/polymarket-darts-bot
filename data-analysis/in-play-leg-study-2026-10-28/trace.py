import json, sys
from datetime import datetime,timezone
day, slugpart, hh, mm, ss, span = sys.argv[1], sys.argv[2], *map(int, sys.argv[3:6]), float(sys.argv[6])
lo=datetime(2026,10,int(day),hh,mm,ss,tzinfo=timezone.utc).timestamp()*1e9; hi=lo+span*1e9
for l in open(f'captures/core-2026-10-{day}.jsonl'):
    if slugpart not in l: continue
    r=json.loads(l); w=r.get('wall_ns',0)
    if not lo<=w<=hi: continue
    dec=[d[1] for d in (r.get('decisions') or []) if slugpart in d[0]]
    out=[]
    for e in (r.get('events') or []):
        k=list(e)[0] if isinstance(e,dict) else e
        if k=='Book': out.append(f"book {[b[0] for b in e['Book']['bids'][:3]]}/{[o[0] for o in e['Book']['offers'][:3]]}")
        elif k=='Execution': x=e['Execution']; f=x.get('fill'); out.append(f"exec {x['kind']} {x['order']['side']} {x['order']['px']} {x['order']['state']}"+(f" FILL {f['last_px']} x{f['last_shares']}" if f else ''))
        elif k=='Trade': x=e['Trade']; out.append(f"trade {x['px']} x{x['qty']} {x.get('taker_side')}")
        elif k=='Game': out.append(f"GAME {e['Game'].get('score')}")
        else: out.append(str(k)[:20])
    cmds=[]
    for c in (r.get('commands') or []):
        if slugpart in json.dumps(c):
            k=list(c)[0]; v=c[k]; cmds.append(f"{k} {v.get('side','')} {v.get('px','')}".strip())
    d=dec[0] if dec else None
    q=f"bid {d['bid'][0] if d['bid'] else '-'} ask {d['ask'][0] if d['ask'] else '-'} fair {round(d['fair'],3) if d['fair'] else None}" if d else ''
    print(f"{(w-lo)/1e9:7.3f} | {q:32} | {cmds} | {out}")
