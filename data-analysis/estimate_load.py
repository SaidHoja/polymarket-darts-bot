#!/usr/bin/env python3
"""Estimate observed traffic peaks and configurable capacity targets from history.

Requires PyArrow. Results describe recorded traffic, not a service benchmark or
an upper bound on future demand. FIX rows and WebSocket books are different units.
"""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import json
import math
from pathlib import Path

WINDOWS = (1, 10, 60, 3600, 86400)
BOOK_TYPES = {'book_change', 'book_snapshot'}


def utc(second):
    return datetime.fromtimestamp(second, timezone.utc).isoformat()


def positive(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError('must be finite and positive')
    return number


def new_bucket():
    # rows, book rows, JSON bytes, trades, contracts, price * size
    return [0, 0, 0, 0, 0.0, 0.0]


class Traffic:
    def __init__(self):
        self.types = Counter()
        self.markets = Counter()
        self.missing_time = 0
        self.missing_payload = 0
        self.invalid_trades = 0
        self.seconds = defaultdict(new_bucket)
        self.minute_markets = defaultdict(set)
        self.first = None
        self.last = None

    def add(self, kind, market, timestamp, payload, price, size):
        self.types[kind] += 1
        if market:
            self.markets[market] += 1
        if timestamp is None:
            self.missing_time += 1
            return
        self.first = timestamp if self.first is None else min(timestamp, self.first)
        self.last = timestamp if self.last is None else max(timestamp, self.last)
        second = timestamp // 1_000_000
        bucket = self.seconds[second]
        bucket[0] += 1
        bucket[1] += kind in BOOK_TYPES
        if payload is None:
            self.missing_payload += 1
        else:
            bucket[2] += len(payload.encode('utf-8'))
        if market:
            self.minute_markets[second // 60].add(market)
        if kind == 'trade':
            bucket[3] += 1
            if (price is not None and size is not None and price.is_finite()
                    and size.is_finite() and 0 <= price <= 1 and size >= 0):
                bucket[4] += float(size)
                bucket[5] += float(price * size)
            else:
                self.invalid_trades += 1

    def report(self, args):
        result = {
            'rows': sum(self.types.values()),
            'record_types': dict(self.types),
            'markets_with_data': len(self.markets),
            'rows_missing_selected_clock': self.missing_time,
            'timed_rows_missing_source_json': self.missing_payload,
            'timed_trades_with_invalid_or_missing_price_size': self.invalid_trades,
            'top_markets_by_rows': self.markets.most_common(10),
        }
        if not self.seconds:
            return result
        seconds = sorted(self.seconds)
        totals = [sum(b[i] for b in self.seconds.values()) for i in range(6)]
        span = seconds[-1] - seconds[0] + 1
        counts = sorted(b[0] for b in self.seconds.values())
        result.update({
            'first_timestamp_utc': utc(self.first / 1_000_000),
            'last_timestamp_utc': utc(self.last / 1_000_000),
            'seconds_with_records': len(seconds),
            'calendar_seconds_inclusive': span,
            'fraction_of_calendar_seconds_with_records': len(seconds) / span,
            'calendar_mean_rows_per_second': totals[0] / span,
            'mean_rows_per_nonempty_second': totals[0] / len(seconds),
            'p99_rows_per_nonempty_second': counts[math.ceil(.99 * len(counts)) - 1],
            'recorded_source_json_bytes': totals[2],
            'recorded_trade_rows': totals[3],
            'recorded_trade_contracts_not_deduplicated': totals[4],
            'recorded_trade_price_times_size_usd_not_deduplicated': totals[5],
            'max_distinct_markets_observed_in_one_utc_minute': max(map(len, self.minute_markets.values())),
            'largest_intervals_between_nonempty_seconds': [
                {'from_utc': utc(a), 'to_utc': utc(b), 'empty_seconds': b - a - 1}
                for a, b in sorted(zip(seconds, seconds[1:]), key=lambda pair: pair[1] - pair[0], reverse=True)[:5]
                if b - a > 1
            ],
        })
        peaks = {}
        for window in WINDOWS:
            bins = defaultdict(new_bucket)
            for second, values in self.seconds.items():
                aggregate = bins[second // window]
                for i, value in enumerate(values):
                    aggregate[i] += value
            metrics = {}
            for i, name in enumerate(('rows', 'book_rows', 'source_json_bytes', 'trade_rows', 'trade_contracts', 'trade_price_times_size_usd')):
                key, values = max(bins.items(), key=lambda item: (item[1][i], -item[0]))
                metrics[name] = {'max_count': values[i], 'per_second': values[i] / window,
                                 'window_start_utc': utc(key * window)}
            peaks[str(window)] = metrics
        result['peaks_in_utc_aligned_windows_seconds'] = peaks
        target = peaks['1']['rows']['max_count'] * args.headroom
        book_target = peaks['1']['book_rows']['max_count'] * args.headroom
        sizing = {
            'headroom_multiplier': args.headroom,
            'target_rows_per_second': math.ceil(target),
            'target_book_rows_per_second': math.ceil(book_target),
            'source_json_mbit_per_second_proxy': peaks['1']['source_json_bytes']['max_count'] * args.headroom * 8 / 1e6,
            'source_json_gb_per_day_if_target_peak_persisted': peaks['1']['source_json_bytes']['max_count'] * args.headroom * 86400 / 1e9,
            'hypothetical_order_requests_per_second': book_target * args.requote_fraction * args.requests_per_requote,
        }
        if args.cpu_us_per_row is not None:
            sizing['cpu_cores_at_target_utilization'] = target * args.cpu_us_per_row / 1e6 / args.cpu_utilization
        result['sizing_scenario'] = sizing
        return result


def analyze(args):
    try:
        import pyarrow as pa
        import pyarrow.parquet as pq
    except ImportError as exc:
        raise SystemExit('PyArrow required: python3 -m pip install pyarrow') from exc
    parquet = pq.ParquetFile(args.path)
    required = {'record_type', 'feed', 'market_slug', args.clock, 'source_record_json', 'price', 'size'}
    missing = required - set(parquet.schema_arrow.names)
    if missing:
        raise SystemExit(f'Missing required columns: {sorted(missing)}')

    # Metadata may be anywhere in the file; finish classification before traffic.
    market_types = {}
    bad_metadata = 0
    for batch in parquet.iter_batches(batch_size=args.batch_size, columns=['record_type', 'market_slug', 'source_record_json']):
        for kind, market, payload in zip(*(batch.column(name).to_pylist() for name in ['record_type', 'market_slug', 'source_record_json'])):
            if kind != 'market_metadata':
                continue
            try:
                metadata = json.loads(payload)
                market_types[market] = metadata.get('sports_market_type', 'unknown')
            except (TypeError, ValueError, AttributeError):
                bad_metadata += 1

    groups = defaultdict(Traffic)
    for batch in parquet.iter_batches(batch_size=args.batch_size, columns=sorted(required)):
        data = batch.to_pydict()
        # Integer microseconds avoid floating-point bucket-boundary rounding.
        timestamps = batch.column(args.clock).cast(pa.timestamp('us', tz='UTC')).cast('int64').to_pylist()
        for i, kind in enumerate(data['record_type']):
            if kind == 'market_metadata':
                continue
            market = data['market_slug'][i]
            market_type = market_types.get(market, 'unknown')
            feed = data['feed'][i] or 'unknown'
            values = (kind, market, timestamps[i], data['source_record_json'][i], data['price'][i], data['size'][i])
            groups[(feed, market_type)].add(*values)

    # Merge aggregate counters, never combine feeds (overlap is not deduplicated).
    for (feed, _), group in list(groups.items()):
        aggregate = groups[(feed, 'all')]
        aggregate.types.update(group.types)
        aggregate.markets.update(group.markets)
        aggregate.missing_time += group.missing_time
        aggregate.missing_payload += group.missing_payload
        aggregate.invalid_trades += group.invalid_trades
        if group.first is not None:
            aggregate.first = group.first if aggregate.first is None else min(aggregate.first, group.first)
            aggregate.last = group.last if aggregate.last is None else max(aggregate.last, group.last)
        for second, values in group.seconds.items():
            bucket = aggregate.seconds[second]
            for i, value in enumerate(values):
                bucket[i] += value
        for minute, markets in group.minute_markets.items():
            aggregate.minute_markets[minute].update(markets)

    return {
        'input': str(args.path), 'parquet_bytes': args.path.stat().st_size,
        'parquet_rows': parquet.metadata.num_rows, 'invalid_metadata_rows': bad_metadata,
        'parameters': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        'interpretation': [
            'WebSocket book_snapshot rows contain full books; FIX book rows are individual entries/markers, not wire messages.',
            'Feeds are reported separately; all includes match markets, futures and any unknown types. Do not add all to its subgroups.',
            'Peaks use UTC-aligned half-open bins, including partial first/last bins. A rolling window can cross a bin boundary and contain up to twice the aligned maximum.',
            'All observed peaks are lower bounds on possible future demand. Missing traffic cannot be recovered; empty seconds do not prove downtime.',
            'Trade totals are raw recorded price*size and contracts, not deduplicated even within a feed; they are not confirmed unique venue volume or bot fills. Cumulative market stats are not summed.',
            'JSON byte counts describe exported source records, not on-wire bytes, compressed Parquet storage, or in-memory books.',
            'Market counts in a minute measure observed activity, not concurrent subscriptions or in-play matches.',
            'Order request scenario = headroom * peak book rows/s * requote fraction * requests per requote. Coalescing, unchanged intents and output gating usually reduce this; retries and private traffic are excluded.',
            'CPU sizing is only emitted with an assumed measured cost per row; no actual CPU/RAM capacity benchmark is performed.',
        ],
        'groups': {f'{feed}/{market_type}': group.report(args) for (feed, market_type), group in sorted(groups.items())},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('path', type=Path, nargs='?', default=Path(__file__).with_name('polymarket_us_darts_history.parquet'))
    parser.add_argument('--output', type=Path, help='Write JSON here instead of stdout')
    parser.add_argument('--clock', choices=['wire_received_at', 'event_time', 'sending_time'], default='wire_received_at')
    parser.add_argument('--batch-size', type=int, default=50000)
    parser.add_argument('--headroom', type=positive, default=3.0)
    parser.add_argument('--requote-fraction', type=float, default=1.0)
    parser.add_argument('--requests-per-requote', type=positive, default=4.0, help='Scenario: cancel + place on each of two sides (default 4)')
    parser.add_argument('--cpu-us-per-row', type=positive, help='Assumed measured processing cost; omitted by default')
    parser.add_argument('--cpu-utilization', type=positive, default=0.7)
    args = parser.parse_args()
    if args.batch_size <= 0 or not 0 <= args.requote_fraction <= 1 or args.cpu_utilization > 1 or args.headroom < 1:
        parser.error('batch size must be positive; requote fraction in [0,1]; utilization in (0,1]; headroom >= 1')
    if not args.path.is_file():
        parser.error(f'Parquet file not found: {args.path}')
    if args.output and args.output.resolve() == args.path.resolve():
        parser.error('output must not overwrite the input')
    report = json.dumps(analyze(args), indent=2, allow_nan=False) + '\n'
    if args.output:
        args.output.write_text(report)
    else:
        print(report, end='')


if __name__ == '__main__':
    main()
