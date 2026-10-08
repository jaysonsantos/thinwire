//! Chat ids for log lines (#238).
//!
//! WhatsApp and Signal chat ids contain the contact's phone number. A log line
//! shows only the protocol prefix, the last four characters of the local part,
//! and the `@server` part if there is one:
//! `whatsapp:4915550100@s.whatsapp.net` logs as `whatsapp:…0100@s.whatsapp.net`.

use std::fmt;

/// Characters of the local part that stay visible.
const KEEP: usize = 4;

/// `Display` wrapper that redacts a conversation id for a log line.
pub struct LogChatId<'a>(pub &'a str);

impl fmt::Display for LogChatId<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prefix, rest) = match self.0.split_once(':') {
            Some((prefix, rest)) => (Some(prefix), rest),
            None => (None, self.0),
        };
        let (local, server) = match rest.split_once('@') {
            Some((local, server)) => (local, Some(server)),
            None => (rest, None),
        };
        if let Some(prefix) = prefix {
            write!(f, "{prefix}:")?;
        }
        let count = local.chars().count();
        if count > KEEP {
            f.write_str("…")?;
            local
                .chars()
                .skip(count - KEEP)
                .try_for_each(|c| write!(f, "{c}"))?;
        } else {
            f.write_str(local)?;
        }
        if let Some(server) = server {
            write!(f, "@{server}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::LogChatId;

    #[test]
    fn whatsapp_phone_number_is_cut_to_last_four() {
        let shown = LogChatId("whatsapp:4915550100@s.whatsapp.net").to_string();
        assert_eq!(shown, "whatsapp:…0100@s.whatsapp.net");
        assert!(!shown.contains("491555"));
    }

    #[test]
    fn signal_phone_number_is_cut_to_last_four() {
        assert_eq!(LogChatId("signal:+4915550100").to_string(), "signal:…0100");
    }

    #[test]
    fn short_ids_stay_as_they_are() {
        assert_eq!(LogChatId("telegram:2").to_string(), "telegram:2");
        assert_eq!(
            LogChatId("whatsapp:111@s.whatsapp.net").to_string(),
            "whatsapp:111@s.whatsapp.net"
        );
    }

    #[test]
    fn id_without_prefix_is_redacted_too() {
        assert_eq!(LogChatId("4915550100").to_string(), "…0100");
    }
}
