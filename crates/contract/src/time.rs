// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

//! The one UTC instant type used across ArtiDezi.
//!
//! [`Timestamp`] wraps [`jiff::Timestamp`] but keeps the exact text forms that stored jobs, attempt
//! records, SQLite rows and JSON contracts were written in before the move off `chrono`:
//!
//! - serde uses RFC 3339 in UTC with a `Z` suffix and the fraction shortened to 0, 3, 6 or 9
//!   digits (`2026-01-02T03:04:05.120Z`, `2026-01-02T03:04:05Z`);
//! - [`Display`](fmt::Display) (used in error and event messages) writes `2026-01-02 03:04:05.120 UTC`;
//! - [`Timestamp::to_rfc3339`] writes the same instant with a `+00:00` offset;
//! - [`Timestamp::to_rfc3339_micros`] always writes six fractional digits and `Z` (the SQLite form).
//!
//! Parsing accepts any RFC 3339 offset (`Z`, `+00:00`, `+02:00`, ...) and normalizes to UTC, so
//! values written by forges or by earlier ArtiDezi versions keep reading back to the same instant.
//! Like `chrono`, it also accepts the `Display` form and truncates fractions beyond nanoseconds.
//! Unlike `chrono`, a leap second (`23:59:60`) reads as `23:59:59` with the same fraction.
//!
//! Arithmetic saturates at [`Timestamp::MIN`]/[`Timestamp::MAX`] (years -9999 and 9999) instead of
//! panicking, since that range is narrower than `chrono`'s.

use std::{borrow::Cow, fmt, ops, str::FromStr, time::SystemTime};

use jiff::tz::Offset;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub use jiff::SignedDuration;

/// A UTC instant with nanosecond precision. See the [module documentation](self) for its text forms.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Timestamp(jiff::Timestamp);

#[derive(Debug, thiserror::Error)]
#[error("invalid RFC 3339 timestamp {input:?}: {source}")]
pub struct ParseTimestampError {
    input: String,
    source: jiff::Error,
}

impl Timestamp {
    /// The Unix epoch, `1970-01-01T00:00:00Z`.
    pub const UNIX_EPOCH: Self = Self(jiff::Timestamp::UNIX_EPOCH);
    /// The latest representable instant (in year 9999); used as "no deadline".
    pub const MAX: Self = Self(jiff::Timestamp::MAX);
    /// The earliest representable instant.
    pub const MIN: Self = Self(jiff::Timestamp::MIN);

    pub fn now() -> Self {
        Self(jiff::Timestamp::now())
    }

    /// Whole seconds since the Unix epoch; `None` outside the supported range (years 1–9999 are safe).
    pub fn from_unix_seconds(seconds: i64) -> Option<Self> {
        jiff::Timestamp::from_second(seconds).ok().map(Self)
    }

    /// Whole seconds since the Unix epoch, rounded towards negative infinity like `chrono`.
    pub fn unix_seconds(self) -> i64 {
        self.0.as_duration().as_secs() - i64::from(self.0.as_duration().subsec_nanos() < 0)
    }

    /// Microseconds since the Unix epoch, rounded towards negative infinity like `chrono`.
    pub fn unix_micros(self) -> i64 {
        i64::try_from(self.0.as_duration().as_nanos().div_euclid(1_000))
            .expect("the supported range fits in i64 microseconds")
    }

    /// Parses RFC 3339 with any offset, normalizing to UTC. See the [module documentation](self).
    pub fn parse_rfc3339(input: &str) -> Result<Self, ParseTimestampError> {
        let normalized = normalize(input);
        normalized
            .parse::<jiff::Timestamp>()
            .map(Self)
            .map_err(|source| ParseTimestampError {
                input: input.to_owned(),
                source,
            })
    }

    /// RFC 3339 with a `+00:00` offset and a 0/3/6/9-digit fraction.
    pub fn to_rfc3339(self) -> String {
        let mut out = String::with_capacity(35);
        self.write(&mut out, 'T', Fraction::Auto, "+00:00")
            .expect("writing to a String cannot fail");
        out
    }

    /// RFC 3339 with exactly six fractional digits and a `Z` suffix.
    pub fn to_rfc3339_micros(self) -> String {
        let mut out = String::with_capacity(27);
        self.write(&mut out, 'T', Fraction::Micros, "Z")
            .expect("writing to a String cannot fail");
        out
    }

    /// The signed time elapsed from `earlier` to `self` (negative when `earlier` is later).
    pub fn duration_since(self, earlier: Self) -> SignedDuration {
        self.0.duration_since(earlier.0)
    }

    pub fn checked_add(self, duration: SignedDuration) -> Option<Self> {
        self.0.checked_add(duration).ok().map(Self)
    }

    pub fn checked_sub(self, duration: SignedDuration) -> Option<Self> {
        self.0.checked_sub(duration).ok().map(Self)
    }

    pub fn as_jiff(self) -> jiff::Timestamp {
        self.0
    }

    fn write(
        self,
        out: &mut impl fmt::Write,
        separator: char,
        fraction: Fraction,
        suffix: &str,
    ) -> fmt::Result {
        let civil = Offset::UTC.to_datetime(self.0);
        let year = civil.year();
        if (0..=9999).contains(&year) {
            write!(out, "{year:04}")?;
        } else {
            write!(out, "{year:+05}")?;
        }
        write!(
            out,
            "-{:02}-{:02}{separator}{:02}:{:02}:{:02}",
            civil.month(),
            civil.day(),
            civil.hour(),
            civil.minute(),
            civil.second()
        )?;
        let nanos = civil.subsec_nanosecond();
        match fraction {
            Fraction::Micros => write!(out, ".{:06}", nanos / 1_000)?,
            Fraction::Auto if nanos == 0 => {}
            Fraction::Auto if nanos % 1_000_000 == 0 => write!(out, ".{:03}", nanos / 1_000_000)?,
            Fraction::Auto if nanos % 1_000 == 0 => write!(out, ".{:06}", nanos / 1_000)?,
            Fraction::Auto => write!(out, ".{nanos:09}")?,
        }
        out.write_str(suffix)
    }
}

/// Accepts the two `chrono` spellings `jiff` does not: the `Display` form with a ` UTC` suffix and
/// fractions longer than nine digits (truncated).
fn normalize(input: &str) -> Cow<'_, str> {
    let mut value = Cow::Borrowed(input);
    // chrono writes years outside 0..=9999 as `-0001`/`+10000`; RFC 3339/ISO 8601 parsers want six.
    if let Some(sign @ (b'-' | b'+')) = input.bytes().next() {
        let digits = input[1..].bytes().take_while(u8::is_ascii_digit).count();
        if (4..6).contains(&digits) {
            let sign = char::from(sign);
            value = Cow::Owned(format!(
                "{sign}{:0>6}{}",
                &input[1..=digits],
                &input[1 + digits..]
            ));
        }
    }
    if let Some(rest) = value.strip_suffix(" UTC") {
        value = Cow::Owned(format!("{rest}Z"));
    }
    if let Some(dot) = value.find('.') {
        let digits = value[dot + 1..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        if digits > 9 {
            let mut owned = value.into_owned();
            owned.replace_range(dot + 10..dot + 1 + digits, "");
            value = Cow::Owned(owned);
        }
    }
    value
}

#[derive(Clone, Copy)]
enum Fraction {
    Auto,
    Micros,
}

impl From<jiff::Timestamp> for Timestamp {
    fn from(value: jiff::Timestamp) -> Self {
        Self(value)
    }
}

impl From<Timestamp> for jiff::Timestamp {
    fn from(value: Timestamp) -> Self {
        value.0
    }
}

/// Saturates at [`Timestamp::MIN`]/[`Timestamp::MAX`] for system times outside years -9999..=9999.
impl From<SystemTime> for Timestamp {
    fn from(value: SystemTime) -> Self {
        jiff::Timestamp::try_from(value)
            .map(Self)
            .unwrap_or_else(|_| {
                if value < SystemTime::UNIX_EPOCH {
                    Self::MIN
                } else {
                    Self::MAX
                }
            })
    }
}

/// `chrono`'s `Display` for `DateTime<Utc>`: `2026-01-02 03:04:05.120 UTC`.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, ' ', Fraction::Auto, " UTC")
    }
}

/// The serde form: `2026-01-02T03:04:05.120Z`.
impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(f, 'T', Fraction::Auto, "Z")
    }
}

impl FromStr for Timestamp {
    type Err = ParseTimestampError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_rfc3339(s)
    }
}

impl ops::Add<SignedDuration> for Timestamp {
    type Output = Self;

    fn add(self, rhs: SignedDuration) -> Self {
        self.checked_add(rhs).unwrap_or(if rhs.is_negative() {
            Self::MIN
        } else {
            Self::MAX
        })
    }
}

impl ops::AddAssign<SignedDuration> for Timestamp {
    fn add_assign(&mut self, rhs: SignedDuration) {
        *self = *self + rhs;
    }
}

impl ops::Sub<SignedDuration> for Timestamp {
    type Output = Self;

    fn sub(self, rhs: SignedDuration) -> Self {
        self.checked_sub(rhs).unwrap_or(if rhs.is_negative() {
            Self::MAX
        } else {
            Self::MIN
        })
    }
}

impl ops::SubAssign<SignedDuration> for Timestamp {
    fn sub_assign(&mut self, rhs: SignedDuration) {
        *self = *self - rhs;
    }
}

impl ops::Sub for Timestamp {
    type Output = SignedDuration;

    fn sub(self, rhs: Self) -> SignedDuration {
        self.duration_since(rhs)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&format_args!("{self:?}"))
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = Timestamp;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an RFC 3339 formatted date and time string")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Timestamp, E> {
                Timestamp::parse_rfc3339(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

impl JsonSchema for Timestamp {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "DateTime".into()
    }

    fn schema_id() -> Cow<'static, str> {
        "artidezi::Timestamp".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "format": "date-time" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    /// The exact strings `chrono` 0.4 produced for `DateTime<Utc>` (serde/`Debug`), for
    /// `to_rfc3339()` and for `to_rfc3339_opts(SecondsFormat::Micros, true)`.
    #[test]
    fn writes_the_same_text_as_chrono() {
        let cases = [
            // input, serde, to_rfc3339, micros
            (
                "2026-07-14T10:00:00Z",
                "2026-07-14T10:00:00Z",
                "2026-07-14T10:00:00+00:00",
                "2026-07-14T10:00:00.000000Z",
            ),
            (
                "2026-07-14T10:00:00.120Z",
                "2026-07-14T10:00:00.120Z",
                "2026-07-14T10:00:00.120+00:00",
                "2026-07-14T10:00:00.120000Z",
            ),
            (
                "2026-07-14T10:00:00.1234Z",
                "2026-07-14T10:00:00.123400Z",
                "2026-07-14T10:00:00.123400+00:00",
                "2026-07-14T10:00:00.123400Z",
            ),
            (
                "2026-07-14T10:00:00.123456789Z",
                "2026-07-14T10:00:00.123456789Z",
                "2026-07-14T10:00:00.123456789+00:00",
                "2026-07-14T10:00:00.123456Z",
            ),
            (
                "2026-07-14T12:30:00.5+02:30",
                "2026-07-14T10:00:00.500Z",
                "2026-07-14T10:00:00.500+00:00",
                "2026-07-14T10:00:00.500000Z",
            ),
            (
                "0999-01-02T03:04:05.000000001Z",
                "0999-01-02T03:04:05.000000001Z",
                "0999-01-02T03:04:05.000000001+00:00",
                "0999-01-02T03:04:05.000000Z",
            ),
        ];
        for (input, serde, rfc3339, micros) in cases {
            let value = at(input);
            assert_eq!(
                serde_json::to_string(&value).unwrap(),
                format!("\"{serde}\"")
            );
            assert_eq!(format!("{value:?}"), serde);
            assert_eq!(
                value.to_string(),
                serde.replace('T', " ").replace('Z', " UTC")
            );
            assert_eq!(at(&value.to_string()), value);
            assert_eq!(value.to_rfc3339(), rfc3339);
            assert_eq!(value.to_rfc3339_micros(), micros);
            let back: Timestamp = serde_json::from_str(&format!("\"{serde}\"")).unwrap();
            assert_eq!(back, value);
            assert_eq!(at(rfc3339), value);
        }
    }

    #[test]
    fn reads_offsets_and_rejects_garbage() {
        assert_eq!(at("2026-07-14T10:00:00+00:00"), at("2026-07-14T10:00:00Z"));
        assert_eq!(at("2026-07-14T13:00:00+03:00"), at("2026-07-14T10:00:00Z"));
        assert_eq!(at("2026-07-14 10:00:00Z"), at("2026-07-14T10:00:00Z"));
        assert_eq!(at("2026-07-14 10:00:00 UTC"), at("2026-07-14T10:00:00Z"));
        assert_eq!(
            at("2026-07-14T10:00:00.1234567891Z"),
            at("2026-07-14T10:00:00.123456789Z")
        );
        assert_eq!(
            at("2026-07-14T10:00:00.1234567891+01:00"),
            at("2026-07-14T09:00:00.123456789Z")
        );
        assert!("2026-07-14T10:00:00".parse::<Timestamp>().is_err());
        assert!("yesterday".parse::<Timestamp>().is_err());
        assert!(serde_json::from_str::<Timestamp>("1700000000").is_err());
    }

    #[test]
    fn unix_seconds_match_chrono() {
        assert_eq!(at("1970-01-01T00:00:01.5Z").unix_seconds(), 1);
        assert_eq!(at("1969-12-31T23:59:59.5Z").unix_seconds(), -1);
        assert_eq!(at("2026-07-14T10:00:00Z").unix_seconds(), 1_784_023_200);
        assert_eq!(
            Timestamp::from_unix_seconds(1_784_023_200),
            Some(at("2026-07-14T10:00:00Z"))
        );
        assert_eq!(at("1970-01-01T00:00:00.000001Z").unix_micros(), 1);
        assert_eq!(at("1969-12-31T23:59:59.999999999Z").unix_micros(), -1);
    }

    #[test]
    fn arithmetic() {
        let start = at("2026-07-14T10:00:00Z");
        let end = start + SignedDuration::from_mins(90);
        assert_eq!(end, at("2026-07-14T11:30:00Z"));
        assert_eq!((end - start).as_secs(), 5_400);
        assert_eq!((start - end).as_secs(), -5_400);
        assert_eq!(
            end - SignedDuration::from_hours(1),
            at("2026-07-14T10:30:00Z")
        );
    }

    #[test]
    fn negative_years_round_trip() {
        let value = at("-0001-01-02T03:04:05Z");
        assert_eq!(format!("{value:?}"), "-0001-01-02T03:04:05Z");
        assert_eq!(at(&format!("{value:?}")), value);
    }

    #[test]
    fn arithmetic_saturates_instead_of_panicking() {
        let now = at("2026-07-14T10:00:00Z");
        assert_eq!(
            now + SignedDuration::from_secs(400_000_000_000),
            Timestamp::MAX
        );
        assert_eq!(
            now - SignedDuration::from_secs(400_000_000_000),
            Timestamp::MIN
        );
        assert_eq!(
            now - SignedDuration::from_secs(-400_000_000_000),
            Timestamp::MAX
        );
    }

    #[test]
    fn schema_matches_chrono() {
        assert_eq!(
            serde_json::to_value(schemars::schema_for!(Timestamp)).unwrap()["format"],
            "date-time"
        );
    }
}
