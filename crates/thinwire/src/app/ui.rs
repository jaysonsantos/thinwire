//! Shell: account switcher + inbox on the left, thread in the center.

use eframe::egui::{self, Color32, RichText};
use thinwire_protocol::{Delivery, ProtocolId, SupportClass};

use thinwire_protocol::WhatsAppPhoneVault;

use super::auth;
use super::secrets::SecretStore;
use super::settings::{Settings, ThemeMode};
use super::snapshot::{
    AccountRow, CenterView, InboxFilter, InboxState, RESUME_CONNECTING, Snapshot, ThreadState,
};

const SUPPORTED: Color32 = Color32::from_rgb(96, 176, 128);
const EXPERIMENTAL: Color32 = Color32::from_rgb(214, 160, 64);
const CONSTRAINED: Color32 = Color32::from_rgb(196, 148, 88);
const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const FAILED: Color32 = Color32::from_rgb(200, 80, 80);
const COMPOSE_MAX_ROWS: usize = 5;

pub(crate) fn draw(
    ui: &mut egui::Ui,
    snapshot: &mut Snapshot,
    settings: &mut Settings,
    secrets: &SecretStore,
    whatsapp_phone: &WhatsAppPhoneVault,
) {
    settings.apply(ui.ctx());
    #[cfg(feature = "whatsapp-web")]
    if snapshot.whatsapp_pairing_available() && snapshot.whatsapp_gate_open() {
        super::whatsapp_gate::draw(ui, snapshot, whatsapp_phone);
        return;
    }
    #[cfg(not(feature = "whatsapp-web"))]
    let _ = whatsapp_phone;
    top_bar(ui, snapshot, settings, secrets);
    status_strip(ui, snapshot);
    left_panel(ui, snapshot);
    center_panel(ui, snapshot, secrets);
}

fn top_bar(
    ui: &mut egui::Ui,
    snapshot: &mut Snapshot,
    settings: &mut Settings,
    secrets: &SecretStore,
) {
    egui::Panel::top("top").show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("thinwire");
            ui.separator();
            for filter in InboxFilter::chrome_filters() {
                if ui
                    .selectable_label(snapshot.filter == *filter, filter.label())
                    .clicked()
                {
                    snapshot.set_filter(*filter);
                }
            }
            ui.separator();
            ui.label("Search");
            ui.add(
                egui::TextEdit::singleline(&mut snapshot.search)
                    .desired_width(160.0)
                    .hint_text("title / participant"),
            );
            if ui.button("Refresh").clicked() {
                snapshot.refresh_visible();
            }
            if ui.button("Add account").clicked() {
                snapshot.open_add_account(secrets);
            }
            if ui.button("Advanced").clicked() {
                snapshot.open_api_override(secrets);
            }
            ui.separator();
            theme_control(ui, settings);
        });
        ui.add_space(2.0);
    });
}

fn theme_control(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.label("Theme");
    let mut preference = settings.theme().to_egui();
    preference.radio_buttons(ui);
    let chosen = ThemeMode::from_egui(preference);
    if chosen != settings.theme() {
        settings.set_theme(chosen);
        settings.apply(ui.ctx());
    }
}

fn status_strip(ui: &mut egui::Ui, snapshot: &Snapshot) {
    egui::Panel::top("status").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Status").strong());
            // The login form has its own Cancel. One Cancel on screen only.
            ui.label(&snapshot.status_text);
        });
        if let Some(banner) = super::auth::stub_banner(snapshot) {
            ui.colored_label(Color32::from_rgb(214, 160, 64), banner);
        }
        if let Some(error) = &snapshot.error {
            let happened = &error.happened;
            let why = &error.why;
            let next = &error.next;
            ui.colored_label(
                Color32::from_rgb(200, 80, 80),
                format!("What happened: {happened}"),
            );
            ui.label(format!("Why: {why}"));
            ui.label(format!("What to do: {next}"));
        }
    });
}

fn left_panel(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    egui::Panel::left("switcher")
        .resizable(true)
        .default_size(280.0)
        .size_range(220.0..=400.0)
        .show(ui, |ui| {
            ui.heading("Accounts");
            ui.separator();

            let unread_by_protocol: Vec<u32> = snapshot
                .accounts
                .iter()
                .map(|account| snapshot.unread_for(account.caps.id))
                .collect();
            let mut clicked: Option<ProtocolId> = None;
            for (account, unread) in snapshot.accounts.iter().zip(unread_by_protocol) {
                if !snapshot.shows_in_switcher(account.caps.id) {
                    continue;
                }
                if account_chip(
                    ui,
                    account,
                    snapshot.selected_protocol == account.caps.id,
                    unread,
                ) {
                    clicked = Some(account.caps.id);
                }
                ui.add_space(4.0);
            }
            if let Some(protocol) = clicked {
                snapshot.select_protocol(protocol);
            }

            #[cfg(feature = "whatsapp-web")]
            if snapshot.whatsapp_pairing_available() {
                super::whatsapp_gate::risk_entry(ui, snapshot);
            }

            ui.add_space(8.0);
            ui.heading("Inbox");
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("inbox")
                .auto_shrink([false, false])
                .show(ui, |ui| inbox(ui, snapshot));
        });
}

fn account_chip(ui: &mut egui::Ui, account: &AccountRow, selected: bool, unread: u32) -> bool {
    let caps = account.caps;
    let mark = if account.linked { "●" } else { "○" };
    let name = caps.id.display_name();
    let label = if unread == 0 {
        format!("{mark} {name}")
    } else {
        format!("{mark} {name}  ({unread})")
    };
    let response = ui.selectable_label(selected, label);
    ui.colored_label(support_color(caps.support), caps.short_label);
    ui.label(
        RichText::new(format!("status: {}", account.status.as_str()))
            .small()
            .color(MUTED),
    );
    if caps.id == ProtocolId::WhatsApp && cfg!(feature = "whatsapp-web") {
        ui.label(
            RichText::new("experimental spike — ban risk")
                .small()
                .color(MUTED),
        );
    } else if !matches!(caps.id, ProtocolId::Telegram) {
        ui.label(
            RichText::new("not ready — no login UI this beat")
                .small()
                .color(MUTED),
        );
    }
    response.clicked()
}

fn inbox(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    let rows: Vec<(String, String, String, u32)> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| {
            (
                row.id.clone(),
                row.title.clone(),
                row.preview.clone(),
                row.unread,
            )
        })
        .collect();

    match snapshot.inbox_state() {
        InboxState::Rows => {}
        InboxState::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Loading chats…").color(MUTED));
            });
            return;
        }
        InboxState::Empty => {
            ui.label(RichText::new("No chats.").italics().color(MUTED));
            return;
        }
        InboxState::NoMatch => {
            let query = snapshot.search.trim();
            ui.label(
                RichText::new(format!("No chats match '{query}'."))
                    .italics()
                    .color(MUTED),
            );
            return;
        }
    }

    let scroll_to_selected = snapshot.take_scroll_to_selected();
    let mut clicked: Option<String> = None;
    for (id, title, preview, unread) in rows {
        let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
        let label = if unread == 0 {
            title
        } else {
            format!("{title}  ({unread})")
        };
        let response = ui.selectable_label(selected, label);
        if selected && scroll_to_selected {
            response.scroll_to_me(None);
        }
        if response.clicked() {
            clicked = Some(id);
        }
        ui.label(RichText::new(preview).small().weak());
    }
    if let Some(id) = clicked {
        snapshot.select_conversation(id);
    }
}

fn center_panel(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    egui::CentralPanel::default().show(ui, |ui| match snapshot.center_view() {
        CenterView::Auth => auth::draw(ui, snapshot, secrets),
        CenterView::Resuming { connecting } => resuming(ui, connecting),
        CenterView::FirstRun => first_run(ui, snapshot, secrets),
        CenterView::Thread => thread(ui, snapshot),
    });
}

fn resuming(ui: &mut egui::Ui, connecting: bool) {
    let top = (ui.available_height() * 0.18).clamp(24.0, 96.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.spinner();
        if connecting {
            ui.add_space(8.0);
            ui.label(RESUME_CONNECTING);
        }
    });
}

fn first_run(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    let top = (ui.available_height() * 0.18).clamp(24.0, 96.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.heading(RichText::new("Start with Telegram").size(22.0));
        ui.add_space(12.0);
        if let Some(banner) = super::auth::stub_banner(snapshot) {
            ui.colored_label(EXPERIMENTAL, banner);
            ui.add_space(10.0);
        }
        if snapshot.has_api_credentials(secrets) {
            ui.label("Sign in with your phone number, then the login code, then optional 2FA.");
        } else {
            ui.label(
                "Credentials missing. Official binaries inject them at release time. Dev: rebuild with TELEGRAM_API_ID and TELEGRAM_API_HASH, or use Advanced to set a keychain override.",
            );
        }
        ui.add_space(16.0);
        if ui.button("Add Telegram").clicked() {
            snapshot.open_telegram(secrets);
        }
    });
}

fn thread(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    match snapshot.selected_conversation_row() {
        Some(conversation) => {
            ui.heading(&conversation.title);
        }
        None => {
            ui.heading("Thread");
            ui.label("Select a conversation in the inbox.");
        }
    }
    ui.separator();

    let messages: Vec<(String, bool, String, String, Delivery)> = snapshot
        .selected_messages()
        .iter()
        .map(|message| {
            (
                message.id.clone(),
                message.outbound,
                message.sender.clone(),
                message.body.clone(),
                message.delivery,
            )
        })
        .collect();

    // Compose grows to COMPOSE_MAX_ROWS lines, then scrolls. Reserve its height.
    let row_height = ui.text_style_height(&egui::TextStyle::Body);
    let compose_rows = snapshot.compose.lines().count().clamp(1, COMPOSE_MAX_ROWS);
    let compose_height = row_height * COMPOSE_MAX_ROWS as f32;
    let reserve = row_height * compose_rows as f32 + 32.0;

    let state = snapshot.thread_state();
    let mut retry: Option<String> = None;
    // One scroll state per chat, so each chat opens at its newest message.
    let salt = snapshot.selected_conversation.clone().unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt(("thread", salt))
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .max_height(ui.available_height() - reserve)
        .show(ui, |ui| {
            match state {
                ThreadState::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Loading messages…").color(MUTED));
                    });
                }
                ThreadState::Empty => {
                    ui.label(
                        RichText::new("No messages in this chat.")
                            .italics()
                            .color(MUTED),
                    );
                }
                ThreadState::NoSelection | ThreadState::Rows => {}
            }
            for (id, outbound, sender, body, delivery) in messages {
                ui.group(|ui| {
                    let who = if outbound { "you" } else { sender.as_str() };
                    ui.strong(who);
                    ui.label(body);
                    match delivery {
                        Delivery::Sent => {}
                        Delivery::Pending => {
                            ui.label(RichText::new("Sending…").small().color(MUTED));
                        }
                        Delivery::Failed => {
                            ui.horizontal(|ui| {
                                ui.colored_label(FAILED, RichText::new("Not sent").small());
                                if ui.small_button("Retry").clicked() {
                                    retry = Some(id.clone());
                                }
                            });
                        }
                    }
                });
                ui.add_space(4.0);
            }
        });
    if let Some(id) = retry {
        snapshot.retry_send(&id);
    }

    ui.separator();
    compose(ui, snapshot, compose_height);
}

/// Multiline compose. Enter sends; Shift+Enter adds a line.
fn compose(ui: &mut egui::Ui, snapshot: &mut Snapshot, max_height: f32) {
    let compose_id = egui::Id::new("thread-compose");
    if snapshot.take_focus_compose() {
        ui.memory_mut(|memory| memory.request_focus(compose_id));
    }
    if ui.memory(|memory| memory.has_focus(compose_id)) {
        let (enter, shift, other) = ui.input(|input| {
            let modifiers = input.modifiers;
            (
                input.key_pressed(egui::Key::Enter),
                modifiers.shift,
                modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd,
            )
        });
        // Eat plain Enter before the text field sees it, so it does not add a line.
        if enter && !other && snapshot.compose_enter(shift) {
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        }
    }
    ui.horizontal(|ui| {
        let width = ui.available_width() - 72.0;
        egui::ScrollArea::vertical()
            .id_salt("thread-compose-scroll")
            .max_height(max_height)
            .max_width(width)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut snapshot.compose)
                        .id(compose_id)
                        .desired_rows(1)
                        .desired_width(width)
                        .hint_text("Message"),
                );
            });
        if ui
            .add_enabled(snapshot.can_send(), egui::Button::new("Send"))
            .clicked()
        {
            snapshot.send_compose();
        }
    });
}

fn support_color(support: SupportClass) -> Color32 {
    match support {
        SupportClass::Supported => SUPPORTED,
        SupportClass::Experimental => EXPERIMENTAL,
        SupportClass::Constrained => CONSTRAINED,
    }
}
