//! What a frontend asks the core to do.
//!
//! Frontends send intents, never `AdapterCommand` values. The core checks
//! each intent against its state, then queues commands for the worker.
//! Each protocol has its own sub-enum, so protocol work does not collide on
//! one match. Typed secrets use [`SecretText`], so `Debug` stays clean.

use thinwire_protocol::ProtocolId;

use crate::{SecretText, ThemeMode};

/// One user action. Frontends map clicks and keys to these values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Intent {
    /// Show the accounts and chats of one protocol.
    SelectProtocol(ProtocolId),
    /// Open a chat of the selected protocol and load its recent messages.
    SelectConversation { id: String },
    /// Limit the inbox to one protocol. `None` shows every protocol.
    SetFilter(Option<ProtocolId>),
    /// Inbox search text. Matches chat title and participant only.
    SetSearch(String),
    /// Load the next page of the chat list for the selected protocol.
    LoadMoreChats,
    /// Unsent text for the selected chat. The core keeps one draft per chat.
    SetDraft(String),
    /// Send the draft of the selected chat.
    SendDraft,
    /// Send a failed outgoing message again.
    Retry { message_id: String },
    /// Store the light/dark preference. Disk I/O runs off the caller thread.
    SetTheme(ThemeMode),
    /// Close every client cleanly. The view reports when all stopped.
    Shutdown,
    /// Telegram login and account actions.
    Telegram(TelegramIntent),
    /// Experimental WhatsApp pairing. Only a `whatsapp-web` build acts on it.
    WhatsApp(WhatsAppIntent),
    /// Discord bot inbox. Only a `discord-bot` build acts on it.
    Discord(DiscordIntent),
    /// Slack workspace app. Only a `slack-oauth` build acts on it.
    Slack(SlackIntent),
}

/// A Telegram login form field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthField {
    ApiId,
    ApiHash,
    Phone,
    Code,
    Password,
}

/// Telegram account actions. Field values go to the memory vault only.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelegramIntent {
    /// Start Add account. Opens the phone step or the missing-credentials step.
    AddAccount,
    /// Open the Advanced API credential override.
    OpenApiOverride,
    /// New text in one login field. The view shows it back to the frontend.
    SetField(AuthField, SecretText),
    /// Submit the current login step.
    Submit,
    /// Leave the login flow and clear every typed field.
    Cancel,
    /// Go back from the code step to the phone step.
    ChangeNumber,
    /// Ask Telegram for a new login code.
    ResendCode,
}

/// Experimental WhatsApp actions. The risk gate comes before any pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhatsAppIntent {
    /// Show the full-screen ToS and ban risk gate.
    OpenRiskGate,
    /// Leave the gate or the pair screen.
    CloseGate,
    /// The user accepted the risk gate.
    AcknowledgeRisk,
    /// Start linked-device pairing. The phone stays in the memory vault.
    BeginLink { phone: SecretText },
    /// Stop pairing and drop the risk acknowledgement.
    CancelLink,
}

/// Discord bot inbox actions. No user-account path exists (ADR 0009).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscordIntent {
    /// Arm the bot client with the token that is in the keychain.
    Connect,
}

/// Slack workspace app actions (ADR 0008).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SlackIntent {
    /// Arm the workspace app with the tokens that are in the keychain.
    Connect,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_of_secret_intents_is_redacted() {
        let intents = [
            Intent::Telegram(TelegramIntent::SetField(
                AuthField::Code,
                SecretText::new("24680"),
            )),
            Intent::Telegram(TelegramIntent::SetField(
                AuthField::Password,
                SecretText::new("hunter2-fixture"),
            )),
            Intent::WhatsApp(WhatsAppIntent::BeginLink {
                phone: SecretText::new("+15550100"),
            }),
        ];
        for intent in intents {
            let shown = format!("{intent:?}");
            for secret in ["24680", "hunter2-fixture", "5550100"] {
                assert!(!shown.contains(secret), "{shown}");
            }
        }
    }
}
