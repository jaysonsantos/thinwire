//! Demo scenarios (#120): fixed screens with invented data.
//!
//! [`Scenario::build`] starts a [`Core`] over scripted demo adapters
//! (`thinwire_protocol::demo`), drives it through [`Intent`] and
//! `AdapterEvent` only (ADR 0010), waits until every answer is applied, and
//! returns it. A frontend renders `core.view()`. The UI snapshot tests and
//! the headless example use the same scenarios.
//!
//! Rules:
//!
//! - Invented names and texts only. No real chat, account, or secret.
//! - No network, no OS keychain, no settings file.
//! - Fixed UTC times. `Clock::fixed_utc` formats day breaks, so the
//!   process time zone does not change a screen.

use std::sync::Arc;
use std::time::Duration;

use thinwire_protocol::demo::{
    DemoAdapter, DemoCounters, DemoLogin, DemoOlder, DemoScript, DemoSend,
};
use thinwire_protocol::{
    AdapterEvent, AdapterHost, AdapterStatus, ChatMessage, ChatTopic, Conversation, Delivery,
    ImageBytes, MessageLink, MessagePhoto, MessageTopic, PhotoSize, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, TelegramApiSource, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, WhatsAppPhoneVault, topic_mute_id,
};
use tokio::runtime::Handle;

use crate::secrets::SecretStore;
use crate::settings::Settings;
use crate::{AuthField, Clock, Core, CoreConfig, Intent, SecretText, TelegramIntent};

/// 2026-03-02 10:00 UTC: "now" in every scenario.
const NOW: i64 = 1_772_445_600;
const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
/// Messages in the long chat.
const LONG_CHAT_MESSAGES: usize = 120;
/// Messages in the short long chat, whose start one older page reaches.
const SHORT_HISTORY_MESSAGES: usize = 40;
/// Messages in one history page of the demo adapters.
const PAGE: usize = 30;
/// The longest wait on the change signal before the idle check runs again.
const SETTLE_POLL: Duration = Duration::from_millis(20);
/// The longest wait for all answers of one step. Reaching it is a bug.
const SETTLE_LIMIT: Duration = Duration::from_secs(5);

const ADA: &str = "telegram:demo-ada";
const BOOK_CLUB: &str = "telegram:demo-book-club";
const MILO: &str = "telegram:demo-milo";
const NORA: &str = "telegram:demo-nora";
const CAMP: &str = "telegram:demo-camp";
const SLACK_GENERAL: &str = "slack:demo-general";
const SLACK_DESIGN: &str = "slack:demo-design";

/// Invented lines for generated chats. One is long, so a bubble wraps.
const LINES: [&str; 8] = [
    "Morning! Did the parcel arrive?",
    "Yes, it came an hour ago.",
    "Great. I will bring the tent on Saturday.",
    concat!(
        "Can you also bring the small stove? Ours has a broken valve, and the shop ",
        "only has the big one until next week, which does not fit in the car with ",
        "four bags and the cooler.",
    ),
    "Sure.",
    "The weather looks good for both days.",
    "Let's leave at 8.",
    "👍",
];

/// One fixed screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// No account. The first-run screen.
    FirstRun,
    /// Telegram signed in, no chats yet.
    EmptyInbox,
    /// A chat with a long history, the newest page open.
    LongChat,
    /// The long chat while an older page loads.
    LongChatLoadingOlder,
    /// A chat whose first message is loaded: "Start of chat".
    LongChatStartOfChat,
    /// A group chat with sender names and a day break.
    GroupChat,
    /// A send that failed: a failed row with Retry, and the error.
    FailedSend,
    /// A send with no answer yet: a pending row.
    PendingSend,
    /// Login: the phone step.
    LoginPhone,
    /// Login: the code step.
    LoginCode,
    /// Login: the 2FA password step.
    Login2fa,
    /// Login: a wrong code.
    LoginError,
    /// The keychain read failed: the Try again screen.
    KeychainTryAgain,
    /// An error line on the status strip.
    StatusError,
    /// A protocol note on the status strip.
    StatusNotice,
    /// Telegram and Slack at once. Slack comes from its demo adapter, so the
    /// default build shows it.
    SeveralProtocols,
    /// Muted chats in the inbox (#153): one muted in thinwire, one muted in
    /// Telegram. A chat with no mute is open: its header shows Mute.
    MutedInbox,
    /// The chat muted in thinwire is open: its header shows Unmute.
    MutedHere,
    /// The chat muted in Telegram is open: its header shows a disabled
    /// "Muted in Telegram".
    MutedByProtocol,
    /// A forum is one sidebar row. The center pane lists its topics.
    ForumTopics,
    /// One topic of the forum is open.
    OpenForumTopic,
    /// A closed topic: a lock, and the compose box does not take text.
    ClosedTopic,
    /// A tall photo with a link in the caption.
    PhotoTall,
    /// A wide photo.
    PhotoWide,
    /// A photo that stays blurred until a click.
    PhotoSpoiler,
    /// A photo that did not load.
    PhotoFailed,
}

impl Scenario {
    /// Every scenario, in a fixed order.
    pub const ALL: [Self; 26] = [
        Self::FirstRun,
        Self::EmptyInbox,
        Self::LongChat,
        Self::LongChatLoadingOlder,
        Self::LongChatStartOfChat,
        Self::GroupChat,
        Self::FailedSend,
        Self::PendingSend,
        Self::LoginPhone,
        Self::LoginCode,
        Self::Login2fa,
        Self::LoginError,
        Self::KeychainTryAgain,
        Self::StatusError,
        Self::StatusNotice,
        Self::SeveralProtocols,
        Self::MutedInbox,
        Self::MutedHere,
        Self::MutedByProtocol,
        Self::ForumTopics,
        Self::OpenForumTopic,
        Self::ClosedTopic,
        Self::PhotoTall,
        Self::PhotoWide,
        Self::PhotoSpoiler,
        Self::PhotoFailed,
    ];

    /// A stable snake_case name, for example for a snapshot file.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::FirstRun => "first_run",
            Self::EmptyInbox => "empty_inbox",
            Self::LongChat => "long_chat",
            Self::LongChatLoadingOlder => "long_chat_loading_older",
            Self::LongChatStartOfChat => "long_chat_start_of_chat",
            Self::GroupChat => "group_chat",
            Self::FailedSend => "failed_send",
            Self::PendingSend => "pending_send",
            Self::LoginPhone => "login_phone",
            Self::LoginCode => "login_code",
            Self::Login2fa => "login_2fa",
            Self::LoginError => "login_error",
            Self::KeychainTryAgain => "keychain_try_again",
            Self::StatusError => "status_error",
            Self::StatusNotice => "status_notice",
            Self::SeveralProtocols => "several_protocols",
            Self::MutedInbox => "muted_inbox",
            Self::MutedHere => "muted_here",
            Self::MutedByProtocol => "muted_by_protocol",
            Self::ForumTopics => "forum_topics",
            Self::OpenForumTopic => "open_topic",
            Self::ClosedTopic => "closed_topic",
            Self::PhotoTall => "photo_tall",
            Self::PhotoWide => "photo_wide",
            Self::PhotoSpoiler => "photo_spoiler",
            Self::PhotoFailed => "photo_failed",
        }
    }

    /// The scenario with this name, if one has it.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    /// Start a core for this scenario and bring it to its screen.
    ///
    /// Call it from a thread that is not a worker of `runtime`, as for every
    /// [`Core`] method. It waits on the change signal only.
    #[must_use]
    pub fn build(self, runtime: &Handle) -> Core {
        let setup = self.setup();
        let secrets = Arc::new(if setup.keychain_failed {
            SecretStore::demo_read_failed()
        } else {
            SecretStore::demo_saved()
        });
        let counters = Arc::new(DemoCounters::default());
        let adapters: Vec<Box<dyn ProtocolAdapter>> = setup
            .scripts
            .into_iter()
            .map(|script| {
                Box::new(DemoAdapter::with_counters(script, Arc::clone(&counters)))
                    as Box<dyn ProtocolAdapter>
            })
            .collect();
        let adapters_count = u64::try_from(adapters.len()).unwrap_or(u64::MAX);
        let host = AdapterHost::spawn_adapters(runtime, adapters);
        let config = CoreConfig::new(Settings::in_memory())
            .with_memory_secrets()
            .with_clock(Clock::fixed_utc(NOW));
        let mut core = Core::with_host(
            runtime,
            config,
            secrets,
            Arc::new(WhatsAppPhoneVault::new()),
            host,
        );
        {
            let state = core.state_mut();
            state.extra_visible.extend(setup.visible);
            state.set_api_source(TelegramApiSource::with_publisher("12345", "demo-api-hash"));
        }
        settle(&mut core, runtime, &counters, adapters_count);
        for intent in setup.intents {
            core.dispatch(intent);
            settle(&mut core, runtime, &counters, adapters_count);
        }
        core
    }

    fn setup(self) -> Setup {
        let mut telegram = telegram_linked(inbox_rows());
        let mut setup = Setup::default();
        match self {
            Self::FirstRun => telegram = telegram_unlinked(),
            Self::EmptyInbox => telegram = telegram_linked(Vec::new()),
            Self::LongChat => setup.intents.push(select(ADA)),
            Self::LongChatLoadingOlder => {
                telegram.older = DemoOlder::Hold;
                setup.intents.extend([select(ADA), older(ADA)]);
            }
            Self::LongChatStartOfChat => {
                telegram
                    .history
                    .insert(ADA.into(), two_person_chat(ADA, SHORT_HISTORY_MESSAGES));
                setup.intents.extend([select(ADA), older(ADA)]);
            }
            Self::GroupChat => setup.intents.push(select(BOOK_CLUB)),
            Self::FailedSend => {
                telegram.send = DemoSend::Reject;
                setup.intents.extend(send(ADA, "See you at 6?"));
            }
            Self::PendingSend => {
                telegram.send = DemoSend::Hold;
                setup.intents.extend(send(ADA, "On my way."));
            }
            Self::LoginPhone | Self::LoginCode | Self::Login2fa | Self::LoginError => {
                telegram = telegram_login(self == Self::LoginError);
                setup.intents.extend(login_steps(self));
            }
            Self::KeychainTryAgain => {
                telegram = telegram_unlinked();
                setup.keychain_failed = true;
            }
            Self::StatusError => telegram.start.push(AdapterEvent::Status {
                protocol: ProtocolId::Telegram,
                status: AdapterStatus::Error,
                detail: "Telegram connection lost. Trying again in 10 seconds.".into(),
            }),
            Self::StatusNotice => telegram.start.push(AdapterEvent::Notice {
                protocol: ProtocolId::Telegram,
                text: "Older chats load in the background.".into(),
            }),
            Self::SeveralProtocols => {
                setup.scripts.push(slack_linked());
                setup.visible.push(ProtocolId::Slack);
                setup.intents.push(select(ADA));
            }
            Self::MutedInbox | Self::MutedHere | Self::MutedByProtocol => {
                telegram = telegram_linked(muted_rows());
                setup.intents.push(Intent::SetChatMute {
                    protocol: ProtocolId::Telegram,
                    conversation_id: MILO.into(),
                    muted: true,
                });
                setup.intents.push(select(match self {
                    Self::MutedHere => MILO,
                    Self::MutedByProtocol => NORA,
                    _ => ADA,
                }));
            }
            Self::ForumTopics | Self::OpenForumTopic | Self::ClosedTopic => {
                telegram = forum_script();
                setup.intents.push(Intent::SetChatMute {
                    protocol: ProtocolId::Telegram,
                    conversation_id: topic_mute_id(CAMP, MessageTopic::Forum(3)),
                    muted: true,
                });
                setup.intents.push(select(CAMP));
                let topic = match self {
                    Self::OpenForumTopic => Some(MessageTopic::Forum(2)),
                    Self::ClosedTopic => Some(MessageTopic::Forum(4)),
                    Self::ForumTopics => None,
                    _ => None,
                };
                if let Some(topic) = topic {
                    setup.intents.push(Intent::SelectTopic {
                        conversation_id: CAMP.into(),
                        topic,
                    });
                }
            }
            Self::PhotoTall | Self::PhotoWide | Self::PhotoSpoiler | Self::PhotoFailed => {
                let photo = match self {
                    Self::PhotoTall => photo_message(PhotoKind::Tall),
                    Self::PhotoWide => photo_message(PhotoKind::Wide),
                    Self::PhotoSpoiler => photo_message(PhotoKind::Spoiler),
                    Self::PhotoFailed => photo_message(PhotoKind::Failed),
                    _ => unreachable!(),
                };
                let preview = if photo.body.is_empty() {
                    "Photo".to_string()
                } else {
                    photo.body.clone()
                };
                set_ada_preview(&mut telegram, &preview);
                telegram.history.insert(ADA.into(), photo_chat(photo));
                setup.intents.push(select(ADA));
            }
        }
        setup.scripts.insert(0, telegram);
        // Every catalog protocol has an adapter, so Shutdown gets a Stopped
        // from each one (PR #122 review).
        for caps in thinwire_protocol::catalog() {
            if !setup.scripts.iter().any(|script| script.caps.id == caps.id) {
                setup.scripts.push(DemoScript::silent(caps, NOW));
            }
        }
        setup
    }
}

/// What one scenario starts with.
#[derive(Default)]
struct Setup {
    scripts: Vec<DemoScript>,
    /// Protocols that the shell shows even with their feature off.
    visible: Vec<ProtocolId>,
    intents: Vec<Intent>,
    /// Use a store whose keychain read failed: the Try again screen.
    keychain_failed: bool,
}

/// Pump until no work is left: every demo adapter started, the demo
/// adapters handled every command the core sent, and the core read every
/// event they sent (#120 qa). No timing
/// guess: a slow machine only waits longer.
///
/// # Panics
///
/// After [`SETTLE_LIMIT`] with work left. That is a bug in a scenario.
fn settle(core: &mut Core, runtime: &Handle, counters: &DemoCounters, adapters: u64) {
    let mut signal = core.signal();
    let started = std::time::Instant::now();
    loop {
        core.pump();
        let (sent, read) = core.work_counts();
        let all_started = counters.started() == adapters;
        if all_started && counters.handled() == sent && counters.emitted() == read {
            return;
        }
        assert!(
            started.elapsed() < SETTLE_LIMIT,
            "demo did not settle: sent {sent}, handled {}, emitted {}, read {read}",
            counters.handled(),
            counters.emitted()
        );
        runtime.block_on(async {
            let _ = tokio::time::timeout(SETTLE_POLL, signal.changed()).await;
        });
    }
}

/// The screen of a built scenario as text: the status line, the center
/// view, the rows, the thread, the older state, and the error. Two builds of
/// one scenario give the same text.
#[must_use]
pub fn summary(core: &Core) -> String {
    use std::fmt::Write as _;

    let view = core.view();
    let mut text = String::new();
    let _ = writeln!(text, "status: {}", view.status_line());
    let _ = writeln!(
        text,
        "screen: {:?} auth: {:?}",
        view.center_view(),
        view.auth
    );
    for row in view.visible_conversations() {
        let mark = if view.selected_conversation.as_deref() == Some(row.id.as_str()) {
            '>'
        } else {
            ' '
        };
        let _ = writeln!(
            text,
            "{mark} {} | {} | {}",
            row.title, row.preview, row.unread
        );
    }
    for message in view.selected_messages() {
        let _ = writeln!(
            text,
            "    {} {}: {} [{:?}]",
            message.sent_at, message.sender, message.body, message.delivery
        );
    }
    let _ = writeln!(text, "older: {:?}", view.older_state());
    if let Some(error) = &view.error {
        let _ = writeln!(
            text,
            "error: {} | {} | {}",
            error.happened, error.why, error.next
        );
    }
    text
}

fn caps(protocol: ProtocolId) -> ProtocolCapabilities {
    thinwire_protocol::catalog()
        .into_iter()
        .find(|caps| caps.id == protocol)
        .expect("every protocol is in the catalog")
}

fn select(id: &str) -> Intent {
    Intent::SelectConversation { id: id.into() }
}

fn older(id: &str) -> Intent {
    Intent::LoadOlderMessages {
        protocol: ProtocolId::Telegram,
        conversation_id: id.into(),
        topic: None,
    }
}

fn send(id: &str, text: &str) -> [Intent; 3] {
    [
        select(id),
        Intent::SetDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: id.into(),
            topic: None,
            text: SecretText::new(text),
        },
        Intent::SendDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: id.into(),
            topic: None,
        },
    ]
}

fn field(field: AuthField, value: &str) -> Intent {
    Intent::Telegram(TelegramIntent::SetField(field, SecretText::new(value)))
}

fn login_steps(scenario: Scenario) -> Vec<Intent> {
    let submit = || Intent::Telegram(TelegramIntent::Submit);
    let mut intents = vec![Intent::Telegram(TelegramIntent::AddAccount)];
    if scenario == Scenario::LoginPhone {
        return intents;
    }
    intents.extend([field(AuthField::Phone, "+1 555 0100"), submit()]);
    if scenario == Scenario::LoginCode {
        return intents;
    }
    intents.extend([field(AuthField::Code, "12345"), submit()]);
    intents
}

/// Telegram signed in, with these rows and the demo histories.
fn telegram_linked(rows: Vec<Conversation>) -> DemoScript {
    let mut script = DemoScript::linked(caps(ProtocolId::Telegram), rows, NOW);
    // Telegram reaches its inbox through the login phase Ready.
    script.start.insert(
        0,
        AdapterEvent::TelegramAuth {
            phase: TelegramAuthPhase::Ready,
        },
    );
    script.page = PAGE;
    script
        .history
        .insert(ADA.into(), two_person_chat(ADA, LONG_CHAT_MESSAGES));
    script.history.insert(BOOK_CLUB.into(), group_chat());
    script.history.insert(MILO.into(), two_person_chat(MILO, 6));
    script.history.insert(NORA.into(), two_person_chat(NORA, 3));
    script
}

fn telegram_unlinked() -> DemoScript {
    DemoScript::unlinked(caps(ProtocolId::Telegram), NOW)
}

/// Telegram with no account, and answers for each login step. With
/// `wrong_code`, the code step is refused.
fn telegram_login(wrong_code: bool) -> DemoScript {
    let mut script = telegram_unlinked();
    let code = if wrong_code {
        DemoLogin::Reject(TelegramAuthError::CodeInvalid)
    } else {
        DemoLogin::Phase(TelegramAuthPhase::NeedTwoFactor)
    };
    script.login = vec![
        (
            TelegramAuthStep::ApiCredentials,
            DemoLogin::Phase(TelegramAuthPhase::NeedPhone),
        ),
        (
            TelegramAuthStep::Phone,
            DemoLogin::Phase(TelegramAuthPhase::NeedCode),
        ),
        (TelegramAuthStep::Code, code),
    ];
    script
}

fn slack_linked() -> DemoScript {
    let rows = vec![
        row(
            ProtocolId::Slack,
            SLACK_GENERAL,
            "#general",
            "Lunch at 12?",
            2,
            true,
        ),
        row(
            ProtocolId::Slack,
            SLACK_DESIGN,
            "#design",
            "New icons are up.",
            1,
            true,
        ),
    ];
    let mut script = DemoScript::linked(caps(ProtocolId::Slack), rows, NOW);
    script
        .history
        .insert(SLACK_GENERAL.into(), two_person_chat(SLACK_GENERAL, 4));
    script
}

fn inbox_rows() -> Vec<Conversation> {
    vec![
        row(
            ProtocolId::Telegram,
            ADA,
            "Ada Park",
            "Let's leave at 8.",
            4,
            false,
        ),
        row(
            ProtocolId::Telegram,
            BOOK_CLUB,
            "Book club",
            "Nora: Chapter 5 next.",
            3,
            true,
        ),
        row(ProtocolId::Telegram, MILO, "Milo Chen", "Sure.", 2, false),
        row(
            ProtocolId::Telegram,
            NORA,
            "Nora Silva",
            "Thanks!",
            1,
            false,
        ),
    ]
}

/// The inbox with unread counts on two muted chats: Milo (muted in thinwire
/// by the scenario) and Nora (muted in Telegram).
fn muted_rows() -> Vec<Conversation> {
    inbox_rows()
        .into_iter()
        .map(|row| match row.id.as_str() {
            MILO => Conversation { unread: 5, ..row },
            NORA => Conversation {
                unread: 1,
                muted: true,
                ..row
            },
            _ => row,
        })
        .collect()
}

fn forum_script() -> DemoScript {
    let mut camp = row(
        ProtocolId::Telegram,
        CAMP,
        "Camp forum",
        "Meet at the ridge.",
        4,
        true,
    );
    camp.has_topics = true;
    let mut script = telegram_linked(vec![camp]);
    script.start.extend([
        forum_topic(1, "General", 1, NOW - 5 * HOUR, true, false),
        forum_topic(2, "Trail", 2, NOW - HOUR, false, false),
        forum_topic(3, "Food", 4, NOW - 2 * HOUR, false, false),
        forum_topic(4, "Archive", 0, NOW - 6 * HOUR, false, true),
    ]);
    script.history.insert(CAMP.into(), forum_messages());
    script
}

fn forum_topic(
    id: i64,
    name: &str,
    unread: u32,
    last_at: i64,
    general: bool,
    closed: bool,
) -> AdapterEvent {
    AdapterEvent::TopicUpsert {
        topic: ChatTopic {
            protocol: ProtocolId::Telegram,
            conversation_id: CAMP.into(),
            topic: MessageTopic::Forum(id),
            name: name.into(),
            unread,
            last_at,
            general,
            hidden: false,
            closed,
            muted: false,
        },
    }
}

fn forum_messages() -> Vec<ChatMessage> {
    let mut welcome = message(CAMP, 1, "Ada", "Welcome to camp.", NOW - 5 * HOUR);
    welcome.topic = Some(MessageTopic::Forum(1));
    let mut meet = message(CAMP, 2, "Ada", "Meet at the ridge.", NOW - HOUR);
    meet.topic = Some(MessageTopic::Forum(2));
    let mut map = message(CAMP, 3, "You", "I will bring the map.", NOW - HOUR + MINUTE);
    map.topic = Some(MessageTopic::Forum(2));
    let mut packed = message(CAMP, 4, "Ada", "The box is packed.", NOW - 6 * HOUR);
    packed.topic = Some(MessageTopic::Forum(4));
    vec![welcome, meet, map, packed]
}

fn row(
    protocol: ProtocolId,
    id: &str,
    title: &str,
    preview: &str,
    order: i64,
    is_group: bool,
) -> Conversation {
    Conversation {
        protocol,
        id: id.into(),
        title: title.into(),
        participant: title.into(),
        preview: preview.into(),
        unread: u32::from(order == 3) * 2,
        order,
        last_at: NOW - (4 - order) * HOUR,
        is_group,
        writable: true,
        muted: false,
        placeholder: false,
        has_topics: false,
    }
}

fn message(chat: &str, index: usize, sender: &str, body: &str, sent_at: i64) -> ChatMessage {
    let protocol = if chat.starts_with("slack:") {
        ProtocolId::Slack
    } else {
        ProtocolId::Telegram
    };
    ChatMessage {
        protocol,
        conversation_id: chat.into(),
        id: format!("{chat}:{index}"),
        sender: sender.into(),
        body: body.into(),
        outbound: sender == "You",
        delivery: Delivery::Sent,
        sent_at,
        arrival: thinwire_protocol::Arrival::History,
        topic: None,
        links: Vec::new(),
        photo: None,
    }
}

const TALL_PNG: &[u8] = include_bytes!("../assets/photo-tall.png");
const WIDE_PNG: &[u8] = include_bytes!("../assets/photo-wide.png");
const MINI_PNG: &[u8] = include_bytes!("../assets/photo-mini.png");

enum PhotoKind {
    Tall,
    Wide,
    Spoiler,
    Failed,
}

fn set_ada_preview(script: &mut DemoScript, preview: &str) {
    for event in &mut script.start {
        if let AdapterEvent::ConversationUpsert { conversation } = event
            && conversation.id == ADA
        {
            conversation.preview = preview.to_owned();
        }
    }
}

fn photo_chat(photo: ChatMessage) -> Vec<ChatMessage> {
    vec![
        message(ADA, 0, "Ada", "The trail photos are in.", NOW - 2 * HOUR),
        message(ADA, 1, "You", "Send one.", NOW - HOUR),
        photo,
    ]
}

fn photo_message(kind: PhotoKind) -> ChatMessage {
    let (body, width, height, bytes, mini, spoiler, failed) = match kind {
        PhotoKind::Tall => (
            "Map: https://example.com/trail",
            144,
            360,
            Some(TALL_PNG),
            None,
            false,
            false,
        ),
        PhotoKind::Wide => ("Hello", 360, 144, Some(WIDE_PNG), None, false, false),
        PhotoKind::Spoiler => ("", 280, 280, None, Some(MINI_PNG), true, false),
        PhotoKind::Failed => ("Hello", 360, 144, None, None, false, true),
    };
    let mut row = message(ADA, 2, "Ada", body, NOW - 10 * MINUTE);
    if let Some(start) = body.find("https://") {
        row.links.push(MessageLink {
            start,
            end: body.len(),
            url: "https://example.com/trail".into(),
        });
    }
    row.photo = Some(MessagePhoto {
        width,
        height,
        mini: mini.map(|bytes| ImageBytes::new(bytes.to_vec())),
        sizes: vec![PhotoSize {
            width,
            height,
            file_id: Some(1),
            path: None,
            bytes: bytes.map(|bytes| ImageBytes::new(bytes.to_vec())),
        }],
        has_spoiler: spoiler,
        is_secret: false,
        failed,
    });
    row
}

/// A chat of two people, oldest first, 20 minutes apart, ending one hour
/// before [`NOW`]. Long chats cross several days.
fn two_person_chat(chat: &str, count: usize) -> Vec<ChatMessage> {
    let other = chat.rsplit('-').next().unwrap_or("demo");
    let other = format!("{}{}", other[..1].to_uppercase(), &other[1..]);
    (0..count)
        .map(|index| {
            let sender = if index % 3 == 1 {
                "You"
            } else {
                other.as_str()
            };
            let back = i64::try_from(count - index).unwrap_or_default() * 20 * MINUTE;
            message(
                chat,
                index,
                sender,
                LINES[index % LINES.len()],
                NOW - HOUR - back,
            )
        })
        .collect()
}

/// A group chat over two days, so a day break shows.
fn group_chat() -> Vec<ChatMessage> {
    let lines = [
        ("Nora", "Who has read chapter 4?", -DAY - 3 * HOUR),
        ("Milo", "Me, last night.", -DAY - 2 * HOUR),
        ("Ada", "Halfway through.", -DAY - 2 * HOUR + 5 * MINUTE),
        ("You", "Done. The ending surprised me.", -DAY - HOUR),
        ("Nora", "Same here!", -3 * HOUR),
        ("Milo", "Shall we meet on Thursday?", -2 * HOUR),
        ("Ada", "Thursday works.", -2 * HOUR + 2 * MINUTE),
        ("Nora", "Chapter 5 next.", -HOUR),
    ];
    lines
        .iter()
        .enumerate()
        .map(|(index, (sender, body, offset))| {
            message(BOOK_CLUB, index, sender, body, NOW + offset)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mutes::ChatMute;
    use crate::state::{AuthScreen, OlderState};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn every_scenario_has_a_unique_name() {
        let mut names: Vec<&str> = Scenario::ALL
            .iter()
            .map(|scenario| scenario.name())
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Scenario::ALL.len());
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
        }
    }

    /// PR #122 review: every catalog protocol has a demo adapter, so a
    /// frontend that waits for all adapters to stop does not time out.
    #[test]
    fn a_demo_core_stops_every_adapter() {
        let runtime = runtime();
        for scenario in [Scenario::FirstRun, Scenario::SeveralProtocols] {
            let mut core = scenario.build(runtime.handle());
            assert!(
                core.block_until_stopped(Duration::from_secs(2)),
                "{}: every adapter stops",
                scenario.name()
            );
        }
    }

    /// #120 qa: a scenario gives the same screen on each build.
    #[test]
    fn a_scenario_builds_the_same_screen_twice() {
        let runtime = runtime();
        for scenario in Scenario::ALL {
            let first = summary(&scenario.build(runtime.handle()));
            let second = summary(&scenario.build(runtime.handle()));
            assert_eq!(first, second, "{}", scenario.name());
        }
    }

    /// The view clock of a demo core is fixed at the scenario's now.
    #[test]
    fn a_demo_core_has_a_fixed_clock() {
        let runtime = runtime();
        let core = Scenario::LongChat.build(runtime.handle());
        assert_eq!(core.view().now(), Clock::fixed_utc(NOW).now());
    }

    /// Each scenario reaches its screen. One runtime serves all of them.
    #[test]
    fn every_scenario_reaches_its_screen() {
        let runtime = runtime();
        for scenario in Scenario::ALL {
            let core = scenario.build(runtime.handle());
            let view = core.view();
            let name = scenario.name();
            match scenario {
                Scenario::FirstRun => {
                    assert!(!view.telegram_ready(), "{name}");
                    assert!(view.visible_conversations().is_empty(), "{name}");
                }
                Scenario::KeychainTryAgain => {
                    assert!(view.secrets.read_failed(), "{name}");
                }
                Scenario::EmptyInbox => {
                    assert!(view.telegram_ready(), "{name}");
                    assert!(view.visible_conversations().is_empty(), "{name}");
                }
                Scenario::LongChat | Scenario::SeveralProtocols => {
                    assert_eq!(view.selected_messages().len(), PAGE, "{name}");
                }
                Scenario::LongChatLoadingOlder => {
                    assert_eq!(view.older_state(), OlderState::Loading, "{name}");
                }
                Scenario::LongChatStartOfChat => {
                    assert_eq!(view.older_state(), OlderState::StartOfChat, "{name}");
                    assert_eq!(
                        view.selected_messages().len(),
                        SHORT_HISTORY_MESSAGES,
                        "{name}"
                    );
                }
                Scenario::GroupChat => {
                    assert_eq!(view.selected_messages().len(), 8, "{name}");
                }
                Scenario::FailedSend => {
                    let last = view.selected_messages().last().expect("rows");
                    assert_eq!(last.delivery, Delivery::Failed, "{name}");
                    assert!(view.error.is_some(), "{name}");
                }
                Scenario::PendingSend => {
                    let last = view.selected_messages().last().expect("rows");
                    assert_eq!(last.delivery, Delivery::Pending, "{name}");
                    assert!(!view.can_send(), "{name}: locked while it sends");
                }
                Scenario::LoginPhone => assert_eq!(view.auth, AuthScreen::TelegramPhone, "{name}"),
                Scenario::LoginCode => assert_eq!(view.auth, AuthScreen::TelegramCode, "{name}"),
                Scenario::Login2fa => assert_eq!(view.auth, AuthScreen::Telegram2fa, "{name}"),
                Scenario::LoginError => {
                    assert_eq!(view.auth, AuthScreen::TelegramCode, "{name}");
                    assert!(
                        view.error.is_some() || view.auth_rejection.is_some(),
                        "{name}"
                    );
                }
                Scenario::StatusError => {
                    assert!(view.status_line().contains("connection lost"), "{name}");
                }
                Scenario::StatusNotice => {
                    assert!(view.notice(ProtocolId::Telegram).is_some(), "{name}");
                }
                Scenario::MutedInbox | Scenario::MutedHere | Scenario::MutedByProtocol => {
                    let open = view.selected_conversation.as_deref().expect("open chat");
                    let expected = match scenario {
                        Scenario::MutedHere => ChatMute::Here,
                        Scenario::MutedByProtocol => ChatMute::Protocol,
                        _ => ChatMute::None,
                    };
                    assert_eq!(
                        view.chat_mute(ProtocolId::Telegram, open),
                        expected,
                        "{name}"
                    );
                    assert_eq!(
                        view.chat_mute(ProtocolId::Telegram, MILO),
                        ChatMute::Here,
                        "{name}"
                    );
                    assert_eq!(
                        view.chat_mute(ProtocolId::Telegram, NORA),
                        ChatMute::Protocol,
                        "{name}"
                    );
                }
                Scenario::ForumTopics | Scenario::OpenForumTopic | Scenario::ClosedTopic => {
                    assert!(
                        view.showing_topic_list() || view.open_topic().is_some(),
                        "{name}"
                    );
                    let rows = view.visible_conversations();
                    let row = rows.iter().find(|row| row.id == CAMP).expect("forum row");
                    match scenario {
                        Scenario::ForumTopics => {
                            assert!(view.showing_topic_list(), "{name}");
                            assert_eq!(view.selected_topics().len(), 4, "{name}");
                            assert_eq!(view.display_unread(row), 3, "{name}");
                            assert_eq!(view.selected_topics()[0].name, "General", "{name}");
                            assert_eq!(view.selected_topics()[1].name, "Trail", "{name}");
                        }
                        Scenario::OpenForumTopic => {
                            let topic = view.open_topic().expect("topic");
                            assert_eq!(topic.name, "Trail", "{name}");
                            assert_eq!(view.compose_hint(), "Message in Trail", "{name}");
                            assert!(!view.selected_messages().is_empty(), "{name}");
                            assert_eq!(view.display_unread(row), 1, "{name}");
                        }
                        Scenario::ClosedTopic => {
                            assert!(view.topic_closed(), "{name}");
                            assert!(!view.can_send(), "{name}");
                            assert_eq!(view.display_unread(row), 3, "{name}");
                        }
                        _ => {}
                    }
                }
                Scenario::PhotoTall
                | Scenario::PhotoWide
                | Scenario::PhotoSpoiler
                | Scenario::PhotoFailed => {
                    let messages = view.selected_messages();
                    let photo = messages
                        .last()
                        .and_then(|message| message.photo.as_ref())
                        .expect("photo");
                    match scenario {
                        Scenario::PhotoTall => {
                            assert!(!photo.failed, "{name}");
                            assert!(
                                photo.sizes.iter().any(|size| size.bytes.is_some()),
                                "{name}"
                            );
                            assert!(
                                messages
                                    .last()
                                    .is_some_and(|message| !message.links.is_empty()),
                                "{name}"
                            );
                        }
                        Scenario::PhotoWide => {
                            assert!(
                                photo.sizes.iter().any(|size| size.bytes.is_some()),
                                "{name}"
                            );
                        }
                        Scenario::PhotoSpoiler => {
                            assert!(photo.has_spoiler, "{name}");
                            assert!(photo.mini.is_some(), "{name}");
                            assert!(
                                photo.sizes.iter().all(|size| size.bytes.is_none()),
                                "{name}"
                            );
                        }
                        Scenario::PhotoFailed => {
                            assert!(photo.failed, "{name}");
                        }
                        _ => {}
                    }
                }
            }
            if scenario == Scenario::SeveralProtocols {
                assert!(view.shows_in_switcher(ProtocolId::Slack), "{name}");
            }
        }
    }
}
