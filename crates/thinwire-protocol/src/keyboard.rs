//! Buttons that a bot attaches to a message (Telegram inline keyboards) or
//! shows over the compose field (Telegram reply keyboards).
//!
//! The model keeps only what the app can do with a button. A press of a
//! callback button carries the data of that button, so a keyboard that the
//! bot changed in the meantime cannot make the press hit another button.

use std::fmt;

/// The rows of buttons of one keyboard, top row first.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct MessageKeyboard {
    pub rows: Vec<Vec<KeyboardButton>>,
}

impl MessageKeyboard {
    /// No row has a button.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.iter().all(Vec::is_empty)
    }
}

/// `Debug` shows only the shape: button text and data come from a bot and
/// can hold user data.
impl fmt::Debug for MessageKeyboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sizes: Vec<usize> = self.rows.iter().map(Vec::len).collect();
        f.debug_struct("MessageKeyboard")
            .field("rows", &sizes)
            .finish()
    }
}

/// One button: its label and what a press does.
#[derive(Clone, PartialEq, Eq)]
pub struct KeyboardButton {
    pub text: String,
    pub action: ButtonAction,
}

impl fmt::Debug for KeyboardButton {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyboardButton")
            .field("action", &self.action.kind())
            .finish_non_exhaustive()
    }
}

/// What a press of a button does.
#[derive(Clone, PartialEq, Eq)]
pub enum ButtonAction {
    /// Ask the bot: [`crate::AdapterCommand::PressButton`] with this data.
    Callback { data: String },
    /// Open this address in the browser.
    Url { url: String },
    /// Put this text on the clipboard.
    CopyText { text: String },
    /// Send the label of the button as a message (a reply keyboard).
    SendText,
    /// The app cannot do this kind of button (a web app, a payment, a
    /// game, an inline query, a login, a request for a phone number). The
    /// button shows, but it is off.
    Unsupported,
}

impl ButtonAction {
    /// A short name of the kind, for `Debug` and logs.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Callback { .. } => "callback",
            Self::Url { .. } => "url",
            Self::CopyText { .. } => "copy_text",
            Self::SendText => "send_text",
            Self::Unsupported => "unsupported",
        }
    }
}

impl fmt::Debug for ButtonAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind())
    }
}

/// The keyboard that a bot shows over the compose field of a chat.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ReplyKeyboard {
    /// The message that carries the keyboard.
    pub message_id: String,
    pub keyboard: MessageKeyboard,
    /// The keyboard goes away after one press.
    pub one_time: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_shows_no_text_and_no_data() {
        let keyboard = MessageKeyboard {
            rows: vec![vec![
                KeyboardButton {
                    text: "secret label".into(),
                    action: ButtonAction::Callback {
                        data: "secret data".into(),
                    },
                },
                KeyboardButton {
                    text: "x".into(),
                    action: ButtonAction::Url {
                        url: "https://secret.example".into(),
                    },
                },
            ]],
        };
        let shown = format!("{keyboard:?} {:?}", keyboard.rows[0]);
        assert!(!shown.contains("secret"), "{shown}");
        assert!(shown.contains("callback"));
    }

    #[test]
    fn a_keyboard_with_no_button_is_empty() {
        assert!(MessageKeyboard::default().is_empty());
        assert!(
            MessageKeyboard {
                rows: vec![Vec::new()]
            }
            .is_empty()
        );
    }
}
