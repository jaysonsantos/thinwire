//! What a frontend asks the core to do.
//!
//! Frontends send intents, never `AdapterCommand` values. The core checks
//! each intent against its state, then queues commands for the worker.
//! Each protocol has its own sub-enum, so protocol work does not collide on
//! one match. Typed secrets use [`SecretText`], so `Debug` stays clean.

use thinwire_protocol::ProtocolId;

use crate::state::{AuthKey, InboxFilter};
use crate::{SecretText, ThemeMode};

/// One user action. Frontends map clicks and keys to these values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Intent {
    /// Show the accounts and chats of one protocol.
    SelectProtocol(ProtocolId),
    /// Open a chat of the selected protocol and load its recent messages.
    SelectConversation { id: String },
    /// Limit the inbox to one protocol tab, or show All.
    SetFilter(InboxFilter),
    /// Inbox search text. Matches chat title and participant only.
    /// `SecretText` keeps the typed text out of `Debug`.
    SetSearch(SecretText),
    /// Reload the chat lists of the visible protocols.
    Refresh,
    /// Close the error block ("What happened / Why / What to do").
    DismissError,
    /// Enter or Escape on the center screen: first run or the login form.
    Key(AuthKey),
    /// Unsent text of one chat. It names the chat it was typed in, so a
    /// selection change in the same frame cannot move the text to another
    /// chat (PR #48 review). `SecretText` keeps the text out of `Debug`.
    SetDraft {
        protocol: ProtocolId,
        conversation_id: String,
        text: SecretText,
    },
    /// Send the draft of this chat. Dropped when the chat is no longer the
    /// selected one, so it never goes to another chat.
    SendDraft {
        protocol: ProtocolId,
        conversation_id: String,
    },
    /// Send a failed outgoing message again.
    Retry { message_id: String },
    /// Read the OS keychain again after a failed read. The read runs off the caller thread.
    RetryKeychain,
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
///
/// The core checks the order: `AcknowledgeRisk` only while the gate is on
/// screen, and `BeginLink` only after that. Any other order is dropped. A
/// future gated protocol must follow the same rule in the core, not in a
/// frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WhatsAppIntent {
    /// Show the full-screen ToS and ban risk gate.
    OpenRiskGate,
    /// Leave the gate or the pair screen. On the pair screen this is
    /// `CancelLink`: pairing stops and the acknowledgement resets.
    CloseGate,
    /// The user accepted the risk gate.
    AcknowledgeRisk,
    /// New text in the optional phone field of the pair screen.
    SetPhone(SecretText),
    /// Start linked-device pairing. The phone goes to the memory vault only.
    BeginLink,
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
            Intent::WhatsApp(WhatsAppIntent::SetPhone(SecretText::new("+15550100"))),
            Intent::SetDraft {
                protocol: ProtocolId::Telegram,
                conversation_id: "telegram:1".into(),
                text: SecretText::new("draft-fixture-3b7"),
            },
            Intent::SetSearch(SecretText::new("search-fixture-6e5")),
        ];
        for intent in intents {
            let shown = format!("{intent:?}");
            for secret in [
                "24680",
                "hunter2-fixture",
                "5550100",
                "draft-fixture-3b7",
                "search-fixture-6e5",
            ] {
                assert!(!shown.contains(secret), "{shown}");
            }
        }
    }
}
