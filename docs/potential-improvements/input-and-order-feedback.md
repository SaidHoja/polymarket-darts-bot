> **Status: open items only.** These are what's left from a review of input handling and order-error handling (2026-10-27, re-checked against the code 2026-10-28). The review's walkthrough of every stage, from sources to reconciliation, is in git history at `05608c2:docs/input-and-order-feedback.md`. The parts of it that are still current are covered by [architecture](../architecture/architecture.md), [pm-live](../architecture/pm-live.md) and [risk](../risk.md). Already fixed: unanswered order queries are retried with backoff (`OrderRec::query_due`), any undecodable private frame resyncs the private socket, a markets-socket subscription error reconnects, an absent tick size logs a warning (an unusable one skips the market), and a cancel the gateway refuses is retried with backoff and, after three refusals, looked up instead of resent every 250 ms.

Ordered by expected impact.

## 1. Reject storms trip too easily, clear themselves, and are mislabelled

`Risk::on_reject` counts rejects toward `max_rejects_per_min` (10); past that, new quotes stop on every market for `reject_pause_s` (30 s). Several things make this blunter than it should be:

- **Per token, not per request.** The gateway batches up to 20 actions into one request with one `RestOutcome`, and `OrderManager::on_rest_result` emits a `Rejected` for every token. One invalid order in a batch of 11 or more trips the storm; for `OurBug` it also pauses every market in the batch and requests one discovery refresh per token. Fix: count one reject per HTTP response, or split a rejected batch and retry the entries one at a time to find the bad one.
- **429s count.** `RejectClass::RateLimited` falls through to the storm counter, although a 429 means we exceeded our own budget, not that the venue refused anything. Return early, as `BookMoved` does, unless the extra 30 s stop is wanted.
- **It clears on a timer.** Most reject pauses (30–60 s) and the storm block expire on their own, so a problem nobody understands can repeat every minute. Proposal, not decided: on a non-routine reject (anything but `BookMoved`, maybe `Transient`), pause the market with no expiry, cancel its orders, show the raw reason and text in the UI, and wait for an operator `resume` (per-market resume and indefinite pauses that survive a restart already exist, see [risk](../risk.md#restarts)). This needs an alert that reliably reaches a human.
- **Auth failures look like storms.** A 401 sets `orders_blocked_until = u64::MAX` (no orders until a restart, by design), but the UI derives one `orders_blocked` flag from it and shows "Orders blocked after a reject storm." for both. Put the reason (storm with its end time, or auth failure) in the snapshot and show it.

## 2. A book without `transactTime` skips the ordering check

`MarketData::on_book` rejects an older book only when both the cached and the new book carry a `transactTime`. The inbox conflates books by arrival, so an untimestamped book that arrives late would replace a newer one. The book-staleness rules (`MarketData::stale_reason`) don't cover this: they catch a book that stops updating, not one that goes backwards. Not observed on the venue. A counter for books without `transactTime` would show whether it ever happens.

## 3. Smaller items

- **Discovery fails the whole round if the futures query fails**, even when the sport query succeeded (`feeds.rs`, discovery loop). With `--include-futures`, a futures outage freezes the match market set too. Applying the kinds that succeeded would fix it.
- **`missed_volume` is computed but nothing consumes it.** The core notes it to the journal when cumulative `sharesTraded` rises past the trades received, and nothing acts on it. Candidates: a metric, or treating it like a stale book (a trade we didn't see).
- **Paper mode exercises none of the error paths.** `run_paper_gateway` acks everything after 20 ms and never rejects, times out or drops a feed. Only unit tests and the backtester's simulated latency cover rejects, `InDoubt`, timeouts and reconciliation. A paper fault-injection option (a reject rate, an ambiguous-result rate, a forced private-socket drop) would let a paper run exercise them end to end.
- **Failed reads produce no event.** `QueryOpenOrders`, `QueryOrder`, `QueryPositions`, `QueryBalance` and `QuerySettlement` only log on failure in the gateway. The core now retries unanswered order queries with backoff, so nothing gets stuck. A failure event would still make the retry immediate and show the failure in the journal instead of only the log.
