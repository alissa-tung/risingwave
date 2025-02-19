// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::{Display, Formatter};
use std::io::Write;
use std::ops::{Add, Sub};

use byteorder::{BigEndian, WriteBytesExt};
use bytes::BytesMut;
use num_traits::{CheckedAdd, CheckedSub};
use risingwave_pb::data::IntervalUnit as IntervalUnitProto;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use super::*;
use crate::error::ErrorCode::IoError;

/// Every interval can be represented by a `IntervalUnit`.
/// Note that the difference between Interval and Instant.
/// For example, `5 yrs 1 month 25 days 23:22:57` is a interval (Can be interpreted by Interval Unit
/// with month = 61, days = 25, seconds = (57 + 23 * 3600 + 22 * 60) * 1000),
/// `1970-01-01 04:05:06` is a Instant or Timestamp
/// One month may contain 28/31 days. One day may contain 23/25 hours.
/// This internals is learned from PG:
/// <https://www.postgresql.org/docs/9.1/datatype-datetime.html#:~:text=field%20is%20negative.-,Internally,-interval%20values%20are>
///
/// FIXME: if this derives `PartialEq` and `PartialOrd`, caller must guarantee the fields are valid.
#[derive(
    Debug, Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct IntervalUnit {
    months: i32,
    days: i32,
    ms: i64,
}

impl IntervalUnit {
    pub fn new(months: i32, days: i32, ms: i64) -> Self {
        IntervalUnit { months, days, ms }
    }

    pub fn get_days(&self) -> i32 {
        self.days
    }

    pub fn get_months(&self) -> i32 {
        self.months
    }

    pub fn get_years(&self) -> i32 {
        self.months / 12
    }

    pub fn get_ms(&self) -> i64 {
        self.ms
    }

    pub fn from_protobuf_bytes(bytes: &[u8], ty: IntervalType) -> Result<Self> {
        // TODO: remove IntervalType later.
        match ty {
            // the unit is months
            Year | YearToMonth | Month => {
                let bytes = bytes.try_into().map_err(|e| {
                    InternalError(format!("Failed to deserialize i32, reason: {:?}", e))
                })?;
                let mouths = i32::from_be_bytes(bytes);
                Ok(IntervalUnit::from_month(mouths))
            }
            // the unit is ms
            Day | DayToHour | DayToMinute | DayToSecond | Hour | HourToMinute | HourToSecond
            | Minute | MinuteToSecond | Second => {
                let bytes = bytes.try_into().map_err(|e| {
                    InternalError(format!("Failed to deserialize i64, reason: {:?}", e))
                })?;
                let ms = i64::from_be_bytes(bytes);
                Ok(IntervalUnit::from_millis(ms))
            }
            Invalid => {
                // Invalid means the interval is from the new frontend.
                // TODO: make this default path later.
                let mut cursor = Cursor::new(bytes);
                read_interval_unit(&mut cursor)
            }
        }
    }

    #[must_use]
    pub fn negative(&self) -> Self {
        IntervalUnit {
            months: -self.months,
            days: -self.days,
            ms: -self.ms,
        }
    }

    #[must_use]
    pub fn from_ymd(year: i32, month: i32, days: i32) -> Self {
        let months = year * 12 + month;
        let days = days;
        let ms = 0;
        IntervalUnit { months, days, ms }
    }

    #[must_use]
    pub fn from_month(months: i32) -> Self {
        IntervalUnit {
            months,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn from_days(days: i32) -> Self {
        Self {
            days,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn from_millis(ms: i64) -> Self {
        Self {
            ms,
            ..Default::default()
        }
    }

    #[must_use]
    pub fn from_minutes(minutes: i64) -> Self {
        Self {
            ms: 1000 * 60 * minutes,
            ..Default::default()
        }
    }

    pub fn to_protobuf_owned(&self) -> Vec<u8> {
        let buf = BytesMut::with_capacity(16);
        let mut writer = buf.writer();
        self.to_protobuf(&mut writer).unwrap();
        writer.into_inner().to_vec()
    }

    pub fn to_protobuf<T: Write>(self, output: &mut T) -> Result<usize> {
        {
            output.write_i32::<BigEndian>(self.months)?;
            output.write_i32::<BigEndian>(self.days)?;
            output.write_i64::<BigEndian>(self.ms)?;
            Ok(16)
        }
        .map_err(|e| RwError::from(IoError(e)))
    }

    /// Multiple [`IntervalUnit`] by an integer with overflow check.
    pub fn checked_mul_int<I>(&self, rhs: I) -> Option<Self>
    where
        I: TryInto<i32>,
    {
        let rhs = rhs.try_into().ok()?;
        let months = self.months.checked_mul(rhs)?;
        let days = self.days.checked_mul(rhs)?;
        let ms = self.ms.checked_mul(rhs as i64)?;

        Some(IntervalUnit { months, days, ms })
    }

    /// Performs an exact division, returns [`None`] if for any unit, lhs % rhs != 0.
    pub fn exact_div(&self, rhs: &Self) -> Option<i64> {
        let mut res = None;
        let mut check_unit = |l: i64, r: i64| {
            if l == 0 && r == 0 {
                return Some(());
            }
            if l != 0 && r == 0 {
                return None;
            }
            if l % r != 0 {
                return None;
            }
            let new_res = l / r;
            if let Some(old_res) = res {
                if old_res != new_res {
                    return None;
                }
            } else {
                res = Some(new_res);
            }

            Some(())
        };

        check_unit(self.months as i64, rhs.months as i64)?;
        check_unit(self.days as i64, rhs.days as i64)?;
        check_unit(self.ms, rhs.ms)?;

        res
    }
}

#[allow(clippy::from_over_into)]
impl Into<IntervalUnitProto> for IntervalUnit {
    fn into(self) -> IntervalUnitProto {
        IntervalUnitProto {
            months: self.months,
            days: self.days,
            ms: self.ms,
        }
    }
}

impl From<&'_ IntervalUnitProto> for IntervalUnit {
    fn from(p: &'_ IntervalUnitProto) -> Self {
        Self {
            months: p.months,
            days: p.days,
            ms: p.ms,
        }
    }
}

impl Add for IntervalUnit {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        let months = self.months + rhs.months;
        let days = self.days + rhs.days;
        let ms = self.ms + rhs.ms;
        IntervalUnit { months, days, ms }
    }
}

impl CheckedAdd for IntervalUnit {
    fn checked_add(&self, other: &Self) -> Option<Self> {
        let months = self.months.checked_add(other.months)?;
        let days = self.days.checked_add(other.days)?;
        let ms = self.ms.checked_add(other.ms)?;
        Some(IntervalUnit { months, days, ms })
    }
}

impl Sub for IntervalUnit {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        let months = self.months - rhs.months;
        let days = self.days - rhs.days;
        let ms = self.ms - rhs.ms;
        IntervalUnit { months, days, ms }
    }
}

impl CheckedSub for IntervalUnit {
    fn checked_sub(&self, other: &Self) -> Option<Self> {
        let months = self.months.checked_sub(other.months)?;
        let days = self.days.checked_sub(other.days)?;
        let ms = self.ms.checked_sub(other.ms)?;
        Some(IntervalUnit { months, days, ms })
    }
}

impl Display for IntervalUnit {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let years = self.months / 12;
        let months = self.months % 12;
        let days = self.days;
        let hours = self.ms / 1000 / 3600;
        let minutes = (self.ms / 1000 / 60) % 60;
        let seconds = ((self.ms % 60000) as f64) / 1000.0;
        let mut v = SmallVec::<[String; 4]>::new();
        if years == 1 {
            v.push(format!("{years} year"));
        } else if years != 0 {
            v.push(format!("{years} years"));
        }
        if months == 1 {
            v.push(format!("{months} mon"));
        } else if months != 0 {
            v.push(format!("{months} mons"));
        }
        if days == 1 {
            v.push(format!("{days} day"));
        } else if days != 0 {
            v.push(format!("{days} days"));
        }
        v.push(format!("{hours:0>2}:{minutes:0>2}:{seconds:0>2}"));
        Display::fmt(&v.join(" "), f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_string() {
        let interval =
            IntervalUnit::new(-14, 3, 11 * 3600 * 1000 + 45 * 60 * 1000 + 14 * 1000 + 233);
        assert_eq!(interval.to_string(), "-1 years -2 mons 3 days 11:45:14.233");
    }

    #[test]
    fn test_exact_div() {
        let cases = [
            ((14, 6, 6), (14, 6, 6), Some(1)),
            ((0, 0, 0), (0, 0, 0), None),
            ((0, 0, 0), (1, 0, 0), Some(0)),
            ((1, 1, 1), (0, 0, 0), None),
            ((1, 1, 1), (1, 0, 0), None),
            ((10, 0, 0), (1, 0, 0), Some(10)),
            ((10, 0, 0), (4, 0, 0), None),
            ((0, 24, 0), (4, 0, 0), None),
            ((6, 8, 9), (3, 1, 3), None),
            ((6, 8, 12), (3, 4, 6), Some(2)),
        ];

        for (lhs, rhs, expected) in cases {
            let lhs = IntervalUnit::new(lhs.0 as i32, lhs.1 as i32, lhs.2 as i64);
            let rhs = IntervalUnit::new(rhs.0 as i32, rhs.1 as i32, rhs.2 as i64);
            let result = std::panic::catch_unwind(|| {
                let actual = lhs.exact_div(&rhs);
                assert_eq!(actual, expected);
            });
            if result.is_err() {
                println!("Failed on {}.exact_div({})", lhs, rhs);
                break;
            }
        }
    }
}
