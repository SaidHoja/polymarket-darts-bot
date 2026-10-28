> **Status: not started.** `pmbot` runs from a dev Mac. No AWS resources, CI, `infra/` or deploy script exist yet, and the repo has no remote. The code-side prerequisites that were blocking live use are done: SIGTERM/SIGHUP run the cancel-all shutdown, the loss limits survive a restart (`--risk-state`), the core thread raises its own priority, and the dead-man handler exists (`pmus/deadman.py`). What's left is [Phase 0](#phase-0-checks-before-building-anything), the [infrastructure](#infrastructure-as-code), CI and deploys, and the small [code changes](#code-changes) still open.

The goal is to run `pmbot` on AWS around the clock: `record` first, then `paper`, then `live`. This covers what has to be checked before anything is built, what to build, the code changes it still needs, and the order to roll it out. The process itself is described in [architecture](../architecture/architecture.md) and [pm-live](../architecture/pm-live.md). This doc only covers where it runs and how it is operated.

Out of scope: backtests in the cloud (they run on a laptop in about a minute), the Python `pmus` client as a service (it stays an operator tool; only its dead-man handler runs in AWS), and any multi-instance or partitioned design ([input queue and workers](input-queue-and-workers.md) is deferred).

## What the process needs from its host

Everything below comes from `crates/pm-live/src/main.rs` and the docs. Each item affects a deployment decision.

| Property | Where | Consequence for deployment |
| --- | --- | --- |
| Single long-lived process: 2 Tokio workers, the core thread, plus journal, watchdog (own runtime), risk-file and, with `--ui`, UI threads | `main.rs`, [pm-live](../architecture/pm-live.md) | A VM or long-running container. Not Lambda. |
| **There must be only one instance per account.** Reconciliation cancels orders it doesn't know about, and shutdown and `cancel-all` act on the whole account | [latency-probe.md](../latency-probe.md#launch-and-collect), [pm-core](../architecture/pm-core.md#order-manager) | Deploys must stop the old process before the new one starts, so no rolling or blue/green overlap. Never run a laptop copy against the same account. |
| Credentials come from `POLYMARKET_KEY_ID` / `POLYMARKET_SECRET_KEY` in the environment, or from `--env-file` (default `.env`) | `pm-venue/src/auth.rs` | Fetch them from a secret store at start and never write them to disk in plaintext. |
| Requests are signed with `X-PM-Timestamp`, which must be within 30 s of server time. `book_lag` is a cross-clock measurement | [connectivity §2, §9](../polymarket-docs/polymarket-connectivity.md#9-latency) | The clock must be disciplined. The dev Mac was once 314 ms off. |
| Ctrl-C, SIGTERM and SIGHUP all run the shutdown: wait up to 2 s for creates already sent, then cancel-all and confirm, three rounds | `main.rs::stop_signal`, [risk](../risk.md#backstops-outside-the-core) | systemd's default `KillSignal=SIGTERM` works. `TimeoutStopSec` must leave room for the cancel rounds. |
| Journals: one JSONL file per stream per UTC day under `--journal-dir`. A one-hour live run wrote about 24 MB (the private stream was the largest at 12 MB) | `journal.rs`, `captures/latency-probe/` | A local disk sized for several days of journals, then a daily copy to S3. |
| Admin is a Unix socket at `<run-dir>/pmbot.sock` (`status`, `metrics`, `pause`, `resume`, `cancel-all`). The web UI (`--ui`) refuses a non-loopback address without `--ui-allow-remote` | `admin.rs`, `ui.rs` | The operator needs a shell on the host, and port forwarding for the UI. No network listener is required. |
| **Risk state lives in `<run-dir>/risk-state-<mode>.json`**: the day's PnL, the daily kill, per-market losses, indefinite pauses. An unreadable file stops the bot from starting | `risk_file.rs`, [risk](../risk.md#restarts) | `--run-dir` must be on persistent disk (EBS, not tmpfs) and survive a deploy. A corrupt file needs an operator, so it should alarm. |
| Logs are `tracing` text on stdout, with a 10 s status line and a 60 s metrics line; the metrics are also journaled to `metrics-<date>.jsonl` | `main.rs`, `metrics.rs` | Ship them to CloudWatch Logs and build alarms on metric filters. |
| If the process dies, resting orders stay up until GTD expiry: up to start + 300 s pre-match, which can be hours away, and 120 s in-play | `[execution]` config | An off-box dead-man switch is needed. See [Monitoring](#monitoring-and-alerting). |
| Rate limits are counted per source IP and per Cloudflare location. `pmbot` limits itself to 15 REST requests/s | [connectivity §8](../polymarket-docs/polymarket-connectivity.md#8-rate-limits-and-request-budget), `REST_RATE` | Use a stable egress IP (Elastic IP). `pmus` runs from the same box share that budget. |

## Phase 0: checks before building anything

These are go/no-go questions. None of them needs infrastructure beyond one throwaway instance.

1. **Is the API allowed from a cloud host?** Polymarket US is a regulated US venue, and retail access requires KYC. Read the retail API terms for anything about hosting location, automation, or geofencing. Cloudflare sits in front of `api.polymarket.us` and `gateway.polymarket.us`, and it sometimes challenges datacenter IP ranges. Test: start a `t3.micro` in each candidate region, then run `pmus markets` and `pmbot --mode record --journal-dir none` for 30 minutes. Look for 403s, challenge pages, or 401s.
2. **Which region?** The institutional path uses AWS PrivateLink ([connectivity §1](../polymarket-docs/polymarket-connectivity.md)), so the exchange is on AWS, but the region is not documented. Candidates are `us-east-1`, `us-east-2` and `us-west-2`. During a live darts session, run `record` mode in all three regions at once with chrony synced to Amazon Time Sync, then compare `ws_rtt_markets`, `book_lag` p50 and p99, and the REST read latency (`cargo run --release -p pm-venue --example http_ab -- <market-slug> 100`). Choose the region with the lowest `book_lag` p99. If the regions are within a few milliseconds of each other, choose `us-east-1`. If the venue has published its region by then, check our result against it.
3. **Does AWS clock sync make `book_lag` usable?** On a synced box, `book_lag` p50 should come close to the ~55–65 ms exchange-side figure, and it should never be negative. If it is still odd, the problem is on the venue side, not our clock.

Exit criteria: the API works from AWS without challenges, a region is chosen with the measured numbers recorded in [connectivity §9](../polymarket-docs/polymarket-connectivity.md#9-latency), and the terms have been read and found to allow cloud hosting.

## Target architecture

```
                      ┌────────────────────── AWS account (one region) ──────────────────────┐
                      │  VPC, 1 public subnet, no NAT                                         │
 Polymarket US  ◄─────┼── EIP ── EC2 c7g.large (AL2023, on-demand)                            │
 (Cloudflare)         │          ├─ systemd: pmbot.service ──► /var/lib/pmbot/{captures,run}  │
                      │          ├─ chrony → Amazon Time Sync (169.254.169.123 / PHC)         │
                      │          ├─ CloudWatch agent: journald logs, disk, chrony offset      │
                      │          └─ systemd timer: daily journal sync ──────► S3 (journals)   │
                      │  SSM Parameter Store (SecureString): API key                          │
                      │  SSM Session Manager: operator shell + UI port forward, no SSH        │
                      │  CloudWatch alarms ──► SNS ──► email / SMS                            │
                      │                    └──► Lambda dead-man (pmus/deadman.py), other IP   │
                      │  S3 (artifacts): pmbot binaries + configs, versioned                  │
                      └───────────────────────────────────────────────────────────────────────┘
```

### Compute: one EC2 instance

Use a single EC2 instance, not ECS or Fargate. The process is a singleton with local state (risk file, admin socket, journals). ECS deployment overlap, Fargate's lack of host clock control, and container-shell friction would all have to be worked around, and in return we would get nothing we need.

- **Instance type:** `c7g.large` (Graviton, 2 vCPU, 4 GiB). This costs about $53 a month on demand. `c7i.large` is the x86 fallback if anything in the dependency tree won't build for `aarch64` (rustls and ring both support it). Avoid the burstable `t` family: when CPU credits run out it shows up as latency spikes in `tick` and `queue`. The core benchmarks run in microseconds, so 2 vCPUs leave plenty of headroom. Use `core_busy` and `queue` p99 to decide whether that changes.
- **Pricing:** on-demand, never Spot, because an interruption leaves orders resting. Once it has run for a month, buy a 1-year Compute Savings Plan.
- **OS:** Amazon Linux 2023. Chrony already uses Amazon Time Sync there. Turn on the PTP hardware clock (`/dev/ptp0` on Nitro) for sub-millisecond accuracy.
- **Disk:** a 40 GiB encrypted gp3 volume. Journals are kept locally for 7 days and then deleted after the S3 copy is verified. The worst case is roughly 0.5 GB a day if the busy-hour rate holds all day. The risk state file sits on the same volume.
- **Recovery:** a CloudWatch `StatusCheckFailed_System` alarm with the EC2 recover action. This keeps the instance ID, EIP and volume, and so the risk state.

### Network

- One VPC with one public subnet, an Internet Gateway and an Elastic IP. The bot only needs outbound 443, and a NAT gateway would add about $32 a month plus a hop on every packet.
- The security group allows no inbound traffic and only outbound TCP 443 (plus UDP 123 if the PHC/link-local time source isn't used).
- The operator gets in through SSM Session Manager, so there are no SSH keys and no port 22. Use the admin socket from that shell: `echo status | sudo -u pmbot nc -U /var/lib/pmbot/run/pmbot.sock`.
- **Operator UI:** run with `--ui 127.0.0.1:8787` and reach it with an SSM port-forwarding session (`aws ssm start-session --document-name AWS-StartPortForwardingSession --parameters portNumber=8787,localPortNumber=8787`). Never use `--ui-allow-remote` with an open security group: the UI can cancel orders and shut the bot down.

### Secrets and config

- **API key:** stored in SSM Parameter Store as a SecureString (`/pmbot/<env>/POLYMARKET_KEY_ID`, `/pmbot/<env>/POLYMARKET_SECRET_KEY`). It's free, and Secrets Manager's automatic rotation is of no use here because the venue key is rotated by hand.
- The instance role is allowed `ssm:GetParameters` on `/pmbot/<env>/*` and `kms:Decrypt` on that key, and nothing else that is sensitive.
- **Delivery:** `ExecStartPre` fetches both parameters into `/run/pmbot/env` (tmpfs, mode 0400, owned by `pmbot`), and the process is started with `--env-file /run/pmbot/env`. This needs no code change. The file disappears at reboot. Deleting it once the process has started is optional, because `main.rs` drops the credentials after it builds the signer.
- **Config:** `pmbot.toml` is versioned with the binary in the artifacts bucket (start from `config/pmbot.example.toml` or one of the `config/pmbot-*-live.toml` files). A config change is deployed exactly like a binary change, because the bot can't reload config.

### Process supervision (systemd)

```ini
[Unit]
Description=pmbot
After=network-online.target time-sync.target
Wants=network-online.target time-sync.target
StartLimitIntervalSec=3600
StartLimitBurst=3

[Service]
User=pmbot
WorkingDirectory=/var/lib/pmbot
ExecStartPre=/opt/pmbot/bin/fetch-secrets
ExecStart=/opt/pmbot/current/pmbot --mode ${PMBOT_MODE} --config /opt/pmbot/current/pmbot.toml \
          --env-file /run/pmbot/env --journal-dir /var/lib/pmbot/captures --run-dir /var/lib/pmbot/run \
          --ui 127.0.0.1:8787
EnvironmentFile=/etc/pmbot/mode          # PMBOT_MODE=record|paper|live
TimeoutStopSec=15
Restart=on-failure
RestartSec=30
LimitNOFILE=65536
AmbientCapabilities=CAP_SYS_NICE         # lets the core thread raise its own priority (see below)
```

- `systemctl stop` sends SIGTERM, which runs the cancel-all shutdown. `TimeoutStopSec=15` covers the 2 s wait for in-flight creates plus three cancel rounds; after that systemd sends SIGKILL and GTD and the dead-man switch take over.
- `--risk-state` is left at its default, `/var/lib/pmbot/run/risk-state-<mode>.json`, so it lives on the EBS volume and survives restarts, deploys and instance recovery.
- `StartLimitBurst=3` per hour stops a crash loop from repeatedly placing orders. The loss limits no longer reset on restart, but each start still re-quotes every market. After three failures the unit stays down and an alarm fires.
- Exit code 1 (bad config, bad credentials, or an unreadable risk state file) also triggers `Restart=on-failure`, but the start limit caps it.
- **Core thread priority.** At start the core thread tries to raise its own priority: nice −10 on Linux (`crates/pm-live/src/priority.rs`). Its ticks take tens of µs, but if the scheduler wakes it late or preempts it partway through a tick, that adds milliseconds (seen on the Mac). Raising priority needs `CAP_SYS_NICE`, granted by the `AmbientCapabilities=CAP_SYS_NICE` line above, or set by `Nice=-10` in the unit instead. Without either, the bot logs `core thread: setting nice -10 failed` and runs at normal priority. Whether it matters on a dedicated 2-vCPU instance is not known yet. Check for `core thread: nice -10` at the first start, then compare `tick` and `queue` p99 with and without it. Real-time scheduling (SCHED_FIFO / `CPUSchedulingPolicy=fifo`) is deliberately not used: a stuck real-time thread can starve the whole host, including the SSM agent.

### Build and deploy

The repo has one local commit and no remote. Push it to a private GitHub repo first.

1. **CI (GitHub Actions, `ubuntu-24.04-arm` runner):** run `cargo test --workspace` and the Python unittests, then `cargo build --release -p pm-live --locked`. Upload `pmbot`, its `sha256` and the config files to `s3://<artifacts>/pmbot/<git-sha>/`. Also build the dead-man Lambda zip: `pmus/` plus `polymarket_us` and PyNaCl installed for the Lambda's architecture (PyNaCl has native wheels, so build on the matching runner). CI reaches AWS through GitHub OIDC with a role that can only write to that prefix, so there are no long-lived keys.
2. **Deploy (manual trigger, `workflow_dispatch` or `make deploy SHA=…`):** an SSM Run Command on the instance runs `deploy.sh <sha>`, which:
   - downloads the release and checks the sha256;
   - **refuses to continue if any match is in-play**, unless `--force` is given. It decides this by parsing `status` from the admin socket (see [code changes](#code-changes));
   - disables the heartbeat alarm's actions so the dead-man switch doesn't fire;
   - runs `systemctl stop pmbot`, which goes through the cancel-all path;
   - points the `/opt/pmbot/current` symlink at the new release and keeps the previous 5;
   - runs `systemctl start pmbot`, waits for the readiness line in the log, and rolls back to the previous symlink if it doesn't appear within 60 s;
   - re-enables the alarm actions.
3. **Rollback:** `deploy.sh <previous-sha>`. It uses the same path.

Containers are not used: a single static-ish binary with a symlink swap is simpler than ECR plus a container runtime on one host.

### Journals and logs

- **Journals:** a systemd timer runs at 00:20 UTC. It gzips the previous UTC day's `*.jsonl`, copies them to `s3://<journals>/<env>/<date>/`, compares sizes, and then deletes local files older than 7 days. The S3 lifecycle moves objects to Glacier Instant Retrieval after 30 days and keeps them indefinitely, because these are the only source for replaying incidents and for in-play calibration ([leg study](strat-improvements/in-play-leg-study-2026-10-28/in-play-leg-study-2026-10-28.md)).
- **Logs:** the CloudWatch agent tails journald for `pmbot.service` and sends it to the log group `/pmbot/<env>` with 30-day retention. Switching `tracing-subscriber` to JSON output (`.json()` behind a `--log-format` flag) makes metric filters and Logs Insights queries much easier.
- **Latency metrics:** the 60 s metrics line goes into CloudWatch through Embedded Metric Format (a small change in the metrics task, which already builds a JSON summary per stage), or through a log metric filter as a stopgap. Publish p50/p99 for `book_lag`, `ws_rtt_*`, `queue`, `rest_create`, `input_to_ack`.

## Monitoring and alerting

All alarms go to one SNS topic, with email plus SMS for the critical ones.

| Alarm | Source | Severity |
| --- | --- | --- |
| Process not running or unit failed | Heartbeat metric missing for 3 min (EMF, or a metric filter on the 10 s status line) | Critical, and triggers the dead-man switch |
| Instance status check failed | EC2 | Critical, with auto-recover |
| `cancel-all failed` / `cancel-all timed out` / watchdog fired | Log metric filter | Critical |
| `401` on either socket | Log metric filter | Critical (key revoked or clock wrong) |
| `risk state: can't write` or an unreadable risk state file at start | Log metric filter | Critical (a restart would lose the loss limits, or the bot won't start) |
| Kill switch tripped (daily, market, account loss) | Log metric filter on the risk `Alert` lines | High |
| Feed down for more than 60 s | Log metric filter on `reconnecting` / `silent` | High |
| Journal `dropped` > 0 | Log metric filter | Medium |
| Clock offset > 5 ms | CloudWatch agent `procstat` / custom metric from `chronyc tracking` | Medium |
| Disk > 70% | CloudWatch agent | Medium |
| `book_lag` p99 or `input_to_ack` p99 above baseline × 3 | EMF metrics | Medium |
| AWS spend over budget | AWS Budgets | Low |

**Dead-man switch.** `pmus.deadman.lambda_handler` (already written) runs the same cancel-all-and-verify as the shutdown hook, three rounds, and raises if it can't confirm that no orders are left, so the invocation counts as failed. It reads the key from SSM when `POLYMARKET_KEY_ID_PARAM` and `POLYMARKET_SECRET_KEY_PARAM` name the parameters. The heartbeat alarm triggers it (via SNS), and because it runs in Lambda it calls the venue from a different IP. It covers the case where the box or the process dies with orders resting until GTD. It must not fire while the process is merely stopped for a deploy, so `deploy.sh` disables the alarm actions for the duration of the deploy. A failed invocation should page. It has to be tested in Phase 3 before live sizes are raised.

## Security

- Use IMDSv2 only, encrypted EBS, a bucket policy that blocks public access, and SSE-S3 on both buckets.
- Least-privilege roles: the instance can read its parameters, put to the journals prefix, read the artifacts prefix, and write logs and metrics. CI can put to the artifacts prefix. The Lambda can read the parameters. Nothing has `*`.
- CloudTrail is on (the default trail is enough). Give the root account MFA, and let the operator use SSO or an MFA'd IAM user.
- **Key rotation runbook:** create a new key on polymarket.us, `aws ssm put-parameter --overwrite`, restart during a quiet window, then revoke the old key. The Lambda reads SSM on each invocation, so it picks the new key up without a redeploy.
- The `.env` file never leaves the laptop, and the laptop copy is not used for the account the cloud box trades on.

## Infrastructure as code

Use Terraform in `infra/`, with one root module and a variables file per environment (`paper`, `live`). Live and paper can use separate accounts or the same account with separate parameter paths. If both run at once, they need separate API keys and separate Polymarket accounts, because of the one-instance-per-account rule. State is kept in an S3 backend with a lock table.

Resources: VPC, subnet, IGW, route table, SG, EIP; EC2 instance, its role and profile, user data (installs the CloudWatch agent, creates the `pmbot` user, the dirs, the systemd units and the timers); two S3 buckets with lifecycle rules; SSM parameters (created empty and filled by hand); the log group; metric filters, alarms and the SNS topic; the dead-man Lambda and its role; the GitHub OIDC provider and CI role; a budget.

## Code changes

Done:

| Change | Where |
| --- | --- |
| SIGTERM and SIGHUP run the same cancel-all shutdown as Ctrl-C | `main.rs::stop_signal` |
| Risk state (day's PnL, daily kill, market losses, pauses) persisted across restarts; `--risk-state`, default `<run-dir>/risk-state-<mode>.json` | `risk_file.rs`, [risk](../risk.md#restarts) |
| Core thread raises its own priority (nice −10 with `CAP_SYS_NICE`) | `priority.rs` |
| Dead-man Lambda handler, reading the key from SSM | `pmus/deadman.py` |

Still to do:

| Change | Why | Size |
| --- | --- | --- |
| A single, stable readiness line (e.g. `feeds ready`) once the markets socket is subscribed and, in live, the private snapshot has been applied. Today the two sockets log separately (`markets socket conn=… subscribed`, `private socket conn=… ready`) | Deploy health check | Small, **required before deploys are automated** |
| A `status --json` admin command, or machine-readable `status` output, that says whether any market is in-play | `deploy.sh` must not stop the bot mid-match | Small, **required before deploys are automated** |
| Emit EMF for the 60 s metrics window and a heartbeat | Alarms (including the dead-man trigger) and dashboards | Small; a metric filter on the status line works as a stopgap |
| `--log-format json` | CloudWatch filters and Insights | Small |
| Log the chrony/clock offset, or a skew estimate from the venue's timestamps, in the metrics line | Explains `book_lag` jumps and 401s without a shell | Small, optional |

## Rollout

| Phase | What runs | Exit criteria |
| --- | --- | --- |
| 0. Checks | Throwaway instances, `record` | [Above](#phase-0-checks-before-building-anything) |
| 1. Infra and record | Terraform applied, `--mode record` 24/7 | 7 days without manual intervention; journals reach S3 every day; alarms tested by killing the process and by revoking the network (SG) |
| 2. Paper | `--mode paper` with the intended live config | 7 days; deploy and rollback exercised twice; a restart mid-day keeps the day's PnL from the risk state file; no core stalls; `core_busy` p99 recorded as a baseline |
| 3. Live, minimal | `--mode live` with the [latency-probe](../latency-probe.md) limits, then the default config at `base_size = 1` | Dead-man switch tested live: stop the instance with a resting order and confirm the Lambda cancels it within 3 min. `systemctl stop` shutdown tested. The `input_to_ack` baseline from AWS recorded in [connectivity §9](../polymarket-docs/polymarket-connectivity.md#9-latency) |
| 4. Live, normal | Normal risk limits | Ongoing: weekly review of journals against the backtest |

## Rough monthly cost (us-east-1, on demand)

| Item | $ / month |
| --- | --- |
| `c7g.large` | ~53 |
| 40 GiB gp3 | ~3 |
| Public IPv4 (EIP) | ~4 |
| CloudWatch Logs (~1–3 GB ingest), metrics, alarms | ~5–10 |
| S3 journals (grows ~5 GB a month compressed) and artifacts | < 1 |
| Lambda, SSM, SNS, data transfer out (small) | < 2 |
| **Total** | **~65–75** |

A 1-year Savings Plan cuts the instance cost by about 30%. Data transfer out is small because subscriptions and orders are tiny, and inbound traffic is free.

## Open questions

- Do the venue's terms allow running from a cloud provider, and does the server's location matter for a KYC'd retail account? (Phase 0, blocking.)
- Which AWS region hosts the venue origin behind Cloudflare? Measure it, and also ask Polymarket support.
- Does a Cloudflare location change (for example after an EIP change) reset or split the per-IP rate-limit budget?
- Should `paper` keep running beside `live` for comparison? That needs a second account and API key, because of the one-instance rule.
- Is SMS paging enough, or is PagerDuty/Opsgenie wanted for the critical alarms?
- Should the risk state file also be copied to S3, so that losing the volume doesn't lose the day's limits?
