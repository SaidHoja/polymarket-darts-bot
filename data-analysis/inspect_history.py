#!/usr/bin/env python3
"""Stream a compact structural report for the darts-history Parquet file."""

from __future__ import annotations

import argparse
import json
from collections import Counter
from datetime import date, datetime
from pathlib import Path
from typing import Any


def printable_time(value: Any) -> str | None:
    if value is None:
        return None
    if isinstance(value, (date, datetime)):
        return value.isoformat()
    return str(value)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "path",
        nargs="?",
        type=Path,
        default=Path(__file__).with_name("polymarket_us_darts_history.parquet"),
    )
    parser.add_argument("--batch-size", type=int, default=50_000)
    parser.add_argument("--top-markets", type=int, default=20)
    args = parser.parse_args()

    try:
        import pyarrow.parquet as pq
    except ImportError as exc:
        raise SystemExit(
            "PyArrow is required. Install it with: python3 -m pip install pyarrow"
        ) from exc

    if args.batch_size <= 0 or args.top_markets < 0:
        parser.error("--batch-size must be positive and --top-markets nonnegative")
    if not args.path.is_file():
        raise SystemExit(f"Parquet file not found: {args.path}")

    parquet = pq.ParquetFile(args.path)
    columns = [
        "record_type",
        "feed",
        "market_slug",
        "event_slug",
        "event_time",
        "sending_time",
        "wire_received_at",
        "dt",
        "price",
        "size",
        "side",
        "action",
        "entry_id",
        "order_id",
        "trade_id",
        "msg_seq",
        "entry_index",
        "is_snapshot_marker",
        "stat_type",
        "value",
        "state",
    ]
    columns = [name for name in columns if name in parquet.schema_arrow.names]

    row_count = 0
    null_counts = Counter()
    record_types = Counter()
    feeds = Counter()
    type_feed = Counter()
    markets = Counter()
    events = Counter()
    snapshot_markers = Counter()
    time_bounds: dict[str, list[Any]] = {
        name: [None, None]
        for name in ("event_time", "sending_time", "wire_received_at", "dt")
        if name in columns
    }

    for batch in parquet.iter_batches(batch_size=args.batch_size, columns=columns):
        row_count += batch.num_rows
        data = batch.to_pydict()
        for name, values in data.items():
            null_counts[name] += sum(value is None for value in values)

        for index in range(batch.num_rows):
            record_type = data.get("record_type", [None] * batch.num_rows)[index]
            feed = data.get("feed", [None] * batch.num_rows)[index]
            market = data.get("market_slug", [None] * batch.num_rows)[index]
            event = data.get("event_slug", [None] * batch.num_rows)[index]
            if record_type is not None:
                record_types[str(record_type)] += 1
            if feed is not None:
                feeds[str(feed)] += 1
            type_feed[(str(record_type), str(feed))] += 1
            if market is not None:
                markets[str(market)] += 1
            if event is not None:
                events[str(event)] += 1
            if "is_snapshot_marker" in data:
                snapshot_markers[str(data["is_snapshot_marker"][index])] += 1

            for name, (minimum, maximum) in time_bounds.items():
                value = data[name][index]
                if value is None:
                    continue
                if minimum is None or value < minimum:
                    minimum = value
                if maximum is None or value > maximum:
                    maximum = value
                time_bounds[name] = [minimum, maximum]

    report = {
        "path": str(args.path),
        "file_bytes": args.path.stat().st_size,
        "rows_read": row_count,
        "row_groups": parquet.metadata.num_row_groups,
        "batch_size": args.batch_size,
        "schema": str(parquet.schema_arrow),
        "null_counts": dict(sorted(null_counts.items())),
        "record_type_counts": dict(record_types.most_common()),
        "feed_counts": dict(feeds.most_common()),
        "record_type_feed_counts": [
            {"record_type": kind, "feed": feed, "rows": count}
            for (kind, feed), count in type_feed.most_common()
        ],
        "market_count": len(markets),
        "event_count": len(events),
        "top_markets": [
            {"market_slug": market, "rows": count}
            for market, count in markets.most_common(args.top_markets)
        ],
        "snapshot_marker_counts": dict(snapshot_markers.most_common()),
        "time_bounds": {
            name: {"min": printable_time(bounds[0]), "max": printable_time(bounds[1])}
            for name, bounds in time_bounds.items()
        },
    }
    print(json.dumps(report, indent=2, default=str))


if __name__ == "__main__":
    main()
