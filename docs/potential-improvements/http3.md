Both hosts advertise HTTP/3 (`alt-svc: h3=":443"`, Cloudflare, Boston edge). reqwest supports it only behind an unstable flag (`--cfg reqwest_unstable` plus the `http3` feature, using quinn over UDP), so it was tested in a throwaway crate outside the workspace. That crate ran the same read-only A/B as `http_ab` (`crates/pm-venue/examples/http_ab.rs`), HTTP/2 against HTTP/3 with prior knowledge, two passes of 100 rounds:

| | Single p50 | Single p90 | Burst, per request p50 | Burst, per request p90 | Whole burst p90 |
| --- | ---: | ---: | ---: | ---: | ---: |
| HTTP/2, pass 1 | 47.4 ms | 65.8 ms | 51.6 ms | 120.0 ms | 128.6 ms |
| HTTP/3, pass 1 | 48.9 ms | 105.1 ms | 49.4 ms | 64.2 ms | 103.9 ms |
| HTTP/2, pass 2 | 47.0 ms | 70.1 ms | 50.0 ms | 103.2 ms | 122.0 ms |
| HTTP/3, pass 2 | 48.0 ms | 106.3 ms | 49.0 ms | 100.6 ms | 110.6 ms |

The medians are the same. HTTP/3 had a shorter burst tail but a consistently worse single-request p90 (105 ms against 66–70 ms, in both passes), and single requests are most of the bot's traffic. **Decision:** not adopted. Beyond the numbers:

- it needs an unstable reqwest flag set for the whole build;
- it runs on UDP, which some networks throttle or block, with no automatic fallback under prior knowledge;
- its main advantages, faster connection setup and no head-of-line blocking on packet loss, matter little with one long-lived connection.

Revisit if reqwest stabilises it, or if the AWS host shows packet loss.
