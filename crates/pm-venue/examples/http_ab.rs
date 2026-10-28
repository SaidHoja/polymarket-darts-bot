//! HTTP/1.1 vs HTTP/2 against the venue's gateway, with public reads only (no orders).
//! Result on 2026-10-28 from a home connection: no clear difference.
//! The live client uses HTTP/2; this forces HTTP/1.1 on one client for comparison.
//!
//!   cargo run --release -p pm-venue --example http_ab -- <market-slug> [rounds]
//!
//! Each round: one request alone, then a burst of 10 at once (like several quotes changing
//! in one tick). Both clients are warmed up first, so connection setup isn't counted
//! except where a burst needs more connections than the client holds. Only successful
//! responses are timed; errors and non-2xx statuses (a 429 is fast) are counted separately.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use tokio::task::JoinSet;

/// Milliseconds and the negotiated version, or why the request didn't count.
async fn get(client: &reqwest::Client, url: &str) -> Result<(f64, reqwest::Version), String> {
    let t = Instant::now();
    let r = client.get(url).send().await.map_err(|e| e.to_string())?;
    let (status, version) = (r.status(), r.version());
    r.bytes().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    Ok((t.elapsed().as_secs_f64() * 1e3, version))
}

fn pct(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1]
}

#[derive(Default)]
struct Samples {
    single: Vec<f64>,
    burst: Vec<f64>,
    burst_wall: Vec<f64>,
    versions: BTreeMap<String, usize>,
    failures: BTreeMap<String, usize>,
}

impl Samples {
    fn add(&mut self, result: Result<(f64, reqwest::Version), String>, into_burst: bool) {
        match result {
            Ok((ms, version)) => {
                *self.versions.entry(format!("{version:?}")).or_default() += 1;
                if into_burst {
                    self.burst.push(ms)
                } else {
                    self.single.push(ms)
                }
            }
            Err(e) => *self.failures.entry(e).or_default() += 1,
        }
    }
}

async fn run(name: &str, client: reqwest::Client, url: &str, rounds: usize) -> Result<(), String> {
    get(&client, url)
        .await
        .map_err(|e| format!("{name} warm-up: {e}"))?;
    let mut s = Samples::default();
    for _ in 0..rounds {
        s.add(get(&client, url).await, false);
        let t = Instant::now();
        let mut set = JoinSet::new();
        for _ in 0..10 {
            let (c, u) = (client.clone(), url.to_string());
            set.spawn(async move { get(&c, &u).await });
        }
        while let Some(joined) = set.join_next().await {
            s.add(joined.map_err(|e| e.to_string()).and_then(|r| r), true);
        }
        s.burst_wall.push(t.elapsed().as_secs_f64() * 1e3);
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    println!("{name} negotiated {:?}: single p50 {:.1} p90 {:.1} ms | burst-of-10 per request p50 {:.1} p90 {:.1} max {:.1} ms | whole burst p50 {:.1} p90 {:.1} ms",
        s.versions, pct(&mut s.single, 0.5), pct(&mut s.single, 0.9), pct(&mut s.burst, 0.5), pct(&mut s.burst, 0.9), pct(&mut s.burst, 1.0),
        pct(&mut s.burst_wall, 0.5), pct(&mut s.burst_wall, 0.9));
    if !s.failures.is_empty() {
        println!("{name} not timed: {:?}", s.failures);
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let usage = "usage: http_ab <market-slug> [rounds > 0]";
    let mut args = std::env::args().skip(1);
    let slug = args.next().ok_or(usage)?;
    let rounds: usize = match args.next() {
        Some(r) => r.parse().ok().filter(|&n| n > 0).ok_or(usage)?,
        None => 30,
    };
    let url = format!("{}/v1/market/slug/{slug}", pm_venue::rest::GATEWAY_URL);
    let base = || {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_nodelay(true)
    };
    let client = |http1: bool| {
        if http1 {
            base().http1_only().build()
        } else {
            base().build()
        }
        .map_err(|e| e.to_string())
    };
    // Alternate which goes first, so drift in network conditions hits both.
    for pass in 0..2 {
        println!("pass {}", pass + 1);
        for http1 in if pass % 2 == 0 {
            [true, false]
        } else {
            [false, true]
        } {
            run(
                if http1 { "HTTP/1.1" } else { "HTTP/2  " },
                client(http1)?,
                &url,
                rounds,
            )
            .await?;
        }
    }
    Ok(())
}
