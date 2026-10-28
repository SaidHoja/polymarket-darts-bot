"""Cancel every open order and check over REST that none are left.

Used by `pmus cancel-all` and by the dead-man switch Lambda (docs/potential-improvements/aws-deployment.md).
In Lambda, credentials come from env vars, or from SSM when `POLYMARKET_KEY_ID_PARAM` and
`POLYMARKET_SECRET_KEY_PARAM` name the parameters (boto3 is part of the Lambda runtime).
"""

import os
import time

from polymarket_us import PolymarketUS


def cancel_all_and_verify(pm, slugs=None, rounds=3):
    """Like pmbot's shutdown: cancel all, then list open orders, up to `rounds` times. Returns what's left."""
    left = []
    for round_ in range(1, rounds + 1):
        try:
            pm.orders.cancel_all({'slugs': slugs} if slugs else None)
        except Exception as e:
            print(f'cancel-all failed (round {round_}): {e!r}')
        try:
            left = pm.orders.list({'slugs': slugs} if slugs else None).get('orders') or []
            if not left:
                return []
            print(f'{len(left)} orders still open after cancel-all (round {round_})')
        except Exception as e:
            print(f'open-orders check failed (round {round_}): {e!r}')
            left = None
        time.sleep(0.25 * round_)
    return left


def lambda_handler(event, context):
    key_id, secret = os.environ.get('POLYMARKET_KEY_ID'), os.environ.get('POLYMARKET_SECRET_KEY')
    if os.environ.get('POLYMARKET_KEY_ID_PARAM'):
        import boto3
        ssm = boto3.client('ssm')
        get = lambda name: ssm.get_parameter(Name=os.environ[name], WithDecryption=True)['Parameter']['Value']
        key_id, secret = get('POLYMARKET_KEY_ID_PARAM'), get('POLYMARKET_SECRET_KEY_PARAM')
    with PolymarketUS(key_id=key_id, secret_key=secret) as pm:
        left = cancel_all_and_verify(pm)
    if left != []:
        # Raise so the invocation counts as failed and the alarm/retry policy sees it.
        raise RuntimeError(f'cancel-all not confirmed: {"unknown" if left is None else len(left)} orders may still be open')
    return {'ok': True}
