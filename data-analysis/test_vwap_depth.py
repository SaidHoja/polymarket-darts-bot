"""Data integrity checks for the depth study's replay and future-price sampling."""

import collections
from pathlib import Path
import tempfile
import unittest

import vwap_depth as study


class VwapDepthStudyTests(unittest.TestCase):
    def test_fix_messages_are_atomic_and_order_sizes_aggregate_by_price(self):
        text = """rx_ns,ev_ns,msg_seq,entry_index,kind,side,price,size,id,aggressor
0,0,1,-1,M,,,,,
0,0,1,0,S,b,0.4,0.5,b1,
0,0,1,1,S,b,0.4,0.5,b2,
0,0,1,2,S,b,0.3,1,b3,
0,0,1,3,S,o,0.6,2,a1,
10000000000,10000000000,2,0,U,o,0.7,1,a1,
10000000000,10000000000,2,1,A,b,0.5,1,b4,
20000000000,20000000000,3,-1,M,,,,,
20000000000,20000000000,3,0,S,b,0.1,1,b5,
20000000000,20000000000,3,1,S,o,0.9,1,a2,
"""
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "market.csv"
            path.write_text(text)
            observations = study.historical_market(path, collections.Counter())
        self.assertEqual(len(observations), 3)
        self.assertAlmostEqual(observations[0][5][1][0], 0.475)
        self.assertAlmostEqual(observations[0][5][1][1], 4)
        self.assertAlmostEqual(observations[1][5][1][0], 0.475)
        self.assertEqual(observations[2][5][1][1], 2)  # snapshot replaces all orders

    def test_future_target_uses_book_before_horizon_and_rejects_capture_gaps(self):
        def obs(t, bid, ask, segment=0):
            return study.observation(t, [(bid, 1)], [(ask, 1)], segment)

        result = study.summarize_market([
            obs(0, .4, .6), obs(29, .5, .6), obs(31, .8, .9),
        ], .4)
        errors = result[3]["future_mae_cents"]
        self.assertEqual(len(errors), 1)
        self.assertAlmostEqual(errors[0], 5)  # mid at 29 s, never the book at 31 s
        result = study.summarize_market([obs(0, .4, .6), obs(3600, .8, .9, 1)], .4)
        self.assertEqual(result[3]["future_mae_cents"], [])

    def test_invalid_books_do_not_enter_the_comparison(self):
        self.assertIsNone(study.observation(0, [], [(.6, 1)], 0))
        self.assertIsNone(study.observation(0, [(.7, 1)], [(.6, 1)], 0))
        self.assertIsNone(study.observation(0, [(.4, -1)], [(.6, 1)], 0))
        self.assertIsNone(study.observation(0, [(.3, 1), (.4, 1)], [(.6, 1)], 0))


if __name__ == "__main__":
    unittest.main()
