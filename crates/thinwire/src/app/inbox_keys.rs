//! Inbox keyboard and AccessKit. A click on the preview opens that chat.

use egui_kittest::Harness;
use egui_kittest::kittest::{By, NodeT, Queryable};
use thinwire_core::state::Snapshot;
use thinwire_core::state::test_support::{
    ready_with_chats, show_only, show_protocol, telegram_chat,
};
use thinwire_core::{Intent, View};
use thinwire_protocol::{
    AdapterEvent, ChatMessage, Delivery, HelperFault, HelperState, ProtocolId,
};

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
    /// Copy one-shot hints from the snapshot each frame, as the app does.
    apply_hints: bool,
    /// Protocols whose Restart the user clicked (#246).
    restarts: Vec<ProtocolId>,
}

impl InboxUi {
    fn ready() -> Self {
        let store = SecretStore::memory();
        let mut snapshot = ready_with_chats(&store);
        // These tests are about the Telegram inbox. The account rows of the
        // other protocols change the layout, so hide them in every build.
        show_only(&mut snapshot, &[ProtocolId::Telegram]);
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
            apply_hints: false,
            restarts: Vec::new(),
        }
    }

    /// The inbox with a WhatsApp account row whose helper has this state.
    fn with_whatsapp_helper(helper: HelperState) -> Self {
        let mut state = Self::ready();
        show_protocol(&mut state.snapshot, ProtocolId::WhatsApp);
        state.snapshot.apply(AdapterEvent::Helper {
            protocol: ProtocolId::WhatsApp,
            state: helper,
        });
        state
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
        if state.apply_hints {
            state.hints = Hints::from_view(&view);
        }
        ui::draw(ui, &view, &mut state.hints, &mut out);
    }
    for intent in out {
        match intent {
            Intent::SelectConversation { id } => {
                state.selects += 1;
                state.snapshot.select_conversation(id);
            }
            Intent::RestartHelper(protocol) => state.restarts.push(protocol),
            Intent::MoveInbox { delta } => state.snapshot.move_inbox_selection(delta),
            Intent::FocusInbox { id } => state.snapshot.focus_inbox_row(id),
            Intent::SetChatMute {
                protocol,
                conversation_id,
                muted,
            } => {
                state
                    .settings
                    .set_chat_muted(protocol, &conversation_id, muted);
            }
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

/// #246 UX: when the helper stopped for good, the WhatsApp account row
/// reads "Helper stopped." and has Restart. A click asks the core once.
#[test]
fn a_stopped_helper_row_reads_helper_stopped_with_restart() {
    let mut harness = harness(InboxUi::with_whatsapp_helper(HelperState::Stopped(
        HelperFault::Crashed,
    )));
    harness.run();
    harness.get_by_label("Helper stopped.");
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Restart")
        .click();
    harness.run();
    assert_eq!(harness.state().restarts, [ProtocolId::WhatsApp]);
}

/// #246 UX: a slow helper shows a busy row. The window stays in use: the
/// Telegram inbox next to it still draws.
#[test]
fn a_busy_helper_row_reads_busy_and_has_no_restart() {
    let mut harness = harness(InboxUi::with_whatsapp_helper(HelperState::Busy));
    harness.run();
    harness.get_by_label("Helper is busy…");
    assert!(harness.query_by_label("Restart").is_none());
    assert!(
        harness.query_all_by_label_contains("Ada").next().is_some(),
        "the Telegram inbox still draws"
    );
}

/// ADR 0013 decision 6: with no helper program the account keeps its row
/// and shows "WhatsApp helper missing. Reinstall thinwire."
#[test]
fn a_missing_helper_row_says_reinstall() {
    let mut harness = harness(InboxUi::with_whatsapp_helper(HelperState::Missing));
    harness.run();
    harness.get_by_label("Helper missing");
    harness.get_by_label("WhatsApp helper missing. Reinstall thinwire.");
    assert!(harness.query_by_label("Restart").is_none());
}

/// A helper that runs changes nothing on the row.
#[test]
fn a_running_helper_row_shows_its_normal_state() {
    let mut harness = harness(InboxUi::with_whatsapp_helper(HelperState::Running));
    harness.run();
    assert!(harness.query_by_label("Helper stopped.").is_none());
    assert!(harness.query_by_label("Helper is busy…").is_none());
    assert!(harness.query_by_label("Restart").is_none());
}

/// #153: the thread header mutes and unmutes the open chat in thinwire.
/// The row reads "muted" for AccessKit.
#[test]
fn the_header_button_mutes_and_unmutes_the_open_chat() {
    let mut harness = harness(InboxUi::ready());
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Mute")
        .click();
    harness.run();
    assert!(
        harness
            .state()
            .settings
            .chat_mutes()
            .contains(ProtocolId::Telegram, "telegram:1")
    );
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Ada, muted");
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Unmute")
        .click();
    harness.run();
    assert!(
        !harness
            .state()
            .settings
            .chat_mutes()
            .contains(ProtocolId::Telegram, "telegram:1")
    );
    harness.get_by_role_and_label(egui::accesskit::Role::Button, "Mute");
}

/// #153: a right click on a row mutes that chat. A chat muted in the
/// protocol shows a disabled "Muted in Telegram": only Telegram unmutes it.
#[test]
fn the_row_menu_mutes_a_chat_and_a_protocol_mute_wins() {
    let mut state = InboxUi::ready();
    let mut muted = telegram_chat(3, "Cy", 1);
    muted.muted = true;
    state.snapshot.apply(AdapterEvent::ConversationUpsert {
        conversation: muted,
    });
    let mut harness = harness(state);
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Bob")
        .click_secondary();
    harness.run();
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Mute chat")
        .click();
    harness.run();
    assert!(
        harness
            .state()
            .settings
            .chat_mutes()
            .contains(ProtocolId::Telegram, "telegram:2")
    );

    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Cy, muted")
        .click();
    harness.step();
    harness
        .state_mut()
        .snapshot
        .apply(AdapterEvent::HistoryLoaded {
            protocol: ProtocolId::Telegram,
            conversation_id: "telegram:3".into(),
        });
    harness.run();
    let header = harness.get_by_role_and_label(egui::accesskit::Role::Button, "Muted in Telegram");
    assert!(
        header.accesskit_node().is_disabled(),
        "only Telegram unmutes it"
    );
    assert!(
        harness.query_by_label("Unmute").is_none(),
        "no thinwire unmute for a protocol mute"
    );

    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Cy, muted")
        .click_secondary();
    harness.run();
    let menu_items: Vec<_> = harness
        .query_all(
            By::new()
                .role(egui::accesskit::Role::Button)
                .label("Muted in Telegram"),
        )
        .collect();
    assert!(
        menu_items.len() >= 2,
        "header and row menu both show the protocol mute"
    );
    for item in &menu_items {
        assert!(
            item.accesskit_node().is_disabled(),
            "the row menu item stays disabled"
        );
    }
    harness.get_by_label("Only Telegram can unmute it");
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
        arrival: thinwire_protocol::Arrival::History,
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
        let spoken = harness
            .query_all(By::new().predicate({
                let body = body.to_owned();
                move |node| {
                    if node.is_hidden() || node.role() == egui::accesskit::Role::TextRun {
                        return false;
                    }
                    let Some(label) = node.label() else {
                        return false;
                    };
                    label.contains("Nora") && label.contains(&body)
                }
            }))
            .count();
        assert_eq!(spoken, 1, "a screen reader reads {body} once");
        let painted = harness.get_by_role_and_label(egui::accesskit::Role::Label, body);
        assert!(
            painted.accesskit_node().is_hidden(),
            "the painted body stays out of the screen reader tree"
        );
        let text = painted.rect();
        let beside = |node: &egui_kittest::Node<'_>| {
            node.rect().left() >= text.right() - 2.0
                && (node.rect().center().y - text.center().y).abs() < 24.0
        };
        let side_labels = harness
            .query_all(By::new().role(egui::accesskit::Role::Label))
            .filter(|node| beside(node))
            .count();
        let hidden_side = harness
            .query_all(By::new().role(egui::accesskit::Role::Label))
            .filter(|node| beside(node) && node.accesskit_node().is_hidden())
            .count();
        assert!(side_labels > 0, "the time is painted beside {body}");
        assert_eq!(
            hidden_side, side_labels,
            "the time stays out of the spoken name"
        );
        let selectable = harness
            .query_all(By::new().role(egui::accesskit::Role::TextRun).predicate({
                let body = body.to_owned();
                move |node| {
                    !node.is_hidden()
                        && node
                            .value()
                            .is_some_and(|value| !value.is_empty() && body.contains(value.as_str()))
                }
            }))
            .count();
        assert!(selectable >= 1, "assistive tech can select part of {body}");
    }
    let sender_headers = harness
        .query_all(By::new().role(egui::accesskit::Role::Label).label("Nora"))
        .count();
    let hidden_headers = harness
        .query_all(By::new().role(egui::accesskit::Role::Label).label("Nora"))
        .filter(|node| node.accesskit_node().is_hidden())
        .count();
    assert!(sender_headers > 0, "the group header is painted");
    assert_eq!(
        hidden_headers, sender_headers,
        "the group header stays out of the spoken name"
    );
}

#[test]
fn an_accesskit_selection_reaches_the_bubble() {
    let body = "Hello from the selectable bubble";
    let mut state = InboxUi::ready();
    state.snapshot.apply(AdapterEvent::MessageReceived {
        message: chat_message("telegram:1:select", body, 1_790_300_000),
    });
    let mut harness = harness(state);
    harness.run();
    let run = harness
        .query_all(By::new().role(egui::accesskit::Role::TextRun))
        .find(|node| {
            !node.accesskit_node().is_hidden()
                && node
                    .value()
                    .is_some_and(|value| !value.is_empty() && body.contains(value.as_str()))
        })
        .expect("text run");
    run.click();
    harness.step();
    let pane = harness.get(By::new().role(egui::accesskit::Role::Pane).predicate({
        let body = body.to_owned();
        move |node| !node.is_hidden() && node.label().is_some_and(|label| label.contains(&body))
    }));
    let run = harness
        .query_all(By::new().role(egui::accesskit::Role::TextRun))
        .find(|node| {
            !node.accesskit_node().is_hidden()
                && node
                    .value()
                    .is_some_and(|value| !value.is_empty() && body.contains(value.as_str()))
        })
        .expect("text run");
    let (target_node, target_tree) = pane.accesskit_node().locate();
    let (run_node, _) = run.accesskit_node().locate();
    harness.event(egui::Event::AccessKitActionRequest(
        egui::accesskit::ActionRequest {
            action: egui::accesskit::Action::SetTextSelection,
            target_node,
            target_tree,
            data: Some(egui::accesskit::ActionData::SetTextSelection(
                egui::accesskit::TextSelection {
                    anchor: egui::accesskit::TextPosition {
                        node: run_node,
                        character_index: 0,
                    },
                    focus: egui::accesskit::TextPosition {
                        node: run_node,
                        character_index: 5,
                    },
                },
            )),
        },
    ));
    harness.step();
    let pane = harness.get(By::new().role(egui::accesskit::Role::Pane).predicate({
        let body = body.to_owned();
        move |node| !node.is_hidden() && node.label().is_some_and(|label| label.contains(&body))
    }));
    let node = pane.accesskit_node();
    let selection = node
        .raw_text_selection()
        .expect("the pane reports a selection");
    assert_eq!(selection.anchor.character_index, 0);
    assert_eq!(selection.focus.character_index, 5);
}

#[test]
fn a_bubble_names_the_message_once() {
    let body = "Hello from the only name on this bubble";
    let mut state = InboxUi::ready();
    state.snapshot.apply(AdapterEvent::MessageReceived {
        message: chat_message("telegram:1:once", body, 1_790_300_000),
    });
    let mut harness = harness(state);
    harness.run();
    let announced = harness
        .query_all(By::new().predicate({
            let body = body.to_owned();
            move |node| !node.is_hidden() && node_text_eq(node, &body)
        }))
        .count();
    assert_eq!(announced, 1, "a screen reader reads the message once");
    let painted = harness.get_by_role_and_label(egui::accesskit::Role::Label, body);
    assert!(
        painted.accesskit_node().is_hidden(),
        "the painted body stays out of the screen reader tree"
    );
}

fn node_text_eq(node: &egui_kittest::kittest::AccessKitNode<'_>, body: &str) -> bool {
    let text = if node.role() == egui::accesskit::Role::Label {
        node.value()
    } else {
        node.label()
    };
    text.as_deref() == Some(body)
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

/// Top of the compose field: below the bottom edge of the message view.
fn compose_top(harness: &Harness<'_, InboxUi>) -> f32 {
    harness
        .get_by_role(egui::accesskit::Role::MultilineTextInput)
        .rect()
        .top()
}

fn bubble_rect(harness: &Harness<'_, InboxUi>, body: &str) -> egui::Rect {
    harness
        .get_by_role_and_label(egui::accesskit::Role::Pane, body)
        .rect()
}

/// A long chat sits at its newest message. A new message is in view after
/// one `run`: the thread asks for the next frame when `stick_to_bottom`
/// moves, not only at the idle tick. Scrolled up, a button jumps back.
#[test]
fn the_thread_follows_new_messages_and_jumps_back_to_the_newest() {
    let mut state = InboxUi::ready();
    for n in 0..40 {
        state.snapshot.apply(AdapterEvent::MessageReceived {
            message: chat_message(
                &format!("telegram:1:{n:02}"),
                &format!("Message number {n}"),
                1_790_000_000 + n * 60,
            ),
        });
    }
    let mut harness = harness(state);
    harness.run();
    let edge = compose_top(&harness);
    assert!(bubble_rect(&harness, "Message number 39").bottom() <= edge);
    assert!(
        harness.query_by_label("Jump to latest message").is_none(),
        "at the newest message there is no jump button"
    );

    harness
        .state_mut()
        .snapshot
        .apply(AdapterEvent::MessageReceived {
            message: chat_message("telegram:1:40", "Message number 40", 1_790_100_000),
        });
    harness.run();
    assert!(
        bubble_rect(&harness, "Message number 40").bottom() <= edge,
        "the new message is in view"
    );

    let thread = bubble_rect(&harness, "Message number 40");
    harness.hover_at(thread.center());
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, 5_000.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    // egui spreads a wheel step over several frames.
    harness.run_steps(12);
    harness.run();
    assert!(
        bubble_rect(&harness, "Message number 40").top() > edge,
        "scrolled up, the newest message is out of view"
    );
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Jump to latest message")
        .click();
    // The click, then egui's animated scroll to the newest message.
    harness.run_steps(6);
    harness.run();
    let newest = bubble_rect(&harness, "Message number 40");
    assert!(
        newest.bottom() <= edge && newest.top() < edge,
        "the jump shows the newest message: {newest:?}, edge {edge}"
    );
    assert!(
        harness.query_by_label("Jump to latest message").is_none(),
        "back at the newest message the button goes"
    );
}

fn row_top(harness: &Harness<'_, InboxUi>, title: &str) -> f32 {
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, title)
        .rect()
        .top()
}

fn row_rect(harness: &Harness<'_, InboxUi>, title: &str) -> egui::Rect {
    harness
        .get_by_role_and_label(egui::accesskit::Role::Button, title)
        .rect()
}

/// Bob is open. A message moves Ada to the top. Bob stays open, the composer
/// keeps its text and focus, and Ada shows the unread badge. At the top of
/// the list Ada appears there and Bob shifts down by one row.
#[test]
fn a_message_in_another_chat_keeps_the_open_chat_and_the_composer() {
    let mut state = InboxUi::ready();
    state.apply_hints = true;
    state.snapshot.select_conversation("telegram:2".into());
    assert!(state.snapshot.take_scroll_to_selected());
    assert!(state.snapshot.take_focus_compose());
    // Opening a chat loads its history. A spinner keeps requesting frames.
    state.snapshot.apply(AdapterEvent::HistoryLoaded {
        protocol: ProtocolId::Telegram,
        conversation_id: "telegram:2".into(),
    });
    // Bob starts above Ada, so her message has to move past him.
    let mut bob_row = telegram_chat(2, "Bob", 20);
    bob_row.preview = "seen from Bob".into();
    state.snapshot.apply(AdapterEvent::ConversationUpsert {
        conversation: bob_row,
    });
    let mut ada_row = telegram_chat(1, "Ada", 5);
    ada_row.preview = "seen from Ada".into();
    state.snapshot.apply(AdapterEvent::ConversationUpsert {
        conversation: ada_row,
    });
    assert!(!state.snapshot.wants_scroll_to_selected());
    state.snapshot.compose = "draft for Bob".into();
    let mut harness = harness(state);
    harness.run();
    harness
        .ctx
        .memory_mut(|memory| memory.request_focus(egui::Id::new("thread-compose")));
    harness.step();
    let compose = egui::Id::new("thread-compose");
    assert!(
        harness.ctx.memory(|memory| memory.has_focus(compose)),
        "the composer has focus"
    );
    let ada_before = row_top(&harness, "Ada");
    let bob_before = row_top(&harness, "Bob");
    assert!(bob_before < ada_before, "Bob starts above Ada");
    let stride = ada_before - bob_before;

    let mut ada = telegram_chat(1, "Ada", 40);
    ada.unread = 2;
    ada.preview = "new from Ada".into();
    ada.last_at = 1_700_000_100;
    harness
        .state_mut()
        .snapshot
        .apply(AdapterEvent::ConversationUpsert { conversation: ada });
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:2")
    );
    assert_eq!(harness.state().snapshot.compose, "draft for Bob");
    assert!(!harness.state().snapshot.wants_scroll_to_selected());
    assert!(!harness.state().snapshot.wants_focus_compose());
    harness.step();

    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:2"),
        "Ada's message does not open Ada"
    );
    assert_eq!(harness.state().snapshot.compose, "draft for Bob");
    assert!(
        harness.ctx.memory(|memory| memory.has_focus(compose)),
        "the composer keeps focus"
    );
    assert!(row_top(&harness, "Ada") < row_top(&harness, "Bob"));
    assert!(
        (row_top(&harness, "Ada") - bob_before).abs() < 2.0,
        "at the top, Ada takes the first row"
    );
    assert!(
        (row_top(&harness, "Bob") - bob_before - stride).abs() < 2.0,
        "Bob shifts down by one row"
    );
    assert!(
        harness.query_all_by_label("unread 2").next().is_some(),
        "Ada shows the unread badge"
    );
    harness.get_by_label("new from Ada");
}

/// Scrolled down, the row under the pointer stays on that screen line when
/// another chat jumps to the top.
#[test]
fn a_scrolled_inbox_keeps_the_row_under_the_pointer() {
    let mut state = InboxUi::ready();
    state.apply_hints = true;
    for n in 3_i64..=16 {
        // Below Ada (order 10) and Bob (order 5), so Ada stays the open top row.
        let mut conversation = telegram_chat(n, &format!("Row {n:02}"), 4 - n);
        conversation.preview = format!("seen from row {n}");
        state
            .snapshot
            .apply(AdapterEvent::ConversationUpsert { conversation });
    }
    state.snapshot.select_conversation("telegram:1".into());
    assert!(state.snapshot.take_scroll_to_selected());
    assert!(state.snapshot.take_focus_compose());
    state.snapshot.apply(AdapterEvent::HistoryLoaded {
        protocol: ProtocolId::Telegram,
        conversation_id: "telegram:1".into(),
    });
    state.snapshot.compose = "draft for Ada".into();
    let mut harness = harness(state);
    harness.run();
    let start = row_top(&harness, "Ada");
    let inbox_point = egui::pos2(120.0, 480.0);
    harness.hover_at(inbox_point);
    harness.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -180.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    harness.run_steps(12);
    harness.run();
    assert!(
        row_top(&harness, "Ada") < start - 40.0,
        "the list is scrolled down, Ada moved up off the top rows"
    );

    let held = row_rect(&harness, "Row 08");
    let point = held.center();
    harness.hover_at(point);
    harness.step();
    let held = row_rect(&harness, "Row 08");
    let point = held.center();
    assert!(
        held.contains(point),
        "the pointer rests on Row 08 before the reorder"
    );

    let mut jumped = telegram_chat(16, "Row 16", 500);
    jumped.unread = 1;
    jumped.preview = "new from row 16".into();
    harness
        .state_mut()
        .snapshot
        .apply(AdapterEvent::ConversationUpsert {
            conversation: jumped,
        });
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1")
    );
    assert_eq!(harness.state().snapshot.compose, "draft for Ada");
    assert!(!harness.state().snapshot.wants_scroll_to_selected());
    assert!(!harness.state().snapshot.wants_scroll_to_focused());
    harness.hover_at(point);
    harness.step();

    let after = row_rect(&harness, "Row 08");
    assert!(
        after.contains(point),
        "Row 08 stayed under the pointer: before {held:?}, after {after:?}, point {point:?}"
    );
    assert!((after.top() - held.top()).abs() < 2.0);
    assert_eq!(
        harness.state().snapshot.selected_conversation.as_deref(),
        Some("telegram:1")
    );
    assert_eq!(harness.state().snapshot.compose, "draft for Ada");
}
