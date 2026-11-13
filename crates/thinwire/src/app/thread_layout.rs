//! Pure layout rules for the thread and the chat list: day breaks, sender
//! runs, and local times. No egui here, so tests use fixed time zones.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone};
use thinwire_protocol::ChatMessage;
use thinwire_protocol::ChatStatus;

/// Days back that the chat list shows as a weekday name.
const WEEKDAY_WINDOW_DAYS: i64 = 6;

/// The mark before the time of an edited message.
const EDITED: &str = "edited";

/// How the thread draws one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowLayout {
    /// Day break above this row: "Today", "Yesterday", or a date.
    pub day_break: Option<String>,
    /// Show the sender name above this row.
    pub show_sender: bool,
    /// Local "HH:MM", after "edited" for an edited message. Empty when the
    /// time is unknown and the message has no edit.
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
        let time = local
            .map(|local| local.format("%H:%M").to_string())
            .unwrap_or_default();
        rows.push(RowLayout {
            show_sender: is_group && !message.outbound && new_run,
            time: match (message.edited, time.is_empty()) {
                (false, _) => time,
                (true, true) => EDITED.to_owned(),
                (true, false) => format!("{EDITED} {time}"),
            },
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

/// The status line under the name of a chat: "online", "last seen at
/// 13:38", "bot", "4 members".
#[must_use]
pub(crate) fn status_line<Tz: TimeZone>(status: ChatStatus, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    match status {
        ChatStatus::Online => "online".into(),
        ChatStatus::LastSeen(at) => last_seen(at, now),
        ChatStatus::Recently => "last seen recently".into(),
        ChatStatus::LastWeek => "last seen in the last week".into(),
        ChatStatus::LastMonth => "last seen in the last month".into(),
        ChatStatus::LongAgo => "last seen a long time ago".into(),
        ChatStatus::Bot => "bot".into(),
        ChatStatus::Members(count) => counted(count, "member", "members"),
        ChatStatus::Subscribers(count) => counted(count, "subscriber", "subscribers"),
    }
}

fn last_seen<Tz: TimeZone>(at: i64, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let Some(local) = local_time(&now.timezone(), at) else {
        return "last seen recently".into();
    };
    let day = local.date_naive();
    let today = now.date_naive();
    let time = local.format("%H:%M");
    match (today - day).num_days() {
        0 => {
            let minutes = (now.timestamp() - at).max(0) / 60;
            match minutes {
                0 => "last seen just now".into(),
                1 => "last seen 1 minute ago".into(),
                2..=59 => format!("last seen {minutes} minutes ago"),
                _ => format!("last seen at {time}"),
            }
        }
        1 => format!("last seen yesterday at {time}"),
        2..=WEEKDAY_WINDOW_DAYS => format!("last seen {} at {time}", local.format("%A")),
        _ if day.year() == today.year() => format!("last seen {}", local.format("%-d %b")),
        _ => format!("last seen {}", local.format("%-d %b %Y")),
    }
}

/// "1 member", "1,204 members".
fn counted(count: u32, one: &str, many: &str) -> String {
    if count == 1 {
        return format!("1 {one}");
    }
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    format!("{grouped} {many}")
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
            topic: None,
            links: Vec::new(),
            photo: None,
            audio: None,
            video: None,
            file: None,
            reactions: Vec::new(),
            reply_to: None,
            edited: false,
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
    fn an_edited_message_shows_edited_before_its_time() {
        let mut edited = message("Ada", false, NOON - HOUR);
        edited.edited = true;
        let mut unknown = message("Ada", false, 0);
        unknown.edited = true;
        let rows = thread_rows(&[edited, unknown], false, &now(0));
        assert_eq!(rows[0].time, "edited 11:00");
        assert_eq!(rows[1].time, "edited");
    }

    /// Codex on #121 (DST): a zone with daylight saving time converts each
    /// message with the offset of its own date, not the offset of now. The
    /// app's view zone (`ViewNow::Local`, `chrono::Local`) works this way.
    #[test]
    fn each_message_uses_the_offset_of_its_own_date() {
        use chrono_tz::Europe::Berlin;

        /// 2026-01-15 12:00 UTC: winter, UTC+1 in Berlin.
        const WINTER_NOW: i64 = 1_768_478_400;
        /// 2025-07-15 10:00 UTC: summer, UTC+2 in Berlin.
        const JULY: i64 = 1_752_573_600;
        /// 2026-01-14 10:00 UTC: winter, UTC+1 in Berlin.
        const JANUARY: i64 = 1_768_384_800;

        let now = Berlin.timestamp_opt(WINTER_NOW, 0).single().expect("now");
        let rows = thread_rows(
            &[message("Ada", false, JULY), message("Ada", false, JANUARY)],
            false,
            &now,
        );
        assert_eq!(rows[0].time, "12:00", "July is UTC+2");
        assert_eq!(rows[1].time, "11:00", "January is UTC+1");
        assert_eq!(list_time(JANUARY, &now), "Yesterday");
        assert_eq!(list_time(JULY, &now), "15 Jul 2025");
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

    #[test]
    fn a_status_line_says_the_presence_in_words() {
        const MINUTE: i64 = 60;
        let now = FixedOffset::east_opt(0)
            .unwrap()
            .timestamp_opt(NOON, 0)
            .unwrap();
        let line = |status| status_line(status, &now);
        assert_eq!(line(ChatStatus::Online), "online");
        assert_eq!(line(ChatStatus::LastSeen(NOON - 20)), "last seen just now");
        assert_eq!(
            line(ChatStatus::LastSeen(NOON - MINUTE)),
            "last seen 1 minute ago"
        );
        assert_eq!(
            line(ChatStatus::LastSeen(NOON - 42 * MINUTE)),
            "last seen 42 minutes ago"
        );
        assert_eq!(
            line(ChatStatus::LastSeen(NOON - 2 * HOUR)),
            "last seen at 10:00"
        );
        assert_eq!(
            line(ChatStatus::LastSeen(NOON - DAY)),
            "last seen yesterday at 12:00"
        );
        assert!(line(ChatStatus::LastSeen(NOON - 3 * DAY)).ends_with("day at 12:00"));
        // A time that is not valid still gives a line.
        assert_eq!(line(ChatStatus::LastSeen(0)), "last seen recently");
        // A clock that is behind the server does not show a negative time.
        assert_eq!(line(ChatStatus::LastSeen(NOON + 30)), "last seen just now");
        assert_eq!(line(ChatStatus::Recently), "last seen recently");
        assert_eq!(line(ChatStatus::LongAgo), "last seen a long time ago");
        assert_eq!(line(ChatStatus::Bot), "bot");
        assert_eq!(line(ChatStatus::Members(1)), "1 member");
        assert_eq!(line(ChatStatus::Members(999)), "999 members");
        assert_eq!(line(ChatStatus::Members(1204)), "1,204 members");
        assert_eq!(
            line(ChatStatus::Subscribers(1_200_300)),
            "1,200,300 subscribers"
        );
    }
}
