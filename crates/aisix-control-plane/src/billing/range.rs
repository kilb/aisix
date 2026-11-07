//! The time range a summary is asked over, which bounds the periods it can
//! return.

use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;

use crate::error::{ApiError, Detail};

/// The period a summary groups by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Day,
    Month,
}

/// The longest range a summary by day covers: about a quarter.
const MAX_DAYS_BY_DAY: i64 = 92;

/// The longest range a summary by month covers: two years.
const MAX_DAYS_BY_MONTH: i64 = 731;

/// `[from, to)` when `from` is before `to` and, for a summary grouped by
/// `group`, the range is at most 92 days by day or 731 days by month;
/// otherwise `422` at `/to`.
pub fn checked_range(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    group: Option<Group>,
) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError> {
    if to <= from {
        return Err(refused("结束时间须晚于开始时间"));
    }
    let max_days = match group {
        None => None,
        Some(Group::Day) => Some(MAX_DAYS_BY_DAY),
        Some(Group::Month) => Some(MAX_DAYS_BY_MONTH),
    };
    if max_days.is_some_and(|days| to - from > TimeDelta::days(days)) {
        return Err(refused("时间范围过大"));
    }
    Ok((from, to))
}

fn refused(message: &str) -> ApiError {
    ApiError::validation(vec![Detail {
        path: "/to".to_string(),
        message: message.to_string(),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("a time")
            .with_timezone(&Utc)
    }

    /// The message of `checked_range`'s refusal, which must be at `/to`.
    fn refusal(from: DateTime<Utc>, to: DateTime<Utc>, group: Option<Group>) -> String {
        let e = checked_range(from, to, group).expect_err("refused");
        assert_eq!((e.status.as_u16(), e.code), (422, "validation_failed"));
        assert_eq!(e.details.len(), 1, "{e:?}");
        assert_eq!(e.details[0].path, "/to");
        e.details[0].message.clone()
    }

    #[test]
    fn a_summary_range_is_bounded() {
        let from = at("2026-01-01T00:00:00Z");
        let days = |n: i64| from + TimeDelta::days(n);
        let day = Some(Group::Day);
        let month = Some(Group::Month);

        assert_eq!(
            checked_range(from, days(92), day).ok(),
            Some((from, days(92)))
        );
        assert_eq!(refusal(from, days(93), day), "时间范围过大");
        assert_eq!(
            refusal(from, days(92) + TimeDelta::seconds(1), day),
            "时间范围过大"
        );

        assert!(checked_range(from, days(731), month).is_ok());
        assert_eq!(refusal(from, days(732), month), "时间范围过大");

        for group in [None, day, month] {
            assert_eq!(refusal(from, from, group), "结束时间须晚于开始时间");
            assert_eq!(refusal(days(1), from, group), "结束时间须晚于开始时间");
        }
        // A range with nothing to group is not bounded.
        assert!(checked_range(from, days(3650), None).is_ok());
    }
}
