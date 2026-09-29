//! Slack Web API and Socket Mode seams for the workspace-app inbox.
//!
//! The session actor talks only to these traits. The live build implements
//! them with `slack-morphism` (`live.rs`, feature `slack-oauth`). Tests use
//! in-process fakes. Tokens have a redacted `Debug` and never reach an event.

use std::fmt;
use std::future::Future;

use tokio::sync::mpsc::UnboundedSender;

use super::install::SlackInstalledWorkspace;

/// Workspace bot token (`xoxb-`). `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackBotToken(String);

impl SlackBotToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Worker-only access. Callers must not log the returned string.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SlackBotToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackBotToken(<redacted>)")
    }
}

/// Socket Mode app-level token (`xapp-`). `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackAppToken(String);

impl SlackAppToken {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Worker-only access. Callers must not log the returned string.
    #[must_use]
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SlackAppToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackAppToken(<redacted>)")
    }
}

/// Values the `oauth.v2.access` exchange needs. `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct SlackCodeExchange {
    pub client_id: String,
    pub client_secret: String,
    pub code: String,
}

impl fmt::Debug for SlackCodeExchange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SlackCodeExchange(<redacted>)")
    }
}

/// Result of a workspace install. The token is a bot token, never a user token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackInstallGrant {
    pub workspace: SlackInstalledWorkspace,
    pub bot_token: SlackBotToken,
}

/// Conversation kinds a workspace bot can read with the ADR 0008 scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackChannelKind {
    Public,
    Private,
    DirectMessage,
    GroupMessage,
}

/// One row of `conversations.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackChannel {
    pub id: String,
    /// Channel name without `#`. For a DM this is empty; `dm_user` names the peer.
    pub name: String,
    pub kind: SlackChannelKind,
    /// The bot is in the channel and can read its history.
    pub is_member: bool,
    /// The bot can post a top-level message. Independent of `is_member`.
    pub can_post: bool,
    /// Peer user id for a direct message.
    pub dm_user: Option<String>,
}

/// Posting permission from the Slack conversation object.
///
/// `is_read_only`, `is_thread_only`, `is_frozen`, and `is_archived` block a
/// top-level post. A public channel stays open to a non-member
/// (`chat:write.public`). A private channel, a DM, or a group DM requires
/// membership.
#[must_use]
#[cfg(any(test, feature = "slack-oauth"))]
pub(crate) fn channel_can_post(
    kind: SlackChannelKind,
    is_member: bool,
    is_read_only: bool,
    is_thread_only: bool,
    is_frozen: bool,
    is_archived: bool,
) -> bool {
    if is_read_only || is_thread_only || is_frozen || is_archived {
        return false;
    }
    match kind {
        SlackChannelKind::Public => true,
        SlackChannelKind::Private
        | SlackChannelKind::DirectMessage
        | SlackChannelKind::GroupMessage => is_member,
    }
}

/// One page of `conversations.list`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SlackChannelPage {
    pub channels: Vec<SlackChannel>,
    /// Empty or `None` means the last page.
    pub next_cursor: Option<String>,
}

/// Displayable posts from one `conversations.history` call.
///
/// Slack counts ignored subtypes (`channel_join` and the rest) toward
/// `limit`. `posts` can be shorter than that limit while older messages
/// still exist. `authoritative` is false then; `oldest_raw_ts` is the
/// oldest raw row the response covered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackHistoryPage {
    /// Displayable posts, newest first.
    pub posts: Vec<SlackPost>,
    /// Oldest raw `ts` in the Slack page, including ignored subtypes.
    /// `None` when Slack returned no rows.
    pub oldest_raw_ts: Option<String>,
    /// `posts` is the latest displayable window. Slack had no older rows,
    /// or this page already held `limit` displayable posts.
    pub authoritative: bool,
}

impl Default for SlackHistoryPage {
    fn default() -> Self {
        Self {
            posts: Vec::new(),
            oldest_raw_ts: None,
            authoritative: true,
        }
    }
}

impl SlackHistoryPage {
    /// Newest-first displayable posts that are the whole channel.
    #[must_use]
    pub fn complete(posts: Vec<SlackPost>) -> Self {
        let oldest_raw_ts = posts.last().map(|post| post.ts.clone());
        Self {
            posts,
            oldest_raw_ts,
            authoritative: true,
        }
    }
}

/// Slack has older history when it says so, or when it returns a cursor.
#[must_use]
#[cfg(any(test, feature = "slack-oauth"))]
pub(crate) fn history_has_more(has_more: Option<bool>, next_cursor: Option<&str>) -> bool {
    let cursor = next_cursor.is_some_and(|cursor| !cursor.trim().is_empty());
    has_more == Some(true) || cursor
}

/// A full displayable page, or the end of the channel, is the latest window.
#[must_use]
#[cfg(any(test, feature = "slack-oauth"))]
pub(crate) fn history_page_is_authoritative(shown: usize, limit: u16, has_more: bool) -> bool {
    !has_more || shown >= usize::from(limit)
}

/// One message from history, `chat.postMessage`, or a Socket Mode event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPost {
    pub channel: String,
    pub ts: String,
    pub user: Option<String>,
    /// Display name Slack sent with the message (bots, integrations).
    pub username: Option<String>,
    pub text: String,
    /// Sender id Slack uses to collapse a redelivery of the same post.
    pub client_msg_id: Option<String>,
}

/// Recoverable Slack failure. Display never includes tokens or message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackApiError {
    /// Slack answered `ok: false` with this error code (for example `not_in_channel`).
    Api(String),
    RateLimited,
    Network,
}

impl SlackApiError {
    /// Keep only a short `snake_case` Slack error code. Anything else is dropped.
    #[must_use]
    pub fn api(code: &str) -> Self {
        let safe = code.len() <= 64
            && !code.is_empty()
            && code
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit());
        Self::Api(if safe {
            code.to_string()
        } else {
            "unknown_error".into()
        })
    }
}

impl fmt::Display for SlackApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(code) => write!(f, "Slack returned {code}"),
            Self::RateLimited => f.write_str("Slack rate limit; try again soon"),
            Self::Network => f.write_str("could not reach Slack"),
        }
    }
}

impl std::error::Error for SlackApiError {}

/// Slack Web API calls the inbox needs. Implementations run on the tokio worker.
pub trait SlackWebApi: Send + Sync + 'static {
    /// `auth.test`: team and bot user for a stored bot token. App id may be empty.
    fn identify(
        &self,
        token: &SlackBotToken,
    ) -> impl Future<Output = Result<SlackInstalledWorkspace, SlackApiError>> + Send;

    fn exchange_code(
        &self,
        exchange: SlackCodeExchange,
    ) -> impl Future<Output = Result<SlackInstallGrant, SlackApiError>> + Send;

    fn list_channels(
        &self,
        token: &SlackBotToken,
        cursor: Option<String>,
    ) -> impl Future<Output = Result<SlackChannelPage, SlackApiError>> + Send;

    /// Newest-first displayable posts from `conversations.history`.
    ///
    /// `limit` is the raw page size. Ignored subtypes count toward it.
    fn history(
        &self,
        token: &SlackBotToken,
        channel: &str,
        limit: u16,
    ) -> impl Future<Output = Result<SlackHistoryPage, SlackApiError>> + Send;

    fn post_message(
        &self,
        token: &SlackBotToken,
        channel: &str,
        text: &str,
    ) -> impl Future<Output = Result<SlackPost, SlackApiError>> + Send;

    /// Display name for a user id (`users.info`).
    fn user_name(
        &self,
        token: &SlackBotToken,
        user: &str,
    ) -> impl Future<Output = Result<String, SlackApiError>> + Send;
}

/// Live event pushed from Socket Mode into the session actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackInbound {
    Message(SlackPost),
    /// `message_changed`: replace the body of an existing row.
    Edited {
        channel: String,
        ts: String,
        text: String,
    },
    /// `message_deleted`: drop the row.
    Deleted {
        channel: String,
        ts: String,
    },
    /// The app was uninstalled or the token was revoked.
    Revoked,
}

/// Running Socket Mode connection. Dropping it must stop the connection.
pub trait SlackEventStream: Send + 'static {
    fn stop(self) -> impl Future<Output = ()> + Send;
}

/// The workspace this process installed. One app-level Socket Mode token is
/// shared by every install of the publisher app, so events for other
/// workspaces must be dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackSocketScope {
    pub team_id: String,
    pub app_id: String,
}

/// Opens Socket Mode with the app-level token. Never on the UI thread.
pub trait SlackEventSource: Send + Sync + 'static {
    type Stream: SlackEventStream;

    fn connect(
        &self,
        app_token: SlackAppToken,
        scope: SlackSocketScope,
        sink: UnboundedSender<SlackInbound>,
    ) -> impl Future<Output = Result<Self::Stream, SlackApiError>> + Send;
}

/// Opens the workspace install page. The live build uses the system browser.
pub trait SlackBrowser: Send + Sync + 'static {
    /// Returns false when no browser could be opened. Must not log `url`.
    fn open(&self, url: &str) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_exchange_debug_are_redacted() {
        let bot = SlackBotToken::new("xoxb-test-bot");
        let app = SlackAppToken::new("xapp-test-app");
        let exchange = SlackCodeExchange {
            client_id: "client-id-test".into(),
            client_secret: "client-secret-test".into(),
            code: "code-test".into(),
        };
        let shown = format!("{bot:?} {app:?} {exchange:?}");
        assert!(!shown.contains("xoxb-test-bot"));
        assert!(!shown.contains("xapp-test-app"));
        assert!(!shown.contains("client-secret-test"));
        assert!(!shown.contains("code-test"));
        assert_eq!(bot.reveal(), "xoxb-test-bot");
        assert_eq!(app.reveal(), "xapp-test-app");
    }

    #[test]
    fn posting_follows_slack_conversation_fields() {
        use SlackChannelKind::{DirectMessage, GroupMessage, Private, Public};
        assert!(
            !channel_can_post(Public, true, true, false, false, false),
            "a member of a read-only channel cannot post"
        );
        assert!(
            !channel_can_post(Public, true, false, true, false, false),
            "a thread-only channel refuses a top-level post"
        );
        assert!(!channel_can_post(Public, true, false, false, true, false));
        assert!(!channel_can_post(Public, true, false, false, false, true));
        assert!(
            channel_can_post(Public, false, false, false, false, false),
            "a non-member can post in a public channel"
        );
        assert!(!channel_can_post(
            Private, false, false, false, false, false
        ));
        assert!(!channel_can_post(
            DirectMessage,
            false,
            false,
            false,
            false,
            false
        ));
        assert!(!channel_can_post(
            GroupMessage,
            false,
            false,
            false,
            false,
            false
        ));
        assert!(channel_can_post(Public, true, false, false, false, false));
        assert!(channel_can_post(Private, true, false, false, false, false));
    }

    #[test]
    fn api_error_keeps_only_safe_codes() {
        assert_eq!(
            SlackApiError::api("not_in_channel"),
            SlackApiError::Api("not_in_channel".into())
        );
        assert_eq!(
            SlackApiError::api("xoxb-123 leaked"),
            SlackApiError::Api("unknown_error".into())
        );
        assert_eq!(
            SlackApiError::api("").to_string(),
            "Slack returned unknown_error"
        );
    }

    #[test]
    fn a_cursor_means_more_history_and_an_empty_one_does_not() {
        assert!(!history_has_more(None, None));
        assert!(!history_has_more(Some(false), None));
        assert!(!history_has_more(Some(false), Some("  ")));
        assert!(history_has_more(Some(true), None));
        assert!(history_has_more(None, Some("next")));
        assert!(history_has_more(Some(false), Some("next")));
    }

    #[test]
    fn a_short_page_with_more_history_is_not_the_latest_window() {
        assert!(!history_page_is_authoritative(0, 50, true));
        assert!(!history_page_is_authoritative(49, 50, true));
        assert!(history_page_is_authoritative(50, 50, true));
        assert!(history_page_is_authoritative(0, 50, false));
        assert!(history_page_is_authoritative(2, 50, false));
    }
}
