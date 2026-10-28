"""Small end-to-end fixture covering units, boundaries, and feed isolation."""
import argparse
from datetime import datetime, timezone
from decimal import Decimal
from pathlib import Path
import tempfile
import unittest

import pyarrow as pa
import pyarrow.parquet as pq

from estimate_load import analyze


class LoadEstimateTest(unittest.TestCase):
    def test_unsorted_rows_missing_clock_and_late_metadata(self):
        def row(kind, feed, second, price=None, size=None):
            return dict(record_type=kind, feed=feed, market_slug='match',
                        wire_received_at=None if second is None else datetime.fromtimestamp(second, timezone.utc),
                        source_record_json='{}', price=price, size=size)

        rows = [row('book_snapshot', 'websocket', 60),
                row('trade', 'websocket', 59, Decimal('.25'), Decimal('8')),
                row('book_snapshot', 'websocket', 59),
                row('book_change', 'fix', 59),
                row('book_snapshot', 'websocket', None),
                row('market_metadata', 'metadata', None)]
        rows[-1]['source_record_json'] = '{"sports_market_type":"darts_match_winner"}'
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'test.parquet'
            pq.write_table(pa.Table.from_pylist(rows), path, row_group_size=2)
            args = argparse.Namespace(path=path, clock='wire_received_at', batch_size=1,
                                      headroom=3, requote_fraction=.5, requests_per_requote=4,
                                      cpu_us_per_row=10, cpu_utilization=.5)
            report = analyze(args)
        ws = report['groups']['websocket/darts_match_winner']
        self.assertEqual(ws['rows'], 4)
        self.assertEqual(ws['rows_missing_selected_clock'], 1)
        self.assertEqual(ws['recorded_trade_contracts_not_deduplicated'], 8)
        self.assertEqual(ws['recorded_trade_price_times_size_usd_not_deduplicated'], 2)
        peaks = ws['peaks_in_utc_aligned_windows_seconds']
        self.assertEqual(peaks['1']['rows']['max_count'], 2)
        self.assertEqual(peaks['60']['rows']['max_count'], 2)
        self.assertEqual(peaks['3600']['rows']['max_count'], 3)
        self.assertEqual(peaks['1']['rows']['window_start_utc'], '1970-01-01T00:00:59+00:00')
        self.assertEqual(ws['sizing_scenario']['target_rows_per_second'], 6)
        self.assertEqual(ws['sizing_scenario']['hypothetical_order_requests_per_second'], 6)
        self.assertAlmostEqual(ws['sizing_scenario']['cpu_cores_at_target_utilization'], .00012)
        self.assertEqual(report['groups']['websocket/all'], ws)
        self.assertEqual(report['groups']['fix/all']['rows'], 1)

    def test_empty_file(self):
        schema = pa.schema([(name, pa.string()) for name in ['record_type', 'feed', 'market_slug', 'source_record_json']] +
                           [('wire_received_at', pa.timestamp('us', tz='UTC')), ('price', pa.decimal128(20, 9)), ('size', pa.decimal128(20, 9))])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'empty.parquet'
            pq.write_table(pa.Table.from_pylist([], schema=schema), path)
            report = analyze(argparse.Namespace(path=path, clock='wire_received_at', batch_size=10))
        self.assertEqual(report['groups'], {})
        self.assertEqual(report['parquet_rows'], 0)


if __name__ == '__main__':
    unittest.main()
