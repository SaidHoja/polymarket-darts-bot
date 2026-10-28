//! Backtester: replays the Parquet history through the same `pm-core`
//! the live bot runs, against a simulated exchange.

pub mod book;
pub mod data;
pub mod report;
pub mod sim;
