"""python -m pmus <command>. Run with --help for the list."""

import argparse
import asyncio
import json
import sys
from datetime import datetime, timedelta, timezone
from pathlib import Path

from pmus import client, discover
from pmus.deadman import cancel_all_and_verify

INTENTS = {
    'buy-yes': 'ORDER_INTENT_BUY_LONG',
    'sell-yes': 'ORDER_INTENT_SELL_LONG',
    'buy-no': 'ORDER_INTENT_BUY_SHORT',
    'sell-no': 'ORDER_INTENT_SELL_SHORT',
}


def show(value):
    print(json.dumps(value, indent=2))


def confirm(args, what):
    if not args.yes and input(f'{what}? [y/N] ').strip().lower() != 'y':
        sys.exit('aborted')


def cmd_markets(pm, args):
    for m in sorted(discover(pm, args.include_futures), key=lambda m: m.get('gameStartTime') or ''):
        print(f"{m['slug']:55} {m.get('sportsMarketType', ''):22} {m.get('gameStartTime') or '-':22} {m.get('question') or m.get('title', '')}")


def cmd_book(pm, args):
    show(pm.markets.book(args.slug))


def cmd_balances(pm, args):
    show(pm.account.balances())


def cmd_positions(pm, args):
    positions, cursor = {}, None
    while True:
        page = pm.portfolio.positions({'limit': 100, **({'cursor': cursor} if cursor else {})})
        positions.update(page.get('positions') or {})
        cursor = page.get('nextCursor')
        if page.get('eof') or not cursor:
            break
    show(positions)


def cmd_orders(pm, args):
    orders = pm.orders.list({'slugs': args.slugs} if args.slugs else None).get('orders') or []
    for o in orders:
        print(f"{o['id']:14} {o['marketSlug']:45} {o['intent'].removeprefix('ORDER_INTENT_'):10} "
              f"{o['price']['value']:>6} x {o.get('leavesQuantity')} {o.get('state', '').removeprefix('ORDER_STATE_')} "
              f"until {o.get('goodTillTime', '-')}")
    print(f'{len(orders)} open orders')


def cmd_order(pm, args):
    # The venue takes YES prices only: a NO price X is sent as 1 - X.
    yes_px = 1 - args.price if args.side.endswith('-no') else args.price
    expiry = datetime.now(timezone.utc).replace(microsecond=0) + timedelta(seconds=args.gtd)
    body = {
        'marketSlug': args.slug,
        'intent': INTENTS[args.side],
        'type': 'ORDER_TYPE_LIMIT',
        'price': {'value': f'{yes_px:.4f}', 'currency': 'USD'},
        'quantity': args.qty,
        'tif': 'TIME_IN_FORCE_GOOD_TILL_DATE',
        'goodTillTime': expiry.strftime('%Y-%m-%dT%H:%M:%SZ'),
        'participateDontInitiate': not args.allow_take,
        'manualOrderIndicator': 'MANUAL_ORDER_INDICATOR_MANUAL',
    }
    confirm(args, f"{args.side} {args.qty} {args.slug} @ {args.price} (YES {yes_px:.4f}), "
                  f"{'may take' if args.allow_take else 'post-only'}, expires {body['goodTillTime']}")
    show(pm.orders.create(body))


def cmd_cancel_all(pm, args):
    confirm(args, f"cancel all open orders{' in ' + ', '.join(args.slugs) if args.slugs else ''}")
    left = cancel_all_and_verify(pm, args.slugs or None)
    if left != []:
        sys.exit(f'cancel-all not confirmed: {"unknown" if left is None else len(left)} orders may still be open')
    print('no open orders')


# ---------------------------------------------------------------- record

class Capture:
    """Appends frames as `{"conn", "raw", "recv_at"}` lines to captures/<prefix><UTC date>.jsonl."""

    def __init__(self, directory):
        self.dir = Path(directory)
        self.dir.mkdir(parents=True, exist_ok=True)
        self.files = {}

    def write(self, prefix, conn, message):
        now = datetime.now(timezone.utc)
        name = f'{prefix}{now:%Y-%m-%d}.jsonl'
        if name not in self.files:
            self.files[name] = open(self.dir / name, 'a')
        line = {'conn': conn, 'raw': json.dumps(message, separators=(',', ':')), 'recv_at': now.strftime('%Y-%m-%dT%H:%M:%S.%fZ')}
        self.files[name].write(json.dumps(line) + '\n')
        self.files[name].flush()


async def stream(pm, capture, private, include_futures):
    """One socket, reconnecting after 5 s. The markets socket rediscovers every 5 minutes."""
    conn = 0
    while True:
        conn += 1
        ws = pm.ws.private() if private else pm.ws.markets()
        closed = asyncio.Event()
        ws.on('message', lambda m, c=conn: capture.write('private-' if private else '', c, m))
        ws.on('close', closed.set)
        def on_error(e, ws=ws, closed=closed):
            print(f'{"private" if private else "markets"} socket: {e!r}')
            if not ws.is_connected:  # the SDK emits only `error`, not `close`, when its read loop dies
                closed.set()
        ws.on('error', on_error)
        try:
            await ws.connect()
            if private:
                for kind in ['ORDER', 'ORDER_SNAPSHOT', 'POSITION', 'ACCOUNT_BALANCE']:
                    await ws.subscribe_all(kind.lower(), f'SUBSCRIPTION_TYPE_{kind}')
                await closed.wait()
            subscribed = set()
            while not private and ws.is_connected:
                slugs = [m['slug'] for m in await asyncio.to_thread(discover, pm, include_futures)]
                new = [s for s in slugs if s not in subscribed]
                for i in range(0, len(new), 100):  # at most 100 slugs per subscription
                    group = new[i:i + 100]
                    await ws.subscribe_market_data(f'md-{len(subscribed)}', group)
                    await ws.subscribe_trades(f'tr-{len(subscribed)}', group)
                    subscribed.update(group)
                print(f'markets socket conn={conn}: {len(subscribed)} markets subscribed')
                try:
                    await asyncio.wait_for(closed.wait(), 300)
                except TimeoutError:
                    pass
        except Exception as e:
            print(f'{"private" if private else "markets"} socket conn={conn} failed: {e!r}')
        finally:
            await ws.close()
        await asyncio.sleep(5)


def cmd_record(pm, args):
    capture = Capture(args.dir)
    tasks = [stream(pm, capture, False, args.include_futures)]
    if args.private:
        tasks.append(stream(pm, capture, True, False))

    async def run():
        try:
            await asyncio.wait_for(asyncio.gather(*tasks), args.duration)
        except TimeoutError:
            pass

    try:
        asyncio.run(run())
    except KeyboardInterrupt:
        pass


def main():
    p = argparse.ArgumentParser(prog='pmus', description='Polymarket US darts operator tool')
    sub = p.add_subparsers(dest='command', required=True)

    s = sub.add_parser('markets', help='active darts markets')
    s.add_argument('--include-futures', action='store_true')
    s = sub.add_parser('book', help='REST order book of one market')
    s.add_argument('slug')
    sub.add_parser('balances')
    sub.add_parser('positions')
    s = sub.add_parser('orders', help='open orders')
    s.add_argument('slugs', nargs='*')
    s = sub.add_parser('order', help='place one limit order (post-only, GTD 1 h by default)')
    s.add_argument('slug')
    s.add_argument('side', choices=INTENTS)
    s.add_argument('price', type=float, help="price of the side you're trading, e.g. 0.42")
    s.add_argument('qty', type=float)
    s.add_argument('--allow-take', action='store_true', help='allow crossing the book (not post-only)')
    s.add_argument('--gtd', type=int, default=3600, help='seconds until the order expires')
    s.add_argument('-y', '--yes', action='store_true', help="don't ask to confirm")
    s = sub.add_parser('cancel-all', help='cancel all open orders, then check none are left')
    s.add_argument('slugs', nargs='*')
    s.add_argument('-y', '--yes', action='store_true', help="don't ask to confirm")
    s = sub.add_parser('record', help='record raw market (and private) frames to captures/')
    s.add_argument('--include-futures', action='store_true')
    s.add_argument('--private', action='store_true')
    s.add_argument('--duration', type=float, help='seconds; runs until Ctrl-C when omitted')
    s.add_argument('--dir', default='captures')

    args = p.parse_args()
    with client() as pm:
        globals()['cmd_' + args.command.replace('-', '_')](pm, args)


if __name__ == '__main__':
    main()
