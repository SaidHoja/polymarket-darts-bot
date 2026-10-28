//! Venue enums. Strings on the wire, typed here. Unknown values map to `Unknown`
//! so a new enum value never breaks decoding.

use serde::{Deserialize, Serialize};

macro_rules! wire_enum {
    ($name:ident { $($variant:ident => $wire:literal),* $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name { $($variant,)* Unknown }

        impl $name {
            pub fn from_wire(s: &str) -> Self {
                match s { $($wire => $name::$variant,)* _ => $name::Unknown }
            }

            pub fn wire(self) -> &'static str {
                match self { $($name::$variant => $wire,)* $name::Unknown => "UNKNOWN" }
            }
        }
    };
}

wire_enum!(Side {
    Buy => "ORDER_SIDE_BUY",
    Sell => "ORDER_SIDE_SELL",
});

wire_enum!(Intent {
    BuyLong => "ORDER_INTENT_BUY_LONG",
    SellLong => "ORDER_INTENT_SELL_LONG",
    BuyShort => "ORDER_INTENT_BUY_SHORT",
    SellShort => "ORDER_INTENT_SELL_SHORT",
});

wire_enum!(OrderState {
    PendingNew => "ORDER_STATE_PENDING_NEW",
    New => "ORDER_STATE_NEW",
    PartiallyFilled => "ORDER_STATE_PARTIALLY_FILLED",
    Filled => "ORDER_STATE_FILLED",
    PendingReplace => "ORDER_STATE_PENDING_REPLACE",
    PendingCancel => "ORDER_STATE_PENDING_CANCEL",
    PendingRisk => "ORDER_STATE_PENDING_RISK",
    Replaced => "ORDER_STATE_REPLACED",
    Canceled => "ORDER_STATE_CANCELED",
    Rejected => "ORDER_STATE_REJECTED",
    Expired => "ORDER_STATE_EXPIRED",
});

wire_enum!(ExecKind {
    New => "EXECUTION_TYPE_NEW",
    PartialFill => "EXECUTION_TYPE_PARTIAL_FILL",
    Fill => "EXECUTION_TYPE_FILL",
    Canceled => "EXECUTION_TYPE_CANCELED",
    Replace => "EXECUTION_TYPE_REPLACE",
    Rejected => "EXECUTION_TYPE_REJECTED",
    Expired => "EXECUTION_TYPE_EXPIRED",
    DoneForDay => "EXECUTION_TYPE_DONE_FOR_DAY",
});

wire_enum!(MarketState {
    Open => "MARKET_STATE_OPEN",
    Preopen => "MARKET_STATE_PREOPEN",
    Suspended => "MARKET_STATE_SUSPENDED",
    Halted => "MARKET_STATE_HALTED",
    Auction => "MARKET_STATE_MATCH_AND_CLOSE_AUCTION",
    Expired => "MARKET_STATE_EXPIRED",
    Terminated => "MARKET_STATE_TERMINATED",
});

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
            Side::Unknown => Side::Unknown,
        }
    }

    /// +1 for a YES buy, −1 for a YES sell.
    pub fn sign(self) -> i64 {
        match self {
            Side::Buy => 1,
            Side::Sell => -1,
            Side::Unknown => 0,
        }
    }
}

impl Intent {
    /// The YES-book side this intent trades on.
    pub fn side(self) -> Side {
        match self {
            Intent::BuyLong | Intent::SellShort => Side::Buy,
            Intent::SellLong | Intent::BuyShort => Side::Sell,
            Intent::Unknown => Side::Unknown,
        }
    }
}

impl OrderState {
    /// `Replaced` is not terminal: a modify keeps the order id, and the venue reports the
    /// still-working order as `REPLACED` (on the socket and over REST) until it fills or is cancelled.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Canceled | OrderState::Rejected | OrderState::Expired
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaced_orders_are_still_working() {
        assert!(!OrderState::Replaced.is_terminal());
        assert!(!OrderState::from_wire("ORDER_STATE_REPLACED").is_terminal());
        for s in [
            OrderState::Filled,
            OrderState::Canceled,
            OrderState::Rejected,
            OrderState::Expired,
        ] {
            assert!(s.is_terminal(), "{s:?}");
        }
    }
}

/// Reject classes: the core handles these, the raw reason and text are kept for the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RejectClass {
    /// Post-only would cross: the touch moved.
    BookMoved,
    /// `Global Rate Limit Exceeded`: the 5 s latency stopgap, not a rate limit.
    Transient,
    /// HTTP 429.
    RateLimited,
    /// Insufficient buying power.
    Funding,
    /// Exchange closed, halted, unknown market.
    MarketUnavailable,
    /// Off tick, off min qty, out of bounds, bad order type, HTTP 400.
    OurBug,
    /// HTTP 401: can't sign.
    Fatal,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Feed {
    Markets,
    Private,
    Game,
}

/// Time in force for orders we send: always good-till-date. Never GTC and never DAY (DAY
/// doesn't auto-cancel at the roll), and never IOC (the bot only makes markets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tif {
    /// Good till this Unix ns (sent with whole-second precision).
    Gtd(i64),
}
