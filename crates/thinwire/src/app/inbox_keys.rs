//! Inbox keyboard and AccessKit. A click on the preview opens that chat.

use egui_kittest::Harness;
use egui_kittest::kittest::{By, NodeT, Queryable};
use thinwire_core::state::Snapshot;
use thinwire_core::state::test_support::{ready_with_chats, telegram_chat};
use thinwire_core::{Intent, View};
use thinwire_protocol::{AdapterEvent, ChatMessage, Delivery, ProtocolId};

use super::theme;
use super::ui::{self, Hints};
use thinwire_core::secrets::SecretStore;
use thinwire_core::settings::Settings;

struct InboxUi {
    snapshot: Snapshot,
    store: SecretStore,
    settings: Settings,
    hints: Hints,
    /// When set, a thread Retry control takes keyboard focus.
    focus_retry: bool,
    selects: u32,
}

impl InboxUi {
    fn ready() -> Self {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        for (id, title, order, preview) in [
            (1, "Ada", 10, "seen from Ada"),
            (2, "Bob", 5, "seen from Bob"),
        ] {
            let mut conversation = telegram_chat(id, title, order);
            conversation.preview = preview.into();
            snapshot.apply(AdapterEvent::ConversationUpsert { conversation });
        }
        snapshot.apply(AdapterEvent::ChatListLoaded {
            protocol: thinwire_protocol::ProtocolId::Telegram,
        });
        snapshot.apply(AdapterEvent::HistoryLoaded {
            protocol: thinwire_protocol::ProtocolId::Telegram,
            conversation_id: "telegram:1".into(),
        });
        snapshot.take_commands();
        let settings = Settings::load_from(std::env::temp_dir().join("thinwire-inbox-keys-test"));
        Self {
            snapshot,
            store,
            settings,
            hints: Hints::default(),
            focus_retry: false,
            selects: 0,
        }
    }
}

fn draw(ui: &mut egui::Ui, state: &mut InboxUi) {
    // The harness runs one frame before the test can install fonts. That frame
    // still has the default style, so it only installs and returns.
    theme::install(ui.ctx());
    if !ui.style().text_styles.contains_key(&theme::row_title()) {
        return;
    }
    let mut out = Vec::new();
    {
        let view = View::from_parts(&state.snapshot, &state.store, &state.settings);
        ui::draw(ui, &view, &mut state.hints, &mut out);
    }
    for intent in out {
        match intent {
            Intent::SelectConversation { id } => {
                state.selects += 1;
                state.snapshot.select_conversation(id);
            }
            Intent::MoveInbox { delta } => state.snapshot.move_inbox_selection(delta),
            Intent::FocusInbox { id } => state.snapshot.focus_inbox_row(id),
            _ => {}
        }
    }
    if state.focus_retry {
        ui.interact(
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(8.0, 8.0)),
            egui::Id::new("thread-retry"),
            egui::Sense::click(),
        )
        .request_focus();
    }
}

fn harness(state: InboxUi) -> Harness<'static, InboxUi> {
    let harness = Harness::builder()
        .with_size(egui::vec2(960.0, 720.0))
        .build_ui_state(draw, state);
    theme::install(&harness.ctx);
    harness
}

#[test]
fn a_click_on_the_preview_opens_that_chat() {
    let mut harness = harness(InboxUi::ready());
    harness.run();
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1")
    );
    harness.get_by_label("seen from Bob").click();
    harness.step();
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:2")
    );
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Bob");
}

#[test]
fn arrows_move_the_highlight_and_enter_opens_it() {
    let mut harness = harness(InboxUi::ready());
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.step();
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1"),
        "an arrow does not replace the open chat"
    );
    assert_eq!(
        harness.state().snapshot.focused_row.as_deref(),
        Some("telegram:2")
    );
    assert!(harness.state_mut().snapshot.take_scroll_to_focused());
    assert!(!harness.state().snapshot.wants_focus_compose());
    assert!(
        harness.state_mut().snapshot.take_commands().is_empty(),
        "an arrow does not open the chat"
    );
    harness.state_mut().focus_retry = true;
    harness.step();
    harness.key_press(egui::Key::Enter);
    harness.step();
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1"),
        "Enter on a thread control does not open the highlighted chat"
    );
    harness.state_mut().focus_retry = false;
    harness
        .ctx
        .memory_mut(|memory| memory.surrender_focus(egui::Id::new("thread-retry")));
    harness.step();
    harness.key_press(egui::Key::Enter);
    harness.step();
    assert!(harness.state().snapshot.wants_focus_compose());
    assert!(
        harness
            .state_mut()
            .snapshot
            .take_commands()
            .iter()
            .any(|command| matches!(command, thinwire_protocol::AdapterCommand::OpenChat { .. }))
    );
}

#[test]
fn tab_to_a_row_then_enter_opens_it() {
    let mut harness = harness(InboxUi::ready());
    harness.run();
    let mut landed = false;
    for _ in 0..40 {
        harness.key_press(egui::Key::Tab);
        harness.step();
        if harness.state().snapshot.focused_row.as_deref() == Some("telegram:2") {
            landed = true;
            break;
        }
    }
    assert!(landed, "Tab reaches Bob and moves the highlight");
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1"),
        "Tab does not open the chat"
    );
    harness.state_mut().selects = 0;
    harness.key_press(egui::Key::Enter);
    harness.step();
    assert_eq!(harness.state().selects, 1, "Enter sends one open");
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:2")
    );
}

#[test]
fn accesskit_selected_is_the_open_chat() {
    let mut harness = harness(InboxUi::ready());
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.step();
    let ada = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Ada");
    let bob = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Bob");
    assert_eq!(
        ada.accesskit_node().toggled(),
        Some(egui::accesskit::Toggled::True),
        "the open chat stays selected"
    );
    assert_eq!(
        bob.accesskit_node().toggled(),
        Some(egui::accesskit::Toggled::False),
        "the highlight is not selected"
    );
}

#[test]
fn arrow_down_twice_keeps_the_highlight_on_row_3() {
    let mut state = InboxUi::ready();
    let mut conversation = telegram_chat(3, "Cara", 1);
    conversation.preview = "seen from Cara".into();
    state
        .snapshot
        .apply(AdapterEvent::ConversationUpsert { conversation });
    let mut harness = harness(state);
    harness.run();
    harness.key_press(egui::Key::ArrowDown);
    harness.step();
    harness.key_press(egui::Key::ArrowDown);
    harness.step();
    assert_eq!(
        harness.state().snapshot.focused_row.as_deref(),
        Some("telegram:3")
    );
    harness.step();
    harness.step();
    assert_eq!(
        harness.state().snapshot.focused_row.as_deref(),
        Some("telegram:3"),
        "the highlight stays on row 3"
    );
}

fn chat_message(id: &str, body: &str, sent_at: i64) -> ChatMessage {
    ChatMessage {
        protocol: ProtocolId::Telegram,
        conversation_id: "telegram:1".into(),
        id: id.into(),
        sender: "Ada".into(),
        body: body.into(),
        outbound: false,
        delivery: Delivery::Sent,
        sent_at,
    }
}

#[test]
fn older_rows_keep_message_text_inside_the_bubble() {
    let mut state = InboxUi::ready();
    state.snapshot.apply(AdapterEvent::MessageReceived {
        message: chat_message(
            "telegram:1:new",
            "Hello from the latest row in this chat",
            1_790_300_000,
        ),
    });
    let mut harness = harness(state);
    harness.run();
    harness
        .state_mut()
        .snapshot
        .apply(AdapterEvent::MessageReceived {
            message: chat_message(
                "telegram:1:old",
                "Older line that must stay wide inside the bubble",
                1_790_200_000,
            ),
        });
    harness.step();
    harness.step();
    for (body, id) in [
        (
            "Hello from the latest row in this chat",
            "Hello from the latest row in this chat",
        ),
        (
            "Older line that must stay wide inside the bubble",
            "Older line that must stay wide inside the bubble",
        ),
    ] {
        let text = harness.get_by_role_and_label(egui::accesskit::Role::Label, body);
        let bubble = harness.get_by_role_and_label(egui::accesskit::Role::Pane, id);
        let text_rect = text.rect();
        let bubble_rect = bubble.rect();
        assert!(
            bubble_rect.contains_rect(text_rect),
            "{body} sits outside its bubble"
        );
        assert!(
            text_rect.width() > 24.0,
            "{body} wraps wider than one letter"
        );
    }
}

#[test]
fn a_wrapped_message_keeps_the_time_on_the_bottom() {
    let body = "Can you also bring the small stove? Ours has a broken valve, and the shop only has the big one until next week, which does not fit in the car.";
    let mut state = InboxUi::ready();
    state.snapshot.apply(AdapterEvent::MessageReceived {
        message: chat_message("telegram:1:wrap", body, 1_790_300_000),
    });
    let mut harness = harness(state);
    harness.run();
    let text = harness.get_by_role_and_label(egui::accesskit::Role::Label, body);
    let time = harness
        .query_all(By::new().role(egui::accesskit::Role::Label))
        .filter(|node| node.rect().left() >= text.rect().right() - 2.0)
        .min_by(|left, right| {
            (left.rect().bottom() - text.rect().bottom())
                .abs()
                .total_cmp(&(right.rect().bottom() - text.rect().bottom()).abs())
        })
        .expect("time beside the message");
    let gap = (time.rect().bottom() - text.rect().bottom()).abs();
    assert!(gap < 6.0, "the time sits on the last line, gap {gap}");
}

#[test]
fn a_pending_message_keeps_the_time_on_the_last_line() {
    let body = "On my way with the long note that wraps onto another line in this thread.";
    let mut message = chat_message("telegram:1:pend", body, 1_790_300_000);
    message.outbound = true;
    message.delivery = Delivery::Pending;
    let mut state = InboxUi::ready();
    state
        .snapshot
        .apply(AdapterEvent::MessageReceived { message });
    let mut harness = harness(state);
    harness.run();
    let text = harness.get_by_role_and_label(egui::accesskit::Role::Label, body);
    let time = harness
        .query_all(By::new().role(egui::accesskit::Role::Label))
        .filter(|node| node.rect().left() >= text.rect().right() - 2.0)
        .min_by(|left, right| {
            (left.rect().bottom() - text.rect().bottom())
                .abs()
                .total_cmp(&(right.rect().bottom() - text.rect().bottom()).abs())
        })
        .expect("time beside the message");
    let gap = (time.rect().bottom() - text.rect().bottom()).abs();
    assert!(gap < 6.0, "the time stays on the last line, gap {gap}");
    let sending = harness.get_by_role_and_label(egui::accesskit::Role::Label, "Sending…");
    assert!(
        sending.rect().top() + 2.0 >= time.rect().bottom(),
        "Sending… sits under the time"
    );
}

#[test]
fn a_group_run_names_the_sender_on_every_message() {
    let mut state = InboxUi::ready();
    let mut conversation = telegram_chat(1, "Book club", 10);
    conversation.is_group = true;
    state
        .snapshot
        .apply(AdapterEvent::ConversationUpsert { conversation });
    for (id, body) in [
        ("telegram:1:a", "First line from Nora"),
        ("telegram:1:b", "Second line from Nora"),
    ] {
        let mut message = chat_message(id, body, 1_790_300_000);
        message.sender = "Nora".into();
        state
            .snapshot
            .apply(AdapterEvent::MessageReceived { message });
    }
    let mut harness = harness(state);
    harness.run();
    for body in ["First line from Nora", "Second line from Nora"] {
        harness.get(By::new().role(egui::accesskit::Role::Pane).predicate({
            let body = body.to_owned();
            move |node| {
                let Some(label) = node.label() else {
                    return false;
                };
                label.contains("Nora") && label.contains(&body)
            }
        }));
    }
}

#[test]
fn inbox_row_puts_the_title_left_and_the_badge_right() {
    let mut state = InboxUi::ready();
    let mut conversation = telegram_chat(1, "Zelda Title", 30);
    conversation.preview = "zelda preview line".into();
    conversation.unread = 4;
    conversation.last_at = 1_790_300_000;
    state
        .snapshot
        .apply(AdapterEvent::ConversationUpsert { conversation });
    let mut harness = harness(state);
    harness.run();
    let title = harness
        .get_all_by_role_and_label(egui::accesskit::Role::Label, "Zelda Title")
        .min_by(|left, right| left.rect().left().total_cmp(&right.rect().left()))
        .expect("inbox title");
    let preview = harness.get_by_role_and_label(egui::accesskit::Role::Label, "zelda preview line");
    let badge = harness
        .get_all_by_label("unread 4")
        .filter(|node| (node.rect().center().y - preview.rect().center().y).abs() < 24.0)
        .max_by(|left, right| left.rect().left().total_cmp(&right.rect().left()))
        .expect("unread badge");
    assert!(
        title.rect().left() <= preview.rect().left() + 2.0,
        "the title starts with the preview"
    );
    assert!(
        preview.rect().right() < badge.rect().left(),
        "the badge sits at the right of the preview"
    );
    assert!(
        title.rect().left() < badge.rect().left(),
        "the title stays left of the badge"
    );
}

#[test]
fn tab_then_two_arrows_land_on_row_3_and_enter_opens_it() {
    let mut state = InboxUi::ready();
    let mut conversation = telegram_chat(3, "Cara", 1);
    conversation.preview = "seen from Cara".into();
    state
        .snapshot
        .apply(AdapterEvent::ConversationUpsert { conversation });
    let mut harness = harness(state);
    harness.run();
    let mut tabbed = false;
    for _ in 0..40 {
        harness.key_press(egui::Key::Tab);
        harness.step();
        if harness.state().snapshot.focused_row.as_deref() == Some("telegram:1") {
            tabbed = true;
            break;
        }
    }
    assert!(tabbed, "Tab reaches Ada");
    let ada = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Ada");
    assert!(
        ada.accesskit_node().is_focused(),
        "widget focus is on the Tab row"
    );
    assert_eq!(
        harness.state().snapshot.focused_row.as_deref(),
        Some("telegram:1"),
        "the highlight is on the Tab row"
    );
    harness.key_press(egui::Key::ArrowDown);
    harness.step();
    harness.key_press(egui::Key::ArrowDown);
    harness.step();
    assert_eq!(
        harness.state().snapshot.focused_row.as_deref(),
        Some("telegram:3")
    );
    let cara = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Cara");
    assert!(
        cara.accesskit_node().is_focused(),
        "widget focus is on row 3"
    );
    assert_eq!(
        harness.state().selects,
        0,
        "an arrow does not open the chat"
    );
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1"),
        "an arrow leaves Ada open"
    );
    harness.state_mut().selects = 0;
    harness.key_press(egui::Key::Enter);
    harness.step();
    assert_eq!(harness.state().selects, 1, "Enter sends one open");
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:3")
    );
}
