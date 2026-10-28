//! Replay files written by `data-analysis/export_backtest.py`: `markets.json` plus
//! one CSV of FIX rows per market, already in replay order.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use pm_types::time::parse_rfc3339_ns;
use pm_types::{MarketParams, Px, Qty, Side, TeamOrdering};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub source_sha256: String,
    pub git_revision: Option<String>,
    pub markets: Vec<MarketMeta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MarketMeta {
    pub slug: String,
    pub title: String,
    pub event_id: String,
    pub event_slug: String,
    pub league: String,
    pub sports_market_type: String,
    pub tick: String,
    pub min_qty: String,
    pub fee_coefficient: Option<f64>,
    pub game_start: Option<String>,
    pub yes_name: Option<String>,
    pub yes_ordering: Option<String>,
    /// 1 / 0 / 0.5, or none if the market never settled in the export.
    pub settlement: Option<f64>,
    pub rows: u64,
    pub trades: u64,
}

impl MarketMeta {
    pub fn game_start_ns(&self) -> Option<i64> {
        self.game_start.as_deref().and_then(parse_rfc3339_ns)
    }

    pub fn params(&self, fee_coefficient: f64) -> MarketParams {
        MarketParams {
            slug: Arc::from(self.slug.as_str()),
            title: self.title.clone(),
            event_id: self.event_id.clone(),
            event_slug: self.event_slug.clone(),
            league: self.league.clone(),
            sports_market_type: self.sports_market_type.clone(),
            tick: Px::parse(&self.tick).unwrap_or(Px(100)),
            min_qty: Qty::parse(&self.min_qty).unwrap_or(Qty(100)),
            fee_coefficient,
            game_start_ns: self.game_start_ns(),
            yes_name: self.yes_name.clone(),
            no_name: None,
            yes_ordering: match self.yes_ordering.as_deref() {
                Some("home") => Some(TeamOrdering::Home),
                Some("away") => Some(TeamOrdering::Away),
                _ => None,
            },
            is_futures: false,
        }
    }
}

pub fn load_manifest(dir: &Path) -> Result<Manifest, String> {
    let path = dir.join("markets.json");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e} (run data-analysis/export_backtest.py first)",
            path.display()
        )
    })?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Snapshot start marker.
    Marker,
    /// Snapshot entry.
    Snap,
    Add,
    Change,
    Delete,
    Trade,
}

/// Bid = Buy, Offer = Sell, in YES terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Row {
    pub rx_ns: i64,
    pub ev_ns: i64,
    pub seq: i64,
    pub kind: Kind,
    pub side: Side,
    pub px: Px,
    pub qty: Qty,
    /// Interned order id (book rows) or trade id (trades).
    pub id: u32,
    /// Trades: the taker's side, if the feed said.
    pub aggressor: Side,
}

/// Rows that arrived as one FIX message: apply them together.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Message {
    pub start: usize,
    pub end: usize,
    pub ev_ns: i64,
    pub rx_ns: i64,
    pub snapshot: bool,
}

#[derive(Default)]
pub struct Interner {
    ids: HashMap<String, u32>,
}

impl Interner {
    pub fn get(&mut self, s: &str) -> u32 {
        let n = self.ids.len() as u32;
        *self.ids.entry(s.to_string()).or_insert(n)
    }
}

pub fn parse_rows(text: &str) -> Result<Vec<Row>, String> {
    let mut ids = Interner::default();
    let mut rows = Vec::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() != 10 {
            return Err(format!("line {}: {} fields", n + 1, f.len()));
        }
        let num = |s: &str| s.parse::<i64>().map_err(|e| format!("line {}: {e}", n + 1));
        let kind = match f[4] {
            "M" => Kind::Marker,
            "S" => Kind::Snap,
            "A" => Kind::Add,
            "U" => Kind::Change,
            "D" => Kind::Delete,
            "T" => Kind::Trade,
            other => return Err(format!("line {}: kind {other:?}", n + 1)),
        };
        let side = match f[5] {
            "b" => Side::Buy,
            "o" => Side::Sell,
            _ => Side::Unknown,
        };
        let aggressor = match f[9] {
            "b" => Side::Buy,
            "s" => Side::Sell,
            _ => Side::Unknown,
        };
        let px = if f[6].is_empty() {
            Px::ZERO
        } else {
            Px::parse(f[6]).map_err(|e| format!("line {}: price {e:?}", n + 1))?
        };
        let qty = if f[7].is_empty() {
            Qty::ZERO
        } else {
            Qty::parse(f[7]).map_err(|e| format!("line {}: size {e:?}", n + 1))?
        };
        rows.push(Row {
            rx_ns: num(f[0])?,
            ev_ns: num(f[1])?,
            seq: num(f[2])?,
            kind,
            side,
            px,
            qty,
            id: ids.get(f[8]),
            aggressor,
        });
    }
    Ok(rows)
}

pub fn load_rows(dir: &Path, slug: &str) -> Result<Vec<Row>, String> {
    let path = dir.join(format!("{slug}.csv"));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_rows(&text)
}

/// Group consecutive rows with the same receive time and sequence number. A
/// snapshot message starts with its marker; its trades are a recap of old prints.
pub fn messages(rows: &[Row]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match out.last_mut() {
            Some(m)
                if rows[m.start].rx_ns == r.rx_ns
                    && rows[m.start].seq == r.seq
                    && r.kind != Kind::Marker =>
            {
                m.end = i + 1
            }
            _ => out.push(Message {
                start: i,
                end: i + 1,
                ev_ns: r.ev_ns,
                rx_ns: r.rx_ns,
                snapshot: r.kind == Kind::Marker,
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "rx_ns,ev_ns,msg_seq,entry_index,kind,side,price,size,id,aggressor
100,90,7,-1,M,,,,,
100,90,7,0,S,b,0.6,300,A1,
100,90,7,1,S,o,0.65,10,A2,
200,195,8,0,U,b,0.6,250,A1,
200,195,8,1,T,,0.6,50,T1,s
300,290,9,0,A,o,0.64,5,A3,
";

    #[test]
    fn parses_and_groups_messages() {
        let rows = parse_rows(CSV).unwrap();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[1].px, Px(6000));
        assert_eq!(rows[1].qty, Qty(3_000_000));
        assert_eq!(rows[4].aggressor, Side::Sell);
        assert_eq!(rows[1].id, rows[3].id);
        let m = messages(&rows);
        assert_eq!(m.len(), 3);
        assert!(m[0].snapshot && !m[1].snapshot);
        assert_eq!((m[0].start, m[0].end, m[1].start, m[1].end), (0, 3, 3, 5));
    }
}
