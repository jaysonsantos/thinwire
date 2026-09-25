//! Pure layout rules for the thread and the chat list: day breaks, sender
//! runs, and local times. No egui here, so tests use fixed time zones.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone};
use thinwire_protocol::ChatMessage;

/// Days back that the chat list shows as a weekday name.
const WEEKDAY_WINDOW_DAYS: i64 = 6;

/// How the thread draws one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowLayout {
    /// Day break above this row: "Today", "Yesterday", or a date.
    pub day_break: Option<String>,
    /// Show the sender name above this row.
    pub show_sender: bool,
    /// Local "HH:MM". Empty when the time is unknown.
    pub time: String,
    /// Last bubble of a sender run. The next row is another sender, the
    /// other side, a new day, or the end of the thread.
    pub run_end: bool,
}

/// Day breaks, sender runs, and times for `messages` (oldest first).
///
/// A group chat names the sender at the start of each run of incoming
/// messages from one sender. A private chat never names the sender.
#[must_use]
pub(crate) fn thread_rows<Tz: TimeZone>(
    messages: &[ChatMessage],
    is_group: bool,
    now: &DateTime<Tz>,
) -> Vec<RowLayout>
where
    Tz::Offset: std::fmt::Display,
{
    let zone = now.timezone();
    let today = now.date_naive();
    let mut last_day: Option<NaiveDate> = None;
    let mut previous: Option<&ChatMessage> = None;
    let mut rows = Vec::with_capacity(messages.len());
    for message in messages {
        let local = local_time(&zone, message.sent_at);
        let day = local.as_ref().map(DateTime::date_naive);
        let day_break = match day {
            Some(day) if last_day != Some(day) => {
                last_day = Some(day);
                Some(day_label(day, today))
            }
            _ => None,
        };
        let new_run = previous.is_none_or(|previous| {
            previous.outbound || previous.sender != message.sender || day_break.is_some()
        });
        rows.push(RowLayout {
            show_sender: is_group && !message.outbound && new_run,
            time: local
                .map(|local| local.format("%H:%M").to_string())
                .unwrap_or_default(),
            day_break,
            run_end: false,
        });
        previous = Some(message);
    }
    for index in 0..rows.len() {
        rows[index].run_end = match messages.get(index + 1) {
            None => true,
            Some(next) => {
                rows[index + 1].day_break.is_some()
                    || next.outbound != messages[index].outbound
                    || next.sender != messages[index].sender
            }
        };
    }
    rows
}

/// Short time for a chat-list row: "HH:MM" today, then "Yesterday", a
/// weekday, or a date. Empty when the time is unknown.
#[must_use]
pub(crate) fn list_time<Tz: TimeZone>(at: i64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(local) = local_time(&now.timezone(), at) else {
        return String::new();
    };
    let day = local.date_naive();
    let today = now.date_naive();
    let age = (today - day).num_days();
    match age {
        0 => local.format("%H:%M").to_string(),
        1 => "Yesterday".into(),
        2..=WEEKDAY_WINDOW_DAYS => local.format("%a").to_string(),
        _ if day.year() == today.year() => local.format("%-d %b").to_string(),
        _ => local.format("%-d %b %Y").to_string(),
    }
}

fn local_time<Tz: TimeZone>(zone: &Tz, at: i64) -> Option<DateTime<Tz>> {
    if at <= 0 {
        return None;
    }
    zone.timestamp_opt(at, 0).single()
}

fn day_label(day: NaiveDate, today: NaiveDate) -> String {
    match (today - day).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ if day.year() == today.year() => day.format("%A, %-d %B").to_string(),
        _ => day.format("%-d %B %Y").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::FixedOffset;
    use thinwire_protocol::{Delivery, ProtocolId};

    use super::*;

    /// 2026-09-23 12:00:00 UTC.
    const NOON: i64 = 1_790_164_800;
    const HOUR: i64 = 3_600;
    const DAY: i64 = 24 * HOUR;

    fn message(sender: &str, outbound: bool, sent_at: i64) -> ChatMessage {
        ChatMessage {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
            id: format!("telegram:1:{sent_at}"),
            arrival: thinwire_protocol::Arrival::History,
            sender: sender.into(),
            body: "text".into(),
            outbound,
            delivery: Delivery::Sent,
            sent_at,
        }
    }

    fn zone(hours: i32) -> FixedOffset {
        FixedOffset::east_opt(hours * 3_600).expect("offset")
    }

    fn now(hours: i32) -> DateTime<FixedOffset> {
        zone(hours).timestamp_opt(NOON, 0).single().expect("now")
    }

    #[test]
    fn day_breaks_show_today_yesterday_and_dates_in_local_time() {
        let messages = [
            message("Ada", false, NOON - 400 * DAY),
            message("Ada", false, NOON - 3 * DAY),
            message("Ada", false, NOON - 3 * DAY + 60),
            message("Ada", false, NOON - DAY),
            message("Ada", false, NOON - HOUR),
            message("Ada", false, 0),
        ];
        let rows = thread_rows(&messages, false, &now(0));
        let breaks: Vec<Option<&str>> = rows.iter().map(|row| row.day_break.as_deref()).collect();
        assert_eq!(
            breaks,
            vec![
                Some("19 August 2025"),
                Some("Sunday, 20 September"),
                None,
                Some("Yesterday"),
                Some("Today"),
                None,
            ]
        );
        assert_eq!(rows[4].time, "11:00");
        assert_eq!(rows[5].time, "", "unknown time shows nothing");
    }

    #[test]
    fn local_offset_moves_the_day_and_the_clock() {
        // 23:30 UTC yesterday is 01:30 today at UTC+2.
        let late = NOON - 12 * HOUR - 30 * 60;
        let utc = thread_rows(&[message("Ada", false, late)], false, &now(0));
        assert_eq!(utc[0].day_break.as_deref(), Some("Yesterday"));
        assert_eq!(utc[0].time, "23:30");
        let plus_two = thread_rows(&[message("Ada", false, late)], false, &now(2));
        assert_eq!(plus_two[0].day_break.as_deref(), Some("Today"));
        assert_eq!(plus_two[0].time, "01:30");
    }

    #[test]
    fn groups_name_each_sender_run_and_private_chats_never_do() {
        let messages = [
            message("Ada", false, NOON),
            message("Ada", false, NOON + 60),
            message("Bob", false, NOON + 120),
            message("you", true, NOON + 180),
            message("Bob", false, NOON + 240),
            message("Bob", false, NOON + DAY),
        ];
        let group: Vec<bool> = thread_rows(&messages, true, &now(0))
            .iter()
            .map(|row| row.show_sender)
            .collect();
        assert_eq!(group, vec![true, false, true, false, true, true]);
        let private = thread_rows(&messages, false, &now(0));
        assert!(private.iter().all(|row| !row.show_sender));
    }

    #[test]
    fn run_end_marks_the_last_bubble_of_a_sender_run() {
        let messages = [
            message("Ada", false, NOON),
            message("Ada", false, NOON + 60),
            message("you", true, NOON + 120),
            message("you", true, NOON + 180),
            message("Ada", false, NOON + DAY),
        ];
        let ends: Vec<bool> = thread_rows(&messages, true, &now(0))
            .iter()
            .map(|row| row.run_end)
            .collect();
        assert_eq!(ends, vec![false, true, false, true, true]);
    }

    #[test]
    fn chat_list_time_shortens_with_age() {
        let now = now(0);
        assert_eq!(list_time(NOON - HOUR, &now), "11:00");
        assert_eq!(list_time(NOON - DAY, &now), "Yesterday");
        assert_eq!(list_time(NOON - 3 * DAY, &now), "Sun");
        assert_eq!(list_time(NOON - 30 * DAY, &now), "24 Aug");
        assert_eq!(list_time(NOON - 400 * DAY, &now), "19 Aug 2025");
        assert_eq!(list_time(0, &now), "");
    }
}
