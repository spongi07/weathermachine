//! Exact fixed-point units.
//!
//! Weather Machine never uses binary floating point for money, prices, share
//! quantities or temperatures that feed a trading decision. All such values are
//! integers in a fixed unit:
//!
//! * [`TempC`]  — tenths of a degree Celsius (deci-Celsius).
//! * [`Price`]  — micro-units of collateral per share, `0..=1_000_000` (`1.0` = 1 000 000).
//! * [`Usd`]    — micro-units of USD collateral (6 decimals, matching on-chain USDC/pUSD).
//! * [`Shares`] — micro-units of outcome tokens (6 decimals, matching CTF token decimals).
//!
//! Integer arithmetic is exact, `Copy`, allocation free and fast on the hot path.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// Number of micro-units in one whole unit.
pub const MICROS_PER_UNIT: i64 = 1_000_000;

/// Errors produced when constructing or parsing units.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnitError {
    #[error("value {0} is outside the valid range")]
    OutOfRange(String),
    #[error("cannot parse decimal '{0}'")]
    Parse(String),
    #[error("arithmetic overflow")]
    Overflow,
}

/// Rounding mode for fixed-point division. Callers must choose explicitly:
/// amounts we pay are rounded up, amounts we receive are rounded down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    Down,
    Up,
    Nearest,
}

fn div_round(num: i128, den: i128, rounding: Rounding) -> i128 {
    debug_assert!(den > 0);
    let q = num.div_euclid(den);
    let r = num.rem_euclid(den);
    match rounding {
        Rounding::Down => q,
        Rounding::Up => {
            if r == 0 {
                q
            } else {
                q + 1
            }
        }
        Rounding::Nearest => {
            if r * 2 >= den {
                q + 1
            } else {
                q
            }
        }
    }
}

/// Parse a decimal string (e.g. `"0.953"`, `"-12.5"`, `"10"`) into micro-units.
/// More than six fractional digits are rounded half-up to the nearest micro-unit.
pub fn parse_decimal_micros(input: &str) -> Result<i64, UnitError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(UnitError::Parse(input.to_owned()));
    }
    let (negative, body) = match s.as_bytes()[0] {
        b'-' => (true, &s[1..]),
        b'+' => (false, &s[1..]),
        _ => (false, s),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(UnitError::Parse(input.to_owned()));
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(UnitError::Parse(input.to_owned()));
    }
    let int_value: i128 = if int_part.is_empty() {
        0
    } else {
        int_part
            .parse::<i128>()
            .map_err(|_| UnitError::Parse(input.to_owned()))?
    };
    let mut frac_micros: i128 = 0;
    let mut round_up = false;
    for (idx, b) in frac_part.bytes().enumerate() {
        let digit = i128::from(b - b'0');
        if idx < 6 {
            frac_micros = frac_micros * 10 + digit;
        } else if idx == 6 {
            round_up = digit >= 5;
        }
    }
    for _ in frac_part.len().min(6)..6 {
        frac_micros *= 10;
    }
    let mut total = int_value
        .checked_mul(i128::from(MICROS_PER_UNIT))
        .and_then(|v| v.checked_add(frac_micros))
        .ok_or(UnitError::Overflow)?;
    if round_up {
        total += 1;
    }
    if negative {
        total = -total;
    }
    i64::try_from(total).map_err(|_| UnitError::Overflow)
}

fn fmt_micros(f: &mut fmt::Formatter<'_>, micros: i64, min_decimals: usize) -> fmt::Result {
    let negative = micros < 0;
    let abs = micros.unsigned_abs();
    let int_part = abs / MICROS_PER_UNIT as u64;
    let frac = abs % MICROS_PER_UNIT as u64;
    let mut frac_str = format!("{frac:06}");
    while frac_str.len() > min_decimals && frac_str.ends_with('0') {
        frac_str.pop();
    }
    if negative {
        write!(f, "-")?;
    }
    if frac_str.is_empty() {
        write!(f, "{int_part}")
    } else {
        write!(f, "{int_part}.{frac_str}")
    }
}

// ---------------------------------------------------------------------------
// Temperature
// ---------------------------------------------------------------------------

/// Temperature in tenths of a degree Celsius.
///
/// METAR reports whole degrees (`18` → `TempC::from_whole(18)` = 180 tenths);
/// US T-groups and KNMI 10-minute data carry tenths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TempC(i32);

impl TempC {
    pub const fn from_tenths(tenths: i32) -> Self {
        Self(tenths)
    }

    pub const fn from_whole(celsius: i32) -> Self {
        Self(celsius * 10)
    }

    pub const fn tenths(self) -> i32 {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / 10.0
    }

    /// Whole-degree value using the ICAO Annex 3 convention: values involving
    /// 0.5 °C are rounded up to the next higher whole degree (toward +∞).
    pub fn round_half_up_whole(self) -> i32 {
        (self.0 + 5).div_euclid(10)
    }

    /// Exact whole-degree value, or `None` if the value has a fractional part.
    pub fn exact_whole(self) -> Option<i32> {
        (self.0 % 10 == 0).then_some(self.0 / 10)
    }

    /// Difference `self - other` in tenths of a degree.
    pub fn diff_tenths(self, other: TempC) -> i32 {
        self.0 - other.0
    }

    /// Fahrenheit value for display purposes only (never for bucket decisions).
    pub fn to_fahrenheit_f64(self) -> f64 {
        self.as_f64() * 9.0 / 5.0 + 32.0
    }
}

impl fmt::Display for TempC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        write!(f, "{sign}{}.{}°C", abs / 10, abs % 10)
    }
}

// ---------------------------------------------------------------------------
// Price
// ---------------------------------------------------------------------------

/// Price of one outcome share in collateral units, `0.0..=1.0`, stored in micro-units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Price(u32);

impl Price {
    pub const ZERO: Price = Price(0);
    pub const ONE: Price = Price(1_000_000);

    pub fn from_micros(micros: u32) -> Result<Self, UnitError> {
        if micros > 1_000_000 {
            return Err(UnitError::OutOfRange(format!("price micros {micros}")));
        }
        Ok(Self(micros))
    }

    /// Infallible constructor for constants: values above 1.0 saturate to 1.0.
    pub const fn saturating_from_micros(micros: u32) -> Self {
        if micros > 1_000_000 {
            Self(1_000_000)
        } else {
            Self(micros)
        }
    }

    /// Parse an exact decimal price such as `"0.953"`.
    pub fn parse(s: &str) -> Result<Self, UnitError> {
        let micros = parse_decimal_micros(s)?;
        let micros = u32::try_from(micros).map_err(|_| UnitError::OutOfRange(s.to_owned()))?;
        Self::from_micros(micros)
    }

    /// Convert a float price (e.g. from a JSON number) rounding to the nearest micro-unit.
    pub fn from_f64(value: f64) -> Result<Self, UnitError> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(UnitError::OutOfRange(value.to_string()));
        }
        Self::from_micros((value * 1_000_000.0).round() as u32)
    }

    pub const fn micros(self) -> u32 {
        self.0
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.0) / 1_000_000.0
    }

    /// `1 - self`: the price of the complementary outcome in a binary market.
    pub const fn complement(self) -> Price {
        Price(1_000_000 - self.0)
    }

    pub fn is_on_tick(self, tick: Price) -> bool {
        tick.0 != 0 && self.0.is_multiple_of(tick.0)
    }

    pub fn floor_to_tick(self, tick: Price) -> Price {
        if tick.0 == 0 {
            return self;
        }
        Price(self.0 - self.0 % tick.0)
    }

    pub fn ceil_to_tick(self, tick: Price) -> Price {
        if tick.0 == 0 || self.0.is_multiple_of(tick.0) {
            return self;
        }
        Price((self.0 - self.0 % tick.0 + tick.0).min(1_000_000))
    }

    pub fn saturating_add(self, other: Price) -> Price {
        Price((self.0 + other.0).min(1_000_000))
    }

    pub fn saturating_sub(self, other: Price) -> Price {
        Price(self.0.saturating_sub(other.0))
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_micros(f, i64::from(self.0), 2)
    }
}

// ---------------------------------------------------------------------------
// USD and Shares
// ---------------------------------------------------------------------------

macro_rules! signed_micro_unit {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Default,
            Serialize,
            Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            pub const ZERO: $name = $name(0);

            pub const fn from_micros(micros: i64) -> Self {
                Self(micros)
            }

            pub const fn from_whole(units: i64) -> Self {
                Self(units * MICROS_PER_UNIT)
            }

            pub fn parse(s: &str) -> Result<Self, UnitError> {
                parse_decimal_micros(s).map(Self)
            }

            pub const fn micros(self) -> i64 {
                self.0
            }

            pub fn as_f64(self) -> f64 {
                self.0 as f64 / MICROS_PER_UNIT as f64
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            pub const fn is_negative(self) -> bool {
                self.0 < 0
            }

            pub const fn abs(self) -> Self {
                Self(self.0.abs())
            }

            pub fn checked_add(self, other: Self) -> Option<Self> {
                self.0.checked_add(other.0).map(Self)
            }

            pub fn max(self, other: Self) -> Self {
                if self >= other { self } else { other }
            }

            pub fn min(self, other: Self) -> Self {
                if self <= other { self } else { other }
            }
        }

        impl Add for $name {
            type Output = $name;
            fn add(self, rhs: $name) -> $name {
                $name(self.0 + rhs.0)
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: $name) {
                self.0 += rhs.0;
            }
        }

        impl Sub for $name {
            type Output = $name;
            fn sub(self, rhs: $name) -> $name {
                $name(self.0 - rhs.0)
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: $name) {
                self.0 -= rhs.0;
            }
        }

        impl Neg for $name {
            type Output = $name;
            fn neg(self) -> $name {
                $name(-self.0)
            }
        }

        impl std::iter::Sum for $name {
            fn sum<I: Iterator<Item = $name>>(iter: I) -> $name {
                iter.fold($name::ZERO, |a, b| a + b)
            }
        }
    };
}

signed_micro_unit!(Usd, "USD collateral amount in micro-units (6 decimals).");
signed_micro_unit!(
    Shares,
    "Outcome-token quantity in micro-units (6 decimals)."
);

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "$")?;
        fmt_micros(f, self.0, 2)
    }
}

impl fmt::Display for Shares {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_micros(f, self.0, 0)
    }
}

/// Cost (or proceeds) of `shares` at `price`: `price × shares`.
pub fn notional(price: Price, shares: Shares, rounding: Rounding) -> Usd {
    let num = i128::from(price.micros()) * i128::from(shares.micros());
    let v = div_round(num, i128::from(MICROS_PER_UNIT), rounding);
    Usd(i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX }))
}

/// Number of shares purchasable with `usd` at `price`: `usd / price`.
/// Returns zero shares for a zero price (degenerate book).
pub fn shares_for_notional(usd: Usd, price: Price, rounding: Rounding) -> Shares {
    if price.micros() == 0 {
        return Shares::ZERO;
    }
    let num = i128::from(usd.micros()) * i128::from(MICROS_PER_UNIT);
    let v = div_round(num, i128::from(price.micros()), rounding);
    Shares(i64::try_from(v).unwrap_or(if v < 0 { i64::MIN } else { i64::MAX }))
}

/// Round a share quantity to a lot size (e.g. whole shares = 1_000_000 micros).
pub fn round_shares_to_lot(shares: Shares, lot: Shares, rounding: Rounding) -> Shares {
    if lot.micros() <= 0 {
        return shares;
    }
    let lots = div_round(
        i128::from(shares.micros()),
        i128::from(lot.micros()),
        rounding,
    );
    Shares((lots * i128::from(lot.micros())) as i64)
}

/// A probability in `[0, 1]`. Models work in `f64`; decisions compare against
/// exact prices via [`Probability::as_price_floor`] to stay conservative.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Probability(f64);

impl Probability {
    pub const ZERO: Probability = Probability(0.0);
    pub const ONE: Probability = Probability(1.0);

    /// Clamp into `[0, 1]`; NaN maps to 0 (fail closed: no confidence).
    pub fn new(p: f64) -> Self {
        if p.is_nan() {
            Probability(0.0)
        } else {
            Probability(p.clamp(0.0, 1.0))
        }
    }

    pub const fn value(self) -> f64 {
        self.0
    }

    pub fn complement(self) -> Probability {
        Probability::new(1.0 - self.0)
    }

    /// Probability expressed as a price, rounded down to the micro-unit.
    pub fn as_price_floor(self) -> Price {
        Price((self.0 * 1_000_000.0).floor().clamp(0.0, 1_000_000.0) as u32)
    }
}

impl fmt::Display for Probability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.4}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn parse_decimal_examples() {
        assert_eq!(parse_decimal_micros("0.953").unwrap(), 953_000);
        assert_eq!(parse_decimal_micros("10").unwrap(), 10_000_000);
        assert_eq!(parse_decimal_micros("10.00").unwrap(), 10_000_000);
        assert_eq!(parse_decimal_micros("-12.5").unwrap(), -12_500_000);
        assert_eq!(parse_decimal_micros(".5").unwrap(), 500_000);
        assert_eq!(parse_decimal_micros("0.0000005").unwrap(), 1);
        assert_eq!(parse_decimal_micros("0.0000004").unwrap(), 0);
        assert!(parse_decimal_micros("").is_err());
        assert!(parse_decimal_micros("abc").is_err());
        assert!(parse_decimal_micros("1.2.3").is_err());
        assert!(parse_decimal_micros("-").is_err());
        assert!(parse_decimal_micros(".").is_err());
    }

    #[test]
    fn price_parse_and_display() {
        let p = Price::parse("0.953").unwrap();
        assert_eq!(p.micros(), 953_000);
        assert_eq!(p.to_string(), "0.953");
        assert_eq!(Price::parse("0.9").unwrap().to_string(), "0.90");
        assert_eq!(p.complement().micros(), 47_000);
        assert!(Price::parse("1.01").is_err());
        assert!(Price::parse("-0.1").is_err());
        assert_eq!(Price::ONE.to_string(), "1.00");
    }

    #[test]
    fn price_ticks() {
        let tick = Price::parse("0.01").unwrap();
        let p = Price::parse("0.953").unwrap();
        assert!(!p.is_on_tick(tick));
        assert_eq!(p.floor_to_tick(tick), Price::parse("0.95").unwrap());
        assert_eq!(p.ceil_to_tick(tick), Price::parse("0.96").unwrap());
        let fine = Price::parse("0.001").unwrap();
        assert!(p.is_on_tick(fine));
        assert_eq!(
            Price::parse("0.999").unwrap().ceil_to_tick(tick),
            Price::ONE
        );
    }

    #[test]
    fn temperature_rounding_follows_icao_half_up() {
        assert_eq!(TempC::from_tenths(175).round_half_up_whole(), 18);
        assert_eq!(TempC::from_tenths(174).round_half_up_whole(), 17);
        assert_eq!(TempC::from_tenths(-15).round_half_up_whole(), -1);
        assert_eq!(TempC::from_tenths(-16).round_half_up_whole(), -2);
        assert_eq!(TempC::from_tenths(-5).round_half_up_whole(), 0);
        assert_eq!(TempC::from_whole(18).exact_whole(), Some(18));
        assert_eq!(TempC::from_tenths(178).exact_whole(), None);
        assert_eq!(TempC::from_tenths(-3).to_string(), "-0.3°C");
        assert_eq!(TempC::from_whole(18).to_string(), "18.0°C");
    }

    #[test]
    fn notional_and_shares() {
        let price = Price::parse("0.95").unwrap();
        let usd = Usd::parse("10.00").unwrap();
        let shares = shares_for_notional(usd, price, Rounding::Down);
        assert_eq!(shares.micros(), 10_526_315);
        let cost = notional(price, shares, Rounding::Up);
        assert!(cost <= usd);
        assert_eq!(Usd::parse("10").unwrap().to_string(), "$10.00");
        assert_eq!(Usd::from_micros(1_234_567).to_string(), "$1.234567");
        assert_eq!(Shares::from_whole(5).to_string(), "5");
        assert_eq!(
            shares_for_notional(usd, Price::ZERO, Rounding::Down),
            Shares::ZERO
        );
    }

    #[test]
    fn lot_rounding() {
        let lot = Shares::from_whole(1);
        assert_eq!(
            round_shares_to_lot(Shares::from_micros(10_526_315), lot, Rounding::Down),
            Shares::from_whole(10)
        );
        assert_eq!(
            round_shares_to_lot(Shares::from_micros(10_526_315), lot, Rounding::Up),
            Shares::from_whole(11)
        );
    }

    #[test]
    fn probability_is_fail_closed() {
        assert_eq!(Probability::new(f64::NAN).value(), 0.0);
        assert_eq!(Probability::new(1.5).value(), 1.0);
        assert_eq!(Probability::new(-0.5).value(), 0.0);
        assert_eq!(Probability::new(0.9531).as_price_floor().micros(), 953_100);
    }

    proptest! {
        #[test]
        fn decimal_roundtrip(micros in -10_000_000_000i64..10_000_000_000i64) {
            let text = Usd::from_micros(micros).to_string();
            let parsed = Usd::parse(text.trim_start_matches('$').trim_start_matches('-')).unwrap();
            prop_assert_eq!(parsed.micros(), micros.abs());
        }

        #[test]
        fn cost_never_exceeds_budget(usd_micros in 1i64..1_000_000_000, price_micros in 1u32..=1_000_000) {
            let price = Price::from_micros(price_micros).unwrap();
            let usd = Usd::from_micros(usd_micros);
            let shares = shares_for_notional(usd, price, Rounding::Down);
            let cost = notional(price, shares, Rounding::Up);
            prop_assert!(cost <= usd, "cost {} > budget {}", cost, usd);
        }

        #[test]
        fn complement_is_involution(m in 0u32..=1_000_000) {
            let p = Price::from_micros(m).unwrap();
            prop_assert_eq!(p.complement().complement(), p);
        }

        #[test]
        fn half_up_rounding_matches_float_reference(t in -600i32..600) {
            let expected = ((f64::from(t) / 10.0) + 0.5).floor() as i32;
            prop_assert_eq!(TempC::from_tenths(t).round_half_up_whole(), expected);
        }
    }
}

/// Serde helpers so configuration can write money and prices as exact
/// decimal strings (`position_size_usd = "10.00"`). Numbers are also accepted.
pub mod decimal_serde {
    use super::{Price, Shares, Usd, parse_decimal_micros};
    use serde::{Deserialize, Deserializer, Serializer};

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Int(i64),
        Float(f64),
    }

    fn micros<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        match Raw::deserialize(d)? {
            Raw::Text(s) => parse_decimal_micros(&s).map_err(serde::de::Error::custom),
            Raw::Int(i) => i
                .checked_mul(1_000_000)
                .ok_or_else(|| serde::de::Error::custom("overflow")),
            Raw::Float(f) => {
                if f.is_finite() {
                    parse_decimal_micros(&format!("{f}")).map_err(serde::de::Error::custom)
                } else {
                    Err(serde::de::Error::custom("non-finite number"))
                }
            }
        }
    }

    pub mod usd {
        use super::*;
        pub fn serialize<S: Serializer>(v: &Usd, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(v.to_string().trim_start_matches('$'))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Usd, D::Error> {
            micros(d).map(Usd::from_micros)
        }
    }

    pub mod opt_usd {
        use super::*;
        pub fn serialize<S: Serializer>(v: &Option<Usd>, s: S) -> Result<S::Ok, S::Error> {
            match v {
                Some(u) => s.serialize_str(u.to_string().trim_start_matches('$')),
                None => s.serialize_none(),
            }
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Usd>, D::Error> {
            #[derive(Deserialize)]
            struct W(#[serde(with = "super::usd")] Usd);
            Option::<W>::deserialize(d).map(|o| o.map(|w| w.0))
        }
    }

    pub mod price {
        use super::*;
        pub fn serialize<S: Serializer>(v: &Price, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&v.to_string())
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Price, D::Error> {
            let m = micros(d)?;
            let m = u32::try_from(m).map_err(|_| serde::de::Error::custom("price out of range"))?;
            Price::from_micros(m).map_err(serde::de::Error::custom)
        }
    }

    pub mod shares {
        use super::*;
        pub fn serialize<S: Serializer>(v: &Shares, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&v.to_string())
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Shares, D::Error> {
            micros(d).map(Shares::from_micros)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::super::*;
        use serde::{Deserialize, Serialize};

        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Cfg {
            #[serde(with = "super::usd")]
            size: Usd,
            #[serde(with = "super::opt_usd", default)]
            cap: Option<Usd>,
            #[serde(with = "super::price")]
            max_price: Price,
        }

        #[test]
        fn decimal_strings_and_numbers() {
            let c: Cfg =
                serde_json::from_str(r#"{"size":"10.00","cap":"100","max_price":"0.99"}"#).unwrap();
            assert_eq!(c.size, Usd::from_whole(10));
            assert_eq!(c.cap, Some(Usd::from_whole(100)));
            assert_eq!(c.max_price, Price::parse("0.99").unwrap());
            let c2: Cfg = serde_json::from_str(r#"{"size":10,"max_price":0.95}"#).unwrap();
            assert_eq!(c2.size, Usd::from_whole(10));
            assert_eq!(c2.cap, None);
            assert_eq!(c2.max_price, Price::parse("0.95").unwrap());
            let json = serde_json::to_string(&c).unwrap();
            assert!(json.contains("\"size\":\"10.00\""));
            assert!(serde_json::from_str::<Cfg>(r#"{"size":"abc","max_price":"0.5"}"#).is_err());
            assert!(serde_json::from_str::<Cfg>(r#"{"size":"1","max_price":"1.5"}"#).is_err());
        }
    }
}
