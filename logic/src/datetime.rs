#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,

    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub voltage_low: bool,
}

impl DateTime {
    pub fn from_unix(epoch: u64) -> Self {
        let secs = (epoch % 86400) as u32;
        let days = (epoch / 86400) as i64;
        let (year, month, day) = date_from_days(days);
        let hour = (secs / 3600) as u8;
        let minute = ((secs % 3600) / 60) as u8;
        let second = (secs % 60) as u8;
        let weekday = weekday_from_days(days);
        Self {
            year,
            month,
            day,
            weekday,
            hour,
            minute,
            second,
            voltage_low: false,
        }
    }

    pub fn to_unix(self) -> u64 {
        let days = days_since_epoch(self.year, self.month, self.day) as u64;
        days * 86_400 + self.hour as u64 * 3_600 + self.minute as u64 * 60 + self.second as u64
    }

    pub fn shifted_minutes(self, minutes: i32) -> Self {
        let shifted = (self.to_unix() as i64 + minutes as i64 * 60).max(0) as u64;
        Self::from_unix(shifted)
    }
}

pub fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: u16, month: u8) -> u8 {
    let m = (month as i64 - 1).clamp(0, 11) as usize;
    month_lengths(year as i64)[m] as u8
}

fn month_lengths(year: i64) -> [i64; 12] {
    if is_leap(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    }
}

pub fn days_since_epoch(year: u16, month: u8, day: u8) -> i64 {
    let mut days: i64 = 0;
    for y in 1970..year as i64 {
        days += if is_leap(y) { 366 } else { 365 };
    }
    days += month_lengths(year as i64)
        .iter()
        .take(month.saturating_sub(1) as usize)
        .sum::<i64>();
    days + day.saturating_sub(1) as i64
}

pub fn date_from_days(mut days: i64) -> (u16, u8, u8) {
    let mut year = 1970i64;
    loop {
        let dim = if is_leap(year) { 366 } else { 365 };
        if days < dim {
            break;
        }
        days -= dim;
        year += 1;
    }
    for (idx, dim) in month_lengths(year).iter().enumerate() {
        if days < *dim {
            return (year as u16, (idx + 1) as u8, (days + 1) as u8);
        }
        days -= *dim;
    }
    unreachable!("date_from_days ran past a year's day count")
}

pub(crate) fn weekday_from_days(days: i64) -> u8 {
    ((days + 4).rem_euclid(7)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zeller_weekday(year: i64, month: i64, day: i64) -> u8 {
        let (y, m) = if month < 3 {
            (year - 1, month + 12)
        } else {
            (year, month)
        };
        let k = y.rem_euclid(100);
        let j = y.div_euclid(100);
        let h = (day + (13 * (m + 1)) / 5 + k + k / 4 + j / 4 + 5 * j).rem_euclid(7);

        ((h + 6) % 7) as u8
    }

    #[test]
    fn zeller_matches_known_anchors() {
        assert_eq!(zeller_weekday(1970, 1, 1), 4, "1970-01-01 was a Thursday");
        assert_eq!(zeller_weekday(2000, 1, 1), 6, "2000-01-01 was a Saturday");
        assert_eq!(zeller_weekday(2024, 1, 1), 1, "2024-01-01 was a Monday");
    }

    #[test]
    fn from_unix_epoch_is_thursday() {
        assert_eq!(DateTime::from_unix(0).weekday, 4);
    }

    #[test]
    fn weekday_matches_zeller_across_a_wide_date_range() {
        let mut days: i64 = 0;
        let end_days: i64 = 60 * 365 + 15;
        while days < end_days {
            let dt = DateTime::from_unix((days * 86_400) as u64);
            let expected = zeller_weekday(dt.year as i64, dt.month as i64, dt.day as i64);
            assert_eq!(
                dt.weekday, expected,
                "weekday mismatch at {:04}-{:02}-{:02} (day offset {days})",
                dt.year, dt.month, dt.day
            );
            days += 37;
        }
    }

    #[test]
    fn known_bug_report_date_is_saturday() {
        let epoch = DateTime {
            year: 2026,
            month: 8,
            day: 22,
            ..Default::default()
        }
        .to_unix();
        assert_eq!(
            DateTime::from_unix(epoch).weekday,
            6,
            "2026-08-22 is a Saturday"
        );
    }

    #[test]
    fn to_unix_from_unix_roundtrip() {
        for epoch in [0u64, 86_400, 1_700_000_000, 1_900_000_000] {
            let dt = DateTime::from_unix(epoch);
            assert_eq!(dt.to_unix(), epoch, "roundtrip failed for epoch {epoch}");
        }
    }

    #[test]
    fn leap_year_rules() {
        assert!(is_leap(2000));
        assert!(!is_leap(1900));
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
    }

    #[test]
    fn shifted_minutes_crosses_midnight_and_year_boundary() {
        let new_years_eve_2359 = DateTime {
            year: 2025,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            ..Default::default()
        };
        let shifted = new_years_eve_2359.shifted_minutes(2);
        assert_eq!((shifted.year, shifted.month, shifted.day), (2026, 1, 1));
        assert_eq!((shifted.hour, shifted.minute), (0, 1));
    }

    #[test]
    fn shifted_minutes_never_underflows_before_epoch() {
        let near_epoch = DateTime::from_unix(30);
        let shifted = near_epoch.shifted_minutes(-10);
        assert_eq!(shifted.to_unix(), 0);
    }
}
