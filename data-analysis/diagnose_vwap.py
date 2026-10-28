"""Prepare an isolated diagnostic copy of the backtester for own-order feedback analysis.

From the repo root:
    python3 data-analysis/diagnose_vwap.py
    cargo build --offline --release --manifest-path /tmp/pmbot-vwap-diagnosis/Cargo.toml -p pm-backtest

Use the copy's target/release/pm-backtest, --data pointing at the original export,
--config /tmp/pmbot-vwap-diagnosis/comparison.toml and --strategy mid,vwap.
PM_SYNC_OWN_BOOK=1 delivers an own action's private ack and public book together;
PM_VWAP_TOUCH_ONLY=1 disables depth-driven re-decisions but keeps VWAP as fair.
PM_DIAG_NO_TRACE=1 suppresses individual traces while retaining summary counts.
These are diagnostic interventions, not production execution policies.
The copy uses its own target directory by default, so the workspace binary is preserved.
"""

from pathlib import Path
import json, shutil
root=Path(__file__).resolve().parent.parent
out=Path('/tmp/pmbot-vwap-diagnosis')
out.mkdir(exist_ok=True)
for name in ['Cargo.toml','Cargo.lock']:
 shutil.copy2(root/name,out/name)
shutil.copytree(root/'crates',out/'crates',dirs_exist_ok=True)
shutil.copytree(root/'config',out/'config',dirs_exist_ok=True)
shutil.copytree(root/'tests',out/'tests',dirs_exist_ok=True)
comparison=json.loads((root/'data-analysis/vwap-strategy-results.json').read_text())['comparison_config_toml']
(out/'comparison.toml').write_text(comparison)
p=out/'crates/pm-core/src/strategy/spread.rs'
s=p.read_text().replace('        self.vwap_center\n','        self.vwap_center && std::env::var_os("PM_VWAP_TOUCH_ONLY").is_none()\n',1)
p.write_text(s)
p=out/'crates/pm-backtest/src/sim.rs'
s=p.read_text()
def replace(old,new):
 global s
 assert s.count(old)==1,(old,s.count(old))
 s=s.replace(old,new,1)
replace('    last_t: i64,','    diagnostic: BTreeMap<String, u64>,\n    last_history: i64,\n    own_action: bool,\n    last_input: String,\n    last_t: i64,')
replace('            last_t: t0,','            diagnostic: BTreeMap::new(),\n            last_history: t0,\n            own_action: false,\n            last_input: String::new(),\n            last_t: t0,')
replace('        let lag = self.lag(median_ms, self.p.md_lag_sigma);','        let lag = self.lag(median_ms, self.p.md_lag_sigma);\n        if self.own_action && std::env::var_os("PM_SYNC_OWN_BOOK").is_some() {\n            let at = (self.now + 63_000_000).max(self.md_last).max(self.priv_last);\n            self.md_last = at;\n            self.schedule(at, Item::Deliver(event));\n            return;\n        }')
replace('        let lag = self.order_lag();\n        let at = (self.now + lag).max(self.priv_last);','        let lag = self.order_lag();\n        if self.own_action && std::env::var_os("PM_SYNC_OWN_BOOK").is_some() {\n            let at = (self.now + 63_000_000).max(self.md_last).max(self.priv_last);\n            self.priv_last = at;\n            self.schedule(at, Item::Deliver(event));\n            return;\n        }\n        let at = (self.now + lag).max(self.priv_last);')
replace('        let out = self.core.tick(clock, events);','''        let input_kind = if events.iter().any(|e| matches!(e, Event::Book(_))) { "book" }
            else if events.iter().any(|e| matches!(e, Event::Execution(_))) { "execution" }
            else if events.iter().any(|e| matches!(e, Event::RestResult(_))) { "rest" }
            else if events.iter().any(|e| matches!(e, Event::Trade(_))) { "trade" }
            else { "other" };
        self.last_input = input_kind.into();
        let input_details = if self.stats.order_requests < 80 && std::env::var_os("PM_DIAG_NO_TRACE").is_none() { format!("{events:?}") } else { String::new() };
        let out = self.core.tick(clock, events);
        if !out.commands.is_empty() && self.stats.order_requests < 80 && std::env::var_os("PM_DIAG_NO_TRACE").is_none() {
            let row = self.core.snapshot(clock).markets.into_iter().next();
            let book = self.core.market_detail(&self.slug, clock);
            eprintln!("TRACE {}", serde_json::json!({
                "ns": self.now, "since_history_ms": (self.now-self.last_history) as f64 / 1e6,
                "inputs": input_details, "fair":row.as_ref().and_then(|r|r.fair),
                "venue":book.as_ref().map(|b| (&b.bids,&b.offers)),
                "historical":format!("{:?}",self.book.l2(4)),
                "orders": self.core.om.active().map(|r|format!("{:?} {:?} px={} qty={} modify={:?}",r.side,r.state,r.px,r.leaves_qty,r.modify_to)).collect::<Vec<_>>(),
                "commands":out.commands.iter().map(|c|format!("{c:?}")).collect::<Vec<_>>(),
            }));
        }''')
replace('            Item::Arrive(a) => self.on_arrive(a),','            Item::Arrive(a) => { self.own_action = true; self.on_arrive(a); self.own_action = false; },')
replace('                self.stats.order_requests += 1;','''                self.stats.order_requests += 1;
                let kind = match &cmd {
                    Command::Place { .. } => "place",
                    Command::Cancel { .. } => "cancel",
                    Command::CancelAll { .. } => "cancel_all",
                    Command::Modify { order_id, px, qty, .. } => {
                        match self.core.om.active().find(|r|r.venue_id.as_ref()==Some(order_id)) {
                            Some(rec) if rec.px != *px => "modify_price",
                            Some(rec) if rec.leaves_qty != *qty => "modify_size",
                            _ => "modify_expiry",
                        }
                    },
                    _ => "other",
                };
                *self.diagnostic.entry(format!("kind/{kind}")).or_default() += 1;
                *self.diagnostic.entry(format!("priority/{:?}",cmd.priority())).or_default() += 1;
                *self.diagnostic.entry(format!("input/{}",self.last_input)).or_default() += 1;
                if self.now-self.last_history > SEC { *self.diagnostic.entry("no_history_1s".into()).or_default() += 1; }
                if self.now-self.last_history > 10*SEC { *self.diagnostic.entry("no_history_10s".into()).or_default() += 1; }''')
replace('    fn on_message(&mut self, mi: usize) {','    fn on_message(&mut self, mi: usize) {\n        self.last_history = self.now;')
replace('    fn finish(mut self) -> SimOutput {','    fn finish(mut self) -> SimOutput {\n        eprintln!("DIAGNOSTIC {}", serde_json::json!({"slug":self.slug,"requests":self.stats.order_requests,"counts":self.diagnostic}));')
p.write_text(s)
print(out)
