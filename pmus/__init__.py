"""Small operator tool for Polymarket US darts, built on the official `polymarket-us` SDK."""

import os
from pathlib import Path

from polymarket_us import PolymarketUS


def load_env(path='.env'):
    """Fill missing env vars from a `KEY=value` file, the same way `pmbot` reads `.env`."""
    try:
        lines = Path(path).read_text().splitlines()
    except OSError:
        return
    for line in lines:
        line = line.strip()
        if not line or line.startswith('#') or '=' not in line:
            continue
        key, value = line.split('=', 1)
        key = key.strip().removeprefix('export ').strip()
        if not os.environ.get(key):
            os.environ[key] = value.strip().strip('"\'')


def client():
    """An SDK client, authenticated when credentials are set (env or `.env`)."""
    load_env()
    return PolymarketUS(key_id=os.environ.get('POLYMARKET_KEY_ID'), secret_key=os.environ.get('POLYMARKET_SECRET_KEY'))


def darts_events(pm, kind='sport'):
    """Every darts event of one kind (`sport` or `futures`). The SDK has no wrapper for this endpoint."""
    events, offset = [], 0
    while True:
        page = pm.get('/v2/sports/darts/events', query={'limit': 100, 'offset': offset, 'type': kind})['events']
        events += page
        if len(page) < 100:
            return events
        offset += 100


def discover(pm, include_futures=False):
    """Tradable darts markets (`active && !closed`), each with its event attached as `_event`."""
    markets = {}
    for kind in ['sport', 'futures'] if include_futures else ['sport']:
        for event in darts_events(pm, kind):
            for market in event.get('markets') or []:
                if market.get('active') and not market.get('closed'):
                    markets.setdefault(market['slug'], {**market, '_event': event})
    return list(markets.values())
