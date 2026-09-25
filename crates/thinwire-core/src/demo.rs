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
//! - Fixed UTC times. The day breaks and the clock times depend on the
//!   local time zone, so a snapshot run sets `TZ=UTC`.

use std::sync::Arc;
use std::time::Duration;

use thinwire_protocol::demo::{DemoAdapter, DemoLogin, DemoOlder, DemoScript, DemoSend};
use thinwire_protocol::{
    AdapterEvent, AdapterHost, AdapterStatus, ChatMessage, Conversation, Delivery, ProtocolAdapter,
    ProtocolCapabilities, ProtocolId, TelegramApiSource, TelegramAuthError, TelegramAuthPhase,
    TelegramAuthStep, WhatsAppPhoneVault,
};
use tokio::runtime::Handle;

use crate::secrets::SecretStore;
use crate::settings::Settings;
use crate::{AuthField, Core, CoreConfig, Intent, SecretText, TelegramIntent};

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
/// A time with no change that ends a wait for answers.
const QUIET: Duration = Duration::from_millis(100);
/// The longest wait for all answers of one step.
const SETTLE_LIMIT: Duration = Duration::from_secs(5);

const ADA: &str = "telegram:demo-ada";
const BOOK_CLUB: &str = "telegram:demo-book-club";
const MILO: &str = "telegram:demo-milo";
const NORA: &str = "telegram:demo-nora";
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
}

impl Scenario {
    /// Every scenario, in a fixed order.
    pub const ALL: [Self; 16] = [
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
        let adapters: Vec<Box<dyn ProtocolAdapter>> = setup
            .scripts
            .into_iter()
            .map(|script| Box::new(DemoAdapter::new(script)) as Box<dyn ProtocolAdapter>)
            .collect();
        let host = AdapterHost::spawn_adapters(runtime, adapters);
        let config = CoreConfig::new(Settings::in_memory()).with_memory_secrets();
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
        settle(&mut core, runtime);
        for intent in setup.intents {
            core.dispatch(intent);
            settle(&mut core, runtime);
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

/// Pump until no change comes for [`QUIET`], at most [`SETTLE_LIMIT`].
fn settle(core: &mut Core, runtime: &Handle) {
    let mut signal = core.signal();
    let started = std::time::Instant::now();
    core.pump();
    while started.elapsed() < SETTLE_LIMIT {
        let changed =
            runtime.block_on(async { tokio::time::timeout(QUIET, signal.changed()).await.is_ok() });
        if !changed {
            break;
        }
        core.pump();
    }
    core.pump();
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
    }
}

fn send(id: &str, text: &str) -> [Intent; 3] {
    [
        select(id),
        Intent::SetDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: id.into(),
            text: SecretText::new(text),
        },
        Intent::SendDraft {
            protocol: ProtocolId::Telegram,
            conversation_id: id.into(),
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
        placeholder: false,
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
    }
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
            }
            if scenario == Scenario::SeveralProtocols {
                assert!(view.shows_in_switcher(ProtocolId::Slack), "{name}");
            }
        }
    }
}
