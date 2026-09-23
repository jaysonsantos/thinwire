//! Shell: account switcher + inbox on the left, thread in the center.

use chrono::Local;
use eframe::egui::{self, Color32, RichText};
use thinwire_protocol::{Delivery, ProtocolId, SupportClass};

use thinwire_protocol::WhatsAppPhoneVault;

use super::auth;
use super::secrets::SecretStore;
use super::settings::{Settings, ThemeMode};
use super::snapshot::{
    AccountRow, CenterView, InboxFilter, InboxState, RESUME_CONNECTING, Snapshot, ThreadState,
};
use super::thread_layout::{RowLayout, list_time, thread_rows};

const SUPPORTED: Color32 = Color32::from_rgb(96, 176, 128);
const EXPERIMENTAL: Color32 = Color32::from_rgb(214, 160, 64);
const CONSTRAINED: Color32 = Color32::from_rgb(196, 148, 88);
const MUTED: Color32 = Color32::from_rgb(160, 160, 168);
const WARN: Color32 = Color32::from_rgb(214, 160, 64);

/// Shown while the OS keychain is not available. Secrets stay in memory.
pub(crate) const KEYCHAIN_UNAVAILABLE_NOTICE: &str =
    "Sign-in is not saved on this device: keychain unavailable.";

/// One notice for the whole window. `None` while the keychain works or loads.
#[must_use]
pub(crate) fn keychain_notice(secrets: &SecretStore) -> Option<&'static str> {
    secrets.memory_only().then_some(KEYCHAIN_UNAVAILABLE_NOTICE)
}
const FAILED: Color32 = Color32::from_rgb(200, 80, 80);
const COMPOSE_MAX_ROWS: usize = 5;
/// A message bubble uses at most this share of the thread width.
const BUBBLE_WIDTH: f32 = 0.75;

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
    status_strip(ui, snapshot, secrets);
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

fn status_strip(ui: &mut egui::Ui, snapshot: &Snapshot, secrets: &SecretStore) {
    egui::Panel::top("status").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Status").strong());
            // The login form has its own Cancel. One Cancel on screen only.
            ui.label(&snapshot.status_text);
        });
        if let Some(notice) = keychain_notice(secrets) {
            ui.colored_label(WARN, notice);
        }
        if let Some(banner) = super::auth::stub_banner(snapshot) {
            ui.colored_label(WARN, banner);
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
    let now = Local::now();
    let rows: Vec<(String, String, String, u32, String)> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| {
            (
                row.id.clone(),
                row.title.clone(),
                row.preview.clone(),
                row.unread,
                list_time(row.last_at, &now),
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
    for (id, title, preview, unread, time) in rows {
        let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
        let label = if unread == 0 {
            title
        } else {
            format!("{title}  ({unread})")
        };
        let response = ui
            .horizontal(|ui| {
                let response = ui.selectable_label(selected, label);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(time).small().color(MUTED));
                });
                response
            })
            .inner;
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

    let is_group = snapshot
        .selected_conversation_row()
        .is_some_and(|row| row.is_group);
    let layout = thread_rows(snapshot.selected_messages(), is_group, &Local::now());
    let messages: Vec<Bubble> = snapshot
        .selected_messages()
        .iter()
        .zip(layout)
        .map(|(message, layout)| Bubble {
            id: message.id.clone(),
            outbound: message.outbound,
            sender: message.sender.clone(),
            body: message.body.clone(),
            delivery: message.delivery,
            layout,
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
            for message in &messages {
                bubble(ui, message, &mut retry);
            }
        });
    if let Some(id) = retry {
        snapshot.retry_send(&id);
    }

    ui.separator();
    compose(ui, snapshot, compose_height);
}

/// One message row, ready to draw.
struct Bubble {
    id: String,
    outbound: bool,
    sender: String,
    body: String,
    delivery: Delivery,
    layout: RowLayout,
}

/// Own messages sit on the right in the theme selection color. Others sit
/// on the left. Both follow the light or dark theme.
fn bubble(ui: &mut egui::Ui, message: &Bubble, retry: &mut Option<String>) {
    if let Some(day) = &message.layout.day_break {
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(day).small().color(MUTED));
        });
        ui.add_space(2.0);
    }
    let max_width = ui.available_width() * BUBBLE_WIDTH;
    let (align, fill) = if message.outbound {
        (egui::Align::Max, ui.visuals().selection.bg_fill)
    } else {
        (egui::Align::Min, ui.visuals().widgets.inactive.weak_bg_fill)
    };
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        egui::Frame::new()
            .fill(fill)
            .corner_radius(8)
            .inner_margin(egui::Margin::symmetric(10, 6))
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    if message.layout.show_sender {
                        ui.label(RichText::new(&message.sender).small().strong());
                    }
                    ui.add(egui::Label::new(&message.body).selectable(true).wrap());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&message.layout.time).small().color(MUTED));
                        match message.delivery {
                            Delivery::Sent => {}
                            Delivery::Pending => {
                                ui.label(RichText::new("Sending…").small().color(MUTED));
                            }
                            Delivery::Failed => {
                                ui.colored_label(FAILED, RichText::new("Not sent").small());
                                if ui.small_button("Retry").clicked() {
                                    *retry = Some(message.id.clone());
                                }
                            }
                        }
                    });
                });
            });
    });
    ui.add_space(4.0);
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
