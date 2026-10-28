"""Export the Parquet history to per-market replay files for `pm-backtest`.

    venv/bin/python data-analysis/export_backtest.py            # -> data-analysis/backtest/

For every `darts_match_winner` market with FIX data it writes `<slug>.csv`, the
FIX rows in replay order, and adds the market to `markets.json` (parameters,
settlement, row counts) together with the source checksum and export settings.

Replay order is `wire_received_at`, then `msg_seq`, then `entry_index`. Exact
duplicate rows (same session receive time, sequence, index and payload) are
dropped and counted. Market stats are skipped; WebSocket rows (futures only) are
not exported.

CSV columns: rx_ns, ev_ns, msg_seq, entry_index, kind, side, price, size, id, aggressor
  kind: M snapshot marker, S snapshot entry, A new, U change, D delete, T trade
  ev_ns: exchange `event_time`, else `sending_time`, else receive time
  side: b / o (bid / offer) for book rows; aggressor: b / s for trades
"""

import argparse
import collections
import hashlib
import json
import os
import subprocess
import sys

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq

DEFAULT_SOURCE = os.path.join(os.path.dirname(__file__), "polymarket_us_darts_history.parquet")
DEFAULT_OUT = os.path.join(os.path.dirname(__file__), "backtest")
COLUMNS = ["record_type", "market_slug", "event_time", "sending_time", "wire_received_at", "price", "size",
           "side", "action", "order_id", "trade_id", "msg_seq", "entry_index", "is_snapshot_marker"]


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def git_revision():
    try:
        return subprocess.check_output(["git", "rev-parse", "HEAD"], stderr=subprocess.DEVNULL, text=True).strip()
    except Exception:
        return None


def ns(col):
    return pc.cast(pc.cast(col, pa.timestamp("ns", tz="UTC")), pa.int64()).to_pylist()


def market_params(m):
    raw = m.get("raw_market")
    raw = json.loads(raw) if isinstance(raw, str) else (raw or {})
    yes_ordering = None
    for side in raw.get("marketSides") or []:
        if side.get("long"):
            yes_ordering = (side.get("team") or {}).get("ordering")
    return {
        "slug": m["slug"],
        "title": m.get("market_title") or m.get("event_title") or m["slug"],
        "event_id": str(m.get("event_id") or ""),
        "event_slug": m.get("event_slug") or "",
        "league": m.get("league") or "",
        "sports_market_type": m.get("sports_market_type"),
        "tick": str(raw.get("orderPriceMinTickSize") or m.get("order_price_min_tick_size") or "0.01"),
        "min_qty": str(raw.get("minimumTradeQty") or "0.01"),
        "fee_coefficient": raw.get("feeCoefficient"),
        "game_start": m.get("game_start_time"),
        "yes_name": m.get("side_yes_description"),
        "yes_ordering": yes_ordering,
        "settlement": m.get("settlement"),
    }


def fmt_decimal(v):
    if v is None:
        return ""
    s = format(v, "f").rstrip("0").rstrip(".")
    return s or "0"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--source", default=DEFAULT_SOURCE)
    ap.add_argument("--out", default=DEFAULT_OUT)
    ap.add_argument("--market", action="append", help="only these slugs (repeatable)")
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)

    metas = {}
    for s in pq.read_table(args.source, columns=["source_record_json"], filters=[("record_type", "=", "market_metadata")]).column(0).to_pylist():
        m = json.loads(s)
        if m.get("sports_market_type") == "darts_match_winner" and (not args.market or m["slug"] in args.market):
            metas[m["slug"]] = m
    print(f"{len(metas)} match-winner markets in metadata", file=sys.stderr)

    filters = [("feed", "=", "fix"), ("market_slug", "in", sorted(metas))]
    t = pq.read_table(args.source, columns=COLUMNS, filters=filters)
    # Aggressor side lives only in the source JSON; read it for trades alone.
    trades = pq.read_table(args.source, columns=["market_slug", "msg_seq", "entry_index", "wire_received_at", "source_record_json"],
                           filters=filters + [("record_type", "=", "trade")])
    aggressor = {}
    for slug, seq, idx, rx, js in zip(trades.column(0).to_pylist(), trades.column(1).to_pylist(), trades.column(2).to_pylist(),
                                      ns(trades.column(3)), trades.column(4).to_pylist()):
        side = (json.loads(js).get("aggressor_side") or "").lower()
        aggressor[(slug, rx, seq, idx)] = {"buy": "b", "sell": "s"}.get(side, "")

    ev, sent, rx = ns(t.column("event_time")), ns(t.column("sending_time")), ns(t.column("wire_received_at"))
    cols = {c: t.column(c).to_pylist() for c in COLUMNS if c not in ("event_time", "sending_time", "wire_received_at")}
    rows = collections.defaultdict(list)
    for i in range(t.num_rows):
        rt = cols["record_type"][i]
        if rt == "book_snapshot":
            kind = "M" if cols["is_snapshot_marker"][i] else "S"
        elif rt == "book_change":
            kind = {"0": "A", "1": "U", "2": "D"}.get(str(cols["action"][i]))
        elif rt == "trade":
            kind = "T"
        else:
            kind = None
        if kind is None or rx[i] is None:
            continue
        slug = cols["market_slug"][i]
        seq, idx = cols["msg_seq"][i], cols["entry_index"][i]
        side = {"bid": "b", "offer": "o"}.get(cols["side"][i] or "", "")
        rid = cols["trade_id"][i] if kind == "T" else (cols["order_id"][i] or "")
        rows[slug].append((rx[i], seq if seq is not None else -1, idx if idx is not None else -1,
                           ev[i] or sent[i] or rx[i], kind, side, fmt_decimal(cols["price"][i]), fmt_decimal(cols["size"][i]), rid or "",
                           aggressor.get((slug, rx[i], seq, idx), "") if kind == "T" else ""))

    manifest = {"source": os.path.abspath(args.source), "source_sha256": sha256(args.source), "git_revision": git_revision(),
                "order": "wire_received_at, msg_seq, entry_index", "markets": []}
    for slug in sorted(rows):
        r = rows[slug]
        r.sort(key=lambda x: (x[0], x[1], x[2]))
        out, dupes, prev = [], 0, None
        for x in r:
            if x == prev:
                dupes += 1
                continue
            out.append(x)
            prev = x
        kinds = collections.Counter(x[4] for x in out)
        with open(os.path.join(args.out, f"{slug}.csv"), "w") as f:
            f.write("rx_ns,ev_ns,msg_seq,entry_index,kind,side,price,size,id,aggressor\n")
            for x in out:
                f.write(f"{x[0]},{x[3]},{x[1]},{x[2]},{x[4]},{x[5]},{x[6]},{x[7]},{x[8]},{x[9]}\n")
        p = market_params(metas[slug])
        p.update({"rows": len(out), "duplicates_dropped": dupes, "trades": kinds["T"], "snapshots": kinds["M"],
                  "first_rx_ns": out[0][0], "last_rx_ns": out[-1][0]})
        manifest["markets"].append(p)
    with open(os.path.join(args.out, "markets.json"), "w") as f:
        json.dump(manifest, f, indent=1)
    n_tr = sum(1 for m in manifest["markets"] if m["trades"] > 0)
    print(f"wrote {len(manifest['markets'])} markets ({n_tr} with trades) to {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
