"""Compare book VWAP depths on the existing FIX export and public WS captures.

Run from the repository root:
    python3 data-analysis/vwap_depth.py --out data-analysis/vwap-depth-results.json

This measures a displayed book statistic, not a strategy's profitability.
Only complete, valid, two-sided books enter the comparison. FIX orders are
aggregated into price levels after applying each complete message. WS frames
are full books, kept separate from FIX and from the duplicate probe journals.
"""

import argparse
import bisect
import collections
import csv
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import random
import statistics
import subprocess

DEPTHS = (1, 3, 4, 5, 10, 20)
BUCKET_S = 10
HORIZON_S = 30
MAX_GAP_S = 60


def timestamp(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()


def source_info(path):
    # Bound growing journals to the byte size observed before reading them.
    size = path.stat().st_size
    digest = hashlib.sha256()
    with path.open("rb") as f:
        remaining = size
        while remaining:
            data = f.read(min(1 << 20, remaining))
            if not data:
                break
            digest.update(data)
            remaining -= len(data)
    return {"path": str(path), "bytes": size, "sha256": digest.hexdigest()}


def observation(t, bids, asks, segment):
    if not bids or not asks or bids[0][0] >= asks[0][0]:
        return None
    if any(not (0.01 <= p <= 0.99 and q > 0) for p, q in bids + asks):
        return None
    if any(x[0] <= y[0] for x, y in zip(bids, bids[1:])):
        return None
    if any(x[0] >= y[0] for x, y in zip(asks, asks[1:])):
        return None
    values = []
    for depth in DEPTHS:
        levels = bids[:depth] + asks[:depth]
        qty = sum(q for _, q in levels)
        px = sum(p * q for p, q in levels) / qty
        edge = sum(q for p, q in levels if p <= 0.02000001 or p >= 0.97999999) / qty
        far = (sum(q for p, q in bids[:depth] if p < bids[0][0] - 0.10000001)
               + sum(q for p, q in asks[:depth] if p > asks[0][0] + 0.10000001)) / qty
        values.append((px, qty, edge, far))
    return (t, bids[0][0], asks[0][0], segment, min(len(bids), len(asks)), tuple(values))


def historical_market(path, quality):
    orders, levels = {}, {"b": {}, "o": {}}
    observations = []
    seeded, segment, last_seq, last_t = False, 0, None, None

    def remove(rid):
        old = orders.pop(rid, None)
        if old:
            side, px, qty = old
            remaining = levels[side][px] - qty
            if remaining < 1e-7:
                levels[side].pop(px)
            else:
                levels[side][px] = remaining

    def message(rows):
        nonlocal seeded, segment, last_seq, last_t
        t, seq = int(rows[0][0]) / 1e9, int(rows[0][2])
        reset = last_seq is not None and seq < last_seq
        snapshot = rows[0][4] == "M"
        if reset or snapshot or (last_t is not None and t - last_t > MAX_GAP_S):
            segment += 1
        if reset and not snapshot:
            seeded = False
            quality["unseeded_sequence_resets"] += 1
        if snapshot:
            orders.clear()
            levels["b"].clear()
            levels["o"].clear()
            seeded = True
            quality["snapshots"] += 1
        last_seq, last_t = seq, t
        quality["messages"] += 1
        for row in rows:
            kind, side, rid = row[4], row[5], row[8]
            if kind == "D":
                remove(rid)
            elif kind in ("S", "A", "U"):
                old = orders.get(rid)
                if kind == "U" and old is None:
                    quality["unknown_changes"] += 1
                side = side or (old[0] if old else "")
                px = round(float(row[6]) * 10000) if row[6] else (old[1] if old else 0)
                qty = float(row[7]) if row[7] else 0
                remove(rid)
                if side in levels and px and qty > 0:
                    orders[rid] = (side, px, qty)
                    levels[side][px] = levels[side].get(px, 0) + qty
        if not seeded:
            quality["unseeded_messages"] += 1
            return
        bids = [(p / 10000, q) for p, q in sorted(levels["b"].items(), reverse=True)]
        asks = [(p / 10000, q) for p, q in sorted(levels["o"].items())]
        obs = observation(t, bids, asks, segment)
        if obs:
            observations.append(obs)
        else:
            segment += 1
            quality["invalid_or_one_sided"] += 1

    with path.open() as f:
        reader = csv.reader(f)
        next(reader)
        pending, key = [], None
        for row in reader:
            new_key = (row[0], row[2])
            if pending and (new_key != key or row[4] == "M"):
                message(pending)
                pending = []
            key = new_key
            pending.append(row)
        if pending:
            message(pending)
    return observations


def capture_markets(paths, quality, sources):
    markets = collections.defaultdict(list)
    for file_index, path in enumerate(paths):
        info = source_info(path)
        sources.append(info)
        last = {}
        with path.open("rb") as f:
            while f.tell() < info["bytes"]:
                line = f.readline(info["bytes"] - f.tell())
                if not line.endswith(b"\n"):
                    quality["partial_lines"] += 1
                    break
                try:
                    row = json.loads(line)
                    raw = row.get("raw", {})
                    raw = json.loads(raw) if isinstance(raw, str) else raw
                    book = raw.get("marketData")
                    if not book:
                        continue
                    slug = book["marketSlug"]
                    # aec-* are match markets; tec-* are futures. No probe journals.
                    if not slug.startswith("aec-"):
                        quality["non_match_books"] += 1
                        continue
                    quality["match_books"] += 1
                    t = timestamp(row["recv_at"])
                    txn = timestamp(book["transactTime"]) if book.get("transactTime") else None
                    prev = last.get(slug)
                    session = row.get("conn")
                    segment = prev[3] if prev else 0
                    if prev and (session != prev[2] or t - prev[0] > MAX_GAP_S):
                        segment += 1
                    if prev and session == prev[2] and txn is not None and prev[1] is not None and txn < prev[1]:
                        quality["older_books"] += 1
                        continue
                    last[slug] = (t, txn, session, segment)
                    if book.get("state") != "MARKET_STATE_OPEN":
                        quality["non_open_books"] += 1
                        last[slug] = (t, txn, session, segment + 1)
                        continue

                    def side(name):
                        result = []
                        for level in book.get(name) or []:
                            px = level["px"]
                            px = px["value"] if isinstance(px, dict) else px
                            result.append((float(px), float(level["qty"])))
                        return result

                    obs = observation(t, side("bids"), side("offers"), (file_index, segment))
                    if obs:
                        markets[slug].append(obs)
                    else:
                        quality["invalid_or_one_sided"] += 1
                        last[slug] = (t, txn, session, segment + 1)
                except (ValueError, KeyError, TypeError):
                    quality["malformed_lines"] += 1
    return markets


def percentile(values, fraction):
    if not values:
        return None
    values = sorted(values)
    index = (len(values) - 1) * fraction
    lo = int(index)
    hi = min(lo + 1, len(values) - 1)
    return values[lo] + (values[hi] - values[lo]) * (index - lo)


def summarize_market(observations, max_spread):
    # Last valid book observed in each 10-second bucket. Do not fill quiet gaps.
    buckets = {}
    for i, obs in enumerate(observations):
        buckets[int(obs[0] // BUCKET_S)] = i
    times = [o[0] for o in observations]
    data = {d: collections.defaultdict(list) for d in DEPTHS}
    prev = None
    for i in buckets.values():
        obs = observations[i]
        t, bid, ask, segment, min_levels, values = obs
        if ask - bid > max_spread + 1e-8:
            continue
        mid = (bid + ask) / 2
        future_t = t + HORIZON_S
        j = bisect.bisect_right(times, future_t) - 1
        after = bisect.bisect_left(times, future_t)
        covered = (after < len(observations) and j >= 0
                   and observations[after][3] == segment)
        target = observations[j] if covered else None
        if target and (target[3] != segment or future_t - target[0] > HORIZON_S
                       or target[2] - target[1] > max_spread + 1e-8):
            target = None
        future_mid = (target[1] + target[2]) / 2 if target else None
        for k, depth in enumerate(DEPTHS):
            px, qty, edge, far = values[k]
            d = data[depth]
            d["abs_mid_cents"].append(100 * abs(px - mid))
            d["outside_spread_pct"].append(100 * (px < bid - 1e-8 or px > ask + 1e-8))
            d["edge_qty_pct"].append(100 * edge)
            d["far_qty_pct"].append(100 * far)
            d["fewer_levels_pct"].append(100 * (min_levels < depth))
            d["contracts"].append(qty)
            if prev and prev[3] == segment and t - prev[0] <= MAX_GAP_S:
                d["step_cents"].append(100 * abs(px - prev[5][k][0]))
            if future_mid is not None:
                d["future_mae_cents"].append(100 * abs(px - future_mid))
                d["mid_future_mae_cents"].append(100 * abs(mid - future_mid))
        prev = obs
    return data


def aggregate(markets):
    result = {}
    for depth in DEPTHS:
        group = {}
        keys = sorted({key for m in markets for key in m[depth]})
        for key in keys:
            per_market = [m[depth][key] for m in markets if m[depth][key]]
            combined = [value for values in per_market for value in values]
            group[key] = {"observations": len(combined), "markets": len(per_market),
                          "pooled_mean": statistics.fmean(combined) if combined else None,
                          "market_mean": statistics.fmean(statistics.fmean(v) for v in per_market) if per_market else None,
                          "pooled_p95": percentile(combined, 0.95)}
        result[str(depth)] = group
    # Paired bootstrap: each market carries equal weight; deterministic seed.
    paired = []
    for market in markets:
        a, b = market[3]["future_mae_cents"], market[5]["future_mae_cents"]
        if a and b:
            paired.append(statistics.fmean(a) - statistics.fmean(b))
    rng = random.Random(20261028)
    boot = [statistics.fmean(rng.choices(paired, k=len(paired))) for _ in range(1000)] if paired else []
    result["paired_3_minus_5_future_mae_cents"] = {
        "markets": len(paired), "mean": statistics.fmean(paired) if paired else None,
        "ci95": [percentile(boot, 0.025), percentile(boot, 0.975)],
    }
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--history", type=Path, default=Path("data-analysis/backtest"))
    parser.add_argument("--captures", type=Path, default=Path("captures"))
    parser.add_argument("--out", type=Path, default=Path("data-analysis/vwap-depth-results.json"))
    args = parser.parse_args()
    groups = collections.defaultdict(list)
    quality = {"history": collections.Counter(), "captures": collections.Counter()}
    sources = []
    metadata = json.loads((args.history / "markets.json").read_text())
    sources.append(source_info(args.history / "markets.json"))
    bounds = {}

    def add(group, observations):
        if not observations:
            return
        if group not in bounds:
            bounds[group] = [observations[0][0], observations[-1][0], 0, 0]
        b = bounds[group]
        b[0], b[1] = min(b[0], observations[0][0]), max(b[1], observations[-1][0])
        b[2] += 1
        b[3] += len(observations)
        groups[group].append(summarize_market(observations, 0.40))
        groups[group + "/all_spreads"].append(summarize_market(observations, 1.0))

    for i, market in enumerate(metadata["markets"]):
        path = args.history / (market["slug"] + ".csv")
        sources.append(source_info(path))
        observations = historical_market(path, quality["history"])
        group = "history/early" if (market.get("game_start") or "") < "2026-09-29" else "history/late"
        add(group, observations)
        if (i + 1) % 75 == 0:
            print(f"Rebuilt {i + 1}/{len(metadata['markets'])} historical markets", flush=True)
    paths = sorted(p for p in args.captures.glob("????-??-??.jsonl"))
    recent = capture_markets(paths, quality["captures"], sources)
    cutoff = timestamp("2026-10-27T00:00:00Z")
    for slug, observations in recent.items():
        observations.sort(key=lambda o: o[0])
        add("captures/earlier", [o for o in observations if o[0] < cutoff])
        add("captures/current", [o for o in observations if o[0] >= cutoff])
        days = collections.defaultdict(list)
        for obs in observations:
            days[datetime.fromtimestamp(obs[0], timezone.utc).strftime("%Y-%m-%d")].append(obs)
        for day, day_obs in days.items():
            add("day/" + day, day_obs)
    try:
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    except subprocess.SubprocessError:
        revision = None
    result = {"depths_per_side": DEPTHS, "bucket_seconds": BUCKET_S,
              "future_horizon_seconds": HORIZON_S, "max_gap_seconds": MAX_GAP_S,
              "max_spread": 0.40, "git_revision": revision,
              "historical_export_source_sha256": metadata.get("source_sha256"),
              "bounds": bounds, "quality": quality, "sources": sources,
              "groups": {name: aggregate(markets) for name, markets in sorted(groups.items())}}
    args.out.write_text(json.dumps(result, indent=2) + "\n")
    print(f"Wrote {args.out}")
    for name in ("history/early", "history/late", "captures/earlier", "captures/current"):
        print("\n" + name)
        for depth in (3, 4, 5, 20):
            metrics = result["groups"][name][str(depth)]
            print(depth, {key: round(metrics[key]["market_mean"], 3) if metrics[key]["market_mean"] is not None else None
                          for key in ("abs_mid_cents", "outside_spread_pct", "edge_qty_pct", "future_mae_cents", "mid_future_mae_cents")})


if __name__ == "__main__":
    main()
