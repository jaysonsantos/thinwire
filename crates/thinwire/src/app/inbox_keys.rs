//! Inbox keyboard and AccessKit. A click on the preview opens that chat.

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use thinwire_core::state::Snapshot;
use thinwire_core::state::test_support::{ready_with_chats, telegram_chat};
use thinwire_core::{Intent, View};
use thinwire_protocol::AdapterEvent;

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
            Intent::SelectConversation { id } => state.snapshot.select_conversation(id),
            Intent::MoveInbox { delta } => state.snapshot.move_inbox_selection(delta),
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
