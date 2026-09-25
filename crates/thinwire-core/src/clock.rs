//! The clock and the time zone of the view (#120).
//!
//! A frontend formats every time from [`crate::View::now`], never from the
//! system clock directly. The app uses [`Clock::System`]. The demo scenarios
//! and tests use [`Clock::Fixed`], so a rendered screen is the same on each
//! run, on each date, and in each time zone.

use chrono::{DateTime, FixedOffset, Local};

/// UTC as a fixed offset: the fallback for an offset out of range.
const UTC: FixedOffset = match FixedOffset::east_opt(0) {
    Some(zone) => zone,
    None => unreachable!(),
};

/// Where "now" and the time zone come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clock {
    /// The system clock and the local time zone.
    #[default]
    System,
    /// A fixed time (Unix seconds) in a zone with a fixed UTC offset.
    Fixed { now: i64, utc_offset_secs: i32 },
}

impl Clock {
    /// A fixed time in UTC.
    #[must_use]
    pub const fn fixed_utc(now: i64) -> Self {
        Self::Fixed {
            now,
            utc_offset_secs: 0,
        }
    }

    /// Now, in the zone of this clock.
    #[must_use]
    pub fn now(self) -> ViewNow {
        match self {
            Self::System => ViewNow::Local(Local::now()),
            Self::Fixed {
                now,
                utc_offset_secs,
            } => {
                let zone = FixedOffset::east_opt(utc_offset_secs).unwrap_or(UTC);
                let now = DateTime::from_timestamp(now, 0)
                    .unwrap_or_default()
                    .with_timezone(&zone);
                ViewNow::Fixed(now)
            }
        }
    }
}

/// "Now" with its time zone, not one UTC offset. A frontend turns each
/// message time into a local time with this zone, so each time gets the
/// offset of its own date: `Local` keeps the daylight saving rules (Codex on
/// #121). `Fixed` is for the demo and tests, which use UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewNow {
    Local(DateTime<Local>),
    Fixed(DateTime<FixedOffset>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_clock_is_the_same_on_each_call() {
        let clock = Clock::Fixed {
            now: 1_772_445_600,
            utc_offset_secs: 3_600,
        };
        assert_eq!(clock.now(), clock.now());
        let ViewNow::Fixed(now) = clock.now() else {
            panic!("fixed");
        };
        assert_eq!(now.timestamp(), 1_772_445_600);
        assert_eq!(now.format("%H:%M").to_string(), "11:00");
    }
}
