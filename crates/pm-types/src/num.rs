//! Fixed-point prices and quantities in units of 10⁻⁴.
//!
//! The venue sends decimals with at most 4 places as strings or JSON numbers.
//! Both are parsed from their text, so 0.01 stays exactly 100 units. Extra
//! decimal places are accepted only if they are zeros. Arithmetic saturates at the
//! `i64` bounds instead of panicking or wrapping; real values are far inside them.

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

pub const SCALE: i64 = 10_000;
const DECIMALS: usize = 4;
/// The map key `serde_json`'s `arbitrary_precision` feature uses for a number's text.
const SERDE_JSON_NUMBER: &str = "$serde_json::private::Number";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not a 4-decimal fixed-point value: {:?}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse a decimal string into 10⁻⁴ units.
pub fn parse_fixed(text: &str) -> Result<i64, ParseError> {
    let err = || ParseError(text.to_string());
    let s = text.trim();
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int_part, frac_part) = s.split_once('.').unwrap_or((s, ""));
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(err());
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(err());
    }
    if frac_part.len() > DECIMALS && frac_part[DECIMALS..].bytes().any(|b| b != b'0') {
        return Err(err());
    }
    // The magnitude is built wider than i64 so that `i64::MIN` units, whose magnitude is one
    // more than `i64::MAX`, still parse back from their own text.
    let int: i128 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| err())?
    };
    let mut frac: i128 = 0;
    for i in 0..DECIMALS {
        let digit = frac_part
            .as_bytes()
            .get(i)
            .map_or(0, |b| (b - b'0') as i128);
        frac = frac * 10 + digit;
    }
    let units = int
        .checked_mul(SCALE as i128)
        .and_then(|v| v.checked_add(frac))
        .ok_or_else(err)?;
    i64::try_from(if negative { -units } else { units }).map_err(|_| err())
}

fn format_fixed(units: i64, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let sign = if units < 0 { "-" } else { "" };
    let abs = units.unsigned_abs();
    write!(f, "{sign}{}.{:04}", abs / SCALE as u64, abs % SCALE as u64)
}

macro_rules! fixed_type {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub i64);

        impl $name {
            pub const ZERO: $name = $name(0);

            pub fn parse(text: &str) -> Result<Self, ParseError> {
                parse_fixed(text).map($name)
            }

            /// Rounds to the nearest unit. For values known to be finite and in range (model
            /// output, validated config); NaN becomes zero and out-of-range values saturate.
            pub fn from_f64(value: f64) -> Self {
                $name((value * SCALE as f64).round() as i64)
            }

            /// Rounds to the nearest unit; `None` for NaN, infinities and values outside the range.
            pub fn try_from_f64(value: f64) -> Option<Self> {
                let scaled = (value * SCALE as f64).round();
                // i64::MAX isn't exactly representable as f64; 2^63 is the first value out of range.
                (scaled.is_finite() && scaled >= i64::MIN as f64 && scaled < i64::MAX as f64)
                    .then(|| $name(scaled as i64))
            }

            pub fn as_f64(self) -> f64 {
                self.0 as f64 / SCALE as f64
            }

            pub fn is_zero(self) -> bool {
                self.0 == 0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_fixed(self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                format_fixed(self.0, f)
            }
        }

        impl std::ops::Add for $name {
            type Output = $name;
            fn add(self, rhs: $name) -> $name {
                $name(self.0.saturating_add(rhs.0))
            }
        }

        impl std::ops::Sub for $name {
            type Output = $name;
            fn sub(self, rhs: $name) -> $name {
                $name(self.0.saturating_sub(rhs.0))
            }
        }

        impl std::ops::Neg for $name {
            type Output = $name;
            fn neg(self) -> $name {
                $name(self.0.saturating_neg())
            }
        }

        impl std::ops::AddAssign for $name {
            fn add_assign(&mut self, rhs: $name) {
                self.0 = self.0.saturating_add(rhs.0);
            }
        }

        impl std::ops::SubAssign for $name {
            fn sub_assign(&mut self, rhs: $name) {
                self.0 = self.0.saturating_sub(rhs.0);
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct Visitor;
                impl<'de> de::Visitor<'de> for Visitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str("a decimal string or number")
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$name, E> {
                        $name::parse(v).map_err(E::custom)
                    }
                    /// Through the shortest text that round-trips (`0.1` → `"0.1"`), so a float with
                    /// more than 4 decimals is rejected like the same string would be, not rounded.
                    fn visit_f64<E: de::Error>(self, v: f64) -> Result<$name, E> {
                        $name::parse(&v.to_string()).map_err(E::custom)
                    }
                    /// `serde_json` with `arbitrary_precision` (enabled in this workspace) hands a JSON
                    /// number over as a one-entry map holding its exact text.
                    fn visit_map<A: de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<$name, A::Error> {
                        match map.next_key::<String>()? {
                            Some(key) if key == SERDE_JSON_NUMBER => {
                                let text: String = map.next_value()?;
                                $name::parse(&text).map_err(de::Error::custom)
                            }
                            _ => Err(de::Error::invalid_type(de::Unexpected::Map, &self)),
                        }
                    }
                    fn visit_i64<E: de::Error>(self, v: i64) -> Result<$name, E> {
                        v.checked_mul(SCALE)
                            .map($name)
                            .ok_or_else(|| E::custom(ParseError(v.to_string())))
                    }
                    fn visit_u64<E: de::Error>(self, v: u64) -> Result<$name, E> {
                        i64::try_from(v)
                            .ok()
                            .and_then(|v| v.checked_mul(SCALE))
                            .map($name)
                            .ok_or_else(|| E::custom(ParseError(v.to_string())))
                    }
                }
                deserializer.deserialize_any(Visitor)
            }
        }
    };
}

fixed_type!(Px);
fixed_type!(Qty);

impl Px {
    pub const ONE: Px = Px(SCALE);
    /// Lowest and highest valid YES prices.
    pub const MIN: Px = Px(100);
    pub const MAX: Px = Px(9_900);

    /// The same price seen from the NO side: 1 − p.
    pub fn complement(self) -> Px {
        Px(SCALE.saturating_sub(self.0))
    }

    pub fn is_on_tick(self, tick: Px) -> bool {
        tick.0 > 0 && self.0 % tick.0 == 0
    }

    /// A non-positive tick leaves the price unchanged, as `Qty::round_down` does.
    pub fn floor_to(self, tick: Px) -> Px {
        if tick.0 <= 0 {
            return self;
        }
        Px(self.0.div_euclid(tick.0) * tick.0)
    }

    pub fn ceil_to(self, tick: Px) -> Px {
        if tick.0 <= 0 {
            return self;
        }
        Px(-((-self.0).div_euclid(tick.0)) * tick.0)
    }

    pub fn clamp_to(self, lo: Px, hi: Px) -> Px {
        Px(self.0.clamp(lo.0, hi.0))
    }
}

impl Qty {
    /// Round down to a multiple of `min_qty`; the venue truncates silently otherwise.
    pub fn round_down(self, min_qty: Qty) -> Qty {
        if min_qty.0 <= 0 {
            return self;
        }
        Qty(self.0.div_euclid(min_qty.0) * min_qty.0)
    }

    pub fn abs(self) -> Qty {
        Qty(self.0.abs())
    }

    pub fn min(self, other: Qty) -> Qty {
        Qty(self.0.min(other.0))
    }

    pub fn max(self, other: Qty) -> Qty {
        Qty(self.0.max(other.0))
    }

    /// Dollar value of this many contracts at `px`.
    pub fn notional(self, px: Px) -> f64 {
        self.as_f64() * px.as_f64()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_venue_values() {
        assert_eq!(Px::parse("0.5550").unwrap(), Px(5550));
        assert_eq!(Px::parse("0.555").unwrap(), Px(5550));
        assert_eq!(Px::parse("1").unwrap(), Px(10_000));
        assert_eq!(Px::parse(".01").unwrap(), Px(100));
        assert_eq!(Qty::parse("300.0000").unwrap(), Qty(3_000_000));
        assert_eq!(Qty::parse("0.010000000").unwrap(), Qty(100));
        assert_eq!(Qty::parse("-2.5").unwrap(), Qty(-25_000));
        assert!(Px::parse("0.55551").is_err());
        assert!(Px::parse("abc").is_err());
        assert!(Px::parse("").is_err());
    }

    #[test]
    fn formats_and_rounds() {
        assert_eq!(Px(5550).to_string(), "0.5550");
        assert_eq!(Qty(-25_000).to_string(), "-2.5000");
        let tick = Px(100);
        assert_eq!(Px(5555).floor_to(tick), Px(5500));
        assert_eq!(Px(5555).ceil_to(tick), Px(5600));
        assert_eq!(Px(5500).ceil_to(tick), Px(5500));
        assert_eq!(Qty(1_550).round_down(Qty(100)), Qty(1_500));
        assert_eq!(Px(4200).complement(), Px(5800));
        assert_eq!(Px(5555).floor_to(Px::ZERO), Px(5555));
        assert_eq!(Px(5555).ceil_to(Px::ZERO), Px(5555));
    }

    /// With the workspace's `serde_json` features (`arbitrary_precision`), as the bot uses it.
    #[test]
    fn json_numbers_and_strings_deserialize_alike() {
        let px = |s: &str| serde_json::from_str::<Px>(s);
        assert_eq!(px("0.42").unwrap(), Px(4200));
        assert_eq!(px("\"0.42\"").unwrap(), Px(4200));
        assert_eq!(px("1").unwrap(), Px(10_000));
        assert_eq!(px("0.0100000").unwrap(), Px(100));
        assert_eq!(serde_json::from_str::<Qty>("-2.5").unwrap(), Qty(-25_000));
        assert!(px("0.42001").is_err());
        assert!(px("\"0.42001\"").is_err());
        assert!(px("1e400").is_err());
        assert!(px("{\"a\":1}").is_err());
        // Inside a struct too, and back out as a string.
        let v: Vec<Qty> = serde_json::from_str("[0.01, \"3\"]").unwrap();
        assert_eq!(v, vec![Qty(100), Qty(30_000)]);
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"["0.0100","3.0000"]"#);
    }

    #[test]
    fn floats_from_other_formats_are_exact_or_rejected() {
        use serde::de::value::{Error, F64Deserializer};
        let from_f64 = |v: f64| Px::deserialize(F64Deserializer::<Error>::new(v));
        assert_eq!(from_f64(0.1).unwrap(), Px(1000));
        assert_eq!(from_f64(0.005).unwrap(), Px(50));
        assert!(from_f64(0.1 + 0.2).is_err());
        assert!(from_f64(f64::NAN).is_err());
        assert!(from_f64(f64::INFINITY).is_err());
    }

    #[test]
    fn whole_stored_range_round_trips_through_text() {
        for v in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX] {
            assert_eq!(Qty::parse(&Qty(v).to_string()), Ok(Qty(v)), "{v}");
            assert_eq!(
                serde_json::from_str::<Qty>(&serde_json::to_string(&Qty(v)).unwrap()).unwrap(),
                Qty(v)
            );
        }
        assert!(Qty::parse("-922337203685477.5809").is_err());
        assert!(Qty::parse("922337203685477.5808").is_err());
    }

    #[test]
    fn checked_float_conversion_and_saturating_arithmetic() {
        assert_eq!(Qty::try_from_f64(2.5), Some(Qty(25_000)));
        assert_eq!(Qty::try_from_f64(f64::NAN), None);
        assert_eq!(Qty::try_from_f64(f64::INFINITY), None);
        assert_eq!(Qty::try_from_f64(1e15), None);
        assert_eq!(Qty::try_from_f64(-1e15), None);
        assert_eq!(Qty(i64::MAX) + Qty(1), Qty(i64::MAX));
        assert_eq!(Qty(i64::MIN) - Qty(1), Qty(i64::MIN));
        assert_eq!(-Qty(i64::MIN), Qty(i64::MAX));
        let mut q = Qty(i64::MAX);
        q += Qty(5);
        assert_eq!(q, Qty(i64::MAX));
        assert_eq!(Px(i64::MIN).complement(), Px(i64::MAX));
    }

    #[test]
    fn integer_deserialize_rejects_overflow() {
        use serde::de::value::{Error, I64Deserializer, U64Deserializer};
        let from_i64 = |v: i64| Px::deserialize(I64Deserializer::<Error>::new(v));
        let from_u64 = |v: u64| Px::deserialize(U64Deserializer::<Error>::new(v));
        assert_eq!(from_i64(3).unwrap(), Px(30_000));
        assert_eq!(from_u64(3).unwrap(), Px(30_000));
        assert!(from_i64(i64::MAX / SCALE + 1).is_err());
        assert!(from_i64(i64::MIN).is_err());
        assert!(from_u64(u64::MAX).is_err());
    }
}
