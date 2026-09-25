//! Source-text tests for the egui files. State tests live in `thinwire-core`.

#![allow(
    unused_imports,
    reason = "shared import list with the core state tests"
)]

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[cfg(feature = "whatsapp-web")]
use thinwire_protocol::WhatsAppPhoneVault;
use thinwire_protocol::{
    AdapterCommand, AdapterEvent, AdapterStatus, ChatMessage, Conversation, Delivery,
    DiscordAdapter, ProtocolCapabilities, ProtocolId, TelegramApiSource, TelegramAuthError,
    TelegramAuthPhase, TelegramAuthStep, TelegramCodeVia, TelegramSecretVault, catalog,
    parse_telegram_chat_id, telegram_api_available,
};

use thinwire_core::secrets::OsBackend;
use thinwire_core::secrets::{SecretKey, SecretStore};
use thinwire_core::state::test_support::*;
use thinwire_core::state::*;

#[test]
fn inbox_and_thread_scroll_and_show_load_states() {
    let ui = include_str!("ui.rs");
    let left = &ui[ui.find("fn left_panel").expect("left panel")..];
    let left = &left[..left.find("\nfn ").expect("next fn")];
    assert!(left.contains("ScrollArea::vertical()"));
    assert!(ui.contains(".stick_to_bottom(true)"));
    assert!(ui.contains("scroll_to_me"));
    assert!(ui.contains("Loading chats…"));
    assert!(ui.contains("Loading messages…"));
    assert!(ui.contains("No messages in this chat."));
    assert!(!ui.contains("No conversations yet."));
}

#[test]
fn compose_ui_uses_enter_multiline_and_disabled_send() {
    let ui = include_str!("ui.rs");
    assert!(ui.contains("edited(ui, &snapshot.compose"));
    assert!(ui.contains("TextEdit::multiline(text)"));
    assert!(ui.contains("if enter && !other && !shift {"));
    assert!(ui.contains("add_enabled(snapshot.can_send()"));
    assert!(ui.contains("request_focus(compose_id)"));
    assert!(ui.contains("\"Not sent\""));
    assert!(ui.contains("\"Retry\""));
}

#[test]
fn login_copy_has_no_developer_words_and_one_cancel() {
    let auth = include_str!("auth.rs");
    let draw = &auth[auth.find("pub(crate) fn draw(").expect("draw")..];
    let draw = &draw[..draw.find("fn need_credentials(").expect("next")];
    for word in ["adapter", "UI thread", "TDLib", "tdlib-rs", "secret store"] {
        assert!(!draw.contains(word), "{word}");
    }
    let steps = &auth[auth.find("fn telegram_phone(").expect("phone")..];
    for word in [
        "adapter",
        "UI thread",
        "TDLib",
        "secret store",
        "Optional",
        "optional",
    ] {
        assert!(!steps.contains(word), "{word}");
    }
    assert!(!TELEGRAM_STUB_UNTIL_READY.contains("TDLib"));
    assert!(auth.contains("\"Two-step verification\""));
    assert!(auth.contains("\"Enter your Telegram password.\""));
    assert!(auth.contains("\"12345\"") && auth.contains("\"word or phrase\""));
    assert!(auth.contains("\"Change number\""));
    assert!(auth.contains("\"Send a new code\""));
    let code = &auth[auth.find("fn telegram_code(").expect("code")..];
    let code = &code[..code.find("\nfn ").expect("next")];
    assert!(
        !code.contains("password(true)"),
        "the code field shows digits"
    );
    let ui = include_str!("ui.rs");
    let strip = &ui[ui.find("fn status_strip(").expect("strip")..];
    let strip = &strip[..strip.find("\nfn ").expect("next")];
    assert!(!strip.contains("\"Cancel\""));
}

#[test]
fn thread_draws_bubbles_by_side_with_times_and_day_breaks() {
    let ui = include_str!("ui.rs");
    let bubble = &ui[ui.find("fn bubble(").expect("bubble")..];
    let bubble = &bubble[..bubble.find("\nfn ").expect("next")];
    assert!(bubble.contains("palette.out"));
    assert!(bubble.contains("palette.surface"));
    assert!(bubble.contains("egui::Align::Max"));
    assert!(bubble.contains("egui::Align::Min"));
    assert!(bubble.contains("layout.day_break"));
    assert!(bubble.contains("layout.show_sender"));
    assert!(bubble.contains("layout.time"));
    assert!(bubble.contains(".selectable(true)"));
    assert!(bubble.contains(".wrap()"));
    assert!(
        !bubble.contains("Color32::from_rgb"),
        "colors come from the theme"
    );
    assert!(ui.contains("thread_rows(snapshot.selected_messages(), is_group"));
    assert!(ui.contains("list_time(row.last_at, &now)"));
}

#[test]
fn keychain_notice_shows_only_when_secrets_stay_in_memory() {
    use super::ui::{KEYCHAIN_UNAVAILABLE_NOTICE, keychain_notice};
    assert_eq!(
        keychain_notice(SecretStore::memory().persistence()),
        Some(KEYCHAIN_UNAVAILABLE_NOTICE)
    );
    let attaching = SecretStore::detached_for_test();
    assert_eq!(
        keychain_notice(attaching.persistence()),
        None,
        "no notice while loading"
    );
    attaching.complete_ready_attach_for_test(&[]);
    attaching.set_backend_for_test(OsBackend::SecretService);
    assert_eq!(
        keychain_notice(attaching.persistence()),
        None,
        "Secret Service keeps it"
    );
    attaching.set_backend_for_test(OsBackend::Native);
    assert_eq!(keychain_notice(attaching.persistence()), None);
    attaching.set_backend_for_test(OsBackend::KernelKeyring);
    assert_eq!(
        keychain_notice(attaching.persistence()),
        Some(super::ui::KEYCHAIN_UNTIL_RESTART_NOTICE),
        "keyutils is lost at restart"
    );
    let ui = include_str!("ui.rs");
    let strip = &ui[ui.find("fn status_strip(").expect("strip")..];
    let strip = &strip[..strip.find("\nfn ").expect("next")];
    assert!(strip.contains("keychain_notice(snapshot.persistence())"));
    assert_eq!(
        ui.matches("keychain_notice(snapshot.persistence())")
            .count(),
        1,
        "one notice only"
    );
}

#[test]
fn keys_are_read_once_in_the_center_panel() {
    let auth = include_str!("auth.rs");
    assert!(
        !auth.contains("key_pressed"),
        "auth.rs does not read keys again"
    );
    let ui = include_str!("ui.rs");
    let center = &ui[ui.find("fn center_panel(").expect("center")..];
    let center = &center[..center.find("\nfn ").expect("next")];
    assert!(center.contains("Intent::Key(AuthKey::Enter)"));
    assert!(center.contains("Intent::Key(AuthKey::Escape)"));
    let inbox = &ui[ui.find("fn inbox_keys(").expect("inbox keys")..];
    let inbox = &inbox[..inbox.find("\nfn ").expect("next")];
    assert!(inbox.contains("m.focused()).is_none()"));
    assert!(inbox.contains("inbox_list_id"));
    assert!(inbox.contains("CenterView::Thread"));
    assert!(inbox.contains("Intent::MoveInbox"));
    assert!(inbox.contains("visible_focused_row()"));
    assert!(inbox.contains("Intent::SelectConversation"));
    assert!(!inbox.contains("selected_conversation.clone()"));
}

#[test]
fn phone_step_shows_only_after_the_adapter_asks_for_it() {
    let store = SecretStore::memory();
    seed_override(&store);
    let mut snapshot = Snapshot::new();
    snapshot.open_telegram(&store);
    assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
    assert_eq!(
        auth_steps(&mut snapshot),
        vec![TelegramAuthStep::ApiCredentials]
    );
    snapshot.telegram_phone = "+15551234567".into();
    snapshot.center_key(AuthKey::Enter, &store);
    assert!(
        auth_steps(&mut snapshot).is_empty(),
        "no phone before NeedPhone"
    );

    snapshot.apply(AdapterEvent::TelegramAuth {
        phase: TelegramAuthPhase::Failed,
    });
    assert_eq!(snapshot.auth, AuthScreen::TelegramConnecting);
    assert!(snapshot.can_submit_auth(), "Try again is enabled");
    snapshot.center_key(AuthKey::Enter, &store);
    snapshot.center_key(AuthKey::Enter, &store);
    let commands = snapshot.take_commands();
    assert!(
        matches!(
            commands.as_slice(),
            [
                AdapterCommand::Disconnect {
                    protocol: ProtocolId::Telegram
                },
                AdapterCommand::TelegramAuth {
                    step: TelegramAuthStep::ApiCredentials,
                    epoch: 0
                }
            ]
        ),
        "Try again restarts the client once (qa R59): {commands:?}"
    );
    snapshot.apply(AdapterEvent::TelegramAuth {
        phase: TelegramAuthPhase::NeedPhone,
    });
    assert_eq!(snapshot.auth, AuthScreen::TelegramPhone);
    let auth = include_str!("auth.rs");
    assert!(auth.contains("\"Try again\""));
}

#[test]
fn resuming_screen_shows_the_keychain_wait_text() {
    let ui = include_str!("ui.rs");
    let resuming = &ui[ui.find("fn resuming(").expect("resuming")..];
    let resuming = &resuming[..resuming.find("\nfn ").expect("next")];
    assert!(
        resuming.contains("keychain_wait_text("),
        "no bare spinner (qa R1)"
    );
}

#[test]
fn word_and_phrase_codes_keep_their_letters() {
    use super::auth::code_is_digits;
    assert!(code_is_digits(None));
    assert!(code_is_digits(Some(TelegramCodeVia::Sms)));
    assert!(code_is_digits(Some(TelegramCodeVia::TelegramApp)));
    assert!(!code_is_digits(Some(TelegramCodeVia::SmsWord)));
    let auth = include_str!("auth.rs");
    let code = &auth[auth.find("fn telegram_code(").expect("code")..];
    let code = &code[..code.find("\nfn ").expect("next")];
    let guard = code.find("if digits_only").expect("guard");
    let filter = code.find("retain(|c| c.is_ascii_digit())").expect("filter");
    assert!(guard < filter, "the digit filter runs only for digit codes");
}

#[test]
fn a_live_session_hides_add_account_and_cancel_keeps_it() {
    let store = SecretStore::memory();
    let mut snapshot = ready_with_chats(&store);
    store.set_secret(SecretKey::Session, "live-session");
    assert!(snapshot.telegram_ready());
    assert!(!snapshot.can_add_account(), "one Telegram account (ux F8)");
    snapshot.open_add_account(&store);
    assert_eq!(snapshot.auth, AuthScreen::Idle);
    assert!(snapshot.take_commands().is_empty());

    snapshot.open_api_override(&store);
    assert_eq!(
        snapshot.auth,
        AuthScreen::Idle,
        "no Advanced while signed in"
    );
    // A form can still be over a live session (for example one opened just
    // before Ready). Cancel then closes the form only.
    snapshot.auth = AuthScreen::TelegramApi;
    snapshot.center_key(AuthKey::Escape, &store);
    assert_eq!(snapshot.auth, AuthScreen::Idle);
    assert!(snapshot.telegram_ready(), "Cancel does not sign out");
    assert!(
        !snapshot
            .take_commands()
            .iter()
            .any(|command| matches!(command, AdapterCommand::Disconnect { .. })),
        "no Disconnect on a live session, so the worker never logs out"
    );
    assert_eq!(
        store.get(SecretKey::Session).expect("read").as_deref(),
        Some("live-session"),
        "the session marker stays"
    );
    assert_eq!(
        snapshot.visible_conversations().len(),
        2,
        "the chat list stays"
    );
    assert_eq!(snapshot.center_view(), CenterView::Thread);

    let ui = include_str!("ui.rs");
    let bar = &ui[ui.find("fn top_bar(").expect("top bar")..];
    let bar = &bar[..bar.find("\nfn ").expect("next")];
    let guard = bar.find("can_add_account()").expect("guard");
    let button = bar.find("\"Add account\"").expect("button");
    assert!(guard < button, "Add account hides when Telegram is ready");
    let advanced = bar.find("\"Advanced\"").expect("advanced");
    let advanced_guard = bar[..advanced]
        .rfind("can_add_account()")
        .expect("advanced guard");
    assert!(advanced_guard > button, "Advanced has its own guard");
}

#[test]
fn slack_sign_in_is_compiled_only_with_the_feature() {
    let ui = include_str!("ui.rs");
    let on = ui
        .find("#[cfg(feature = \"slack-oauth\")]\nfn add_slack_workspace")
        .expect("feature-on helper");
    let off = ui
        .find("#[cfg(not(feature = \"slack-oauth\"))]\nfn add_slack_workspace")
        .expect("feature-off helper");
    let on_body = &ui[on..off];
    assert!(on_body.contains("Add Slack workspace"));
    assert!(on_body.contains("SlackIntent::Connect"));
    assert!(on_body.contains("SlackIntent::Cancel"));
    let off_body = &ui[off..];
    let off_body = &off_body[..off_body.find("\nfn ").expect("next")];
    assert!(
        !off_body.contains("Add Slack workspace"),
        "the default build helper draws no Slack control"
    );
    let first = &ui[ui.find("fn first_run(").expect("first")..];
    let first = &first[..first.find("\nfn ").expect("next")];
    assert!(first.contains("add_slack_workspace("));
    assert!(!first.contains("Add Slack workspace"));
    let bar = &ui[ui.find("fn top_bar(").expect("bar")..];
    let bar = &bar[..bar.find("\nfn ").expect("next")];
    assert!(bar.contains("add_slack_workspace("));
}

#[test]
fn auth_ui_is_telegram_only_this_beat() {
    let src = include_str!("auth.rs");
    assert!(src.contains("TELEGRAM_API_ID"));
    assert!(src.contains("credentials missing") || src.contains("Credentials missing"));
    assert!(src.contains("my.telegram.org"));
    assert!(src.contains("Cancel"));
    assert!(src.contains("Send code"));
    assert!(!src.contains("Continue (stub)"));
    assert!(!src.contains("Send code (stub)"));
    assert!(!src.contains("Finish (stub)"));
    assert!(!src.contains("WhatsApp"));
    assert!(!src.contains("Discord"));
    assert!(!src.contains("Slack"));
    assert!(!src.contains("UserAccount"));
    assert!(!src.contains("user_token"));
    let ui = include_str!("ui.rs");
    assert!(ui.contains("not ready"));
    assert!(ui.contains("Start with Telegram"));
    assert!(!ui.contains("not login peers"));
    assert!(!ui.contains("Experimental chips stay visible"));
    assert!(!ui.contains("tokio worker"));
    assert!(!ui.contains("Supported goals:"));
    assert!(!ui.contains("Telegram is live"));
    assert!(!ui.contains("caps.detail"));
    assert!(!ui.contains("account.caps.short_label"));
    assert!(!ui.contains("\"Experimental\""));
    assert!(!ui.contains("my.telegram.org"));
    assert!(TDLIB_UNAVAILABLE_BANNER.contains("telegram-tdlib"));
    assert!(src.contains("stub_banner("));
    assert!(ui.contains("stub_banner"));
}

#[test]
fn whatsapp_spike_ui_is_feature_gated() {
    let app = include_str!("mod.rs");
    assert!(app.contains("#[cfg(feature = \"whatsapp-web\")]\nmod whatsapp_gate;"));
    let ui = include_str!("ui.rs");
    assert!(ui.contains("not ready"));
    assert!(ui.contains("whatsapp_pairing_available"));
    assert!(ui.contains("whatsapp_gate::risk_entry"));
    let gate = include_str!("whatsapp_gate.rs");
    assert!(gate.contains("CRITIC_RISK_BULLETS"));
    assert!(gate.contains("Review WhatsApp ban risk"));
    assert!(gate.contains("No QR code and no pair code are shown on this screen."));
    assert!(
        !gate
            .split(|ch: char| !ch.is_ascii_alphabetic())
            .any(|word| word.eq_ignore_ascii_case("reliable"))
    );
    let ci = include_str!("../../../../.github/workflows/ci.yml");
    assert!(!ci.contains("whatsapp-web"));
    let os_zips = include_str!("../../../../.github/workflows/os-zips.yml");
    assert!(!os_zips.contains("whatsapp-web"));
    let test_sh = include_str!("../../../../scripts/test.sh");
    assert!(!test_sh.contains("whatsapp-web"));
}

/// Try again on a failed keychain read goes through the core, and the core
/// reads the keychain again off the UI thread.
#[test]
fn keychain_try_again_reaches_the_core_attach() {
    let ui = include_str!("ui.rs");
    let screen = &ui[ui.find("fn keychain_failed(").expect("screen")..];
    let screen = &screen[..screen.find("\nfn ").expect("next")];
    assert!(screen.contains("KEYCHAIN_READ_FAILED"));
    assert!(screen.contains("out.push(Intent::RetryKeychain)"));
    let core = include_str!("../../../thinwire-core/src/core.rs");
    assert!(core.contains("Intent::RetryKeychain => self.state.retry_keychain()"));
    let retry = &core[core.find("take_keychain_retry()").expect("retry")..];
    let retry = &retry[..retry.find('}').expect("end")];
    assert!(retry.contains("spawn_os_retry("));
}

/// qa L1: a hint the frame did not draw stays in the core for a later frame.
#[test]
fn hints_count_as_used_only_where_their_widget_draws() {
    let store = SecretStore::memory();
    let mut snapshot = ready_with_chats(&store);
    snapshot.select_conversation(telegram_chat(1, "Alice", 1).id);
    assert!(snapshot.wants_focus_compose());
    assert!(
        snapshot.wants_focus_compose(),
        "reading the hint does not clear it"
    );

    let src = include_str!("mod.rs");
    let ui_fn = &src[src.find("fn ui(").expect("ui")..];
    let ui_fn = &ui_fn[..ui_fn.find("\n    fn ").expect("next fn")];
    let draw = ui_fn.find("ui::draw(").expect("draw");
    let take = ui_fn
        .find("self.core.take_focus_compose()")
        .expect("take focus");
    assert!(draw < take, "the core hint clears after the draw");
    assert!(ui_fn.contains("if hints.used_focus_compose()"));
    assert!(ui_fn.contains("if hints.used_scroll_to_selected()"));

    let ui = include_str!("ui.rs");
    assert_eq!(ui.matches("hints.take_focus_compose()").count(), 1);
    let compose = &ui[ui.find("fn compose(").expect("compose")..];
    assert!(compose.contains("hints.take_focus_compose()"));
    assert_eq!(ui.matches("hints.take_scroll_to_selected()").count(), 1);
    let inbox = &ui[ui.find("fn inbox(").expect("inbox")..];
    let inbox = &inbox[..inbox.find("\nfn ").expect("next")];
    let rows_only = inbox.find("InboxState::Rows").expect("rows");
    assert!(inbox.find("hints.take_scroll_to_selected()").expect("take") > rows_only);
}

#[test]
fn a_ready_loading_line_stays_busy_until_the_history_loads() {
    use super::ui::status_strip_visible;
    let store = SecretStore::memory();
    let mut snapshot = ready_with_chats(&store);
    snapshot.apply(AdapterEvent::ChatListLoaded {
        protocol: ProtocolId::Telegram,
    });
    snapshot.apply(AdapterEvent::HistoryLoaded {
        protocol: ProtocolId::Telegram,
        conversation_id: "telegram:1".into(),
    });
    assert!(!snapshot.is_loading());
    // Open a chat: its history starts to load.
    snapshot.select_conversation("telegram:2".into());
    let status = |status, detail: &str| AdapterEvent::Status {
        protocol: ProtocolId::Telegram,
        status,
        detail: detail.into(),
    };
    snapshot.apply(status(AdapterStatus::Ready, "Loading recent messages."));
    assert!(snapshot.is_loading());
    assert!(!snapshot.status_is_idle(), "busy while the history loads");
    assert!(
        status_strip_visible(&snapshot, None),
        "the loading line shows"
    );

    snapshot.apply(AdapterEvent::HistoryLoaded {
        protocol: ProtocolId::Telegram,
        conversation_id: "telegram:2".into(),
    });
    assert!(!snapshot.is_loading());
    assert!(snapshot.status_is_idle());
    assert!(!status_strip_visible(&snapshot, None));

    snapshot.apply(status(AdapterStatus::Ready, "Message sent."));
    assert!(snapshot.status_is_idle(), "a finished send is idle");
}

#[test]
fn notifications_reach_the_os_thread_and_the_title_counts_unread() {
    use super::window_title;
    assert_eq!(window_title(0), "thinwire");
    assert_eq!(window_title(3), "thinwire (3)");

    let app = include_str!("mod.rs");
    let pass = &app[app.find("fn ui(&mut self").expect("ui")..];
    let pass = &pass[..pass.find("\n    }\n").expect("end")];
    let intents = pass
        .find("self.notification_intents(")
        .expect("focus and clicks");
    let dispatch = pass.find("self.core.dispatch(intent)").expect("dispatch");
    let send = pass
        .find("self.notifier.send(self.core.take_notify())")
        .expect("to the OS thread");
    assert!(
        intents < dispatch && dispatch < send,
        "intents, then dispatch, then show"
    );
    assert!(pass.contains("self.update_title("));
    assert!(app.contains("Intent::WindowFocus(focused)"));
    assert!(
        app.contains("ViewportCommand::Focus"),
        "a click raises the window"
    );

    let ui = include_str!("ui.rs");
    let control = &ui[ui.find("fn notification_control(").expect("switches")..];
    let control = &control[..control.find("\nfn ").expect("next")];
    assert!(control.contains("Intent::SetNotifications("));
    assert!(control.contains("Intent::SetNotificationPreview("));
    assert!(
        control.contains("add_enabled(on"),
        "the preview switch needs notifications on"
    );
}
