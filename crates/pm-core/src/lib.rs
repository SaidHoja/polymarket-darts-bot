//! Trading core: book and game state, fair value, strategy, risk, order
//! management and the tick loop. No I/O and no clocks, so live and backtest
//! drive the same code.

pub mod config;
pub mod core;
pub mod game;
pub mod market_data;
pub mod orders;
pub mod portfolio;
pub mod risk;
pub mod snapshot;
pub mod strategy;

pub use crate::config::Config;
pub use crate::core::{Clock, Core, Derived, TickOutput};
pub use crate::portfolio::FillLog;
pub use crate::risk::RiskState;
pub use crate::snapshot::{DepthLevel, MarketDetail, MarketRow, OrderRow, Snapshot, TradeRow};
pub use crate::strategy::{Strategy, StrategyConfig};
