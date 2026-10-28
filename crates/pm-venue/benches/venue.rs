//! Per-frame CPU cost of the venue layer: decoding socket frames and signing requests.
//! `cargo bench -p pm-venue`

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use criterion::{black_box, criterion_group, criterion_main, BatchSize, Criterion};
use pm_venue::decode::{decode_market_frame, decode_private_frame};
use pm_venue::{Credentials, Signer};

/// A real 40-level book frame from a capture.
fn book_frame() -> String {
    let line = include_str!("../../../tests/fixtures/market_frame.jsonl")
        .lines()
        .next()
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(line).unwrap();
    v["raw"].as_str().unwrap().to_string()
}

const TRADE: &str = r#"{"requestId":"tr-0","subscriptionType":"SUBSCRIPTION_TYPE_TRADE","trade":{"marketSlug":"aec-pdcdarts-luklit-wilcon-2026-10-23",
    "price":{"value":"0.1500","currency":"USD"},"quantity":{"value":"12.0000","currency":"USD"},"tradeTime":"2026-10-21T03:46:16.483269046Z",
    "maker":{"side":"ORDER_SIDE_SELL","intent":"ORDER_INTENT_UNDEFINED"},
    "taker":{"side":"ORDER_SIDE_BUY","intent":"ORDER_INTENT_SELL_SHORT","outcomeSide":"OUTCOME_SIDE_NO","action":"ORDER_ACTION_SELL"},
    "id":"D7NKT28G6ZRM","state":"TRADE_STATE_NEW"}}"#;

const EXECUTION: &str = r#"{"requestId":"orders","orderSubscriptionUpdate":{"execution":{"id":"E1",
    "order":{"id":"O1","marketSlug":"aec-pdcdarts-luklit-wilcon-2026-10-23","side":"ORDER_SIDE_SELL","type":"ORDER_TYPE_LIMIT",
      "price":{"value":"0.81","currency":"USD"},"quantity":1,"cumQuantity":0,"leavesQuantity":1,
      "intent":"ORDER_INTENT_BUY_SHORT","state":"ORDER_STATE_NEW"},
    "lastShares":"0.0000","lastPx":{"value":"0.0000","currency":"USD"},"type":"EXECUTION_TYPE_NEW","text":"",
    "orderRejectReason":"ORD_REJECT_REASON_EXCHANGE_OPTION","transactTime":"2026-10-21T22:07:56.283266966Z",
    "tradeId":"","aggressor":false,"unsolicitedCancelReason":"UNSOLICITED_CXL_REASON_UNDEFINED"}}}"#;

/// A partial fill, shaped like the captured ones (`lastShares` text, Amount prices and commission).
const FILL: &str = r#"{"requestId":"orders","orderSubscriptionUpdate":{"execution":{"id":"E2",
    "order":{"id":"O1","marketSlug":"aec-pdcdarts-luklit-wilcon-2026-10-23","side":"ORDER_SIDE_BUY","type":"ORDER_TYPE_LIMIT",
      "price":{"value":"0.38","currency":"USD"},"quantity":2,"cumQuantity":0.9,"leavesQuantity":1.1,
      "intent":"ORDER_INTENT_BUY_LONG","state":"ORDER_STATE_PARTIALLY_FILLED"},
    "lastShares":"0.9000","lastPx":{"value":"0.3800","currency":"USD"},"type":"EXECUTION_TYPE_PARTIAL_FILL","text":"",
    "orderRejectReason":"ORD_REJECT_REASON_EXCHANGE_OPTION","transactTime":"2026-10-28T10:17:11.761839218Z",
    "tradeId":"T1","aggressor":false,"commissionNotionalCollected":{"value":"0.0000","currency":"USD"},
    "unsolicitedCancelReason":"UNSOLICITED_CXL_REASON_UNDEFINED"}}}"#;

/// A position update with the venue's full `marketMetadata` block (ids and URLs made up).
const POSITION: &str = r##"{"requestId":"positions","subscriptionType":"SUBSCRIPTION_TYPE_POSITION","positionSubscription":{"beforePosition":null,
    "afterPosition":{"netPosition":"1","qtyBought":"1","qtySold":"0","cost":{"value":"0.3420","currency":"USD"},"realized":{"value":"0.0000","currency":"USD"},
      "bodPosition":"0","expired":false,"updateTime":"2026-10-28T10:17:11.761839218Z",
      "marketMetadata":{"slug":"aec-pdcdarts-luklit-wilcon-2026-10-23","icon":"","title":"Luke Littler vs. Will Cones","outcome":"Luke Littler",
        "eventSlug":"pdcdarts-luklit-wilcon-2026-10-23","teamId":1,"team":{"id":1,"name":"Luke Littler","abbreviation":"luklit","league":"pdc","record":"",
          "logo":"https://example.invalid/logo.jpg","alias":"Luke Littler","safeName":"Luke Littler","colorPrimary":"#2E5CFF","ordering":"home",
          "longIcon":"https://example.invalid/long.jpg","shortIcon":"https://example.invalid/short.jpg","displayAbbreviation":"Littler",
          "providerIds":[{"provider":"PROVIDER_SPORTRADAR","providerId":"sr:competitor:1"}],"color":{"light":"#2E5CFF","dark":"#2E5CFF"}},"eventId":"1"},
      "cashValue":{"value":"0.4770","currency":"USD"},"avgPx":{"value":"0.3800","currency":"USD"},"fees":null,"baseCost":{"value":"0.3420","currency":"USD"},
      "costPerShare":{"value":"0.3800","currency":"USD"},"netPositionDecimal":"0.9000","qtyBoughtDecimal":"0.9000","qtySoldDecimal":"0"}}}"##;

fn bench(c: &mut Criterion) {
    let book = book_frame();
    c.bench_function("decode/book_frame", |b| {
        b.iter(|| decode_market_frame(black_box(&book)).unwrap())
    });
    c.bench_function("decode/trade_frame", |b| {
        b.iter(|| decode_market_frame(black_box(TRADE)).unwrap())
    });
    c.bench_function("decode/execution_frame", |b| {
        b.iter(|| decode_private_frame(black_box(EXECUTION)).unwrap())
    });
    c.bench_function("decode/fill_frame", |b| {
        b.iter(|| decode_private_frame(black_box(FILL)).unwrap())
    });
    c.bench_function("decode/position_frame", |b| {
        b.iter(|| decode_private_frame(black_box(POSITION)).unwrap())
    });

    // Discovery: one real 22-event page from `GET /v2/sports/darts/events` (public data, 2026-10-28).
    let page: serde_json::Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/events_page.json")).unwrap();
    // The clone stands in for the owned response body and isn't timed.
    c.bench_function("rest/events_page", |b| {
        b.iter_batched(
            || page.clone(),
            |p| pm_venue::rest::events_page(p).unwrap(),
            BatchSize::LargeInput,
        )
    });
    c.bench_function("decode/events_page", |b| {
        b.iter(|| pm_venue::decode::decode_events_page(black_box(&page), false))
    });
    let created = serde_json::json!({"createdOrderIds": (0..20).map(|i| format!("DCKBKTHG{i:04}")).collect::<Vec<_>>()});
    c.bench_function("rest/created_ids_20", |b| {
        b.iter(|| pm_venue::rest::created_ids(black_box(&created), 20))
    });

    let secret = STANDARD.encode((0u8..64).collect::<Vec<_>>());
    let signer = Signer::new(&Credentials {
        key_id: "bench".into(),
        secret_key: secret,
    })
    .unwrap();
    c.bench_function("sign/request_headers", |b| {
        b.iter(|| signer.headers_at("POST", black_box("/v1/orders"), 1_760_000_000_000))
    });

    // Building and serialising one REST create body: the CPU part of sending an order.
    let order = pm_types::NewOrder {
        slug: "aec-pdcdarts-luklit-wilcon-2026-10-23".into(),
        intent: pm_types::Intent::BuyLong,
        px: pm_types::Px(3500),
        qty: pm_types::Qty(10_000),
        tif: pm_types::Tif::Gtd(1_793_150_400_000_000_000),
        post_only: true,
    };
    c.bench_function("rest/create_body_to_json", |b| {
        b.iter(|| serde_json::to_vec(&pm_venue::rest::order_body(black_box(&order))).unwrap())
    });
    c.bench_function("rest/modify_body_to_json", |b| {
        b.iter(|| {
            serde_json::to_vec(&pm_venue::rest::modify_body(
                black_box("O1"),
                &order.slug,
                order.px,
                order.qty,
                order.tif,
                true,
            ))
            .unwrap()
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
